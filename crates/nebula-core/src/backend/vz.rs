//! Virtualization.framework backend — hosts the primary Vessel VM.
//!
//! Threading model: VZVirtualMachine must only be touched on the serial dispatch
//! queue it was created with. All access goes through `VzVm::on_queue`, which is
//! what makes the `unsafe impl Send` below sound.

use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::rc::Retained;
use objc2::AnyThread;
use objc2_foundation::{NSArray, NSError, NSString, NSURL};
use objc2_virtualization::*;

use crate::error::{Error, Result};
use crate::spec::{BootSpec, ConsoleSpec, NetSpec, VmSpec};

use super::{VmHandle, VmState, VmmBackend};

const BACKEND: &str = "vz";

pub struct VzBackend;

impl VzBackend {
    pub fn new() -> Self {
        VzBackend
    }
}

impl Default for VzBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl VmmBackend for VzBackend {
    fn name(&self) -> &'static str {
        BACKEND
    }

    fn is_available(&self) -> Result<()> {
        // Hardware/OS support; the entitlement can only be verified by starting a VM.
        if cfg!(target_arch = "aarch64") {
            Ok(())
        } else {
            Err(Error::BackendUnavailable(
                BACKEND,
                "requires Apple Silicon".into(),
            ))
        }
    }

    fn create(&self, spec: &VmSpec) -> Result<Box<dyn VmHandle>> {
        spec.validate()?;
        let queue = DispatchQueue::new(&format!("dev.nebula.vz.{}", spec.name), None);
        let config = build_config(spec)?;
        unsafe { config.validateWithError() }
            .map_err(|e| Error::backend(BACKEND, format!("invalid VZ configuration: {e}")))?;
        let vm = VmCell(unsafe {
            VZVirtualMachine::initWithConfiguration_queue(
                VZVirtualMachine::alloc(),
                &config,
                &queue,
            )
        });
        Ok(Box::new(VzVm {
            name: spec.name.clone(),
            queue,
            vm,
            started: false,
        }))
    }
}

/// Holds the VZVirtualMachine. Safety invariant: the inner object is only ever
/// used from closures executed on the VM's serial dispatch queue.
struct VmCell(Retained<VZVirtualMachine>);
unsafe impl Send for VmCell {}
unsafe impl Sync for VmCell {}

pub struct VzVm {
    name: String,
    queue: DispatchRetained<DispatchQueue>,
    vm: VmCell,
    started: bool,
}

impl VzVm {
    fn on_queue<R: Send>(&self, f: impl FnOnce(&VZVirtualMachine) -> R + Send) -> R {
        let vm = &self.vm;
        let mut out: Option<R> = None;
        let out_ref = &mut out;
        self.queue.exec_sync(move || {
            *out_ref = Some(f(&vm.0));
        });
        out.expect("dispatch_sync closure ran")
    }

    /// Run an async VZ operation that reports through a completion handler and
    /// block until the handler fires.
    fn completion_op(
        &self,
        op_name: &str,
        op: impl FnOnce(&VZVirtualMachine, RcBlock<dyn Fn(*mut NSError)>) + Send,
    ) -> Result<()> {
        let (tx, rx) = mpsc::channel::<std::result::Result<(), String>>();
        self.on_queue(move |vm| {
            let block = RcBlock::new(move |err: *mut NSError| {
                let res = if err.is_null() {
                    Ok(())
                } else {
                    Err(unsafe { &*err }.localizedDescription().to_string())
                };
                let _ = tx.send(res);
            });
            op(vm, block);
        });
        match rx.recv_timeout(Duration::from_secs(60)) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(msg)) => Err(Error::backend(BACKEND, format!("{op_name} failed: {msg}"))),
            Err(_) => Err(Error::backend(
                BACKEND,
                format!("{op_name}: no completion within 60s"),
            )),
        }
    }
}

impl VmHandle for VzVm {
    fn name(&self) -> &str {
        &self.name
    }

    fn state(&self) -> VmState {
        if !self.started {
            return VmState::Created;
        }
        let s = self.on_queue(|vm| unsafe { vm.state() });
        match s {
            VZVirtualMachineState::Stopped => VmState::Stopped,
            VZVirtualMachineState::Running
            | VZVirtualMachineState::Paused
            | VZVirtualMachineState::Pausing
            | VZVirtualMachineState::Resuming => VmState::Running,
            VZVirtualMachineState::Error => VmState::Failed,
            VZVirtualMachineState::Starting => VmState::Starting,
            VZVirtualMachineState::Stopping => VmState::Stopping,
            _ => VmState::Failed,
        }
    }

    fn start(&mut self) -> Result<()> {
        self.completion_op("start", |vm, block| unsafe {
            vm.startWithCompletionHandler(&block);
        })?;
        self.started = true;
        Ok(())
    }

    fn stop(&mut self, force: bool) -> Result<()> {
        if force {
            self.completion_op("stop", |vm, block| unsafe {
                vm.stopWithCompletionHandler(&block);
            })
        } else {
            self.on_queue(|vm| unsafe { vm.requestStopWithError() })
                .map_err(|e| Error::backend(BACKEND, format!("requestStop failed: {e}")))
        }
    }

    fn vsock_connect(&self, port: u32) -> Result<std::os::unix::net::UnixStream> {
        use std::os::fd::FromRawFd;
        let (tx, rx) = mpsc::channel::<std::result::Result<i32, String>>();
        self.on_queue(move |vm| {
            let devices = unsafe { vm.socketDevices() };
            let Some(dev) = devices.firstObject() else {
                let _ = tx.send(Err("VM has no vsock device".into()));
                return;
            };
            let Ok(vsock) = dev.downcast::<VZVirtioSocketDevice>() else {
                let _ = tx.send(Err("socket device is not virtio".into()));
                return;
            };
            let block = RcBlock::new(
                move |conn: *mut VZVirtioSocketConnection, err: *mut NSError| {
                    let res = if conn.is_null() {
                        let msg = if err.is_null() {
                            "vsock connect failed".to_string()
                        } else {
                            unsafe { &*err }.localizedDescription().to_string()
                        };
                        Err(msg)
                    } else {
                        // Dup: VZ owns (and will close) the original descriptor.
                        let fd = unsafe { libc::dup((*conn).fileDescriptor()) };
                        if fd < 0 {
                            Err("dup(vsock fd) failed".into())
                        } else {
                            Ok(fd)
                        }
                    };
                    let _ = tx.send(res);
                },
            );
            unsafe { vsock.connectToPort_completionHandler(port, &block) };
        });
        match rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(fd)) => Ok(unsafe { std::os::unix::net::UnixStream::from_raw_fd(fd) }),
            Ok(Err(msg)) => Err(Error::backend(BACKEND, format!("vsock:{port}: {msg}"))),
            Err(_) => Err(Error::backend(
                BACKEND,
                format!("vsock:{port}: connect timeout"),
            )),
        }
    }

    fn balloon_set_guest_mib(&mut self, target_mib: u64) -> Result<()> {
        let ok = self.on_queue(move |vm| {
            let devices = unsafe { vm.memoryBalloonDevices() };
            let Some(dev) = devices.firstObject() else {
                return false;
            };
            let Ok(traditional) = dev.downcast::<VZVirtioTraditionalMemoryBalloonDevice>() else {
                return false;
            };
            unsafe {
                traditional.setTargetVirtualMachineMemorySize(target_mib * 1024 * 1024);
            }
            true
        });
        if ok {
            Ok(())
        } else {
            Err(Error::backend(
                BACKEND,
                "VM has no traditional memory balloon device",
            ))
        }
    }
}

fn file_url(path: &Path) -> Retained<NSURL> {
    let s = NSString::from_str(&path.to_string_lossy());
    NSURL::fileURLWithPath(&s)
}

fn build_config(spec: &VmSpec) -> Result<Retained<VZVirtualMachineConfiguration>> {
    unsafe {
        let config = VZVirtualMachineConfiguration::new();
        config.setCPUCount(spec.cpus as usize);
        config.setMemorySize(spec.mem_mib * 1024 * 1024);

        // Boot loader: direct kernel boot.
        let BootSpec::Kernel {
            kernel,
            initramfs,
            cmdline,
        } = &spec.boot;
        let boot =
            VZLinuxBootLoader::initWithKernelURL(VZLinuxBootLoader::alloc(), &file_url(kernel));
        boot.setCommandLine(&NSString::from_str(cmdline));
        if let Some(initrd) = initramfs {
            boot.setInitialRamdiskURL(Some(&file_url(initrd)));
        }
        config.setBootLoader(Some(&boot.into_super()));

        // Console -> file (kernel hvc0 via virtio-console serial port).
        if let ConsoleSpec::File(path) = &spec.console {
            // VZFileSerialPortAttachment requires the parent dir; create the file fresh.
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(path, b"")?;
            let attach = VZFileSerialPortAttachment::initWithURL_append_error(
                VZFileSerialPortAttachment::alloc(),
                &file_url(path),
                false,
            )
            .map_err(|e| Error::backend(BACKEND, format!("console attachment: {e}")))?;
            let port = VZVirtioConsoleDeviceSerialPortConfiguration::new();
            port.setAttachment(Some(&attach.into_super()));
            config.setSerialPorts(&NSArray::from_retained_slice(&[Retained::into_super(port)]));
        }

        // Disks.
        if !spec.disks.is_empty() {
            let mut storage: Vec<Retained<VZStorageDeviceConfiguration>> = Vec::new();
            for d in &spec.disks {
                let attach = VZDiskImageStorageDeviceAttachment::initWithURL_readOnly_error(
                    VZDiskImageStorageDeviceAttachment::alloc(),
                    &file_url(&d.path),
                    d.read_only,
                )
                .map_err(|e| Error::backend(BACKEND, format!("disk {}: {e}", d.path.display())))?;
                let blk = VZVirtioBlockDeviceConfiguration::initWithAttachment(
                    VZVirtioBlockDeviceConfiguration::alloc(),
                    &attach.into_super(),
                );
                storage.push(Retained::into_super(blk));
            }
            config.setStorageDevices(&NSArray::from_retained_slice(&storage));
        }

        // virtiofs shares (incl. the Rosetta directory share).
        if !spec.shares.is_empty() || spec.rosetta {
            let mut sharing: Vec<Retained<VZDirectorySharingDeviceConfiguration>> = Vec::new();
            if spec.rosetta {
                if VZLinuxRosettaDirectoryShare::availability()
                    == VZLinuxRosettaAvailability::Installed
                {
                    let share = VZLinuxRosettaDirectoryShare::initWithError(
                        VZLinuxRosettaDirectoryShare::alloc(),
                    )
                    .map_err(|e| Error::backend(BACKEND, format!("rosetta share: {e}")))?;
                    let fs = VZVirtioFileSystemDeviceConfiguration::initWithTag(
                        VZVirtioFileSystemDeviceConfiguration::alloc(),
                        &NSString::from_str("rosetta"),
                    );
                    fs.setShare(Some(&share.into_super()));
                    sharing.push(Retained::into_super(fs));
                } else {
                    // Not fatal: amd64 support degrades, arm64 still works.
                    tracing::warn!(
                        "Rosetta is not installed (run `softwareupdate --install-rosetta`); \
                         amd64 containers will not work"
                    );
                }
            }
            for s in &spec.shares {
                let tag = NSString::from_str(&s.tag);
                VZVirtioFileSystemDeviceConfiguration::validateTag_error(&tag)
                    .map_err(|e| Error::backend(BACKEND, format!("share tag `{}`: {e}", s.tag)))?;
                let dir = VZSharedDirectory::initWithURL_readOnly(
                    VZSharedDirectory::alloc(),
                    &file_url(&s.host_path),
                    s.read_only,
                );
                let share = VZSingleDirectoryShare::initWithDirectory(
                    VZSingleDirectoryShare::alloc(),
                    &dir,
                );
                let fs = VZVirtioFileSystemDeviceConfiguration::initWithTag(
                    VZVirtioFileSystemDeviceConfiguration::alloc(),
                    &tag,
                );
                fs.setShare(Some(&share.into_super()));
                sharing.push(Retained::into_super(fs));
            }
            config.setDirectorySharingDevices(&NSArray::from_retained_slice(&sharing));
        }

        // Network.
        if matches!(spec.net, NetSpec::Nat) {
            let net = VZVirtioNetworkDeviceConfiguration::new();
            net.setAttachment(Some(&Retained::into_super(
                VZNATNetworkDeviceAttachment::new(),
            )));
            config.setNetworkDevices(&NSArray::from_retained_slice(&[Retained::into_super(net)]));
        }

        // vsock.
        if spec.vsock {
            let vsock = VZVirtioSocketDeviceConfiguration::new();
            config.setSocketDevices(&NSArray::from_retained_slice(&[Retained::into_super(
                vsock,
            )]));
        }

        // Balloon.
        if spec.balloon {
            let balloon = VZVirtioTraditionalMemoryBalloonDeviceConfiguration::new();
            config.setMemoryBalloonDevices(&NSArray::from_retained_slice(&[Retained::into_super(
                balloon,
            )]));
        }

        // Entropy.
        if spec.rng {
            let rng = VZVirtioEntropyDeviceConfiguration::new();
            config.setEntropyDevices(&NSArray::from_retained_slice(&[Retained::into_super(rng)]));
        }

        Ok(config)
    }
}
