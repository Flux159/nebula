// Copyright 2019 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Enables pre-boot setup, instantiation and booting of a Firecracker VMM.

use crossbeam_channel::Sender;
#[cfg(target_os = "macos")]
use crossbeam_channel::unbounded;
use kernel::cmdline::Cmdline;
#[cfg(target_os = "macos")]
use std::collections::HashMap;
use std::fmt::{Display, Formatter};
use std::fs::File;
use std::io::{self, IsTerminal, Read};
#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::fd::{BorrowedFd, FromRawFd};
#[cfg(windows)]
use std::os::windows::io::{AsRawHandle, BorrowedHandle, FromRawHandle};
use std::path::PathBuf;
use std::sync::atomic::AtomicI32;
use std::sync::{Arc, Mutex};
#[cfg(windows)]
use utils::windows::SendHandle;
#[cfg(windows)]
use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
#[cfg(windows)]
use windows_sys::Win32::System::Console::{
    GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
};

use super::{Error, Vmm};

#[cfg(target_arch = "x86_64")]
use crate::device_manager::legacy::PortIODeviceManager;
use crate::device_manager::mmio::MMIODeviceManager;
use crate::resources::{
    DefaultVirtioConsoleConfig, PortConfig, TsiFlags, VirtioConsoleConfigMode, VmResources,
};
use crate::vmm_config::external_kernel::{ExternalKernel, KernelFormat};
#[cfg(feature = "net")]
use crate::vmm_config::net::NetBuilder;
#[cfg(target_arch = "x86_64")]
use devices::legacy::Cmos;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
use devices::legacy::IoApic;
#[cfg(target_arch = "x86_64")]
use devices::legacy::IrqChipT;
#[cfg(all(target_os = "linux", target_arch = "riscv64"))]
use devices::legacy::KvmAia;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
use devices::legacy::KvmIoapic;
use devices::legacy::Serial;
#[cfg(target_os = "macos")]
use devices::legacy::VcpuList;
#[cfg(target_os = "macos")]
use devices::legacy::{GicV3, HvfGicV3};
use devices::legacy::{IrqChip, IrqChipDevice};
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
use devices::legacy::{KvmGicV2, KvmGicV3};
use devices::virtio::{MmioTransport, PortDescription, VirtioDevice, Vsock, port_io};

#[cfg(feature = "tee")]
use kbs_types::Tee;

use crate::device_manager;
#[cfg(all(feature = "vhost-user", target_os = "linux"))]
use crate::resources::VhostUserDeviceConfig;
#[cfg(target_os = "linux")]
use crate::signal_handler::register_sigint_handler;
#[cfg(target_os = "linux")]
use crate::signal_handler::register_sigwinch_handler;
use crate::terminal::{term_restore_mode, term_set_raw_mode};
#[cfg(feature = "blk")]
use crate::vmm_config::block::BlockBuilder;
#[cfg(all(unix, not(any(feature = "tee", feature = "aws-nitro"))))]
use crate::vmm_config::fs::FsDeviceConfig;
use crate::vmm_config::kernel_cmdline::DEFAULT_KERNEL_CMDLINE;
#[cfg(target_os = "linux")]
use crate::vstate::KvmContext;
#[cfg(all(target_os = "linux", feature = "tee"))]
use crate::vstate::MeasuredRegion;
use crate::vstate::{Error as VstateError, Vcpu, VcpuConfig, Vm};
use arch::{ArchMemoryInfo, InitrdConfig};
use device_manager::shm::ShmManager;
#[cfg(not(any(feature = "tee", feature = "aws-nitro")))]
use devices::virtio::VirtioShmRegion;
#[cfg(feature = "gpu")]
use devices::virtio::display::DisplayInfo;
#[cfg(feature = "gpu")]
use devices::virtio::display::NoopDisplayBackend;
#[cfg(all(unix, not(any(feature = "tee", feature = "aws-nitro"))))]
use devices::virtio::fs::ExportTable;
use flate2::read::GzDecoder;
#[cfg(feature = "gpu")]
use krun_display::DisplayBackend;
#[cfg(feature = "gpu")]
use krun_display::IntoDisplayBackend;
#[cfg(feature = "amd-sev")]
use kvm_bindings::KVM_MAX_CPUID_ENTRIES;
#[cfg(unix)]
use libc::{STDERR_FILENO, STDIN_FILENO, STDOUT_FILENO};
#[cfg(target_arch = "x86_64")]
use linux_loader::loader::{self, KernelLoader};
#[cfg(unix)]
use nix::unistd::isatty;
use polly::event_manager::{Error as EventManagerError, EventManager};
use utils::eventfd::EventFd;
use utils::worker_message::WorkerMessage;
#[cfg(not(any(feature = "tee", feature = "aws-nitro")))]
use vm_memory::Address;
use vm_memory::Bytes;
#[cfg(all(feature = "vhost-user", target_os = "linux"))]
use vm_memory::FileOffset;
#[cfg(not(feature = "aws-nitro"))]
use vm_memory::GuestMemory;
#[cfg(all(target_arch = "x86_64", not(feature = "tee")))]
use vm_memory::GuestRegionMmap;
#[cfg(all(target_arch = "x86_64", not(feature = "tee")))]
use vm_memory::mmap::MmapRegion;
use vm_memory::{GuestAddress, GuestMemoryMmap};

#[cfg(all(target_arch = "x86_64", target_os = "windows"))]
use arch::x86_64::layout::AP_TRAMPOLINE_START;

/// Errors associated with starting the instance.
#[derive(Debug)]
pub enum StartMicrovmError {
    /// Unable to attach block device to Vmm.
    AttachBlockDevice(io::Error),
    #[cfg(target_os = "macos")]
    /// Failed to create HVF in-kernel IrqChip.
    CreateHvfIrqChip(hvf::Error),
    #[cfg(target_os = "linux")]
    /// Failed to create KVM in-kernel IrqChip.
    CreateKvmIrqChip(kvm_ioctls::Error),
    /// Failed to create a `RateLimiter` object.
    CreateRateLimiter(io::Error),
    /// Cannot open the file containing the kernel code.
    ElfOpenKernel(io::Error),
    /// Cannot load the kernel into the VM.
    ElfLoadKernel(linux_loader::loader::Error),
    /// The firmware can't be loaded into the provided memory address.
    FirmwareInvalidAddress(vm_memory::GuestMemoryError),
    /// Cannot read firmware contents from file.
    FirmwareRead(io::Error),
    /// Memory regions are overlapping or mmap fails.
    GuestMemoryMmap(String),
    /// The BZIP2 decoder couldn't decompress the kernel.
    ImageBz2Decoder(io::Error),
    /// Cannot find compressed kernel in file.
    ImageBz2Invalid,
    /// Cannot load the kernel from the uncompressed ELF data.
    ImageBz2LoadKernel(linux_loader::loader::Error),
    /// Cannot open the file containing the kernel code.
    ImageBz2OpenKernel(io::Error),
    /// The GZIP decoder couldn't decompress the kernel.
    ImageGzDecoder(io::Error),
    /// Cannot find compressed kernel in file.
    ImageGzInvalid,
    /// Cannot load the kernel from the uncompressed ELF data.
    ImageGzLoadKernel(linux_loader::loader::Error),
    /// Cannot open the file containing the kernel code.
    ImageGzOpenKernel(io::Error),
    /// The ZSTD decoder couldn't decompress the kernel.
    ImageZstdDecoder(io::Error),
    /// Cannot find compressed kernel in file.
    ImageZstdInvalid,
    /// Cannot load the kernel from the uncompressed ELF data.
    ImageZstdLoadKernel(linux_loader::loader::Error),
    /// Cannot open the file containing the kernel code.
    ImageZstdOpenKernel(io::Error),
    /// Cannot load initrd due to an invalid memory configuration.
    InitrdLoad,
    /// Cannot load initrd due to an invalid image.
    InitrdRead(io::Error),
    /// Internal error encountered while starting a microVM.
    Internal(Error),
    /// Cannot inject the kernel into the guest memory due to a problem with the bundle.
    InvalidKernelBundle(vm_memory::mmap::MmapRegionError),
    /// The kernel command line is invalid.
    KernelCmdline(String),
    /// The kernel doesn't fit into the microVM memory.
    KernelDoesNotFit(u64, usize),
    /// The supplied kernel format is not supported.
    KernelFormatUnsupported,
    /// Cannot load command line string.
    LoadCommandline(kernel::cmdline::Error),
    /// The start command was issued more than once.
    MicroVMAlreadyRunning,
    /// Cannot start the VM because the kernel was not configured.
    MissingKernelConfig,
    /// Cannot start the VM because the size of the guest memory  was not specified.
    MissingMemSizeConfig,
    /// The net device configuration is missing the tap device.
    NetDeviceNotConfigured,
    /// Cannot open the block device backing file.
    OpenBlockDevice(io::Error),
    /// Cannot open console output file.
    OpenConsoleFile(io::Error),
    /// The GZIP decoder couldn't decompress the kernel.
    PeGzDecoder(io::Error),
    /// Cannot open the file containing the kernel code.
    PeGzOpenKernel(io::Error),
    /// Cannot find compressed kernel in file.
    PeGzInvalid,
    /// Cannot open the file containing the kernel code.
    RawOpenKernel(io::Error),
    /// Cannot initialize a MMIO Balloon device or add a device to the MMIO Bus.
    RegisterBalloonDevice(device_manager::mmio::Error),
    /// Cannot initialize a MMIO Block Device or add a device to the MMIO Bus.
    RegisterBlockDevice(device_manager::mmio::Error),
    /// Cannot register an EventHandler.
    RegisterEvent(EventManagerError),
    /// Cannot initialize a MMIO Fs Device or add ad device to the MMIO Bus.
    RegisterFsDevice(device_manager::mmio::Error),
    // Cannot initialize a MMIO Fs Device or add ad device to the MMIO Bus.
    RegisterConsoleDevice(device_manager::mmio::Error),
    /// Cannot register SIGWINCH event file descriptor.
    #[cfg(target_os = "linux")]
    RegisterFsSigwinch(kvm_ioctls::Error),
    /// Cannot initialize a MMIO Gpu device or add a device to the MMIO Bus.
    RegisterGpuDevice(device_manager::mmio::Error),
    /// Cannot initialize a MMIO Input device or add a device to the MMIO Bus.
    RegisterInputDevice(device_manager::mmio::Error),
    /// Cannot initialize a MMIO Network Device or add a device to the MMIO Bus.
    RegisterNetDevice(device_manager::mmio::Error),
    /// Cannot initialize a MMIO Rng device or add a device to the MMIO Bus.
    RegisterRngDevice(device_manager::mmio::Error),
    /// Cannot initialize a vhost-user device or add a device to the MMIO Bus.
    RegisterVhostUserDevice(device_manager::mmio::Error),
    /// Cannot initialize a MMIO Vsock Device or add a device to the MMIO Bus.
    RegisterVsockDevice(device_manager::mmio::Error),
    /// Cannot restore the VM from a snapshot directory.
    RestoreSnapshot(String),
    /// Cannot attest the VM in the Secure Virtualization context.
    SecureVirtAttest(VstateError),
    /// Cannot initialize the Secure Virtualization backend.
    SecureVirtPrepare(VstateError),
    /// Error configuring an SHM region.
    ShmConfig(device_manager::shm::Error),
    /// Error creating an SHM region.
    ShmCreate(device_manager::shm::Error),
    /// Error obtaining the host address of an SHM region.
    ShmHostAddr(vm_memory::GuestMemoryError),
    /// The TEE specified is not supported.
    InvalidTee,
}

/// It's convenient to automatically convert `kernel::cmdline::Error`s
/// to `StartMicrovmError`s.
impl std::convert::From<kernel::cmdline::Error> for StartMicrovmError {
    fn from(e: kernel::cmdline::Error) -> StartMicrovmError {
        StartMicrovmError::KernelCmdline(e.to_string())
    }
}

impl Display for StartMicrovmError {
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        use self::StartMicrovmError::*;
        match *self {
            AttachBlockDevice(ref err) => {
                write!(f, "Unable to attach block device to Vmm. Error: {err}")
            }
            #[cfg(target_os = "macos")]
            CreateHvfIrqChip(ref err) => {
                write!(f, "Cannot create HVF in-kernel IrqChip: {err}")
            }
            #[cfg(target_os = "linux")]
            CreateKvmIrqChip(ref err) => {
                write!(f, "Cannot create KVM in-kernel IrqChip: {err}")
            }
            CreateRateLimiter(ref err) => write!(f, "Cannot create RateLimiter: {err}"),
            ElfOpenKernel(ref err) => {
                write!(f, "Cannot open the file containing the kernel code: {err}")
            }
            ElfLoadKernel(ref err) => {
                write!(f, "Cannot load the kernel into the VM: {err}")
            }
            FirmwareInvalidAddress(ref err) => {
                write!(
                    f,
                    "The firmware can't be loaded into the guest memory: {err}"
                )
            }
            FirmwareRead(ref err) => {
                write!(f, "Cannot read firmware contents from file: {err}")
            }
            GuestMemoryMmap(ref err) => {
                // Remove imbricated quotes from error message.
                let mut err_msg = format!("{err:?}");
                err_msg = err_msg.replace('\"', "");
                write!(f, "Invalid Memory Configuration: {err_msg}")
            }
            ImageBz2Decoder(ref err) => {
                write!(f, "The BZIP2 decoder couldn't decompress the kernel. {err}")
            }
            ImageBz2Invalid => {
                write!(f, "Cannot find compressed kernel in file.")
            }
            ImageBz2LoadKernel(ref err) => {
                write!(
                    f,
                    "Cannot load the kernel from the uncompressed ELF data. {err}"
                )
            }
            ImageBz2OpenKernel(ref err) => {
                write!(f, "Cannot open the file containing the kernel code. {err}")
            }
            ImageGzDecoder(ref err) => {
                write!(f, "The GZIP decoder couldn't decompress the kernel. {err}")
            }
            ImageGzInvalid => {
                write!(f, "Cannot find compressed kernel in file.")
            }
            ImageGzLoadKernel(ref err) => {
                write!(
                    f,
                    "Cannot load the kernel from the uncompressed ELF data. {err}"
                )
            }
            ImageGzOpenKernel(ref err) => {
                write!(f, "Cannot open the file containing the kernel code. {err}")
            }
            ImageZstdDecoder(ref err) => {
                write!(f, "The ZSTD decoder couldn't decompress the kernel. {err}")
            }
            ImageZstdInvalid => {
                write!(f, "Cannot find compressed kernel in file.")
            }
            ImageZstdLoadKernel(ref err) => {
                write!(
                    f,
                    "Cannot load the kernel from the uncompressed ELF data. {err}"
                )
            }
            ImageZstdOpenKernel(ref err) => {
                write!(f, "Cannot open the file containing the kernel code. {err}")
            }
            InitrdLoad => write!(
                f,
                "Cannot load initrd due to an invalid memory configuration."
            ),
            InitrdRead(ref err) => write!(f, "Cannot load initrd due to an invalid image: {err}"),
            Internal(ref err) => write!(f, "Internal error while starting microVM: {err:?}"),
            InvalidKernelBundle(ref err) => {
                let mut err_msg = format!("{err}");
                err_msg = err_msg.replace('\"', "");
                write!(
                    f,
                    "Cannot inject the kernel into the guest memory due to a problem with the \
                     bundle. {err_msg}"
                )
            }
            KernelCmdline(ref err) => write!(f, "Invalid kernel command line: {err}"),
            KernelDoesNotFit(load_addr, size) => write!(
                f,
                "The kernel doesn't fit in the microVM memory (load_addr={load_addr}, size={size})"
            ),
            KernelFormatUnsupported => {
                write!(f, "The supplied kernel format is not supported.")
            }
            LoadCommandline(ref err) => {
                let mut err_msg = format!("{err}");
                err_msg = err_msg.replace('\"', "");
                write!(f, "Cannot load command line string. {err_msg}")
            }
            MicroVMAlreadyRunning => write!(f, "Microvm already running."),
            MissingKernelConfig => write!(f, "Cannot start microvm without kernel configuration."),
            MissingMemSizeConfig => {
                write!(f, "Cannot start microvm without guest mem_size config.")
            }
            NetDeviceNotConfigured => {
                write!(f, "The net device configuration is missing the tap device.")
            }
            OpenBlockDevice(ref err) => {
                let mut err_msg = format!("{err:?}");
                err_msg = err_msg.replace('\"', "");

                write!(f, "Cannot open the block device backing file. {err_msg}")
            }
            OpenConsoleFile(ref err) => {
                let mut err_msg = format!("{err:?}");
                err_msg = err_msg.replace('\"', "");

                write!(f, "Cannot open the console output file. {err_msg}")
            }
            PeGzDecoder(ref err) => {
                write!(f, "The GZIP decoder couldn't decompress the kernel. {err}")
            }
            PeGzOpenKernel(ref err) => {
                write!(f, "Cannot open the file containing the kernel code. {err}")
            }
            PeGzInvalid => {
                write!(f, "Cannot find compressed kernel in file.")
            }
            RawOpenKernel(ref err) => {
                write!(f, "Cannot open the file containing the kernel code: {err}")
            }
            RegisterBalloonDevice(ref err) => {
                let mut err_msg = format!("{err}");
                err_msg = err_msg.replace('\"', "");
                write!(
                    f,
                    "Cannot initialize a MMIO Balloon Device or add a device to the MMIO Bus. {err_msg}"
                )
            }
            RegisterBlockDevice(ref err) => {
                let mut err_msg = format!("{err}");
                err_msg = err_msg.replace('\"', "");
                write!(
                    f,
                    "Cannot initialize a MMIO Block Device or add a device to the MMIO Bus. {err_msg}"
                )
            }
            RegisterEvent(ref err) => write!(f, "Cannot register EventHandler. {err:?}"),
            RegisterFsDevice(ref err) => {
                let mut err_msg = format!("{err}");
                err_msg = err_msg.replace('\"', "");

                write!(
                    f,
                    "Cannot initialize a MMIO Fs Device or add a device to the MMIO Bus. {err_msg}"
                )
            }
            RegisterConsoleDevice(ref err) => {
                let mut err_msg = format!("{err}");
                err_msg = err_msg.replace('\"', "");

                write!(
                    f,
                    "Cannot initialize a MMIO Console Device or add a device to the MMIO Bus. {err_msg}"
                )
            }
            #[cfg(target_os = "linux")]
            RegisterFsSigwinch(ref err) => {
                let mut err_msg = format!("{err}");
                err_msg = err_msg.replace('\"', "");

                write!(
                    f,
                    "Cannot register SIGWINCH file descriptor for Fs Device. {err_msg}"
                )
            }
            RegisterGpuDevice(ref err) => {
                let mut err_msg = format!("{err}");
                err_msg = err_msg.replace('\"', "");
                write!(
                    f,
                    "Cannot initialize a MMIO Gpu Device or add a device to the MMIO Bus. {err_msg}"
                )
            }
            RegisterInputDevice(ref err) => {
                let mut err_msg = format!("{err}");
                err_msg = err_msg.replace('\"', "");
                write!(
                    f,
                    "Cannot initialize a MMIO Input Device or add a device to the MMIO Bus. {err_msg}"
                )
            }
            RegisterNetDevice(ref err) => {
                let mut err_msg = format!("{err}");
                err_msg = err_msg.replace('\"', "");

                write!(
                    f,
                    "Cannot initialize a MMIO Network Device or add a device to the MMIO Bus. {err_msg}"
                )
            }
            RegisterRngDevice(ref err) => {
                let mut err_msg = format!("{err}");
                err_msg = err_msg.replace('\"', "");
                write!(
                    f,
                    "Cannot initialize a MMIO Rng Device or add a device to the MMIO Bus. {err_msg}"
                )
            }
            RegisterVhostUserDevice(ref err) => {
                let mut err_msg = err.to_string();
                err_msg = err_msg.replace('\"', "");
                write!(
                    f,
                    "Cannot initialize a vhost-user device or add a device to the MMIO Bus. {err_msg}"
                )
            }
            RegisterVsockDevice(ref err) => {
                let mut err_msg = format!("{err}");
                err_msg = err_msg.replace('\"', "");

                write!(
                    f,
                    "Cannot initialize a MMIO Vsock Device or add a device to the MMIO Bus. {err_msg}"
                )
            }
            RestoreSnapshot(ref err) => {
                write!(f, "Cannot restore the VM from a snapshot: {err}")
            }
            SecureVirtAttest(ref err) => {
                let mut err_msg = format!("{err}");
                err_msg = err_msg.replace('\"', "");

                write!(
                    f,
                    "Cannot attest the VM in the Secure Virtualization context. {err_msg}"
                )
            }
            SecureVirtPrepare(ref err) => {
                let mut err_msg = format!("{err}");
                err_msg = err_msg.replace('\"', "");

                write!(
                    f,
                    "Cannot initialize the Secure Virtualization backend. {err_msg}"
                )
            }
            ShmHostAddr(ref err) => {
                let mut err_msg = format!("{err:?}");
                err_msg = err_msg.replace('\"', "");

                write!(
                    f,
                    "Error obtaining the host address of an SHM region. {err_msg}"
                )
            }
            ShmConfig(ref err) => {
                let mut err_msg = format!("{err:?}");
                err_msg = err_msg.replace('\"', "");

                write!(f, "Error while configuring an SHM region. {err_msg}")
            }
            ShmCreate(ref err) => {
                let mut err_msg = format!("{err:?}");
                err_msg = err_msg.replace('\"', "");

                write!(f, "Error while creating an SHM region. {err_msg}")
            }
            InvalidTee => {
                write!(f, "TEE selected is not currently supported")
            }
        }
    }
}

pub enum Payload {
    #[cfg(all(unix, target_arch = "x86_64", not(feature = "tee")))]
    KernelMmap,
    #[cfg(any(target_arch = "aarch64", target_arch = "riscv64"))]
    KernelCopy,
    ExternalKernel(ExternalKernel),
    #[cfg(test)]
    Empty,
    Firmware,
    #[cfg(feature = "tee")]
    Tee,
}

fn choose_payload(vm_resources: &VmResources) -> Result<Payload, StartMicrovmError> {
    if let Some(_kernel_bundle) = &vm_resources.kernel_bundle {
        #[cfg(feature = "tee")]
        if vm_resources.qboot_bundle.is_none() || vm_resources.initrd_bundle.is_none() {
            return Err(StartMicrovmError::MissingKernelConfig);
        }

        #[cfg(feature = "tee")]
        return Ok(Payload::Tee);

        #[cfg(all(target_os = "linux", target_arch = "x86_64", not(feature = "tee")))]
        return Ok(Payload::KernelMmap);

        #[cfg(any(target_arch = "aarch64", target_arch = "riscv64"))]
        return Ok(Payload::KernelCopy);

        // Embedded kernel bundles aren't supported on Windows; pass an
        // external kernel via krun_set_kernel (nebula always does).
        #[cfg(target_os = "windows")]
        return Err(StartMicrovmError::MissingKernelConfig);
    } else if let Some(external_kernel) = vm_resources.external_kernel() {
        Ok(Payload::ExternalKernel(external_kernel.clone()))
    } else if vm_resources.firmware_config.is_some() {
        Ok(Payload::Firmware)
    } else {
        Err(StartMicrovmError::MissingKernelConfig)
    }
}

/// Builds and starts a microVM based on the current Firecracker VmResources configuration.
///
/// This is the default build recipe, one could build other microVM flavors by using the
/// independent functions in this module instead of calling this recipe.
///
/// An `Arc` reference of the built `Vmm` is also plugged in the `EventManager`, while another
/// is returned.
pub fn build_microvm(
    vm_resources: &super::resources::VmResources,
    event_manager: &mut EventManager,
    _shutdown_efd: Option<EventFd>,
    _sender: Sender<WorkerMessage>,
) -> std::result::Result<Arc<Mutex<Vmm>>, StartMicrovmError> {
    let payload = choose_payload(vm_resources)?;

    let (guest_memory, arch_memory_info, mut _shm_manager, payload_config) = create_guest_memory(
        vm_resources
            .vm_config()
            .mem_size_mib
            .ok_or(StartMicrovmError::MissingMemSizeConfig)?,
        vm_resources,
        &payload,
    )?;

    let vcpu_config = vm_resources.vcpu_config();

    // Clone the command-line so that a failed boot doesn't pollute the original.
    #[allow(unused_mut)]
    let mut kernel_cmdline = Cmdline::new(arch::CMDLINE_MAX_SIZE);
    if let Some(cmdline) = payload_config.kernel_cmdline {
        kernel_cmdline.insert_str(cmdline.as_str()).unwrap();
    } else if let Some(cmdline) = &vm_resources.kernel_cmdline.prolog {
        kernel_cmdline.insert_str(cmdline).unwrap();
    } else {
        kernel_cmdline.insert_str(DEFAULT_KERNEL_CMDLINE).unwrap();
    }

    if let Some(cmdline) = &vm_resources.kernel_cmdline.krun_env {
        kernel_cmdline.insert_str(cmdline.as_str()).unwrap();
    }

    if let Some(kernel_console) = &vm_resources.kernel_console {
        let cmdline = kernel_cmdline.as_str();
        let console_start_idx = cmdline.find("console=").unwrap();
        let console_end_idx = cmdline
            .get(console_start_idx..)
            .and_then(|s| s.find(" ").map(|i| i + console_start_idx));

        let cmdline = cmdline.replace(
            &cmdline[console_start_idx..console_end_idx.unwrap()],
            format!("console={kernel_console}").as_str(),
        );
        kernel_cmdline = Cmdline::new(arch::CMDLINE_MAX_SIZE);
        kernel_cmdline.insert_str(cmdline).unwrap();
    }

    #[cfg(all(unix, not(feature = "tee")))]
    #[allow(unused_mut)]
    let mut vm = setup_vm(&guest_memory, vm_resources.nested_enabled)?;

    #[cfg(target_os = "windows")]
    #[allow(unused_mut)]
    let mut vm = setup_vm(&guest_memory, vcpu_config.vcpu_count)?;

    #[cfg(feature = "tee")]
    let (_kvm, vm) = {
        let kvm = KvmContext::new()
            .map_err(Error::KvmContext)
            .map_err(StartMicrovmError::Internal)?;
        let vm = setup_vm(
            &kvm,
            &guest_memory,
            vm_resources,
            #[cfg(feature = "tdx")]
            _sender.clone(),
        )?;
        (kvm, vm)
    };

    #[cfg(feature = "tee")]
    let tee = vm_resources.tee_config().tee;

    #[cfg(feature = "amd-sev")]
    let snp_launcher = match tee {
        Tee::Snp => Some(
            vm.snp_secure_virt_prepare(&guest_memory)
                .map_err(StartMicrovmError::SecureVirtPrepare)?,
        ),
        _ => None,
    };

    #[cfg(feature = "tdx")]
    let mut tdx_launcher = match tee {
        Tee::Tdx => vm
            .tdx_secure_virt_prepare()
            .map_err(StartMicrovmError::SecureVirtPrepare)?,
        _ => panic!(),
    };

    #[cfg(all(feature = "tee", not(feature = "tdx")))]
    let measured_regions = {
        println!("Injecting and measuring memory regions. This may take a while.");

        let qboot_size = if let Some(qboot_bundle) = &vm_resources.qboot_bundle {
            qboot_bundle.size
        } else {
            return Err(StartMicrovmError::MissingKernelConfig);
        };
        let (kernel_guest_addr, kernel_size) =
            if let Some(kernel_bundle) = &vm_resources.kernel_bundle {
                (kernel_bundle.guest_addr, kernel_bundle.size)
            } else {
                return Err(StartMicrovmError::MissingKernelConfig);
            };
        let (initrd_addr, initrd_size) = if let Some(initrd_config) = &payload_config.initrd_config
        {
            (initrd_config.address, initrd_config.size)
        } else {
            return Err(StartMicrovmError::MissingKernelConfig);
        };

        vec![
            MeasuredRegion {
                guest_addr: arch::FIRMWARE_START,
                host_addr: guest_memory
                    .get_host_address(GuestAddress(arch::FIRMWARE_START))
                    .unwrap() as u64,
                size: qboot_size,
            },
            MeasuredRegion {
                guest_addr: kernel_guest_addr,
                host_addr: guest_memory
                    .get_host_address(GuestAddress(kernel_guest_addr))
                    .unwrap() as u64,
                size: kernel_size,
            },
            MeasuredRegion {
                guest_addr: initrd_addr.0,
                host_addr: guest_memory.get_host_address(initrd_addr).unwrap() as u64,
                size: initrd_size,
            },
            MeasuredRegion {
                guest_addr: arch::x86_64::layout::ZERO_PAGE_START,
                host_addr: guest_memory
                    .get_host_address(GuestAddress(arch::x86_64::layout::ZERO_PAGE_START))
                    .unwrap() as u64,
                size: 4096,
            },
        ]
    };

    #[cfg(feature = "tdx")]
    let measured_regions = {
        println!("Injecting and measuring memory regions. This may take a while.");
        let qboot_size = if let Some(qboot_bundle) = &vm_resources.qboot_bundle {
            qboot_bundle.size
        } else {
            return Err(StartMicrovmError::MissingKernelConfig);
        };
        let m = vec![
            MeasuredRegion {
                guest_addr: 0,
                host_addr: guest_memory.get_host_address(GuestAddress(0)).unwrap() as u64,
                size: 0x8000_0000,
            },
            MeasuredRegion {
                guest_addr: arch::FIRMWARE_START,
                host_addr: guest_memory
                    .get_host_address(GuestAddress(arch::FIRMWARE_START))
                    .unwrap() as u64,
                size: qboot_size,
            },
        ];

        m
    };

    let mut serial_devices = Vec::new();

    // Create the legacy serial device if we're booting from a firmware
    if vm_resources.firmware_config.is_some() && !vm_resources.disable_implicit_console {
        serial_devices.push(setup_serial_device(
            event_manager,
            None,
            None,
            // Uncomment this to get EFI output when debugging EDK2.
            //Some(Box::new(io::stdout())),
        )?);
    };

    // We can't call to `setup_terminal_raw_mode` until `Vmm` is created,
    // so let's keep track of FDs connected to legacy serial devices here
    // and set raw mode on them later.
    let mut serial_ttys = Vec::new();

    #[cfg(unix)]
    for s in &vm_resources.serial_consoles {
        let input: Option<Box<dyn devices::legacy::ReadableFd + Send>> = if s.input_fd >= 0 {
            let file = unsafe { File::from_raw_fd(s.input_fd) };
            if file.is_terminal() {
                serial_ttys.push(unsafe { BorrowedFd::borrow_raw(file.as_raw_fd()) });
            }
            Some(Box::new(file))
        } else {
            None
        };

        let output: Option<Box<dyn io::Write + Send>> = if s.output_fd >= 0 {
            Some(Box::new(unsafe { File::from_raw_fd(s.output_fd) }))
        } else {
            None
        };

        serial_devices.push(setup_serial_device(event_manager, input, output)?);
    }

    #[cfg(windows)]
    for s in &vm_resources.serial_consoles {
        let input: Option<Box<dyn devices::legacy::ReadableFd + Send>> =
            if is_valid_handle(s.input_handle.as_raw_handle()) {
                if unsafe {
                    BorrowedHandle::borrow_raw(s.input_handle.as_raw_handle()).is_terminal()
                } {
                    serial_ttys.push(s.input_handle);
                }
                Some(Box::new(unsafe {
                    File::from_raw_handle(s.input_handle.as_raw_handle())
                }))
            } else {
                None
            };

        let output: Option<Box<dyn io::Write + Send>> =
            if is_valid_handle(s.output_handle.as_raw_handle()) {
                Some(Box::new(unsafe {
                    File::from_raw_handle(s.output_handle.as_raw_handle())
                }))
            } else {
                None
            };

        serial_devices.push(setup_serial_device(event_manager, input, output)?);
    }

    let exit_evt = EventFd::new(utils::eventfd::EFD_NONBLOCK)
        .map_err(Error::EventFd)
        .map_err(StartMicrovmError::Internal)?;

    #[cfg(target_arch = "x86_64")]
    // Safe to unwrap 'serial_device' as it's always 'Some' on x86_64.
    // x86_64 uses the i8042 reset event as the Vmm exit event.
    let mut pio_device_manager = PortIODeviceManager::new(
        Arc::new(Mutex::new(Cmos::new(
            arch_memory_info.ram_below_gap,
            arch_memory_info.ram_above_gap,
        ))),
        serial_devices,
        exit_evt
            .try_clone()
            .map_err(Error::EventFd)
            .map_err(StartMicrovmError::Internal)?,
    )
    .map_err(Error::CreateLegacyDevice)
    .map_err(StartMicrovmError::Internal)?;

    // Instantiate the MMIO device manager.
    // 'mmio_base' address has to be an address which is protected by the kernel
    // and is architectural specific.
    #[allow(unused_mut)]
    let mut mmio_device_manager = MMIODeviceManager::new(
        &mut (arch::MMIO_MEM_START.clone()),
        (arch::IRQ_BASE, arch::IRQ_MAX),
    );

    #[cfg(target_os = "macos")]
    let vcpu_list = {
        let cpu_count = vm_resources.vm_config().vcpu_count.unwrap();
        Arc::new(VcpuList::new(cpu_count as u64))
    };

    let vcpus;
    let intc: IrqChip;
    // For x86_64 we need to create the interrupt controller before calling `KVM_CREATE_VCPUS`
    // while on aarch64 we need to do it the other way around.
    #[cfg(all(target_arch = "x86_64", target_os = "windows"))]
    {
        let ioapic: Box<dyn IrqChipT> =
            Box::new(devices::legacy::WhpIoapic::new(vm.whp_vm().clone()));
        intc = Arc::new(Mutex::new(IrqChipDevice::new(ioapic)));

        attach_legacy_devices(
            &vm,
            &mut pio_device_manager,
            &mut mmio_device_manager,
            Some(intc.clone()),
        )?;

        let kernel_boot = vm_resources.firmware_config.is_none();

        vcpus = create_vcpus_x86_64(
            &vm,
            &vcpu_config,
            &guest_memory,
            payload_config.entry_addr,
            &pio_device_manager.io_bus,
            &exit_evt,
            kernel_boot,
        )
        .map_err(StartMicrovmError::Internal)?;
    }

    #[cfg(all(target_arch = "x86_64", unix))]
    {
        let ioapic: Box<dyn IrqChipT> = if vm_resources.split_irqchip {
            Box::new(
                IoApic::new(vm.fd(), _sender.clone())
                    .map_err(StartMicrovmError::CreateKvmIrqChip)?,
            )
        } else {
            Box::new(KvmIoapic::new(vm.fd()).map_err(StartMicrovmError::CreateKvmIrqChip)?)
        };
        intc = Arc::new(Mutex::new(IrqChipDevice::new(ioapic)));

        attach_legacy_devices(
            &vm,
            vm_resources.split_irqchip,
            &mut pio_device_manager,
            &mut mmio_device_manager,
            Some(intc.clone()),
        )?;

        let kernel_boot = vm_resources.firmware_config.is_none() && !cfg!(feature = "tee");

        vcpus = create_vcpus_x86_64(
            &vm,
            &vcpu_config,
            &guest_memory,
            payload_config.entry_addr,
            &pio_device_manager.io_bus,
            &exit_evt,
            kernel_boot,
            payload_config.pvh,
            #[cfg(feature = "tee")]
            _sender,
        )
        .map_err(StartMicrovmError::Internal)?;
    }

    #[cfg(feature = "tdx")]
    {
        for vcpu in &vcpus {
            vcpu.tdx_secure_virt_prepare(&mut tdx_launcher);
        }
        vm.tdx_secure_virt_init_vcpus(&mut tdx_launcher).unwrap();
    }

    // On aarch64, the vCPUs need to be created (i.e call KVM_CREATE_VCPU) and configured before
    // setting up the IRQ chip because the `KVM_CREATE_VCPU` ioctl will return error if the IRQCHIP
    // was already initialized.
    // Search for `kvm_arch_vcpu_create` in arch/arm/kvm/arm.c.
    #[cfg(all(target_arch = "aarch64", target_os = "linux"))]
    {
        vcpus = create_vcpus_aarch64(
            &vm,
            &vcpu_config,
            &arch_memory_info,
            payload_config.entry_addr,
            &exit_evt,
        )
        .map_err(StartMicrovmError::Internal)?;

        intc = {
            // The SoC in some popular boards (namely, the RPi family) doesn't support an
            // architected vGIC, which is required for requesting KVM the instantiation of a
            // GICv3. To relieve the users from having to configure the gic version manually,
            // try first to instantiate a GICv3, and fall back to a GICv2 if it fails.
            let vcpu_count = vm_resources.vm_config().vcpu_count.unwrap() as u64;
            let gic = match KvmGicV3::new(vm.fd(), vcpu_count) {
                Ok(gicv3) => IrqChipDevice::new(Box::new(gicv3)),
                Err(_) => {
                    warn!("KVM GICv3 creation failed, falling back to KVM GICv2");
                    IrqChipDevice::new(Box::new(KvmGicV2::new(vm.fd(), vcpu_count)))
                }
            };
            Arc::new(Mutex::new(gic))
        };

        attach_legacy_devices(
            &vm,
            &mut mmio_device_manager,
            &mut kernel_cmdline,
            intc.clone(),
            serial_devices,
        )?;
    }

    #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
    {
        intc = {
            // If the system supports the in-kernel GIC, use it. Otherwise, fall back to the
            // userspace implementation.
            let gic = match HvfGicV3::new(vm_resources.vm_config().vcpu_count.unwrap() as u64) {
                Ok(hvfgic) => IrqChipDevice::new(Box::new(hvfgic)),
                Err(_) => IrqChipDevice::new(Box::new(GicV3::new(vcpu_list.clone()))),
            };
            Arc::new(Mutex::new(gic))
        };

        vcpus = create_vcpus_aarch64(
            &vm,
            &vcpu_config,
            &arch_memory_info,
            payload_config.entry_addr,
            &exit_evt,
            vcpu_list.clone(),
            vm_resources.nested_enabled,
        )
        .map_err(StartMicrovmError::Internal)?;

        attach_legacy_devices(
            &vm,
            &mut mmio_device_manager,
            &mut kernel_cmdline,
            intc.clone(),
            serial_devices,
            event_manager,
            _shutdown_efd,
        )?;
    }

    #[cfg(all(target_arch = "riscv64", target_os = "linux"))]
    {
        vcpus = create_vcpus_riscv64(
            &vm,
            &vcpu_config,
            &guest_memory,
            payload_config.entry_addr,
            &exit_evt,
        )
        .map_err(StartMicrovmError::Internal)?;

        intc = Arc::new(Mutex::new(IrqChipDevice::new(Box::new(
            KvmAia::new(vm.fd(), vm_resources.vm_config().vcpu_count.unwrap() as u32).unwrap(),
        ))));

        attach_legacy_devices(
            &vm,
            &mut mmio_device_manager,
            &mut kernel_cmdline,
            intc.clone(),
            serial_devices,
        )?;
    }

    // We use this atomic to record the exit code set by init/init.c in the VM.
    let exit_code = Arc::new(AtomicI32::new(i32::MAX));

    let mut vmm = Vmm {
        guest_memory,
        arch_memory_info,
        kernel_cmdline,
        vcpus_handles: Vec::new(),
        exit_evt,
        exit_observers: Vec::new(),
        exit_code: exit_code.clone(),
        vm,
        mmio_device_manager,
        #[cfg(target_arch = "x86_64")]
        pio_device_manager,
    };

    // Set raw mode for FDs that are connected to legacy serial devices.
    for serial_tty in serial_ttys {
        setup_terminal_raw_mode(&mut vmm, Some(serial_tty), false);
    }

    #[cfg(all(unix, not(feature = "tee")))]
    attach_balloon_device(&mut vmm, event_manager, intc.clone())?;
    #[cfg(all(unix, not(feature = "tee")))]
    {
        #[cfg(all(feature = "vhost-user", target_os = "linux"))]
        {
            const VIRTIO_ID_RNG: u32 = 4;
            for device_config in &vm_resources.vhost_user_devices {
                attach_vhost_user_device(&mut vmm, event_manager, intc.clone(), device_config)?;
            }

            let has_vhost_user_rng = vm_resources
                .vhost_user_devices
                .iter()
                .any(|dev| dev.device_type == VIRTIO_ID_RNG);

            if !has_vhost_user_rng {
                attach_rng_device(&mut vmm, event_manager, intc.clone())?;
            }
        }

        #[cfg(not(all(feature = "vhost-user", target_os = "linux")))]
        {
            attach_rng_device(&mut vmm, event_manager, intc.clone())?;
        }
    }
    let mut console_id = 0;
    if !vm_resources.disable_implicit_console {
        attach_console_devices(
            &mut vmm,
            event_manager,
            intc.clone(),
            vm_resources,
            None,
            console_id,
        )?;
        console_id += 1;
    }

    for console_cfg in vm_resources.virtio_consoles.iter() {
        attach_console_devices(
            &mut vmm,
            event_manager,
            intc.clone(),
            vm_resources,
            Some(console_cfg),
            console_id,
        )?;
        console_id += 1;
    }

    #[cfg(all(unix, not(any(feature = "tee", feature = "aws-nitro"))))]
    let export_table: Option<ExportTable> = if cfg!(feature = "gpu") {
        Some(Default::default())
    } else {
        None
    };

    #[cfg(feature = "gpu")]
    if let Some(virgl_flags) = vm_resources.gpu_virgl_flags {
        let display_backend = vm_resources
            .display_backend
            .unwrap_or_else(|| NoopDisplayBackend::into_display_backend(None));

        attach_gpu_device(
            &mut vmm,
            &mut _shm_manager,
            #[cfg(not(feature = "tee"))]
            export_table.clone(),
            intc.clone(),
            virgl_flags,
            Box::from(&vm_resources.displays[..]),
            display_backend,
            #[cfg(target_os = "macos")]
            _sender.clone(),
        )?;
    }

    #[cfg(feature = "input")]
    if !vm_resources.input_backends.is_empty() {
        attach_input_devices(&mut vmm, &vm_resources.input_backends, intc.clone())?;
    }

    #[cfg(all(unix, not(any(feature = "tee", feature = "aws-nitro"))))]
    attach_fs_devices(
        &mut vmm,
        &vm_resources.fs,
        &mut _shm_manager,
        #[cfg(not(feature = "tee"))]
        export_table,
        intc.clone(),
        exit_code,
        #[cfg(target_os = "macos")]
        _sender,
    )?;
    #[cfg(feature = "blk")]
    attach_block_devices(&mut vmm, &vm_resources.block, intc.clone())?;

    if let Some(vsock) = vm_resources.vsock.get() {
        attach_unixsock_vsock_device(&mut vmm, vsock, event_manager, intc.clone())?;
        let tsi_flags = vm_resources.vsock.tsi_flags();
        if tsi_flags.contains(TsiFlags::HIJACK_INET) {
            vmm.kernel_cmdline.insert_str("tsi_hijack")?;
        }
        if tsi_flags.contains(TsiFlags::HIJACK_UNIX) {
            vmm.kernel_cmdline.insert_str("tsi_hijack_unix")?;
        }
    }

    #[cfg(feature = "net")]
    attach_net_devices(&mut vmm, &vm_resources.net, intc.clone())?;
    #[cfg(feature = "net")]
    if vm_resources.dhcp_client {
        vmm.kernel_cmdline.insert_str("KRUN_DHCP=1")?;
    }

    if let Some(s) = &vm_resources.kernel_cmdline.epilog {
        vmm.kernel_cmdline.insert_str(s).unwrap();
    };

    // Write the kernel command line to guest memory. This is x86_64 specific, since on
    // aarch64 the command line will be specified through the FDT.
    #[cfg(all(target_arch = "x86_64", not(feature = "tee")))]
    load_cmdline(&vmm)?;

    vmm.configure_system(
        vcpus.as_slice(),
        &intc,
        &payload_config.initrd_config,
        &vm_resources.smbios_oem_strings,
        payload_config.pvh,
    )
    .map_err(StartMicrovmError::Internal)?;

    #[cfg(feature = "tee")]
    {
        match tee {
            #[cfg(feature = "amd-sev")]
            Tee::Snp => {
                let cpuid = _kvm
                    .fd()
                    .get_supported_cpuid(KVM_MAX_CPUID_ENTRIES)
                    .map_err(VstateError::KvmCpuId)
                    .map_err(StartMicrovmError::SecureVirtAttest)?;
                vmm.kvm_vm()
                    .snp_secure_virt_measure(
                        cpuid,
                        vmm.guest_memory(),
                        measured_regions,
                        snp_launcher.unwrap(),
                    )
                    .map_err(StartMicrovmError::SecureVirtAttest)?;
            }
            #[cfg(feature = "tdx")]
            Tee::Tdx => {
                vmm.kvm_vm()
                    .tdx_secure_virt_prepare_memory(&mut tdx_launcher, &measured_regions)
                    .unwrap();
                vmm.kvm_vm()
                    .tdx_secure_virt_finalize_vm(tdx_launcher)
                    .map_err(StartMicrovmError::SecureVirtPrepare)?;
            }
            _ => return Err(StartMicrovmError::InvalidTee),
        }

        println!("Starting TEE/microVM.");
    }

    vmm.start_vcpus(vcpus)
        .map_err(StartMicrovmError::Internal)?;

    // Clippy thinks we don't need Arc<Mutex<...
    // but we don't want to change the event_manager interface
    #[allow(clippy::arc_with_non_send_sync)]
    let vmm = Arc::new(Mutex::new(vmm));
    event_manager
        .add_subscriber(vmm.clone())
        .map_err(StartMicrovmError::RegisterEvent)?;

    Ok(vmm)
}

fn load_external_kernel(
    guest_mem: &GuestMemoryMmap,
    arch_mem_info: &ArchMemoryInfo,
    external_kernel: &ExternalKernel,
) -> std::result::Result<
    (GuestAddress, Option<InitrdConfig>, Option<String>, bool),
    StartMicrovmError,
> {
    #[allow(unused_mut)]
    let mut pvh = false;
    let entry_addr = match external_kernel.format {
        // Raw images are treated as bundled kernels on x86_64
        #[cfg(target_arch = "x86_64")]
        KernelFormat::Raw => unreachable!(),
        #[cfg(any(target_arch = "aarch64", target_arch = "riscv64"))]
        KernelFormat::Raw => {
            let data: Vec<u8> = std::fs::read(external_kernel.path.clone())
                .map_err(StartMicrovmError::RawOpenKernel)?;
            guest_mem.write(&data, GuestAddress(0x8000_0000)).unwrap();
            GuestAddress(0x8000_0000)
        }
        #[cfg(target_arch = "x86_64")]
        KernelFormat::Elf => {
            // Unix: stream straight from the file (vm-memory's rawfd
            // ReadVolatile). Windows has no rawfd impls, go through memory.
            #[cfg(unix)]
            let mut kernel_input = File::options()
                .read(true)
                .write(false)
                .open(external_kernel.path.clone())
                .map_err(StartMicrovmError::ElfOpenKernel)?;
            #[cfg(windows)]
            let mut kernel_input = std::io::Cursor::new(
                std::fs::read(external_kernel.path.clone())
                    .map_err(StartMicrovmError::ElfOpenKernel)?,
            );
            let load_result = loader::Elf::load(guest_mem, None, &mut kernel_input, None)
                .map_err(StartMicrovmError::ElfLoadKernel)?;
            match load_result.pvh_boot_cap {
                loader::PvhBootCapability::PvhEntryPresent(guest_address) => {
                    pvh = true;
                    guest_address
                }
                _ => load_result.kernel_load,
            }
        }
        #[cfg(any(target_arch = "aarch64", target_arch = "riscv64"))]
        KernelFormat::PeGz => {
            let data: Vec<u8> = std::fs::read(external_kernel.path.clone())
                .map_err(StartMicrovmError::PeGzOpenKernel)?;
            if let Some(magic) = data
                .windows(3)
                .position(|window| window == [0x1f, 0x8b, 0x8])
            {
                debug!("Found GZIP header on PE file at: 0x{magic:x}");
                let (_, compressed) = data.split_at(magic);
                let mut gz = GzDecoder::new(compressed);
                let mut kernel_data: Vec<u8> = Vec::new();
                gz.read_to_end(&mut kernel_data)
                    .map_err(StartMicrovmError::PeGzDecoder)?;
                guest_mem
                    .write(&kernel_data, GuestAddress(0x8000_0000))
                    .unwrap();
                GuestAddress(0x8000_0000)
            } else {
                return Err(StartMicrovmError::PeGzInvalid);
            }
        }
        #[cfg(target_arch = "x86_64")]
        KernelFormat::ImageBz2 => {
            let data: Vec<u8> = std::fs::read(external_kernel.path.clone())
                .map_err(StartMicrovmError::ImageBz2OpenKernel)?;
            if let Some(magic) = data
                .windows(3)
                .position(|window| window == [b'B', b'Z', b'h'])
            {
                debug!("Found BZIP2 header on Image file at: 0x{magic:x}");
                let (_, compressed) = data.split_at(magic);
                let mut kernel_data: Vec<u8> = Vec::new();
                let mut bz2 = bzip2::read::BzDecoder::new(compressed);
                bz2.read_to_end(&mut kernel_data)
                    .map_err(StartMicrovmError::ImageBz2Decoder)?;
                let load_result = loader::Elf::load(
                    guest_mem,
                    None,
                    &mut std::io::Cursor::new(kernel_data),
                    None,
                )
                .map_err(StartMicrovmError::ImageBz2LoadKernel)?;
                load_result.kernel_load
            } else {
                return Err(StartMicrovmError::ImageBz2Invalid);
            }
        }
        #[cfg(target_arch = "x86_64")]
        KernelFormat::ImageGz => {
            let data: Vec<u8> = std::fs::read(external_kernel.path.clone())
                .map_err(StartMicrovmError::ImageGzOpenKernel)?;
            if let Some(magic) = data
                .windows(3)
                .position(|window| window == [0x1f, 0x8b, 0x8])
            {
                debug!("Found GZIP header on Image file at: 0x{magic:x}");
                let (_, compressed) = data.split_at(magic);
                let mut gz = GzDecoder::new(compressed);
                let mut kernel_data: Vec<u8> = Vec::new();
                gz.read_to_end(&mut kernel_data)
                    .map_err(StartMicrovmError::ImageGzDecoder)?;
                let load_result = loader::Elf::load(
                    guest_mem,
                    None,
                    &mut std::io::Cursor::new(kernel_data),
                    None,
                )
                .map_err(StartMicrovmError::ImageGzLoadKernel)?;
                load_result.kernel_load
            } else {
                return Err(StartMicrovmError::ImageGzInvalid);
            }
        }
        #[cfg(target_arch = "x86_64")]
        KernelFormat::ImageZstd => {
            let data: Vec<u8> = std::fs::read(external_kernel.path.clone())
                .map_err(StartMicrovmError::ImageZstdOpenKernel)?;
            if let Some(magic) = data
                .windows(4)
                .position(|window| window == [0x28, 0xb5, 0x2f, 0xfd])
            {
                debug!("Found ZSTD header on Image file at: 0x{magic:x}");
                let (_, zstd_data) = data.split_at(magic);
                let mut kernel_data: Vec<u8> = Vec::new();
                let _ = zstd::stream::copy_decode(zstd_data, &mut kernel_data);
                let load_result = loader::Elf::load(
                    guest_mem,
                    None,
                    &mut std::io::Cursor::new(kernel_data),
                    None,
                )
                .map_err(StartMicrovmError::ImageZstdLoadKernel)?;
                load_result.kernel_load
            } else {
                return Err(StartMicrovmError::ImageZstdInvalid);
            }
        }
        _ => return Err(StartMicrovmError::KernelFormatUnsupported),
    };

    debug!("load_external_kernel: 0x{:x}", entry_addr.0);

    let initrd_config = if let Some(initramfs_path) = &external_kernel.initramfs_path {
        let data = std::fs::read(initramfs_path).map_err(StartMicrovmError::InitrdRead)?;
        guest_mem
            .write(&data, GuestAddress(arch_mem_info.initrd_addr))
            .unwrap();
        Some(InitrdConfig {
            address: GuestAddress(arch_mem_info.initrd_addr),
            size: data.len(),
        })
    } else {
        None
    };

    Ok((
        entry_addr,
        initrd_config,
        external_kernel.cmdline.clone(),
        pvh,
    ))
}

struct LoadedPayload {
    guest_mem: GuestMemoryMmap,
    entry_addr: GuestAddress,
    initrd_config: Option<InitrdConfig>,
    kernel_cmdline: Option<String>,
    pvh: bool,
}

fn load_payload(
    _vm_resources: &VmResources,
    guest_mem: GuestMemoryMmap,
    _arch_mem_info: &ArchMemoryInfo,
    payload: &Payload,
) -> std::result::Result<LoadedPayload, StartMicrovmError> {
    match payload {
        #[cfg(any(target_arch = "aarch64", target_arch = "riscv64"))]
        Payload::KernelCopy => {
            let (kernel_entry_addr, kernel_host_addr, kernel_guest_addr, kernel_size) =
                if let Some(kernel_bundle) = &_vm_resources.kernel_bundle {
                    (
                        kernel_bundle.entry_addr,
                        kernel_bundle.host_addr,
                        kernel_bundle.guest_addr,
                        kernel_bundle.size,
                    )
                } else {
                    return Err(StartMicrovmError::MissingKernelConfig);
                };

            let kernel_data =
                unsafe { std::slice::from_raw_parts(kernel_host_addr as *mut u8, kernel_size) };
            if kernel_guest_addr + kernel_size as u64 > _arch_mem_info.ram_last_addr {
                return Err(StartMicrovmError::KernelDoesNotFit(
                    kernel_guest_addr,
                    kernel_size,
                ));
            }
            guest_mem
                .write(kernel_data, GuestAddress(kernel_guest_addr))
                .unwrap();
            Ok(LoadedPayload {
                guest_mem,
                entry_addr: GuestAddress(kernel_entry_addr),
                initrd_config: None,
                kernel_cmdline: None,
                pvh: false,
            })
        }
        #[cfg(all(unix, target_arch = "x86_64", not(feature = "tee")))]
        Payload::KernelMmap => {
            let (kernel_entry_addr, kernel_host_addr, kernel_guest_addr, kernel_size) =
                if let Some(kernel_bundle) = &_vm_resources.kernel_bundle {
                    (
                        kernel_bundle.entry_addr,
                        kernel_bundle.host_addr,
                        kernel_bundle.guest_addr,
                        kernel_bundle.size,
                    )
                } else {
                    return Err(StartMicrovmError::MissingKernelConfig);
                };

            #[cfg(all(feature = "vhost-user", target_os = "linux"))]
            let use_vhost_user = !_vm_resources.vhost_user_devices.is_empty();
            #[cfg(not(all(feature = "vhost-user", target_os = "linux")))]
            let use_vhost_user = false;

            let kernel_region = if use_vhost_user {
                #[cfg(all(feature = "vhost-user", target_os = "linux"))]
                {
                    debug!(
                        "Creating file-backed kernel region for vhost-user (size=0x{:x})",
                        kernel_size
                    );
                    // SAFETY: memfd_create is called with a valid null-terminated C string and valid flags.
                    // File descriptor ownership is transferred to File::from_raw_fd below.
                    let memfd = unsafe {
                        let fd = libc::memfd_create(c"kernel".as_ptr(), libc::MFD_CLOEXEC);
                        if fd < 0 {
                            error!(
                                "Failed to create memfd for kernel: {:?}",
                                io::Error::last_os_error()
                            );
                            return Err(StartMicrovmError::GuestMemoryMmap(format!(
                                "memfd_create failed: {:?}",
                                io::Error::last_os_error()
                            )));
                        }
                        if libc::ftruncate(fd, kernel_size as i64) < 0 {
                            error!(
                                "Failed to ftruncate kernel memfd: {:?}",
                                io::Error::last_os_error()
                            );
                            libc::close(fd);
                            return Err(StartMicrovmError::GuestMemoryMmap(format!(
                                "ftruncate failed: {:?}",
                                io::Error::last_os_error()
                            )));
                        }
                        debug!("Created kernel memfd with fd={}", fd);
                        File::from_raw_fd(fd)
                    };

                    let file_offset = FileOffset::new(memfd, 0);
                    let region = MmapRegion::from_file(file_offset, kernel_size)
                        .map_err(StartMicrovmError::InvalidKernelBundle)?;

                    // SAFETY: kernel_host_addr points to valid kernel data of size kernel_size,
                    // provided by the kernel bundle loader.
                    let kernel_data = unsafe {
                        std::slice::from_raw_parts(kernel_host_addr as *const u8, kernel_size)
                    };
                    // SAFETY: Both source (kernel_data) and destination (region) are valid for
                    // kernel_size bytes. Regions don't overlap as dest is newly allocated memfd-backed
                    // memory and source is from kernel bundle.
                    unsafe {
                        std::ptr::copy_nonoverlapping(
                            kernel_data.as_ptr(),
                            region.as_ptr(),
                            kernel_size,
                        );
                    }
                    debug!("Copied kernel data to file-backed region");

                    region
                }
                #[cfg(not(all(feature = "vhost-user", target_os = "linux")))]
                unreachable!()
            } else {
                // SAFETY: kernel_host_addr points to valid kernel data of size kernel_size.
                // The memory region is managed by the kernel bundle and remains valid.
                unsafe {
                    MmapRegion::build_raw(kernel_host_addr as *mut u8, kernel_size, 0, 0)
                        .map_err(StartMicrovmError::InvalidKernelBundle)?
                }
            };

            Ok(LoadedPayload {
                guest_mem: guest_mem
                    .insert_region(Arc::new(
                        GuestRegionMmap::new(kernel_region, GuestAddress(kernel_guest_addr))
                            .ok_or_else(|| {
                                StartMicrovmError::GuestMemoryMmap(
                                    "Failed to create GuestRegionMmap".to_string(),
                                )
                            })?,
                    ))
                    .map_err(|e| StartMicrovmError::GuestMemoryMmap(format!("{e:?}")))?,
                entry_addr: GuestAddress(kernel_entry_addr),
                initrd_config: None,
                kernel_cmdline: None,
                pvh: false,
            })
        }
        Payload::ExternalKernel(external_kernel) => {
            let (entry_addr, initrd_config, cmdline, pvh) =
                load_external_kernel(&guest_mem, _arch_mem_info, external_kernel)?;
            Ok(LoadedPayload {
                guest_mem,
                entry_addr,
                initrd_config,
                kernel_cmdline: cmdline,
                pvh,
            })
        }
        #[cfg(test)]
        Payload::Empty => Ok(LoadedPayload {
            guest_mem,
            entry_addr: GuestAddress(0),
            initrd_config: None,
            kernel_cmdline: None,
            pvh: false,
        }),
        #[cfg(feature = "tee")]
        Payload::Tee => {
            let (kernel_host_addr, kernel_guest_addr, kernel_size) =
                if let Some(kernel_bundle) = &_vm_resources.kernel_bundle {
                    (
                        kernel_bundle.host_addr,
                        kernel_bundle.guest_addr,
                        kernel_bundle.size,
                    )
                } else {
                    return Err(StartMicrovmError::MissingKernelConfig);
                };
            let kernel_data =
                unsafe { std::slice::from_raw_parts(kernel_host_addr as *mut u8, kernel_size) };
            guest_mem
                .write(kernel_data, GuestAddress(kernel_guest_addr))
                .unwrap();

            let (qboot_host_addr, qboot_size) =
                if let Some(qboot_bundle) = &_vm_resources.qboot_bundle {
                    (qboot_bundle.host_addr, qboot_bundle.size)
                } else {
                    return Err(StartMicrovmError::MissingKernelConfig);
                };
            let qboot_data =
                unsafe { std::slice::from_raw_parts(qboot_host_addr as *mut u8, qboot_size) };
            guest_mem
                .write(qboot_data, GuestAddress(arch::FIRMWARE_START))
                .unwrap();

            let (initrd_host_addr, initrd_size) =
                if let Some(initrd_bundle) = &_vm_resources.initrd_bundle {
                    (initrd_bundle.host_addr, initrd_bundle.size)
                } else {
                    return Err(StartMicrovmError::MissingKernelConfig);
                };
            let initrd_data =
                unsafe { std::slice::from_raw_parts(initrd_host_addr as *mut u8, initrd_size) };
            guest_mem
                .write(initrd_data, GuestAddress(_arch_mem_info.initrd_addr))
                .unwrap();

            let initrd_config = InitrdConfig {
                address: GuestAddress(_arch_mem_info.initrd_addr),
                size: initrd_data.len(),
            };

            Ok(LoadedPayload {
                guest_mem,
                entry_addr: GuestAddress(arch::RESET_VECTOR),
                initrd_config: Some(initrd_config),
                kernel_cmdline: None,
                pvh: false,
            })
        }
        Payload::Firmware => Ok(LoadedPayload {
            guest_mem,
            entry_addr: GuestAddress(arch::RESET_VECTOR),
            initrd_config: None,
            kernel_cmdline: None,
            pvh: false,
        }),
    }
}

pub struct PayloadConfig {
    entry_addr: GuestAddress,
    initrd_config: Option<InitrdConfig>,
    kernel_cmdline: Option<String>,
    pvh: bool,
}

pub fn create_guest_memory(
    mem_size: usize,
    vm_resources: &VmResources,
    payload: &Payload,
) -> std::result::Result<
    (GuestMemoryMmap, ArchMemoryInfo, ShmManager, PayloadConfig),
    StartMicrovmError,
> {
    let mem_size = mem_size << 20;

    let (firmware_data, firmware_size) = if let Some(firmware) = &vm_resources.firmware_config {
        let data = std::fs::read(firmware.path.clone()).map_err(StartMicrovmError::FirmwareRead)?;
        let len = data.len();
        (Some(data), Some(len))
    } else {
        (None, None)
    };

    #[cfg(target_arch = "x86_64")]
    let (arch_mem_info, mut arch_mem_regions) = match payload {
        #[cfg(all(unix, not(feature = "tee")))]
        Payload::KernelMmap => {
            let (kernel_guest_addr, kernel_size) =
                if let Some(kernel_bundle) = &vm_resources.kernel_bundle {
                    (kernel_bundle.guest_addr, kernel_bundle.size)
                } else {
                    return Err(StartMicrovmError::MissingKernelConfig);
                };
            arch::arch_memory_regions(mem_size, Some(kernel_guest_addr), kernel_size, 0, None)
        }
        Payload::ExternalKernel(external_kernel) => arch::arch_memory_regions(
            mem_size,
            None,
            0,
            external_kernel.initramfs_size,
            firmware_size,
        ),
        #[cfg(feature = "tee")]
        Payload::Tee => {
            let (kernel_guest_addr, kernel_size) =
                if let Some(kernel_bundle) = &vm_resources.kernel_bundle {
                    (kernel_bundle.guest_addr, kernel_bundle.size)
                } else {
                    return Err(StartMicrovmError::MissingKernelConfig);
                };
            arch::arch_memory_regions(mem_size, Some(kernel_guest_addr), kernel_size, 0, None)
        }
        #[cfg(test)]
        Payload::Empty => arch::arch_memory_regions(mem_size, None, 0, 0, None),
        Payload::Firmware => arch::arch_memory_regions(mem_size, None, 0, 0, firmware_size),
    };
    #[cfg(any(target_arch = "aarch64", target_arch = "riscv64"))]
    let (arch_mem_info, mut arch_mem_regions) = match payload {
        Payload::ExternalKernel(external_kernel) => {
            arch::arch_memory_regions(mem_size, external_kernel.initramfs_size, None)
        }
        _ => arch::arch_memory_regions(mem_size, 0, firmware_size),
    };

    let mut shm_manager = ShmManager::new(&arch_mem_info);

    #[cfg(not(feature = "tee"))]
    for (index, fs) in vm_resources.fs.iter().enumerate() {
        if let Some(shm_size) = fs.shm_size {
            shm_manager
                .create_fs_region(index, shm_size)
                .map_err(StartMicrovmError::ShmCreate)?;
        }
    }
    if vm_resources.gpu_virgl_flags.is_some() {
        let size = vm_resources.gpu_shm_size.unwrap_or(1 << 33);
        shm_manager
            .create_gpu_region(size)
            .map_err(StartMicrovmError::ShmCreate)?;
    }

    // For vhost-user devices, we need file-backed memory so the backend can mmap it
    #[cfg(all(feature = "vhost-user", target_os = "linux"))]
    let use_vhost_user = !vm_resources.vhost_user_devices.is_empty();
    #[cfg(not(all(feature = "vhost-user", target_os = "linux")))]
    let use_vhost_user = false;

    // Add SHM regions before creating guest memory
    arch_mem_regions.extend(shm_manager.regions());

    let guest_mem = if use_vhost_user {
        #[cfg(all(feature = "vhost-user", target_os = "linux"))]
        {
            debug!(
                "Creating file-backed memory for vhost-user (regions: {})",
                arch_mem_regions.len()
            );
            // Create file-backed memory regions using memfd
            let regions_with_files: Vec<_> = arch_mem_regions
                .iter()
                .map(|(addr, size)| {
                    debug!(
                        "Creating memfd for region: addr=0x{:x}, size=0x{:x}",
                        addr.0, size
                    );
                    // SAFETY: memfd_create is called with a valid null-terminated C string and valid flags.
                    // File descriptor ownership is transferred to File::from_raw_fd below.
                    let memfd = unsafe {
                        let fd = libc::memfd_create(c"guest_mem".as_ptr(), libc::MFD_CLOEXEC);
                        if fd < 0 {
                            error!("Failed to create memfd: {:?}", io::Error::last_os_error());
                            return Err(io::Error::last_os_error());
                        }
                        if libc::ftruncate(fd, *size as i64) < 0 {
                            error!(
                                "Failed to ftruncate memfd: {:?}",
                                io::Error::last_os_error()
                            );
                            libc::close(fd);
                            return Err(io::Error::last_os_error());
                        }
                        debug!("Created memfd with fd={}", fd);
                        File::from_raw_fd(fd)
                    };

                    let file_offset = FileOffset::new(memfd, 0);
                    Ok((*addr, *size, Some(file_offset)))
                })
                .collect::<Result<Vec<_>, io::Error>>()
                .map_err(|e| {
                    StartMicrovmError::GuestMemoryMmap(format!("memfd creation failed: {e:?}"))
                })?;

            debug!(
                "Created {} file-backed memory regions",
                regions_with_files.len()
            );
            GuestMemoryMmap::from_ranges_with_files(&regions_with_files)
                .map_err(|e| StartMicrovmError::GuestMemoryMmap(format!("{e:?}")))?
        }
        #[cfg(not(all(feature = "vhost-user", target_os = "linux")))]
        unreachable!()
    } else {
        GuestMemoryMmap::from_ranges(&arch_mem_regions)
            .map_err(|e| StartMicrovmError::GuestMemoryMmap(format!("{e:?}")))?
    };

    let LoadedPayload {
        guest_mem,
        entry_addr,
        initrd_config,
        kernel_cmdline: cmdline,
        pvh,
    } = load_payload(vm_resources, guest_mem, &arch_mem_info, payload)?;

    // Only write firmware if data exists AND this isn't an ExternalKernel payload
    // (ExternalKernel does direct kernel boot and doesn't use EFI firmware)
    if !matches!(payload, Payload::ExternalKernel(_))
        && let Some(firmware_data) = firmware_data.as_ref()
    {
        guest_mem
            .write(firmware_data, GuestAddress(arch_mem_info.firmware_addr))
            .map_err(StartMicrovmError::FirmwareInvalidAddress)?;
    }

    let payload_config = PayloadConfig {
        entry_addr,
        initrd_config,
        kernel_cmdline: cmdline.clone(),
        pvh,
    };

    Ok((guest_mem, arch_mem_info, shm_manager, payload_config))
}

#[cfg(all(target_arch = "x86_64", not(feature = "tee")))]
fn load_cmdline(vmm: &Vmm) -> std::result::Result<(), StartMicrovmError> {
    kernel::loader::load_cmdline(
        vmm.guest_memory(),
        GuestAddress(arch::x86_64::layout::CMDLINE_START),
        &vmm.kernel_cmdline
            .as_cstring()
            .map_err(StartMicrovmError::LoadCommandline)?,
    )
    .map_err(StartMicrovmError::LoadCommandline)
}

#[cfg(all(target_os = "linux", not(feature = "tee")))]
pub(crate) fn setup_vm(
    guest_memory: &GuestMemoryMmap,
    _nested_enabled: bool,
) -> std::result::Result<Vm, StartMicrovmError> {
    let kvm = KvmContext::new()
        .map_err(Error::KvmContext)
        .map_err(StartMicrovmError::Internal)?;
    let mut vm = Vm::new(kvm.fd())
        .map_err(Error::Vm)
        .map_err(StartMicrovmError::Internal)?;
    vm.memory_init(guest_memory, kvm.max_memslots())
        .map_err(Error::Vm)
        .map_err(StartMicrovmError::Internal)?;
    Ok(vm)
}
#[cfg(all(target_os = "linux", feature = "tee"))]
pub(crate) fn setup_vm(
    kvm: &KvmContext,
    guest_memory: &GuestMemoryMmap,
    resources: &super::resources::VmResources,
    #[cfg(feature = "tdx")] _sender: Sender<WorkerMessage>,
) -> std::result::Result<Vm, StartMicrovmError> {
    let mut vm = Vm::new(
        kvm.fd(),
        resources.tee_config(),
        #[cfg(feature = "tdx")]
        _sender,
    )
    .map_err(Error::Vm)
    .map_err(StartMicrovmError::Internal)?;
    vm.memory_init(guest_memory, kvm.max_memslots())
        .map_err(Error::Vm)
        .map_err(StartMicrovmError::Internal)?;
    Ok(vm)
}
#[cfg(target_os = "macos")]
pub(crate) fn setup_vm(
    guest_memory: &GuestMemoryMmap,
    nested_enabled: bool,
) -> std::result::Result<Vm, StartMicrovmError> {
    let mut vm = Vm::new(nested_enabled)
        .map_err(Error::Vm)
        .map_err(StartMicrovmError::Internal)?;
    vm.memory_init(guest_memory)
        .map_err(Error::Vm)
        .map_err(StartMicrovmError::Internal)?;
    Ok(vm)
}
#[cfg(target_os = "windows")]
pub(crate) fn setup_vm(
    guest_memory: &GuestMemoryMmap,
    vcpu_count: u8,
) -> std::result::Result<Vm, StartMicrovmError> {
    let mut vm = Vm::new(vcpu_count)
        .map_err(Error::Vm)
        .map_err(StartMicrovmError::Internal)?;
    vm.memory_init(guest_memory)
        .map_err(Error::Vm)
        .map_err(StartMicrovmError::Internal)?;
    Ok(vm)
}

/// Sets up the serial device.
pub fn setup_serial_device(
    event_manager: &mut EventManager,
    input: Option<Box<dyn devices::legacy::ReadableFd + Send>>,
    out: Option<Box<dyn io::Write + Send>>,
) -> std::result::Result<Arc<Mutex<Serial>>, StartMicrovmError> {
    let interrupt_evt = EventFd::new(utils::eventfd::EFD_NONBLOCK)
        .map_err(Error::EventFd)
        .map_err(StartMicrovmError::Internal)?;
    let has_input = input.is_some();
    let serial = Arc::new(Mutex::new(Serial::new(interrupt_evt, out, input)));
    if has_input && let Err(e) = event_manager.add_subscriber(serial.clone()) {
        // TODO: We just log this message, and immediately return Ok, instead of returning the
        // actual error because this operation always fails with EPERM when adding a fd which
        // has been redirected to /dev/null via dup2 (this may happen inside the jailer).
        // Find a better solution to this (and think about the state of the serial device
        // while we're at it).
        warn!("Could not add serial input event to epoll: {e:?}");
    }
    Ok(serial)
}

#[cfg(all(target_arch = "x86_64", target_os = "windows"))]
fn attach_legacy_devices(
    _vm: &Vm,
    pio_device_manager: &mut PortIODeviceManager,
    mmio_device_manager: &mut MMIODeviceManager,
    intc: Option<Arc<Mutex<IrqChipDevice>>>,
) -> std::result::Result<(), StartMicrovmError> {
    pio_device_manager
        .pic
        .lock()
        .unwrap()
        .set_vm(_vm.whp_vm().clone());
    if let Some(ioapic) = intc.as_ref() {
        pio_device_manager
            .pic
            .lock()
            .unwrap()
            .set_ioapic(ioapic.clone());
    }
    pio_device_manager
        .register_devices()
        .map_err(Error::LegacyIOBus)
        .map_err(StartMicrovmError::Internal)?;

    // The WHP IOAPIC is always a userspace MMIO device; interrupts are
    // injected via WHvRequestInterrupt from the IrqChipT impl.
    mmio_device_manager
        .register_mmio_ioapic(intc)
        .map_err(Error::RegisterMMIODevice)
        .map_err(StartMicrovmError::Internal)?;

    Ok(())
}

#[cfg(all(target_arch = "x86_64", unix))]
fn attach_legacy_devices(
    vm: &Vm,
    split_irqchip: bool,
    pio_device_manager: &mut PortIODeviceManager,
    mmio_device_manager: &mut MMIODeviceManager,
    intc: Option<Arc<Mutex<IrqChipDevice>>>,
) -> std::result::Result<(), StartMicrovmError> {
    pio_device_manager
        .register_devices()
        .map_err(Error::LegacyIOBus)
        .map_err(StartMicrovmError::Internal)?;

    if split_irqchip {
        mmio_device_manager
            .register_mmio_ioapic(intc)
            .map_err(Error::RegisterMMIODevice)
            .map_err(StartMicrovmError::Internal)?;
    }

    macro_rules! register_irqfd_evt {
        ($evt: ident, $index: expr_2021) => {{
            vm.fd()
                .register_irqfd(&pio_device_manager.$evt, $index)
                .map_err(|e| {
                    Error::LegacyIOBus(device_manager::legacy::Error::EventFd(
                        io::Error::from_raw_os_error(e.errno()),
                    ))
                })
                .map_err(StartMicrovmError::Internal)?;
        }};
    }

    register_irqfd_evt!(com_evt_1, 4);
    register_irqfd_evt!(com_evt_2, 3);
    register_irqfd_evt!(com_evt_3, 4);
    register_irqfd_evt!(com_evt_4, 3);
    register_irqfd_evt!(kbd_evt, 1);
    Ok(())
}

#[cfg(all(
    any(target_arch = "aarch64", target_arch = "riscv64"),
    target_os = "linux"
))]
fn attach_legacy_devices(
    vm: &Vm,
    mmio_device_manager: &mut MMIODeviceManager,
    kernel_cmdline: &mut kernel::cmdline::Cmdline,
    intc: IrqChip,
    serial: Vec<Arc<Mutex<Serial>>>,
) -> std::result::Result<(), StartMicrovmError> {
    for s in serial {
        mmio_device_manager
            .register_mmio_serial(vm.fd(), kernel_cmdline, intc.clone(), s)
            .map_err(Error::RegisterMMIODevice)
            .map_err(StartMicrovmError::Internal)?;
    }

    #[cfg(all(target_arch = "aarch64", target_os = "linux"))]
    mmio_device_manager
        .register_mmio_rtc(vm.fd())
        .map_err(Error::RegisterMMIODevice)
        .map_err(StartMicrovmError::Internal)?;

    Ok(())
}

#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
fn attach_legacy_devices(
    vm: &Vm,
    mmio_device_manager: &mut MMIODeviceManager,
    kernel_cmdline: &mut kernel::cmdline::Cmdline,
    intc: IrqChip,
    serial: Vec<Arc<Mutex<Serial>>>,
    event_manager: &mut EventManager,
    shutdown_efd: Option<EventFd>,
) -> Result<(), StartMicrovmError> {
    for s in serial {
        mmio_device_manager
            .register_mmio_serial(vm, kernel_cmdline, intc.clone(), s)
            .map_err(Error::RegisterMMIODevice)
            .map_err(StartMicrovmError::Internal)?;
    }

    mmio_device_manager
        .register_mmio_rtc(vm, intc.clone())
        .map_err(Error::RegisterMMIODevice)
        .map_err(StartMicrovmError::Internal)?;

    mmio_device_manager
        .register_mmio_gic(vm, intc.clone())
        .map_err(Error::RegisterMMIODevice)
        .map_err(StartMicrovmError::Internal)?;

    if let Some(shutdown_efd) = shutdown_efd {
        mmio_device_manager
            .register_mmio_gpio(vm, intc.clone(), event_manager, shutdown_efd)
            .map_err(Error::RegisterMMIODevice)
            .map_err(StartMicrovmError::Internal)?;
    }

    Ok(())
}

#[cfg(all(target_arch = "x86_64", target_os = "windows"))]
#[allow(clippy::too_many_arguments)]
fn create_vcpus_x86_64(
    vm: &Vm,
    vcpu_config: &VcpuConfig,
    guest_mem: &GuestMemoryMmap,
    entry_addr: GuestAddress,
    io_bus: &devices::Bus,
    exit_evt: &EventFd,
    kernel_boot: bool,
) -> super::Result<Vec<Vcpu>> {
    let mut vcpus = Vec::with_capacity(vcpu_config.vcpu_count as usize);
    for cpu_index in 0..vcpu_config.vcpu_count {
        let mut vcpu = Vcpu::new_x86_64(
            cpu_index,
            vm.whp_vm().clone(),
            guest_mem.clone(),
            io_bus.clone(),
            exit_evt.try_clone().map_err(Error::EventFd)?,
        )
        .map_err(Error::Vcpu)?;

        vcpu.configure_x86_64(guest_mem, entry_addr, kernel_boot)
            .map_err(Error::Vcpu)?;

        vcpus.push(vcpu);
    }
    Ok(vcpus)
}

#[cfg(all(target_arch = "x86_64", unix))]
#[allow(clippy::too_many_arguments)]
fn create_vcpus_x86_64(
    vm: &Vm,
    vcpu_config: &VcpuConfig,
    guest_mem: &GuestMemoryMmap,
    entry_addr: GuestAddress,
    io_bus: &devices::Bus,
    exit_evt: &EventFd,
    kernel_boot: bool,
    pvh: bool,
    #[cfg(feature = "tee")] pm_sender: Sender<WorkerMessage>,
) -> super::Result<Vec<Vcpu>> {
    let mut vcpus = Vec::with_capacity(vcpu_config.vcpu_count as usize);
    for cpu_index in 0..vcpu_config.vcpu_count {
        let mut vcpu = Vcpu::new_x86_64(
            cpu_index,
            vm.fd(),
            vm.supported_cpuid().clone(),
            vm.supported_msrs().clone(),
            io_bus.clone(),
            exit_evt.try_clone().map_err(Error::EventFd)?,
            #[cfg(feature = "tee")]
            pm_sender.clone(),
        )
        .map_err(Error::Vcpu)?;

        vcpu.configure_x86_64(guest_mem, entry_addr, vcpu_config, kernel_boot, pvh)
            .map_err(Error::Vcpu)?;

        vcpus.push(vcpu);
    }
    Ok(vcpus)
}

#[cfg(all(target_arch = "aarch64", target_os = "linux"))]
fn create_vcpus_aarch64(
    vm: &Vm,
    vcpu_config: &VcpuConfig,
    mem_info: &ArchMemoryInfo,
    entry_addr: GuestAddress,
    exit_evt: &EventFd,
) -> super::Result<Vec<Vcpu>> {
    let mut vcpus = Vec::with_capacity(vcpu_config.vcpu_count as usize);
    for cpu_index in 0..vcpu_config.vcpu_count {
        let mut vcpu = Vcpu::new_aarch64(
            cpu_index,
            vm.fd(),
            exit_evt.try_clone().map_err(Error::EventFd)?,
        )
        .map_err(Error::Vcpu)?;

        vcpu.configure_aarch64(vm.fd(), mem_info, entry_addr)
            .map_err(Error::Vcpu)?;

        vcpus.push(vcpu);
    }
    Ok(vcpus)
}

#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
fn create_vcpus_aarch64(
    _vm: &Vm,
    vcpu_config: &VcpuConfig,
    mem_info: &ArchMemoryInfo,
    entry_addr: GuestAddress,
    exit_evt: &EventFd,
    vcpu_list: Arc<VcpuList>,
    nested_enabled: bool,
) -> super::Result<Vec<Vcpu>> {
    let mut vcpus = Vec::with_capacity(vcpu_config.vcpu_count as usize);
    let mut boot_senders: HashMap<u64, Sender<u64>> = HashMap::new();

    for cpu_index in 0..vcpu_config.vcpu_count {
        let (boot_sender, boot_receiver) = if cpu_index != 0 {
            let (boot_sender, boot_receiver) = unbounded();
            (Some(boot_sender), Some(boot_receiver))
        } else {
            (None, None)
        };

        let mut vcpu = Vcpu::new_aarch64(
            cpu_index,
            entry_addr,
            boot_receiver,
            exit_evt.try_clone().map_err(Error::EventFd)?,
            vcpu_list.clone(),
            nested_enabled,
        )
        .map_err(Error::Vcpu)?;

        vcpu.configure_aarch64(mem_info).map_err(Error::Vcpu)?;

        if let Some(boot_sender) = boot_sender {
            boot_senders.insert(vcpu.get_mpidr(), boot_sender);
        }

        vcpus.push(vcpu);
    }

    vcpus[0].set_boot_senders(boot_senders);

    Ok(vcpus)
}

#[cfg(all(target_arch = "riscv64", target_os = "linux"))]
fn create_vcpus_riscv64(
    vm: &Vm,
    vcpu_config: &VcpuConfig,
    guest_mem: &GuestMemoryMmap,
    entry_addr: GuestAddress,
    exit_evt: &EventFd,
) -> super::Result<Vec<Vcpu>> {
    let mut vcpus = Vec::with_capacity(vcpu_config.vcpu_count as usize);
    for cpu_index in 0..vcpu_config.vcpu_count {
        let mut vcpu = Vcpu::new_riscv64(
            cpu_index,
            vm.fd(),
            exit_evt.try_clone().map_err(Error::EventFd)?,
        )
        .map_err(Error::Vcpu)?;

        vcpu.configure_riscv64(vm.fd(), guest_mem, entry_addr)
            .map_err(Error::Vcpu)?;

        vcpus.push(vcpu);
    }
    Ok(vcpus)
}

/// Attaches an virtio mmio device to the device manager.
fn attach_mmio_device(
    vmm: &mut Vmm,
    id: String,
    intc: IrqChip,
    device: Arc<Mutex<dyn VirtioDevice>>,
) -> std::result::Result<(), device_manager::mmio::Error> {
    let mmio_device = MmioTransport::new(vmm.guest_memory().clone(), intc, device)?;

    let type_id = mmio_device.locked_device().device_type();
    let _cmdline = &mut vmm.kernel_cmdline;

    #[cfg(target_os = "linux")]
    let (_mmio_base, _irq) =
        vmm.mmio_device_manager
            .register_mmio_device(vmm.vm.fd(), mmio_device, type_id, id)?;
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    let (_mmio_base, _irq) =
        vmm.mmio_device_manager
            .register_mmio_device(mmio_device, type_id, id)?;

    #[cfg(target_arch = "x86_64")]
    vmm.mmio_device_manager
        .add_device_to_cmdline(_cmdline, _mmio_base, _irq)?;

    Ok(())
}

#[cfg(not(any(feature = "tee", feature = "aws-nitro")))]
#[cfg(unix)]
fn attach_fs_devices(
    vmm: &mut Vmm,
    fs_devs: &[FsDeviceConfig],
    shm_manager: &mut ShmManager,
    #[cfg(not(feature = "tee"))] export_table: Option<ExportTable>,
    intc: IrqChip,
    exit_code: Arc<AtomicI32>,
    #[cfg(target_os = "macos")] map_sender: Sender<WorkerMessage>,
) -> std::result::Result<(), StartMicrovmError> {
    use self::StartMicrovmError::*;

    for (i, config) in fs_devs.iter().enumerate() {
        let fs = Arc::new(Mutex::new(
            devices::virtio::Fs::new(
                config.fs_id.clone(),
                config.shared_dir.clone(),
                exit_code.clone(),
                config.read_only,
                config.virtual_entries.clone(),
            )
            .unwrap(),
        ));

        let id = format!("{}{}", String::from(fs.lock().unwrap().id()), i);

        if let Some(shm_region) = shm_manager.fs_region(i) {
            fs.lock().unwrap().set_shm_region(VirtioShmRegion {
                host_addr: vmm
                    .guest_memory
                    .get_host_address(shm_region.guest_addr)
                    .map_err(StartMicrovmError::ShmHostAddr)? as u64,
                guest_addr: shm_region.guest_addr.raw_value(),
                size: shm_region.size,
            });
        }

        #[cfg(not(feature = "tee"))]
        if let Some(export_table) = export_table.as_ref() {
            fs.lock().unwrap().set_export_table(export_table.clone());
        }

        #[cfg(target_os = "macos")]
        fs.lock().unwrap().set_map_sender(map_sender.clone());

        // The device mutex mustn't be locked here otherwise it will deadlock.
        attach_mmio_device(vmm, id, intc.clone(), fs).map_err(RegisterFsDevice)?;
    }

    Ok(())
}

#[cfg(unix)]
fn autoconfigure_console_ports(
    vmm: &mut Vmm,
    vm_resources: &VmResources,
    cfg: Option<&DefaultVirtioConsoleConfig>,
    creating_implicit_console: bool,
) -> std::result::Result<Vec<PortDescription>, StartMicrovmError> {
    use self::StartMicrovmError::*;

    let mut console_output_path: Option<PathBuf> = None;
    if let Some(path) = vm_resources.console_output.clone()
        && !vm_resources.disable_implicit_console
        && creating_implicit_console
    {
        console_output_path = Some(path)
    }

    if let Some(console_output_path) = console_output_path {
        let file = File::create(console_output_path).map_err(OpenConsoleFile)?;
        // Manually emulate our Legacy behavior: In the case of output_path we have always used the
        // stdin to determine the console size
        let stdin_fd = unsafe { BorrowedFd::borrow_raw(STDIN_FILENO) };
        let term_fd = if isatty(stdin_fd).is_ok_and(|v| v) {
            port_io::term_fd(stdin_fd.as_raw_fd()).unwrap()
        } else {
            port_io::term_fixed_size(0, 0)
        };
        Ok(vec![PortDescription::console(
            Some(port_io::input_empty().unwrap()),
            Some(port_io::output_file(file).unwrap()),
            term_fd,
        )])
    } else {
        let (input_fd, output_fd, err_fd) = match cfg {
            Some(c) => (c.input_fd, c.output_fd, c.err_fd),
            None => (STDIN_FILENO, STDOUT_FILENO, STDERR_FILENO),
        };
        let input_is_terminal =
            input_fd >= 0 && isatty(unsafe { BorrowedFd::borrow_raw(input_fd) }).unwrap_or(false);
        let output_is_terminal =
            output_fd >= 0 && isatty(unsafe { BorrowedFd::borrow_raw(output_fd) }).unwrap_or(false);
        let error_is_terminal =
            err_fd >= 0 && isatty(unsafe { BorrowedFd::borrow_raw(err_fd) }).unwrap_or(false);

        let term_fd = if input_is_terminal {
            Some(unsafe { BorrowedFd::borrow_raw(input_fd) })
        } else if output_is_terminal {
            Some(unsafe { BorrowedFd::borrow_raw(output_fd) })
        } else if error_is_terminal {
            Some(unsafe { BorrowedFd::borrow_raw(err_fd) })
        } else {
            None
        };

        let forwarding_sigint;
        let console_input = if input_is_terminal && input_fd >= 0 {
            forwarding_sigint = false;
            Some(port_io::input_to_raw_fd_dup(input_fd).unwrap())
        } else {
            #[cfg(target_os = "linux")]
            {
                forwarding_sigint = true;
                let sigint_input = port_io::PortInputSigInt::new();
                let sigint_input_fd = sigint_input.sigint_evt().as_raw_fd();
                register_sigint_handler(sigint_input_fd).map_err(RegisterFsSigwinch)?;
                Some(Box::new(sigint_input) as _)
            }
            #[cfg(not(target_os = "linux"))]
            {
                forwarding_sigint = false;
                Some(port_io::input_empty().unwrap())
            }
        };

        let console_output = if output_is_terminal && output_fd >= 0 {
            Some(port_io::output_to_raw_fd_dup(output_fd).unwrap())
        } else {
            Some(port_io::output_to_log_as_err())
        };

        let terminal_properties = term_fd
            .map(|fd| port_io::term_fd(fd.as_raw_fd()).unwrap())
            .unwrap_or_else(|| port_io::term_fixed_size(0, 0));

        setup_terminal_raw_mode(vmm, term_fd, forwarding_sigint);

        let mut ports = vec![PortDescription::console(
            console_input,
            console_output,
            terminal_properties,
        )];

        if input_fd >= 0 && !input_is_terminal {
            ports.push(PortDescription::input_pipe(
                "krun-stdin",
                port_io::input_to_raw_fd_dup(input_fd).unwrap(),
            ));
        }

        if output_fd >= 0 && !output_is_terminal {
            ports.push(PortDescription::output_pipe(
                "krun-stdout",
                port_io::output_to_raw_fd_dup(output_fd).unwrap(),
            ));
        };

        if err_fd >= 0 && !error_is_terminal {
            ports.push(PortDescription::output_pipe(
                "krun-stderr",
                port_io::output_to_raw_fd_dup(err_fd).unwrap(),
            ));
        }

        Ok(ports)
    }
}

#[cfg(windows)]
fn is_valid_handle(h: *mut core::ffi::c_void) -> bool {
    !h.is_null() && h != INVALID_HANDLE_VALUE
}

#[cfg(target_os = "windows")]
fn autoconfigure_console_ports(
    vmm: &mut Vmm,
    vm_resources: &VmResources,
    cfg: Option<&DefaultVirtioConsoleConfig>,
    creating_implicit_console: bool,
) -> std::result::Result<Vec<PortDescription>, StartMicrovmError> {
    use self::StartMicrovmError::*;

    let mut console_output_path: Option<PathBuf> = None;
    if let Some(path) = vm_resources.console_output.clone() {
        if !vm_resources.disable_implicit_console && creating_implicit_console {
            console_output_path = Some(path)
        }
    }

    if let Some(console_output_path) = console_output_path {
        let file = File::create(console_output_path).map_err(OpenConsoleFile)?;
        // Manually emulate our Legacy behavior: In the case of output_path we have always used the
        // stdin to determine the console size
        let stdin_h = unsafe { BorrowedHandle::borrow_raw(GetStdHandle(STD_INPUT_HANDLE)) };
        let term_h = if stdin_h.is_terminal() {
            port_io::term_handle(stdin_h.as_raw_handle()).unwrap()
        } else {
            port_io::term_fixed_size(0, 0)
        };
        Ok(vec![PortDescription::console(
            Some(port_io::input_empty().unwrap()),
            Some(port_io::output_file(file).unwrap()),
            term_h,
        )])
    } else {
        let (input_h, output_h, err_h) = match cfg {
            Some(c) => (
                c.input_handle.as_raw_handle(),
                c.output_handle.as_raw_handle(),
                c.err_handle.as_raw_handle(),
            ),
            None => unsafe {
                (
                    GetStdHandle(STD_INPUT_HANDLE),
                    GetStdHandle(STD_OUTPUT_HANDLE),
                    GetStdHandle(STD_ERROR_HANDLE),
                )
            },
        };
        let input_is_terminal = (unsafe { BorrowedHandle::borrow_raw(input_h) }).is_terminal();
        let output_is_terminal = (unsafe { BorrowedHandle::borrow_raw(output_h) }).is_terminal();
        let error_is_terminal = (unsafe { BorrowedHandle::borrow_raw(err_h) }).is_terminal();

        let term_h = if input_is_terminal {
            Some(SendHandle::new(input_h))
        } else if output_is_terminal {
            Some(SendHandle::new(output_h))
        } else if error_is_terminal {
            Some(SendHandle::new(err_h))
        } else {
            None
        };

        let forwarding_sigint = false;
        let console_input = if input_is_terminal {
            Some(port_io::input_to_handle_dup(input_h).unwrap())
        } else {
            Some(port_io::input_empty().unwrap())
        };

        let console_output = if output_is_terminal {
            Some(port_io::output_to_handle_dup(output_h).unwrap())
        } else {
            Some(port_io::output_to_log_as_err())
        };

        let terminal_properties = term_h
            .map(|h| port_io::term_handle(h.as_raw_handle()).unwrap())
            .unwrap_or_else(|| port_io::term_fixed_size(0, 0));

        setup_terminal_raw_mode(vmm, term_h, forwarding_sigint);

        let mut ports = vec![PortDescription::console(
            console_input,
            console_output,
            terminal_properties,
        )];

        if is_valid_handle(input_h) && !input_is_terminal {
            ports.push(PortDescription::input_pipe(
                "krun-stdin",
                port_io::input_to_handle_dup(input_h).unwrap(),
            ));
        }

        if is_valid_handle(output_h) && !output_is_terminal {
            ports.push(PortDescription::output_pipe(
                "krun-stdout",
                port_io::output_to_handle_dup(output_h).unwrap(),
            ));
        };

        if is_valid_handle(err_h) && !error_is_terminal {
            ports.push(PortDescription::output_pipe(
                "krun-stderr",
                port_io::output_to_handle_dup(err_h).unwrap(),
            ));
        }

        Ok(ports)
    }
}

#[cfg(unix)]
fn setup_terminal_raw_mode(
    vmm: &mut Vmm,
    term_fd: Option<BorrowedFd<'_>>,
    handle_signals_by_terminal: bool,
) {
    if let Some(term_fd) = term_fd {
        match term_set_raw_mode(term_fd, handle_signals_by_terminal) {
            Ok(old_mode) => {
                let raw_fd = term_fd.as_raw_fd();
                vmm.exit_observers.push(Arc::new(Mutex::new(move || {
                    if let Err(e) =
                        term_restore_mode(unsafe { BorrowedFd::borrow_raw(raw_fd) }, &old_mode)
                    {
                        log::error!("Failed to restore terminal mode: {e}")
                    }
                })));
            }
            Err(e) => {
                log::error!("Failed to set terminal to raw mode: {e}")
            }
        };
    }
}

#[cfg(target_os = "windows")]
fn setup_terminal_raw_mode(
    vmm: &mut Vmm,
    term_handle: Option<SendHandle>,
    handle_signals_by_terminal: bool,
) {
    if let Some(term_handle) = term_handle {
        match term_set_raw_mode(term_handle, handle_signals_by_terminal) {
            Ok(old_mode) => {
                vmm.exit_observers.push(Arc::new(Mutex::new(move || {
                    if let Err(e) = term_restore_mode(term_handle, &old_mode) {
                        log::error!("Failed to restore terminal mode: {e}")
                    }
                })));
            }
            Err(e) => {
                log::error!("Failed to set terminal to raw mode: {e}")
            }
        };
    }
}

#[cfg(unix)]
fn create_explicit_ports(
    vmm: &mut Vmm,
    port_configs: &[PortConfig],
) -> std::result::Result<Vec<PortDescription>, StartMicrovmError> {
    let mut ports = Vec::with_capacity(port_configs.len());

    for port_cfg in port_configs {
        let port_desc = match port_cfg {
            PortConfig::Tty { name, tty_fd } => {
                assert!(*tty_fd > 0, "PortConfig::Tty must have a valid tty_fd");
                let term_fd = unsafe { BorrowedFd::borrow_raw(*tty_fd) };
                setup_terminal_raw_mode(vmm, Some(term_fd), false);

                PortDescription {
                    name: name.clone().into(),
                    input: Some(port_io::input_to_raw_fd_dup(*tty_fd).unwrap()),
                    output: Some(port_io::output_to_raw_fd_dup(*tty_fd).unwrap()),
                    terminal: Some(port_io::term_fd(*tty_fd).unwrap()),
                }
            }
            PortConfig::InOut {
                name,
                input_fd,
                output_fd,
            } => PortDescription {
                name: name.clone().into(),
                input: if *input_fd < 0 {
                    None
                } else {
                    Some(port_io::input_to_raw_fd_dup(*input_fd).unwrap())
                },
                output: if *output_fd < 0 {
                    None
                } else {
                    Some(port_io::output_to_raw_fd_dup(*output_fd).unwrap())
                },
                terminal: None,
            },
        };

        ports.push(port_desc);
    }

    Ok(ports)
}

#[cfg(target_os = "windows")]
fn create_explicit_ports(
    vmm: &mut Vmm,
    port_configs: &[PortConfig],
) -> std::result::Result<Vec<PortDescription>, StartMicrovmError> {
    let mut ports = Vec::with_capacity(port_configs.len());

    for port_cfg in port_configs {
        let port_desc = match port_cfg {
            PortConfig::Tty { name, tty_handle } => {
                assert!(
                    is_valid_handle(tty_handle.as_raw_handle()),
                    "PortConfig::Tty must have a valid tty_handle"
                );
                let term_h = SendHandle::new(tty_handle.as_raw_handle());
                setup_terminal_raw_mode(vmm, Some(term_h), false);

                PortDescription {
                    name: name.clone().into(),
                    input: Some(port_io::input_to_handle_dup(tty_handle.as_raw_handle()).unwrap()),
                    output: Some(
                        port_io::output_to_handle_dup(tty_handle.as_raw_handle()).unwrap(),
                    ),
                    terminal: Some(port_io::term_handle(tty_handle.as_raw_handle()).unwrap()),
                }
            }
            PortConfig::InOut {
                name,
                input_handle,
                output_handle,
            } => PortDescription {
                name: name.clone().into(),
                input: if !is_valid_handle(input_handle.as_raw_handle()) {
                    None
                } else {
                    Some(port_io::input_to_handle_dup(input_handle.as_raw_handle()).unwrap())
                },
                output: if !is_valid_handle(output_handle.as_raw_handle()) {
                    None
                } else {
                    Some(port_io::output_to_handle_dup(output_handle.as_raw_handle()).unwrap())
                },
                terminal: None,
            },
        };

        ports.push(port_desc);
    }

    Ok(ports)
}

fn attach_console_devices(
    vmm: &mut Vmm,
    event_manager: &mut EventManager,
    intc: IrqChip,
    vm_resources: &VmResources,
    cfg: Option<&VirtioConsoleConfigMode>,
    id_number: u32,
) -> std::result::Result<(), StartMicrovmError> {
    use self::StartMicrovmError::*;

    let creating_implicit_console = cfg.is_none();

    let ports = match cfg {
        None => autoconfigure_console_ports(vmm, vm_resources, None, creating_implicit_console)?,
        Some(VirtioConsoleConfigMode::Autoconfigure(autocfg)) => autoconfigure_console_ports(
            vmm,
            vm_resources,
            Some(autocfg),
            creating_implicit_console,
        )?,
        Some(VirtioConsoleConfigMode::Explicit(ports)) => create_explicit_ports(vmm, ports)?,
    };

    let console = Arc::new(Mutex::new(devices::virtio::Console::new(ports).unwrap()));

    vmm.exit_observers.push(console.clone());

    event_manager
        .add_subscriber(console.clone())
        .map_err(RegisterEvent)?;

    #[cfg(target_os = "linux")]
    register_sigwinch_handler(console.lock().unwrap().get_sigwinch_fd())
        .map_err(RegisterFsSigwinch)?;

    // The device mutex mustn't be locked here otherwise it will deadlock.
    attach_mmio_device(vmm, format!("hvc{id_number}"), intc, console)
        .map_err(RegisterConsoleDevice)?;

    Ok(())
}

#[cfg(feature = "net")]
fn attach_net_devices(
    vmm: &mut Vmm,
    net_devices: &NetBuilder,
    intc: IrqChip,
) -> Result<(), StartMicrovmError> {
    for net_device in net_devices.list.iter() {
        let id = net_device.lock().unwrap().id().to_string();

        attach_mmio_device(vmm, id, intc.clone(), net_device.clone())
            .map_err(StartMicrovmError::RegisterNetDevice)?;
    }
    Ok(())
}

fn attach_unixsock_vsock_device(
    vmm: &mut Vmm,
    unix_vsock: &Arc<Mutex<Vsock>>,
    event_manager: &mut EventManager,
    intc: IrqChip,
) -> std::result::Result<(), StartMicrovmError> {
    use self::StartMicrovmError::*;

    event_manager
        .add_subscriber(unix_vsock.clone())
        .map_err(RegisterEvent)?;

    let id = String::from(unix_vsock.lock().unwrap().id());

    // The device mutex mustn't be locked here otherwise it will deadlock.
    attach_mmio_device(vmm, id, intc, unix_vsock.clone()).map_err(RegisterVsockDevice)?;

    Ok(())
}

#[cfg(not(feature = "tee"))]
#[cfg(unix)]
fn attach_balloon_device(
    vmm: &mut Vmm,
    event_manager: &mut EventManager,
    intc: IrqChip,
) -> std::result::Result<(), StartMicrovmError> {
    use self::StartMicrovmError::*;

    let balloon = Arc::new(Mutex::new(devices::virtio::Balloon::new().unwrap()));

    event_manager
        .add_subscriber(balloon.clone())
        .map_err(RegisterEvent)?;

    let id = String::from(balloon.lock().unwrap().id());

    // The device mutex mustn't be locked here otherwise it will deadlock.
    attach_mmio_device(vmm, id, intc.clone(), balloon).map_err(RegisterBalloonDevice)?;

    Ok(())
}

#[cfg(feature = "blk")]
fn attach_block_devices(
    vmm: &mut Vmm,
    block_devs: &BlockBuilder,
    intc: IrqChip,
) -> std::result::Result<(), StartMicrovmError> {
    use self::StartMicrovmError::*;

    for block in block_devs.list.iter() {
        let id = String::from(block.lock().unwrap().id());

        // The device mutex mustn't be locked here otherwise it will deadlock.
        attach_mmio_device(vmm, id, intc.clone(), block.clone()).map_err(RegisterBlockDevice)?;
    }

    Ok(())
}

#[cfg(not(feature = "tee"))]
#[cfg(unix)]
fn attach_rng_device(
    vmm: &mut Vmm,
    event_manager: &mut EventManager,
    intc: IrqChip,
) -> std::result::Result<(), StartMicrovmError> {
    use self::StartMicrovmError::*;

    let rng = Arc::new(Mutex::new(devices::virtio::Rng::new().unwrap()));

    event_manager
        .add_subscriber(rng.clone())
        .map_err(RegisterEvent)?;

    let id = String::from(rng.lock().unwrap().id());

    // The device mutex mustn't be locked here otherwise it will deadlock.
    attach_mmio_device(vmm, id, intc.clone(), rng).map_err(RegisterRngDevice)?;

    Ok(())
}

#[cfg(not(feature = "tee"))]
#[cfg(all(feature = "vhost-user", target_os = "linux"))]
fn attach_vhost_user_device(
    vmm: &mut Vmm,
    event_manager: &mut EventManager,
    intc: IrqChip,
    device_config: &VhostUserDeviceConfig,
) -> std::result::Result<(), StartMicrovmError> {
    use self::StartMicrovmError::*;

    let device_name = device_config
        .name
        .clone()
        .unwrap_or_else(|| format!("vhost-user-{}", device_config.device_type));

    let device = Arc::new(Mutex::new(
        devices::virtio::VhostUserDevice::new(
            &device_config.socket_path,
            device_config.device_type,
            device_name.clone(),
            device_config.num_queues,
            &device_config.queue_sizes,
        )
        .map_err(|e| RegisterVhostUserDevice(device_manager::mmio::Error::VhostUserDevice(e)))?,
    ));

    event_manager
        .add_subscriber(device.clone())
        .map_err(RegisterEvent)?;

    attach_mmio_device(vmm, device_name, intc.clone(), device).map_err(RegisterVhostUserDevice)?;

    Ok(())
}

#[cfg(feature = "gpu")]
#[allow(clippy::too_many_arguments)]
fn attach_gpu_device(
    vmm: &mut Vmm,
    shm_manager: &mut ShmManager,
    #[cfg(not(feature = "tee"))] mut export_table: Option<ExportTable>,
    intc: IrqChip,
    virgl_flags: u32,
    displays: Box<[DisplayInfo]>,
    display_backend: DisplayBackend<'static>,
    #[cfg(target_os = "macos")] map_sender: Sender<WorkerMessage>,
) -> std::result::Result<(), StartMicrovmError> {
    use self::StartMicrovmError::*;

    let gpu = Arc::new(Mutex::new(
        devices::virtio::Gpu::new(
            virgl_flags,
            displays,
            display_backend,
            #[cfg(target_os = "macos")]
            map_sender,
        )
        .unwrap(),
    ));

    let id = String::from(gpu.lock().unwrap().id());

    if let Some(shm_region) = shm_manager.gpu_region() {
        gpu.lock().unwrap().set_shm_region(VirtioShmRegion {
            host_addr: vmm
                .guest_memory
                .get_host_address(shm_region.guest_addr)
                .map_err(StartMicrovmError::ShmHostAddr)? as u64,
            guest_addr: shm_region.guest_addr.raw_value(),
            size: shm_region.size,
        });
    }

    #[cfg(not(feature = "tee"))]
    if let Some(export_table) = export_table.take() {
        gpu.lock().unwrap().set_export_table(export_table);
    }

    // The device mutex mustn't be locked here otherwise it will deadlock.
    attach_mmio_device(vmm, id, intc, gpu).map_err(RegisterGpuDevice)?;

    Ok(())
}

#[cfg(feature = "input")]
fn attach_input_devices(
    vmm: &mut Vmm,
    input_backends: &[(
        krun_input::InputConfigBackend<'static>,
        krun_input::InputEventProviderBackend<'static>,
    )],
    intc: IrqChip,
) -> std::result::Result<(), StartMicrovmError> {
    use self::StartMicrovmError::*;

    for (index, (config_backend, events_backend)) in input_backends.iter().enumerate() {
        let input_device = Arc::new(Mutex::new(
            devices::virtio::input::Input::new(*config_backend, *events_backend).unwrap(),
        ));

        let id = format!("input{}", index);
        attach_mmio_device(vmm, id, intc.clone(), input_device).map_err(RegisterInputDevice)?;
    }

    Ok(())
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::vmm_config::kernel_bundle::KernelBundle;

    #[allow(unused)]
    fn default_guest_memory(
        mem_size_mib: usize,
    ) -> std::result::Result<
        (GuestMemoryMmap, ArchMemoryInfo, ShmManager, PayloadConfig),
        StartMicrovmError,
    > {
        let mut vm_resources = VmResources::default();
        vm_resources.kernel_bundle = Some(KernelBundle {
            host_addr: 0x1000,
            guest_addr: 0x1000,
            entry_addr: 0x1000,
            size: 0x1000,
        });

        create_guest_memory(mem_size_mib, &vm_resources, &Payload::Empty)
    }

    #[test]
    #[cfg(target_arch = "x86_64")]
    fn test_create_vcpus_x86_64() {
        let vcpu_count = 2;

        let vcpu_config = VcpuConfig {
            vcpu_count,
            ht_enabled: false,
            cpu_template: None,
            nested_enabled: false,
        };

        let (guest_memory, _arch_memory_info, _shm_manager, _payload_config) =
            default_guest_memory(128).unwrap();
        let vm = setup_vm(&guest_memory, false).unwrap();
        let _kvmioapic = KvmIoapic::new(vm.fd()).unwrap();

        // Dummy entry_addr, vcpus will not boot.
        let entry_addr = GuestAddress(0);
        let bus = devices::Bus::new();
        let vcpu_vec = create_vcpus_x86_64(
            &vm,
            &vcpu_config,
            &guest_memory,
            entry_addr,
            &bus,
            &EventFd::new(utils::eventfd::EFD_NONBLOCK).unwrap(),
            true,
            false,
        )
        .unwrap();
        assert_eq!(vcpu_vec.len(), vcpu_count as usize);
    }

    #[test]
    #[cfg(all(target_arch = "aarch64", target_os = "linux"))]
    fn test_create_vcpus_aarch64() {
        let (guest_memory, arch_memory_info, _shm_manager, _payload_config) =
            default_guest_memory(128).unwrap();
        let vm = setup_vm(&guest_memory, false).unwrap();
        let vcpu_count = 2;

        let vcpu_config = VcpuConfig {
            vcpu_count,
            ht_enabled: false,
            cpu_template: None,
            nested_enabled: false,
        };

        // Dummy entry_addr, vcpus will not boot.
        let entry_addr = GuestAddress(0);
        let vcpu_vec = create_vcpus_aarch64(
            &vm,
            &vcpu_config,
            &arch_memory_info,
            entry_addr,
            &EventFd::new(utils::eventfd::EFD_NONBLOCK).unwrap(),
        )
        .unwrap();
        assert_eq!(vcpu_vec.len(), vcpu_count as usize);
    }

    #[test]
    fn test_error_messages() {
        use crate::builder::StartMicrovmError::*;
        let err = AttachBlockDevice(io::Error::from_raw_os_error(0));
        let _ = format!("{err}{err:?}");

        let err = CreateRateLimiter(io::Error::from_raw_os_error(0));
        let _ = format!("{err}{err:?}");

        let err = Internal(Error::Serial(io::Error::from_raw_os_error(0)));
        let _ = format!("{err}{err:?}");

        let err = InvalidKernelBundle(vm_memory::mmap::MmapRegionError::InvalidPointer);
        let _ = format!("{err}{err:?}");

        let err = KernelCmdline(String::from("dummy --cmdline"));
        let _ = format!("{err}{err:?}");

        let err = LoadCommandline(kernel::cmdline::Error::TooLarge);
        let _ = format!("{err}{err:?}");

        let err = MicroVMAlreadyRunning;
        let _ = format!("{err}{err:?}");

        let err = MissingKernelConfig;
        let _ = format!("{err}{err:?}");

        let err = MissingMemSizeConfig;
        let _ = format!("{err}{err:?}");

        let err = NetDeviceNotConfigured;
        let _ = format!("{err}{err:?}");

        let err = OpenBlockDevice(io::Error::from_raw_os_error(0));
        let _ = format!("{err}{err:?}");

        let err = RegisterBlockDevice(device_manager::mmio::Error::EventFd(
            io::Error::from_raw_os_error(0),
        ));
        let _ = format!("{err}{err:?}");

        let err = RegisterEvent(EventManagerError::EpollCreate(
            io::Error::from_raw_os_error(0),
        ));
        let _ = format!("{err}{err:?}");

        let err = RegisterNetDevice(device_manager::mmio::Error::EventFd(
            io::Error::from_raw_os_error(0),
        ));
        let _ = format!("{err}{err:?}");

        let err = RegisterVsockDevice(device_manager::mmio::Error::EventFd(
            io::Error::from_raw_os_error(0),
        ));
        let _ = format!("{err}{err:?}");
    }

    #[test]
    fn test_kernel_cmdline_err_to_startuvm_err() {
        let err = StartMicrovmError::from(kernel::cmdline::Error::HasSpace);
        let _ = format!("{err}{err:?}");
    }
}

// --- snapshot restore -------------------------------------------------------

/// Loaded contents of a krun snapshot directory (linux/KVM).
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub struct LoadedSnapshot {
    pub guest_memory: GuestMemoryMmap,
    pub vm_state: crate::vstate::VmState,
    pub vcpu_states: Vec<crate::vstate::VcpuState>,
    pub device_states: Vec<devices::virtio::MmioTransportState>,
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub fn load_snapshot(dir: &std::path::Path) -> std::result::Result<LoadedSnapshot, String> {
    use std::io::Read;

    let format = std::fs::read_to_string(dir.join("format.txt"))
        .map_err(|e| format!("read format.txt: {e}"))?;
    if format.trim() != "krun-snapshot-v1" {
        return Err(format!("unsupported snapshot format: {}", format.trim()));
    }

    // Guest memory: each region mapped MAP_PRIVATE from memory.bin — pages
    // fault in on demand and clones share them copy-on-write.
    let mem_file =
        std::fs::File::open(dir.join("memory.bin")).map_err(|e| format!("memory.bin: {e}"))?;
    let mut rd = std::io::BufReader::new(&mem_file);
    let mut u64buf = [0u8; 8];
    let mut read_u64 = |rd: &mut dyn Read| -> std::result::Result<u64, String> {
        rd.read_exact(&mut u64buf)
            .map_err(|e| format!("memory.bin header: {e}"))?;
        Ok(u64::from_le_bytes(u64buf))
    };
    let n_regions = read_u64(&mut rd)? as usize;
    let mut layout = Vec::with_capacity(n_regions);
    for _ in 0..n_regions {
        let addr = read_u64(&mut rd)?;
        let len = read_u64(&mut rd)?;
        layout.push((addr, len));
    }
    // Region data is page-aligned in the file (mmap offset requirement).
    let data_start = (8 + n_regions as u64 * 16).next_multiple_of(4096);

    let mut regions = Vec::with_capacity(n_regions);
    let mut off = data_start;
    for (addr, len) in &layout {
        let file = mem_file.try_clone().map_err(|e| e.to_string())?;
        let mmap = MmapRegion::build(
            Some(vm_memory::FileOffset::new(file, off)),
            *len as usize,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_NORESERVE,
        )
        .map_err(|e| format!("map region @{addr:#x}: {e}"))?;
        regions.push(
            GuestRegionMmap::new(mmap, GuestAddress(*addr))
                .ok_or_else(|| "bad region".to_string())?,
        );
        off += len;
    }
    let guest_memory = GuestMemoryMmap::from_regions(regions)
        .map_err(|e| format!("assemble guest memory: {e}"))?;

    let mut f = std::io::BufReader::new(
        std::fs::File::open(dir.join("vm.state")).map_err(|e| format!("vm.state: {e}"))?,
    );
    let vm_state =
        crate::vstate::VmState::deserialize_from(&mut f).map_err(|e| format!("vm.state: {e}"))?;

    let mut f = std::io::BufReader::new(
        std::fs::File::open(dir.join("vcpus.state")).map_err(|e| format!("vcpus.state: {e}"))?,
    );
    let mut cbuf = [0u8; 8];
    f.read_exact(&mut cbuf).map_err(|e| e.to_string())?;
    let n_vcpus = u64::from_le_bytes(cbuf) as usize;
    let mut vcpu_states = Vec::with_capacity(n_vcpus);
    for _ in 0..n_vcpus {
        vcpu_states.push(
            crate::vstate::VcpuState::deserialize_from(&mut f)
                .map_err(|e| format!("vcpu state: {e}"))?,
        );
    }

    let device_states = load_device_states(&dir.join("devices.state"))?;

    Ok(LoadedSnapshot {
        guest_memory,
        vm_state,
        vcpu_states,
        device_states,
    })
}

#[cfg(all(
    any(target_os = "linux", target_os = "windows"),
    target_arch = "x86_64"
))]
fn load_device_states(
    path: &std::path::Path,
) -> std::result::Result<Vec<devices::virtio::MmioTransportState>, String> {
    use std::io::Read;

    let mut f = std::io::BufReader::new(
        std::fs::File::open(path).map_err(|e| format!("devices.state: {e}"))?,
    );
    let mut b8 = [0u8; 8];
    let mut b4 = [0u8; 4];
    let mut ru64 = |f: &mut dyn Read| -> std::result::Result<u64, String> {
        f.read_exact(&mut b8).map_err(|e| e.to_string())?;
        Ok(u64::from_le_bytes(b8))
    };
    let mut ru32 = |f: &mut dyn Read| -> std::result::Result<u32, String> {
        f.read_exact(&mut b4).map_err(|e| e.to_string())?;
        Ok(u32::from_le_bytes(b4))
    };

    let n = ru64(&mut f)? as usize;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let device_type = ru32(&mut f)?;
        let features_select = ru32(&mut f)?;
        let acked_features_select = ru32(&mut f)?;
        let queue_select = ru32(&mut f)?;
        let device_status = ru32(&mut f)?;
        let config_generation = ru32(&mut f)?;
        let shm_region_select = ru32(&mut f)?;
        let interrupt_status = ru64(&mut f)?;
        let acked_features = ru64(&mut f)?;
        let n_queues = ru64(&mut f)? as usize;
        let mut queue_states = Vec::with_capacity(n_queues);
        for _ in 0..n_queues {
            let mut qs = std::mem::MaybeUninit::<devices::virtio::QueueState>::zeroed();
            // SAFETY: QueueState is repr(C) plain data; filling its bytes
            // fully initializes it.
            unsafe {
                let bytes = std::slice::from_raw_parts_mut(
                    qs.as_mut_ptr() as *mut u8,
                    std::mem::size_of::<devices::virtio::QueueState>(),
                );
                f.read_exact(bytes).map_err(|e| e.to_string())?;
                queue_states.push(qs.assume_init());
            }
        }
        out.push(devices::virtio::MmioTransportState {
            device_type,
            features_select,
            acked_features_select,
            queue_select,
            device_status,
            config_generation,
            shm_region_select,
            interrupt_status,
            acked_features,
            queue_states,
        });
    }
    Ok(out)
}

/// Builds and starts a microVM from a krun snapshot directory instead of
/// booting a kernel.
///
/// The caller must supply the same `VmResources` the VM was originally
/// started with: devices are created in the same order (and thus at the same
/// bus addresses and IRQs) as at save time, then each virtio transport's
/// state is restored on top and DRIVER_OK devices are re-activated against
/// the restored guest RAM. No boot-time guest memory writes happen on this
/// path — the RAM image from the snapshot is authoritative.
#[cfg(all(
    target_os = "linux",
    target_arch = "x86_64",
    not(any(feature = "tee", feature = "aws-nitro"))
))]
pub fn build_microvm_from_snapshot(
    vm_resources: &super::resources::VmResources,
    event_manager: &mut EventManager,
    restore_dir: &std::path::Path,
    _shutdown_efd: Option<EventFd>,
    _sender: Sender<WorkerMessage>,
) -> std::result::Result<Arc<Mutex<Vmm>>, StartMicrovmError> {
    use vm_memory::GuestMemoryRegion;

    let err = StartMicrovmError::RestoreSnapshot;

    if vm_resources.split_irqchip {
        return Err(err(
            "snapshot restore requires the in-kernel irqchip".to_string()
        ));
    }
    if vm_resources.fs.iter().any(|f| f.shm_size.is_some()) {
        return Err(err(
            "snapshot restore does not support fs DAX/shm regions".to_string(),
        ));
    }
    if vm_resources.gpu_virgl_flags.is_some() {
        return Err(err(
            "snapshot restore does not support GPU devices".to_string()
        ));
    }
    #[cfg(feature = "vhost-user")]
    if !vm_resources.vhost_user_devices.is_empty() {
        return Err(err(
            "snapshot restore does not support vhost-user devices".to_string(),
        ));
    }

    let snapshot = load_snapshot(restore_dir).map_err(err)?;

    let mem_size = vm_resources
        .vm_config()
        .mem_size_mib
        .ok_or(StartMicrovmError::MissingMemSizeConfig)?
        << 20;

    // Recompute the layout the boot path would have used; the snapshot's
    // region table must agree or the restored RAM image would land at the
    // wrong guest addresses.
    let payload = choose_payload(vm_resources)?;
    let (arch_memory_info, arch_mem_regions) = match &payload {
        Payload::ExternalKernel(external_kernel) => {
            arch::arch_memory_regions(mem_size, None, 0, external_kernel.initramfs_size, None)
        }
        _ => {
            return Err(err(
                "snapshot restore requires an external kernel configuration".to_string(),
            ));
        }
    };
    let want: Vec<(u64, u64)> = arch_mem_regions
        .iter()
        .map(|(a, l)| (a.raw_value(), *l as u64))
        .collect();
    let got: Vec<(u64, u64)> = snapshot
        .guest_memory
        .iter()
        .map(|r| (r.start_addr().raw_value(), r.len()))
        .collect();
    if want != got {
        return Err(err(format!(
            "snapshot memory layout {got:x?} does not match configuration {want:x?}"
        )));
    }

    let guest_memory = snapshot.guest_memory.clone();
    let vcpu_config = vm_resources.vcpu_config();
    if snapshot.vcpu_states.len() != vcpu_config.vcpu_count as usize {
        return Err(err(format!(
            "snapshot has {} vcpus but the configuration wants {}",
            snapshot.vcpu_states.len(),
            vcpu_config.vcpu_count
        )));
    }

    // The boot command line is irrelevant after a restore, but Vmm keeps one.
    let mut kernel_cmdline = Cmdline::new(arch::CMDLINE_MAX_SIZE);
    kernel_cmdline.insert_str(DEFAULT_KERNEL_CMDLINE).unwrap();

    let vm = setup_vm(&guest_memory, vm_resources.nested_enabled)?;

    let mut _shm_manager = ShmManager::new(&arch_memory_info);

    let mut serial_devices = Vec::new();
    let mut serial_ttys = Vec::new();
    for s in &vm_resources.serial_consoles {
        let input: Option<Box<dyn devices::legacy::ReadableFd + Send>> = if s.input_fd >= 0 {
            let file = unsafe { File::from_raw_fd(s.input_fd) };
            if file.is_terminal() {
                serial_ttys.push(unsafe { BorrowedFd::borrow_raw(file.as_raw_fd()) });
            }
            Some(Box::new(file))
        } else {
            None
        };
        let output: Option<Box<dyn io::Write + Send>> = if s.output_fd >= 0 {
            Some(Box::new(unsafe { File::from_raw_fd(s.output_fd) }))
        } else {
            None
        };
        serial_devices.push(setup_serial_device(event_manager, input, output)?);
    }

    let exit_evt = EventFd::new(utils::eventfd::EFD_NONBLOCK)
        .map_err(Error::EventFd)
        .map_err(StartMicrovmError::Internal)?;

    let mut pio_device_manager = PortIODeviceManager::new(
        Arc::new(Mutex::new(Cmos::new(
            arch_memory_info.ram_below_gap,
            arch_memory_info.ram_above_gap,
        ))),
        serial_devices,
        exit_evt
            .try_clone()
            .map_err(Error::EventFd)
            .map_err(StartMicrovmError::Internal)?,
    )
    .map_err(Error::CreateLegacyDevice)
    .map_err(StartMicrovmError::Internal)?;

    let mut mmio_device_manager = MMIODeviceManager::new(
        &mut (arch::MMIO_MEM_START.clone()),
        (arch::IRQ_BASE, arch::IRQ_MAX),
    );

    // KvmIoapic::new creates the in-kernel irqchip and PIT; both must exist
    // before vcpus are created and before VmState is pushed back in.
    let ioapic: Box<dyn IrqChipT> =
        Box::new(KvmIoapic::new(vm.fd()).map_err(StartMicrovmError::CreateKvmIrqChip)?);
    let intc: IrqChip = Arc::new(Mutex::new(IrqChipDevice::new(ioapic)));

    attach_legacy_devices(
        &vm,
        false,
        &mut pio_device_manager,
        &mut mmio_device_manager,
        Some(intc.clone()),
    )?;

    vm.restore_state(&snapshot.vm_state)
        .map_err(Error::Vm)
        .map_err(StartMicrovmError::Internal)?;

    let mut vcpus = Vec::with_capacity(vcpu_config.vcpu_count as usize);
    for (i, state) in snapshot.vcpu_states.into_iter().enumerate() {
        let vcpu = Vcpu::new_x86_64(
            i as u8,
            vm.fd(),
            vm.supported_cpuid().clone(),
            vm.supported_msrs().clone(),
            pio_device_manager.io_bus.clone(),
            exit_evt
                .try_clone()
                .map_err(Error::EventFd)
                .map_err(StartMicrovmError::Internal)?,
        )
        .map_err(Error::Vcpu)
        .map_err(StartMicrovmError::Internal)?;
        // No configure_x86_64() here: boot configuration scribbles page
        // tables over guest RAM. The saved KVM state carries everything.
        vcpu.restore_state(state)
            .map_err(Error::Vcpu)
            .map_err(StartMicrovmError::Internal)?;
        vcpus.push(vcpu);
    }

    let exit_code = Arc::new(AtomicI32::new(i32::MAX));
    let mut vmm = Vmm {
        guest_memory,
        arch_memory_info,
        kernel_cmdline,
        vcpus_handles: Vec::new(),
        exit_evt,
        exit_observers: Vec::new(),
        exit_code: exit_code.clone(),
        vm,
        mmio_device_manager,
        pio_device_manager,
    };

    for serial_tty in serial_ttys {
        setup_terminal_raw_mode(&mut vmm, Some(serial_tty), false);
    }

    // Device creation below mirrors build_microvm's order exactly — bus
    // addresses and IRQ numbers are assigned by creation order and must
    // match what the snapshotted guest saw.
    attach_balloon_device(&mut vmm, event_manager, intc.clone())?;
    attach_rng_device(&mut vmm, event_manager, intc.clone())?;

    let mut console_id = 0;
    if !vm_resources.disable_implicit_console {
        attach_console_devices(
            &mut vmm,
            event_manager,
            intc.clone(),
            vm_resources,
            None,
            console_id,
        )?;
        console_id += 1;
    }
    for console_cfg in vm_resources.virtio_consoles.iter() {
        attach_console_devices(
            &mut vmm,
            event_manager,
            intc.clone(),
            vm_resources,
            Some(console_cfg),
            console_id,
        )?;
        console_id += 1;
    }

    let export_table: Option<ExportTable> = if cfg!(feature = "gpu") {
        Some(Default::default())
    } else {
        None
    };

    #[cfg(feature = "input")]
    if !vm_resources.input_backends.is_empty() {
        attach_input_devices(&mut vmm, &vm_resources.input_backends, intc.clone())?;
    }

    attach_fs_devices(
        &mut vmm,
        &vm_resources.fs,
        &mut _shm_manager,
        export_table,
        intc.clone(),
        exit_code,
    )?;
    #[cfg(feature = "blk")]
    attach_block_devices(&mut vmm, &vm_resources.block, intc.clone())?;

    if let Some(vsock) = vm_resources.vsock.get() {
        attach_unixsock_vsock_device(&mut vmm, vsock, event_manager, intc.clone())?;
    }

    #[cfg(feature = "net")]
    attach_net_devices(&mut vmm, &vm_resources.net, intc.clone())?;

    // Push the saved virtio state into the freshly-created transports.
    // Bus order is address order, the same order save_devices used.
    let device_states = snapshot.device_states;
    let mut idx = 0usize;
    for (_addr, dev) in vmm.mmio_device_manager.bus.iter_devices() {
        let mut locked = dev.lock().unwrap();
        if let Some(t) = locked.as_mut_any().downcast_mut::<MmioTransport>() {
            let st = device_states.get(idx).ok_or_else(|| {
                StartMicrovmError::RestoreSnapshot(format!(
                    "snapshot has {} virtio devices but the configuration created more",
                    device_states.len()
                ))
            })?;
            let created_type = t.snapshot_state().device_type;
            if st.device_type != created_type {
                return Err(err(format!(
                    "virtio device order mismatch at index {idx}: snapshot type {} \
                     vs created type {created_type}",
                    st.device_type
                )));
            }
            t.restore_from_state(st);
            idx += 1;
        }
    }
    if idx != device_states.len() {
        return Err(err(format!(
            "snapshot has {} virtio devices but the configuration created {idx}",
            device_states.len()
        )));
    }

    vmm.start_vcpus(vcpus)
        .map_err(StartMicrovmError::Internal)?;

    #[allow(clippy::arc_with_non_send_sync)]
    let vmm = Arc::new(Mutex::new(vmm));
    event_manager
        .add_subscriber(vmm.clone())
        .map_err(StartMicrovmError::RegisterEvent)?;

    Ok(vmm)
}

/// Loaded contents of a krun snapshot directory (Windows/WHP). Unlike the
/// Linux loader, guest memory isn't mapped from the file (no MAP_PRIVATE
/// equivalent through vm-memory on Windows yet) — the caller allocates the
/// normal anonymous guest memory and fills it via [`read_snapshot_memory`].
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
pub struct LoadedSnapshot {
    pub ioapic_state: Vec<u8>,
    pub vcpu_states: Vec<crate::vstate::VcpuState>,
    pub device_states: Vec<devices::virtio::MmioTransportState>,
    pub mem_layout: Vec<(u64, u64)>,
    mem_file: File,
    mem_data_start: u64,
}

#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
pub fn load_snapshot(dir: &std::path::Path) -> std::result::Result<LoadedSnapshot, String> {
    let format = std::fs::read_to_string(dir.join("format.txt"))
        .map_err(|e| format!("read format.txt: {e}"))?;
    if format.trim() != "krun-snapshot-v1" {
        return Err(format!("unsupported snapshot format: {}", format.trim()));
    }

    let mut u64buf = [0u8; 8];
    let mut read_u64 = |rd: &mut dyn Read| -> std::result::Result<u64, String> {
        rd.read_exact(&mut u64buf).map_err(|e| e.to_string())?;
        Ok(u64::from_le_bytes(u64buf))
    };

    let mut f = std::io::BufReader::new(
        std::fs::File::open(dir.join("vm.state")).map_err(|e| format!("vm.state: {e}"))?,
    );
    let ioapic_len = read_u64(&mut f)? as usize;
    let mut ioapic_state = vec![0u8; ioapic_len];
    f.read_exact(&mut ioapic_state)
        .map_err(|e| format!("vm.state: {e}"))?;

    let mut f = std::io::BufReader::new(
        std::fs::File::open(dir.join("vcpus.state")).map_err(|e| format!("vcpus.state: {e}"))?,
    );
    let n_vcpus = read_u64(&mut f)? as usize;
    let mut vcpu_states = Vec::with_capacity(n_vcpus);
    for _ in 0..n_vcpus {
        vcpu_states.push(
            crate::vstate::VcpuState::deserialize_from(&mut f)
                .map_err(|e| format!("vcpu state: {e}"))?,
        );
    }

    let device_states = load_device_states(&dir.join("devices.state"))?;

    let mem_file =
        std::fs::File::open(dir.join("memory.bin")).map_err(|e| format!("memory.bin: {e}"))?;
    let mut rd = std::io::BufReader::new(&mem_file);
    let n_regions = read_u64(&mut rd)? as usize;
    let mut mem_layout = Vec::with_capacity(n_regions);
    for _ in 0..n_regions {
        let addr = read_u64(&mut rd)?;
        let len = read_u64(&mut rd)?;
        mem_layout.push((addr, len));
    }
    let mem_data_start = (8 + n_regions as u64 * 16).next_multiple_of(4096);

    Ok(LoadedSnapshot {
        ioapic_state,
        vcpu_states,
        device_states,
        mem_layout,
        mem_file,
        mem_data_start,
    })
}

/// Fill freshly-allocated guest memory with the snapshot's RAM image.
/// Sparse file holes read back as zeros, matching the anonymous mapping's
/// initial state, so this is a plain sequential copy.
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
fn read_snapshot_memory(
    snap: &LoadedSnapshot,
    guest_memory: &GuestMemoryMmap,
) -> std::result::Result<(), String> {
    use std::io::{Seek, SeekFrom};

    let mut f = std::io::BufReader::with_capacity(1 << 20, &snap.mem_file);
    f.seek(SeekFrom::Start(snap.mem_data_start))
        .map_err(|e| e.to_string())?;
    for (addr, len) in &snap.mem_layout {
        let host = guest_memory
            .get_host_address(GuestAddress(*addr))
            .map_err(|e| format!("host addr for region @{addr:#x}: {e}"))?;
        // SAFETY: the region is a live anonymous mapping of `len` bytes that
        // no vcpu can touch yet (they haven't been created).
        let slice = unsafe { std::slice::from_raw_parts_mut(host, *len as usize) };
        f.read_exact(slice)
            .map_err(|e| format!("read region @{addr:#x}: {e}"))?;
    }
    Ok(())
}

/// Builds and starts a microVM from a krun snapshot directory (Windows/WHP).
/// Mirrors the Windows boot path of `build_microvm`: same device creation
/// order, no boot-time configuration, saved state applied on top.
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
pub fn build_microvm_from_snapshot(
    vm_resources: &super::resources::VmResources,
    event_manager: &mut EventManager,
    restore_dir: &std::path::Path,
    _shutdown_efd: Option<EventFd>,
    _sender: Sender<WorkerMessage>,
) -> std::result::Result<Arc<Mutex<Vmm>>, StartMicrovmError> {
    let err = StartMicrovmError::RestoreSnapshot;

    let snapshot = load_snapshot(restore_dir).map_err(err)?;

    let mem_size = vm_resources
        .vm_config()
        .mem_size_mib
        .ok_or(StartMicrovmError::MissingMemSizeConfig)?
        << 20;

    let payload = choose_payload(vm_resources)?;
    let (arch_memory_info, arch_mem_regions) = match &payload {
        Payload::ExternalKernel(external_kernel) => {
            arch::arch_memory_regions(mem_size, None, 0, external_kernel.initramfs_size, None)
        }
        _ => {
            return Err(err(
                "snapshot restore requires an external kernel configuration".to_string(),
            ));
        }
    };
    let want: Vec<(u64, u64)> = arch_mem_regions
        .iter()
        .map(|(a, l)| (a.raw_value(), *l as u64))
        .collect();
    if want != snapshot.mem_layout {
        return Err(err(format!(
            "snapshot memory layout {:x?} does not match configuration {want:x?}",
            snapshot.mem_layout
        )));
    }

    let vcpu_config = vm_resources.vcpu_config();
    if snapshot.vcpu_states.len() != vcpu_config.vcpu_count as usize {
        return Err(err(format!(
            "snapshot has {} vcpus but the configuration wants {}",
            snapshot.vcpu_states.len(),
            vcpu_config.vcpu_count
        )));
    }

    let guest_memory = GuestMemoryMmap::from_ranges(&arch_mem_regions)
        .map_err(|e| StartMicrovmError::GuestMemoryMmap(format!("{e:?}")))?;
    read_snapshot_memory(&snapshot, &guest_memory).map_err(err)?;

    let mut kernel_cmdline = Cmdline::new(arch::CMDLINE_MAX_SIZE);
    kernel_cmdline.insert_str(DEFAULT_KERNEL_CMDLINE).unwrap();

    let vm = setup_vm(&guest_memory, vcpu_config.vcpu_count)?;

    let mut serial_devices = Vec::new();
    let mut serial_ttys = Vec::new();
    for s in &vm_resources.serial_consoles {
        let input: Option<Box<dyn devices::legacy::ReadableFd + Send>> =
            if is_valid_handle(s.input_handle.as_raw_handle()) {
                if unsafe {
                    BorrowedHandle::borrow_raw(s.input_handle.as_raw_handle()).is_terminal()
                } {
                    serial_ttys.push(s.input_handle);
                }
                Some(Box::new(unsafe {
                    File::from_raw_handle(s.input_handle.as_raw_handle())
                }))
            } else {
                None
            };
        let output: Option<Box<dyn io::Write + Send>> =
            if is_valid_handle(s.output_handle.as_raw_handle()) {
                Some(Box::new(unsafe {
                    File::from_raw_handle(s.output_handle.as_raw_handle())
                }))
            } else {
                None
            };
        serial_devices.push(setup_serial_device(event_manager, input, output)?);
    }

    let exit_evt = EventFd::new(utils::eventfd::EFD_NONBLOCK)
        .map_err(Error::EventFd)
        .map_err(StartMicrovmError::Internal)?;

    let mut pio_device_manager = PortIODeviceManager::new(
        Arc::new(Mutex::new(Cmos::new(
            arch_memory_info.ram_below_gap,
            arch_memory_info.ram_above_gap,
        ))),
        serial_devices,
        exit_evt
            .try_clone()
            .map_err(Error::EventFd)
            .map_err(StartMicrovmError::Internal)?,
    )
    .map_err(Error::CreateLegacyDevice)
    .map_err(StartMicrovmError::Internal)?;

    let mut mmio_device_manager = MMIODeviceManager::new(
        &mut (arch::MMIO_MEM_START.clone()),
        (arch::IRQ_BASE, arch::IRQ_MAX),
    );

    let ioapic: Box<dyn IrqChipT> =
        Box::new(devices::legacy::WhpIoapic::new(vm.whp_vm().clone()));
    let intc: IrqChip = Arc::new(Mutex::new(IrqChipDevice::new(ioapic)));

    attach_legacy_devices(
        &vm,
        &mut pio_device_manager,
        &mut mmio_device_manager,
        Some(intc.clone()),
    )?;

    // Userspace ioapic registers (the LAPIC travels with each vcpu's state).
    intc.lock().unwrap().restore_state(&snapshot.ioapic_state);

    let mut vcpus = Vec::with_capacity(vcpu_config.vcpu_count as usize);
    for (i, state) in snapshot.vcpu_states.iter().enumerate() {
        let vcpu = Vcpu::new_x86_64(
            i as u8,
            vm.whp_vm().clone(),
            guest_memory.clone(),
            pio_device_manager.io_bus.clone(),
            exit_evt
                .try_clone()
                .map_err(Error::EventFd)
                .map_err(StartMicrovmError::Internal)?,
        )
        .map_err(Error::Vcpu)
        .map_err(StartMicrovmError::Internal)?;
        // No boot configure — the saved WHP state carries everything.
        vcpu.restore_state(state)
            .map_err(Error::Vcpu)
            .map_err(StartMicrovmError::Internal)?;
        if std::env::var_os("NEBULA_SNAP_DEBUG").is_some() {
            eprintln!(
                "snap-debug vp{i} saved-lapic    {}",
                whp::debug_parse_apic(&state.0.lapic)
            );
            eprintln!(
                "snap-debug vp{i} restored-lapic {}",
                whp::debug_sample_apic(vm.whp_vm(), i as u32)
            );
        }
        vcpus.push(vcpu);
    }

    let exit_code = Arc::new(AtomicI32::new(i32::MAX));
    let mut vmm = Vmm {
        guest_memory,
        arch_memory_info,
        kernel_cmdline,
        vcpus_handles: Vec::new(),
        exit_evt,
        exit_observers: Vec::new(),
        exit_code,
        vm,
        mmio_device_manager,
        pio_device_manager,
    };

    for serial_tty in serial_ttys {
        setup_terminal_raw_mode(&mut vmm, Some(serial_tty), false);
    }

    // Device creation mirrors build_microvm's Windows order exactly:
    // console(s), block, vsock, net.
    let mut console_id = 0;
    if !vm_resources.disable_implicit_console {
        attach_console_devices(
            &mut vmm,
            event_manager,
            intc.clone(),
            vm_resources,
            None,
            console_id,
        )?;
        console_id += 1;
    }
    for console_cfg in vm_resources.virtio_consoles.iter() {
        attach_console_devices(
            &mut vmm,
            event_manager,
            intc.clone(),
            vm_resources,
            Some(console_cfg),
            console_id,
        )?;
        console_id += 1;
    }

    #[cfg(feature = "blk")]
    attach_block_devices(&mut vmm, &vm_resources.block, intc.clone())?;

    if let Some(vsock) = vm_resources.vsock.get() {
        attach_unixsock_vsock_device(&mut vmm, vsock, event_manager, intc.clone())?;
    }

    #[cfg(feature = "net")]
    attach_net_devices(&mut vmm, &vm_resources.net, intc.clone())?;

    // Push the saved virtio state into the freshly-created transports.
    let device_states = snapshot.device_states;
    let mut idx = 0usize;
    for (_addr, dev) in vmm.mmio_device_manager.bus.iter_devices() {
        let mut locked = dev.lock().unwrap();
        if let Some(t) = locked.as_mut_any().downcast_mut::<MmioTransport>() {
            let st = device_states.get(idx).ok_or_else(|| {
                StartMicrovmError::RestoreSnapshot(format!(
                    "snapshot has {} virtio devices but the configuration created more",
                    device_states.len()
                ))
            })?;
            let created_type = t.snapshot_state().device_type;
            if st.device_type != created_type {
                return Err(err(format!(
                    "virtio device order mismatch at index {idx}: snapshot type {} \
                     vs created type {created_type}",
                    st.device_type
                )));
            }
            t.restore_from_state(st);
            idx += 1;
        }
    }
    if idx != device_states.len() {
        return Err(err(format!(
            "snapshot has {} virtio devices but the configuration created {idx}",
            device_states.len()
        )));
    }

    vmm.start_vcpus(vcpus)
        .map_err(StartMicrovmError::Internal)?;

    // Debug aid: sample VP0's execution state after restore so a wedged
    // guest (dead timer, stuck RIP) is visible in the worker log.
    if std::env::var_os("NEBULA_SNAP_DEBUG").is_some() {
        let whp_vm = vmm.vm.whp_vm().clone();
        std::thread::spawn(move || {
            for i in 0..8 {
                std::thread::sleep(std::time::Duration::from_secs(2));
                for vp in 0..2u32 {
                    eprintln!(
                        "snap-debug t={}s vp{vp} {}",
                        (i + 1) * 2,
                        whp::debug_sample_vp(&whp_vm, vp)
                    );
                }
                if i == 2 {
                    // Test: VMM-injected CALL_FUNCTION_SINGLE to vp1. If this
                    // unwedges the guest, guest ICR-initiated IPIs are what's
                    // broken post-restore.
                    eprintln!("snap-debug injecting test IPI 0xfb -> apic id 1");
                    whp_vm.request_fixed_interrupt(0xfb, 1);
                }
            }
        });
    }

    #[allow(clippy::arc_with_non_send_sync)]
    let vmm = Arc::new(Mutex::new(vmm));
    event_manager
        .add_subscriber(vmm.clone())
        .map_err(StartMicrovmError::RegisterEvent)?;

    Ok(vmm)
}
