// Copyright 2026 Red Hat, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Windows Hypervisor Platform (WHP) virtual-machine and vCPU management.
//!
//! This module follows the same structure as `linux/vstate.rs` and
//! `macos/vstate.rs`: it consumes `whp::{WhpVm, WhpVcpu, VcpuExitReason}`
//! and routes I/O and MMIO exits through the WHP instruction emulator.

use log::{debug, error, warn};
use std::ffi::c_void;
use std::fmt::{Display, Formatter};
use std::io;
use std::mem;
use std::result;
use std::sync::Arc;
use std::thread;

use super::super::{FC_EXIT_CODE_GENERIC_ERROR, FC_EXIT_CODE_OK};

use crossbeam_channel::{unbounded, Receiver, Sender, TryRecvError};
use utils::eventfd::EventFd;
use vm_memory::{
    Address, Bytes, GuestAddress, GuestMemory, GuestMemoryError, GuestMemoryMmap,
    GuestMemoryRegion,
};
use whp::{self, CpuidExitInfo, MsrExitInfo, VcpuExitReason, WhpEmulator, WhpVcpu, WhpVm};
use windows_sys::Win32::Foundation::S_OK;
use windows_sys::Win32::System::Hypervisor::{
    WHV_EMULATOR_CALLBACKS, WHV_EMULATOR_IO_ACCESS_INFO, WHV_EMULATOR_MEMORY_ACCESS_INFO,
    WHV_PARTITION_HANDLE, WHV_REGISTER_NAME, WHV_REGISTER_VALUE, WHV_TRANSLATE_GVA_FLAGS,
    WHV_TRANSLATE_GVA_RESULT, WHV_TRANSLATE_GVA_RESULT_CODE, WHvGetVirtualProcessorRegisters,
    WHvSetVirtualProcessorRegisters, WHvTranslateGva, WHvX64RegisterRflags,
};

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum Error {
    /// Cannot create the instruction emulator.
    CreateEmulator(whp::Error),
    /// Instruction emulation failed.
    Emulation(whp::Error),
    /// Invalid guest memory configuration.
    GuestMemoryMmap(GuestMemoryError),
    /// The number of configured slots is bigger than the maximum reported by WHP.
    NotEnoughMemorySlots,
    /// Cannot set the memory regions.
    SetUserMemoryRegion(whp::Error),
    /// Failed to signal Vcpu.
    SignalVcpu(io::Error),
    /// vCPU count is not initialized.
    VcpuCountNotInitialized,
    /// Cannot run the VCPUs.
    VcpuRun(whp::Error),
    /// Cannot spawn a new vCPU thread.
    VcpuSpawn(io::Error),
    /// Unexpected VM exit reason.
    VcpuUnhandledExit,
    /// Cannot configure the microvm.
    VmSetup(whp::Error),
    /// WHP hypervisor not available.
    WhpNotAvailable(whp::Error),
    /// Error configuring the general purpose registers.
    REGSConfiguration(arch::x86_64::regs::Error),
    /// Error configuring the special registers.
    SREGSConfiguration(arch::x86_64::regs::Error),
    /// Error configuring the MSR registers.
    MSRSConfiguration(arch::x86_64::msr::Error),
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        use Error::*;
        match self {
            CreateEmulator(e) => write!(f, "Cannot create instruction emulator: {e}"),
            Emulation(e) => write!(f, "Instruction emulation failed: {e}"),
            GuestMemoryMmap(e) => write!(f, "Guest memory error: {e:?}"),
            NotEnoughMemorySlots => write!(f, "Not enough memory slots"),
            SetUserMemoryRegion(e) => write!(f, "Cannot set memory regions: {e}"),
            SignalVcpu(e) => write!(f, "Failed to signal vCPU: {e}"),
            VcpuCountNotInitialized => write!(f, "vCPU count is not initialized"),
            VcpuRun(e) => write!(f, "Cannot run vCPU: {e}"),
            VcpuSpawn(e) => write!(f, "Cannot spawn vCPU thread: {e}"),
            VcpuUnhandledExit => write!(f, "Unexpected VM exit reason"),
            VmSetup(e) => write!(f, "Cannot configure the VM: {e}"),
            WhpNotAvailable(e) => write!(f, "WHP hypervisor not available: {e}"),
            REGSConfiguration(e) => write!(f, "Error configuring registers: {e:?}"),
            SREGSConfiguration(e) => write!(f, "Error configuring special registers: {e:?}"),
            MSRSConfiguration(e) => write!(f, "Error configuring MSRs: {e:?}"),
        }
    }
}

pub type Result<T> = result::Result<T, Error>;

pub struct Vm {
    whp_vm: Arc<WhpVm>,
}

impl Vm {
    pub fn new(vcpu_count: u8) -> Result<Self> {
        whp::check_hypervisor().map_err(Error::WhpNotAvailable)?;
        let whp_vm = Arc::new(WhpVm::new(vcpu_count as u32).map_err(Error::VmSetup)?);
        Ok(Vm { whp_vm })
    }

    pub fn memory_init(
        &mut self,
        guest_mem: &GuestMemoryMmap,
    ) -> Result<()> {
        for region in guest_mem.iter() {
            // It's safe to unwrap because the guest address is valid.
            let host_addr = guest_mem.get_host_address(region.start_addr()).unwrap();
            debug!(
                "Guest memory host_addr={:x?} guest_addr={:x?} len={:x?}",
                host_addr,
                region.start_addr().raw_value(),
                region.len()
            );
            let map_result = unsafe {
                self.whp_vm.map_memory(
                    host_addr as *mut c_void,
                    region.start_addr().raw_value(),
                    region.len(),
                )
            };
            map_result.map_err(Error::SetUserMemoryRegion)?;
        }
        Ok(())
    }

    pub fn whp_vm(&self) -> &Arc<WhpVm> {
        &self.whp_vm
    }
}

#[derive(Debug, Eq, PartialEq)]
pub struct VcpuConfig {
    pub vcpu_count: u8,
}

/// Passed as the opaque `*const c_void` context to every emulator callback.
///
/// For I/O emulation `bus` points to the I/O port bus; for MMIO emulation it
/// points to the memory-mapped bus.  `guest_mem` is always set so the memory
/// callback can fetch instruction bytes from guest RAM when WHP doesn't supply
/// them in the exit context.
#[repr(C)]
struct CallbackContext {
    partition_handle: WHV_PARTITION_HANDLE,
    vp_index: u32,
    bus: *const devices::Bus,
    guest_mem: *const GuestMemoryMmap,
}

unsafe extern "system" fn io_port_callback(
    context: *const c_void,
    io_access: *mut WHV_EMULATOR_IO_ACCESS_INFO,
) -> i32 {
    let (ctx, io, bus) = unsafe {
        let ctx = &*(context as *const CallbackContext);
        let io = &mut *io_access;
        let bus = &*ctx.bus;
        (ctx, io, bus)
    };
    if io.Direction != 0 {
        let data_bytes = io.Data.to_le_bytes();
        let access_size = (io.AccessSize as usize).min(data_bytes.len());
        bus.write(ctx.vp_index as u64, io.Port as u64, &data_bytes[..access_size]);
    } else {
        let mut data_bytes = [0u8; 4];
        let access_size = (io.AccessSize as usize).min(data_bytes.len());
        bus.read(ctx.vp_index as u64, io.Port as u64, &mut data_bytes[..access_size]);
        io.Data = u32::from_le_bytes(data_bytes);
    }
    S_OK
}

unsafe extern "system" fn memory_callback(
    context: *const c_void,
    mem_access: *mut WHV_EMULATOR_MEMORY_ACCESS_INFO,
) -> i32 {
    let (ctx, ma) = unsafe {
        let ctx = &*(context as *const CallbackContext);
        let ma = &mut *mem_access;
        (ctx, ma)
    };
    let size = (ma.AccessSize as usize).min(ma.Data.len());

    // Try guest RAM first. If the GPA is in a RAM region succeeds immediately. 
    // Otherwise it's an actual MMIO access and we fall through to the MMIO bus.
    if !ctx.guest_mem.is_null() {
        let gpa = GuestAddress(ma.GpaAddress);
        let mem = unsafe { &*ctx.guest_mem };        
        if ma.Direction != 0 {            
            if mem.write_slice(&ma.Data[..size], gpa).is_ok() {
                return S_OK;
            }
        } else if mem.read_slice(&mut ma.Data[..size], gpa).is_ok() {
            return S_OK;
        }
    }

    // GPA is not in a RAM region -- dispatch to the MMIO bus.
    if ctx.bus.is_null() {
        return S_OK;
    }
    let bus = unsafe { &*ctx.bus };

    if ma.Direction != 0 {
        bus.write(ctx.vp_index as u64, ma.GpaAddress, &ma.Data[..size]);
    } else {
        bus.read(ctx.vp_index as u64, ma.GpaAddress, &mut ma.Data[..size]);
    }
    S_OK
}

unsafe extern "system" fn get_vp_registers_callback(
    context: *const c_void,
    register_names: *const WHV_REGISTER_NAME,
    register_count: u32,
    register_values: *mut WHV_REGISTER_VALUE,
) -> i32 {
    let ctx = unsafe { &*(context as *const CallbackContext) };
    WHvGetVirtualProcessorRegisters(
        ctx.partition_handle,
        ctx.vp_index,
        register_names,
        register_count,
        register_values,
    )
}

unsafe extern "system" fn set_vp_registers_callback(
    context: *const c_void,
    register_names: *const WHV_REGISTER_NAME,
    register_count: u32,
    register_values: *const WHV_REGISTER_VALUE,
) -> i32 {
    let ctx = unsafe { &*(context as *const CallbackContext) };
    WHvSetVirtualProcessorRegisters(
        ctx.partition_handle,
        ctx.vp_index,
        register_names,
        register_count,
        register_values,
    )
}

unsafe extern "system" fn translate_gva_callback(
    context: *const c_void,
    gva: u64,
    translate_flags: WHV_TRANSLATE_GVA_FLAGS,
    translation_result: *mut WHV_TRANSLATE_GVA_RESULT_CODE,
    gpa: *mut u64,
) -> i32 {
    let ctx = unsafe { &*(context as *const CallbackContext) };
    let mut result = WHV_TRANSLATE_GVA_RESULT::default();
    let hr = WHvTranslateGva(
        ctx.partition_handle,
        ctx.vp_index,
        gva,
        translate_flags,
        &mut result,
        gpa,
    );
    if hr == S_OK {
        unsafe { *translation_result = result.ResultCode };
    }
    hr
}

fn build_emulator_callbacks() -> WHV_EMULATOR_CALLBACKS {
    WHV_EMULATOR_CALLBACKS {
        Size: mem::size_of::<WHV_EMULATOR_CALLBACKS>() as u32,
        Reserved: 0,
        WHvEmulatorIoPortCallback: Some(io_port_callback),
        WHvEmulatorMemoryCallback: Some(memory_callback),
        WHvEmulatorGetVirtualProcessorRegisters: Some(get_vp_registers_callback),
        WHvEmulatorSetVirtualProcessorRegisters: Some(set_vp_registers_callback),
        WHvEmulatorTranslateGvaPage: Some(translate_gva_callback),
    }
}

pub struct Vcpu {
    whp_vcpu: WhpVcpu,
    emulator: WhpEmulator,
    guest_mem: GuestMemoryMmap,
    io_bus: devices::Bus,
    mmio_bus: Option<devices::Bus>,
    exit_evt: EventFd,

    event_receiver: Receiver<VcpuEvent>,
    event_sender: Option<Sender<VcpuEvent>>,
    response_receiver: Option<Receiver<VcpuResponse>>,
    response_sender: Sender<VcpuResponse>,
}

impl Vcpu {
    /// No-op on Windows -- cancellation uses `WHvCancelRunVirtualProcessor`.
    pub fn register_kick_signal_handler() {}

    pub fn new_x86_64(
        id: u8,
        vm: Arc<WhpVm>,
        guest_mem: GuestMemoryMmap,
        io_bus: devices::Bus,
        exit_evt: EventFd,
    ) -> Result<Self> {
        let whp_vcpu = WhpVcpu::new(vm, id as u32).map_err(Error::VcpuRun)?;
        debug!("vcpu {id}: creating instruction emulator");
        let emulator =
            WhpEmulator::new(build_emulator_callbacks()).map_err(Error::CreateEmulator)?;
        debug!("vcpu {id}: emulator created");
        
        let (event_sender, event_receiver) = unbounded();
        let (response_sender, response_receiver) = unbounded();

        Ok(Vcpu {
            whp_vcpu,
            emulator,
            guest_mem,
            io_bus,
            mmio_bus: None,
            exit_evt,
            event_receiver,
            event_sender: Some(event_sender),
            response_receiver: Some(response_receiver),
            response_sender,
        })
    }

    pub fn configure_x86_64(
        &mut self,
        guest_mem: &GuestMemoryMmap,
        kernel_start_addr: GuestAddress,
        kernel_boot: bool,
    ) -> Result<()> {
        if kernel_boot {
            debug!("vcpu configure: setup_regs entry=0x{:x}", kernel_start_addr.raw_value());
            arch::x86_64::regs::setup_regs(&self.whp_vcpu, kernel_start_addr.raw_value())
                .map_err(Error::REGSConfiguration)?;
            debug!("vcpu configure: setup_sregs");
            arch::x86_64::regs::setup_sregs(guest_mem, &self.whp_vcpu)
                .map_err(Error::SREGSConfiguration)?;
            if self.cpu_index() == 0 {
                debug!("vcpu configure: setup_msrs");
                arch::x86_64::msr::setup_msrs(&self.whp_vcpu)
                    .map_err(Error::MSRSConfiguration)?;
            }
            debug!("vcpu configure: done");
        }

        Ok(())
    }

    pub fn cpu_index(&self) -> u8 {
        self.whp_vcpu.index() as u8
    }

    pub fn set_mmio_bus(&mut self, mmio_bus: devices::Bus) {
        self.mmio_bus = Some(mmio_bus);
    }

    /// Moves the vcpu to its own thread and constructs a VcpuHandle.
    /// The handle can be used to control the remote vCPU.
    pub fn start_threaded(mut self) -> Result<VcpuHandle> {
        let event_sender = self.event_sender.take().unwrap();
        let response_receiver = self.response_receiver.take().unwrap();

        let vm = self.whp_vcpu.vm().clone();
        let vp_index = self.whp_vcpu.index();

        let _vcpu_thread = thread::Builder::new()
            .name(format!("fc_vcpu {}", self.cpu_index()))
            .spawn(move || {
                self.run();
            })
            .map_err(Error::VcpuSpawn)?;

        Ok(VcpuHandle {
            event_sender,
            response_receiver,
            vm,
            vp_index,
        })
    }

    fn run(&mut self) {
        self.wait_for_resume();

        loop {
            match self.event_receiver.try_recv() {
                Ok(VcpuEvent::Resume) => {
                    self.response_sender
                        .send(VcpuResponse::Resumed)
                        .expect("failed to send Resumed");
                }
                Ok(VcpuEvent::Pause) => {
                    self.response_sender
                        .send(VcpuResponse::Paused)
                        .expect("failed to send Paused");
                    self.wait_for_resume();
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => break,
            }

            match self.run_emulation() {
                Ok(VcpuEmulation::Handled) => {}
                Ok(VcpuEmulation::Stopped) => {
                    self.exit(FC_EXIT_CODE_OK);
                    break;
                }
                Ok(VcpuEmulation::Interrupted) => {}
                Err(_) => {
                    self.exit(FC_EXIT_CODE_GENERIC_ERROR);
                    break;
                }
            }
        }
    }

    fn wait_for_resume(&mut self) {
        loop {
            match self.event_receiver.recv() {
                Ok(VcpuEvent::Resume) => {
                    self.response_sender
                        .send(VcpuResponse::Resumed)
                        .expect("failed to send Resumed");
                    return;
                }
                Ok(VcpuEvent::Pause) => continue,
                Err(_) => return,
            }
        }
    }

    fn callback_context(&self, bus: *const devices::Bus) -> CallbackContext {
        CallbackContext {
            partition_handle: self.whp_vcpu.partition_handle(),
            vp_index: self.whp_vcpu.index(),
            bus,
            guest_mem: &self.guest_mem as *const GuestMemoryMmap,
        }
    }

    fn run_emulation(&mut self) -> Result<VcpuEmulation> {
        let reason = self.whp_vcpu.run().map_err(Error::VcpuRun)?;
        let result = match reason {
            VcpuExitReason::IoPortAccess => {
                let ctx = self.callback_context(&self.io_bus as *const devices::Bus);
                unsafe {
                    self.emulator
                        .try_io_emulation(
                            &ctx as *const _ as *const c_void,
                            self.whp_vcpu.vp_exit_context(),
                            self.whp_vcpu.io_port_access_context(),
                        )
                        .map_err(Error::Emulation)?;
                }
                Ok(VcpuEmulation::Handled)
            }
            VcpuExitReason::MemoryAccess => {
                let bus = self
                    .mmio_bus
                    .as_ref()
                    .map_or(std::ptr::null(), |b| b as *const devices::Bus);
                let ctx = self.callback_context(bus);
                unsafe {
                    self.emulator
                        .try_mmio_emulation(
                            &ctx as *const _ as *const c_void,
                            self.whp_vcpu.vp_exit_context(),
                            self.whp_vcpu.memory_access_context(),
                        )
                        .map_err(Error::Emulation)?;
                }
                Ok(VcpuEmulation::Handled)
            }
            VcpuExitReason::MsrAccess => {
                let info = self.whp_vcpu.msr_exit_info();
                self.handle_msr_access(&info)?;
                Ok(VcpuEmulation::Handled)
            }
            VcpuExitReason::Halt => {
                // A Linux guest hits HLT when idle. 
                // WHP handles most HLT states internally (if interrupts are enabled), 
                // but if we land here, the vCPU is truly stalled.
                
                // Check if we should actually be shutting down.
                if self.exit_evt.is_signaled() {
                    return Ok(VcpuEmulation::Stopped);
                }

                // Yield the timeslice so the synthetic-timer and PIT
                // worker threads can run.  sleep(1ms) on Windows rounds
                // up to one scheduler tick (~1-15 ms) which is acceptable
                // for an idle vCPU and avoids burning a full core.
                thread::sleep(std::time::Duration::from_millis(1));
                Ok(VcpuEmulation::Handled)
            }
            VcpuExitReason::InterruptWindow => {
                let _ = self.whp_vcpu.clear_interrupt_window();
                Ok(VcpuEmulation::Handled)
            }
            VcpuExitReason::Canceled => {
                // If the vCPU was canceled while waiting for an interrupt window, 
                // and the guest has now unmasked interrupts (IF=1), clear the window 
                // request to prevent a spurious InterruptWindow exit storm on re-entry.
                let rflags = self.whp_vcpu.get_registers64([WHvX64RegisterRflags]).unwrap_or([0]);
                if rflags[0] & 0x200 != 0 {
                    let _ = self.whp_vcpu.clear_interrupt_window();
                }
                Ok(VcpuEmulation::Handled)
            }
            _ => {
                error!("vCPU {} unhandled or unexpected exit reason: {:?}", self.cpu_index(), reason);
                Err(Error::VcpuUnhandledExit)
            }
        };

        // If a shutdown device (i8042, ACPI PM, …) signaled exit_evt --
        // possibly from this vCPU or from another -- stop immediately.
        if self.exit_evt.is_signaled() {
            return Ok(VcpuEmulation::Stopped);
        }

        result
    }

    /// Handle an MSR read/write exit for Hyper-V synthetic MSRs.
    fn handle_msr_access(&self, info: &MsrExitInfo) -> Result<()> {
        let mut rax = 0u64;
        let mut rdx = 0u64;

        if info.is_write {
            self.whp_vcpu.advance_rip().map_err(Error::Emulation)
        } else {
            self.whp_vcpu
                .complete_msr_read(rax, rdx)
                .map_err(Error::Emulation)
        }
    }

    fn exit(&mut self, exit_code: u8) {
        self.response_sender
            .send(VcpuResponse::Exited(exit_code))
            .expect("failed to send Exited status");

        if let Err(e) = self.exit_evt.write(1) {
            error!("Failed signaling vcpu exit event: {e}");
        }
    }
}

// Allow currently unused Pause and Exit events. These will be used by the vmm later on.
#[allow(unused)]
#[derive(Debug)]
/// List of events that the Vcpu can receive.
pub enum VcpuEvent {
    /// Pause the Vcpu.
    Pause,
    /// Event that should resume the Vcpu.
    Resume,
}

#[derive(Debug, Eq, PartialEq)]
/// List of responses that the Vcpu reports.
pub enum VcpuResponse {
    /// Vcpu is paused.
    Paused,
    /// Vcpu is resumed.
    Resumed,
    /// Vcpu is stopped.
    Exited(u8),
}

/// Wrapper over Vcpu that hides the underlying interactions with the Vcpu thread.
pub struct VcpuHandle {
    /// The transmitting end of the events channel which will be given to the handler.
    event_sender: Sender<VcpuEvent>,
    /// The receiving end of the responses channel which will be given to the handler.
    response_receiver: Receiver<VcpuResponse>,
    /// The VM that the vCPU belongs to.
    vm: Arc<WhpVm>,
    /// The index of the vCPU.
    vp_index: u32,
}

impl VcpuHandle {
    pub fn send_event(&self, event: VcpuEvent) -> Result<()> {
        self.event_sender
            .send(event)
            .expect("event sender channel closed on vcpu end.");

        // Interrupt WHvRunVirtualProcessor so the vCPU picks up the event.
        unsafe {
            let hr = windows_sys::Win32::System::Hypervisor::WHvCancelRunVirtualProcessor(
                self.vm.partition_handle(),
                self.vp_index,
                0,
            );
            if hr != windows_sys::Win32::Foundation::S_OK {
                error!("WHvCancelRunVirtualProcessor failed: HRESULT 0x{hr:08x}");
            }
        }

        Ok(())
    }

    pub fn response_receiver(&self) -> &Receiver<VcpuResponse> {
        &self.response_receiver
    }
}

enum VcpuEmulation {
    Handled,
    Interrupted,
    Stopped,
}

#[cfg(test)]
mod tests {
    use std::mem;
    use std::sync::Arc;
    use std::time::Duration;

    use crossbeam_channel::RecvTimeoutError;
    use vm_memory::{GuestAddress, GuestMemoryMmap};

    use super::*;

    fn setup_vm() -> (Vm, GuestMemoryMmap) {
        let mut vm = Vm::new(1).expect("Cannot create new VM");
        let gm = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x20_0000)]).unwrap();
        vm.memory_init(&gm).expect("memory_init failed");
        (vm, gm)
    }

    fn setup_vcpu() -> (Vm, Vcpu, GuestMemoryMmap) {
        let (vm, gm) = setup_vm();
        let exit_evt = EventFd::new(utils::eventfd::EFD_NONBLOCK).unwrap();
        let vcpu = Vcpu::new_x86_64(
            0,
            vm.whp_vm().clone(),
            gm.clone(),
            devices::Bus::new(),
            exit_evt,
        )
        .unwrap();
        (vm, vcpu, gm)
    }

    fn queue_event_expect_response(
        handle: &VcpuHandle,
        event: VcpuEvent,
        response: VcpuResponse,
    ) {
        handle
            .send_event(event)
            .expect("failed to send event to vcpu");
        assert_eq!(
            handle
                .response_receiver()
                .recv_timeout(Duration::from_millis(1000))
                .expect("did not receive event response from vcpu"),
            response
        );
    }

    fn queue_event_expect_timeout(handle: &VcpuHandle, event: VcpuEvent) {
        handle
            .send_event(event)
            .expect("failed to send event to vcpu");
        assert_eq!(
            handle
                .response_receiver()
                .recv_timeout(Duration::from_millis(100)),
            Err(RecvTimeoutError::Timeout)
        );
    }

    #[test]
    fn test_error_display() {
        use std::io;
        let e = Error::NotEnoughMemorySlots;
        assert_eq!(format!("{e}"), "Not enough memory slots");

        let e = Error::VcpuUnhandledExit;
        assert_eq!(format!("{e}"), "Unexpected VM exit reason");

        let e = Error::VcpuCountNotInitialized;
        assert_eq!(format!("{e}"), "vCPU count is not initialized");

        let e = Error::SignalVcpu(io::Error::new(io::ErrorKind::Other, "test"));
        assert!(format!("{e}").starts_with("Failed to signal vCPU"));
    }

    #[test]
    fn test_vcpu_config_partial_eq() {
        let cfg_a = VcpuConfig { vcpu_count: 2 };
        let cfg_b = VcpuConfig { vcpu_count: 2 };
        let cfg_c = VcpuConfig { vcpu_count: 4 };
        assert_eq!(cfg_a, cfg_b);
        assert_ne!(cfg_a, cfg_c);
    }

    #[test]
    fn test_vcpu_response_partial_eq() {
        assert_eq!(VcpuResponse::Paused, VcpuResponse::Paused);
        assert_eq!(VcpuResponse::Resumed, VcpuResponse::Resumed);
        assert_eq!(VcpuResponse::Exited(0), VcpuResponse::Exited(0));
        assert_ne!(VcpuResponse::Exited(0), VcpuResponse::Exited(1));
        assert_ne!(VcpuResponse::Paused, VcpuResponse::Resumed);
    }

    #[test]
    fn test_build_emulator_callbacks() {
        let cbs = build_emulator_callbacks();
        assert_eq!(
            cbs.Size,
            mem::size_of::<WHV_EMULATOR_CALLBACKS>() as u32,
        );
        assert!(cbs.WHvEmulatorIoPortCallback.is_some());
        assert!(cbs.WHvEmulatorMemoryCallback.is_some());
        assert!(cbs.WHvEmulatorGetVirtualProcessorRegisters.is_some());
        assert!(cbs.WHvEmulatorSetVirtualProcessorRegisters.is_some());
        assert!(cbs.WHvEmulatorTranslateGvaPage.is_some());
    }

    #[test]
    fn test_vm_new() {
        let vm = Vm::new(1);
        assert!(vm.is_ok());
    }

    #[test]
    fn test_vm_memory_init() {
        let mut vm = Vm::new(1).expect("Cannot create new VM");
        let gm = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x20_0000)]).unwrap();
        assert!(vm.memory_init(&gm).is_ok());
    }

    #[test]
    fn test_vcpu_creation() {
        let (_vm, vcpu, _gm) = setup_vcpu();
        assert_eq!(vcpu.cpu_index(), 0);
    }

    #[test]
    fn test_set_mmio_bus() {
        let (_vm, mut vcpu, _gm) = setup_vcpu();
        assert!(vcpu.mmio_bus.is_none());
        vcpu.set_mmio_bus(devices::Bus::new());
        assert!(vcpu.mmio_bus.is_some());
    }

    #[test]
    fn test_register_kick_signal_handler() {
        // No-op on Windows, just verify it doesn't panic.
        Vcpu::register_kick_signal_handler();
    }

    #[test]
    fn test_vcpu_start_threaded() {
        let (_vm, vcpu, _gm) = setup_vcpu();
        let handle = vcpu.start_threaded().expect("failed to start vcpu thread");

        // The vCPU thread starts in wait_for_resume, so sending Resume should
        // get a Resumed response.
        queue_event_expect_response(&handle, VcpuEvent::Resume, VcpuResponse::Resumed);

        // A Pause while running should yield Paused.
        queue_event_expect_response(&handle, VcpuEvent::Pause, VcpuResponse::Paused);

        // Resume after pause.
        queue_event_expect_response(&handle, VcpuEvent::Resume, VcpuResponse::Resumed);
    }

    #[test]
    fn test_vcpu_pause_before_resume_ignored() {
        let (_vm, vcpu, _gm) = setup_vcpu();
        let handle = vcpu.start_threaded().expect("failed to start vcpu thread");

        // Sending Pause before Resume should not produce a response because
        // the vCPU is already waiting for Resume inside wait_for_resume.
        queue_event_expect_timeout(&handle, VcpuEvent::Pause);

        // Now Resume should work.
        queue_event_expect_response(&handle, VcpuEvent::Resume, VcpuResponse::Resumed);
    }
}