// Copyright 2026 Red Hat, Inc.
// SPDX-License-Identifier: Apache-2.0

#![cfg(target_os = "windows")]

use std::ffi::c_void;
use std::fmt::{Display, Formatter};
use std::mem::{self, MaybeUninit};
use std::sync::Arc;

use log::{debug, error};
use windows_sys::Win32::Foundation::S_OK;
use windows_sys::Win32::System::Hypervisor::{
    WHV_CAPABILITY, WHV_EMULATOR_CALLBACKS, WHV_EMULATOR_STATUS, WHV_MEMORY_ACCESS_CONTEXT,
    WHV_PARTITION_HANDLE, WHV_PARTITION_PROPERTY, WHV_PARTITION_PROPERTY_CODE,
    WHV_PROCESSOR_FEATURES_BANKS, WHV_REGISTER_NAME, WHV_REGISTER_VALUE, WHV_RUN_VP_EXIT_CONTEXT,
    WHV_VP_EXIT_CONTEXT, WHV_X64_CPUID_RESULT, WHV_X64_IO_PORT_ACCESS_CONTEXT,
    WHvCancelRunVirtualProcessor, WHvCapabilityCodeHypervisorPresent,
    WHvCapabilityCodeProcessorFeaturesBanks, WHvCreatePartition, WHvCreateVirtualProcessor,
    WHvDeletePartition, WHvDeleteVirtualProcessor, WHvEmulatorCreateEmulator,
    WHvEmulatorDestroyEmulator, WHvEmulatorTryIoEmulation, WHvEmulatorTryMmioEmulation,
    WHvGetCapability, WHvGetVirtualProcessorRegisters, WHvMapGpaRange, WHvMapGpaRangeFlagExecute,
    WHvMapGpaRangeFlagRead, WHvMapGpaRangeFlagWrite, WHvPartitionPropertyCodeCpuidResultList,
    WHvPartitionPropertyCodeExtendedVmExits, WHvPartitionPropertyCodeLocalApicEmulationMode,
    WHvPartitionPropertyCodeProcessorCount, WHvPartitionPropertyCodeProcessorFeaturesBanks,
    WHvPartitionPropertyCodeSyntheticProcessorFeaturesBanks,
    WHvPartitionPropertyCodeX64MsrExitBitmap, WHvRequestInterrupt, WHvRunVirtualProcessor,
    WHvRunVpExitReasonCanceled, WHvRunVpExitReasonInvalidVpRegisterValue,
    WHvRunVpExitReasonMemoryAccess, WHvRunVpExitReasonUnrecoverableException,
    WHvRunVpExitReasonUnsupportedFeature, WHvRunVpExitReasonX64Cpuid, WHvRunVpExitReasonX64Halt,
    WHvRunVpExitReasonX64InterruptWindow, WHvRunVpExitReasonX64IoPortAccess,
    WHvGetPartitionProperty, WHvRunVpExitReasonX64MsrAccess, WHvSetPartitionProperty,
    WHvSetVirtualProcessorRegisters,
    WHvSetupPartition, WHvX64LocalApicEmulationModeXApic,
    WHvX64RegisterDeliverabilityNotifications, WHvX64RegisterRax, WHvX64RegisterRbx,
    WHvX64RegisterRcx, WHvX64RegisterRdx, WHvX64RegisterRip,
};
use windows_sys::Win32::System::Hypervisor::{
    WHvGetVirtualProcessorInterruptControllerState2, WHvGetVirtualProcessorXsaveState,
    WHvRegisterInterruptState, WHvRegisterPendingInterruption,
    WHvSetVirtualProcessorInterruptControllerState2, WHvSetVirtualProcessorXsaveState,
    WHvX64RegisterApicBase, WHvX64RegisterCr0, WHvX64RegisterCr2, WHvX64RegisterCr3,
    WHvX64RegisterCr4, WHvX64RegisterCr8, WHvX64RegisterCs, WHvX64RegisterCstar,
    WHvX64RegisterDr0, WHvX64RegisterDr1, WHvX64RegisterDr2, WHvX64RegisterDr3,
    WHvX64RegisterDr6, WHvX64RegisterDr7, WHvX64RegisterDs, WHvX64RegisterEfer,
    WHvX64RegisterEs, WHvX64RegisterFs, WHvX64RegisterGdtr, WHvX64RegisterGs,
    WHvX64RegisterIdtr, WHvX64RegisterKernelGsBase, WHvX64RegisterLdtr, WHvX64RegisterLstar,
    WHvX64RegisterPat, WHvX64RegisterR8, WHvX64RegisterR9, WHvX64RegisterR10,
    WHvX64RegisterR11, WHvX64RegisterR12, WHvX64RegisterR13, WHvX64RegisterR14,
    WHvX64RegisterR15, WHvX64RegisterRbp, WHvX64RegisterRdi, WHvX64RegisterRflags,
    WHvX64RegisterRsi, WHvX64RegisterRsp, WHvX64RegisterSfmask, WHvX64RegisterSs,
    WHvX64RegisterStar, WHvX64RegisterSysenterCs, WHvX64RegisterSysenterEip,
    WHvX64RegisterSpecCtrl, WHvX64RegisterSysenterEsp, WHvX64RegisterTr, WHvX64RegisterTsc,
    WHvX64RegisterTscAdjust, WHvX64RegisterTscAux, WHvX64RegisterTscDeadline, WHvX64RegisterXCr0,
    WHvX64RegisterXfd, WHvX64RegisterXss,
};
use windows_sys::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

/// Host wall-progressing reference time in 100ns units (the Hyper-V TLFS
/// partition-reference-time format), backed by QPC. WHP's own partition
/// reference time only advances while a VP is inside
/// `WHvRunVirtualProcessor` — with exit-on-HLT idle handling the guest
/// clock froze while idle — so the VMM serves HV_X64_MSR_TIME_REF_COUNT
/// from this instead.
pub fn host_reference_time_100ns() -> u64 {
    let mut qpc: i64 = 0;
    let mut freq: i64 = 0;
    unsafe {
        let _ = QueryPerformanceCounter(&mut qpc);
        let _ = QueryPerformanceFrequency(&mut freq);
    }
    if freq <= 0 {
        return 0;
    }
    // Split to avoid overflow: seconds * 1e7 + remainder scaled.
    let q = qpc as u128;
    let f = freq as u128;
    ((q / f) * 10_000_000 + (q % f) * 10_000_000 / f) as u64
}

/// `WHV_REGISTER_VALUE` is `DECLSPEC_ALIGN(16)` in the SDK, but the
/// windows-sys binding is only 8-aligned. WinHvPlatform reads the array with
/// aligned vector loads, so an 8-mod-16 stack array access-violates.
/// All locally-built register arrays must use this wrapper.
#[repr(C, align(16))]
#[derive(Clone, Copy)]
struct AlignedRegisterValue(WHV_REGISTER_VALUE);

impl AlignedRegisterValue {
    fn zeroed() -> Self {
        unsafe { mem::zeroed() }
    }
}

#[derive(Debug)]
pub enum Error {
    CheckCapability(i32),
    HypervisorNotPresent,
    CreatePartition(i32),
    SetPartitionProperty(WHV_PARTITION_PROPERTY_CODE, i32),
    SetupPartition(i32),
    DeletePartition(i32),
    MapGpaRange(i32),
    RequestInterrupt(i32),
    CreateVirtualProcessor(i32),
    DeleteVirtualProcessor(i32),
    RunVirtualProcessor(i32),
    GetRegisters(i32),
    SetRegisters(i32),
    MemoryAlignment,
    CreateEmulator(i32),
    DestroyEmulator(i32),
    IoEmulation(i32),
    MmioEmulation(i32),
    EmulationFailed(u32),
    GetXsave(i32),
    SetXsave(i32),
    GetInterruptController(i32),
    SetInterruptController(i32),
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        use Error::*;
        match self {
            CheckCapability(hr) => write!(f, "WHvGetCapability failed: HRESULT 0x{hr:08x}"),
            HypervisorNotPresent => write!(f, "WHP hypervisor is not present on this system"),
            CreatePartition(hr) => write!(f, "WHvCreatePartition failed: HRESULT 0x{hr:08x}"),
            SetPartitionProperty(code, hr) => {
                // Naming the property is the whole value of this error. Every
                // call here fails the same way, and "SetPartitionProperty
                // failed" sent two people hunting the wrong one.
                write!(
                    f,
                    "WHvSetPartitionProperty({code}) failed: HRESULT 0x{hr:08x}"
                )
            }
            SetupPartition(hr) => write!(f, "WHvSetupPartition failed: HRESULT 0x{hr:08x}"),
            DeletePartition(hr) => write!(f, "WHvDeletePartition failed: HRESULT 0x{hr:08x}"),
            MapGpaRange(hr) => write!(f, "WHvMapGpaRange failed: HRESULT 0x{hr:08x}"),
            RequestInterrupt(hr) => write!(f, "WHvRequestInterrupt failed: HRESULT 0x{hr:08x}"),
            CreateVirtualProcessor(hr) => {
                write!(f, "WHvCreateVirtualProcessor failed: HRESULT 0x{hr:08x}")
            }
            DeleteVirtualProcessor(hr) => {
                write!(f, "WHvDeleteVirtualProcessor failed: HRESULT 0x{hr:08x}")
            }
            RunVirtualProcessor(hr) => {
                write!(f, "WHvRunVirtualProcessor failed: HRESULT 0x{hr:08x}")
            }
            GetRegisters(hr) => {
                write!(
                    f,
                    "WHvGetVirtualProcessorRegisters failed: HRESULT 0x{hr:08x}"
                )
            }
            SetRegisters(hr) => {
                write!(
                    f,
                    "WHvSetVirtualProcessorRegisters failed: HRESULT 0x{hr:08x}"
                )
            }
            MemoryAlignment => write!(f, "WHP memory mapping must be 4KB aligned"),
            CreateEmulator(hr) => {
                write!(f, "WHvEmulatorCreateEmulator failed: HRESULT 0x{hr:08x}")
            }
            DestroyEmulator(hr) => {
                write!(f, "WHvEmulatorDestroyEmulator failed: HRESULT 0x{hr:08x}")
            }
            IoEmulation(hr) => {
                write!(f, "WHvEmulatorTryIoEmulation failed: HRESULT 0x{hr:08x}")
            }
            MmioEmulation(hr) => {
                write!(f, "WHvEmulatorTryMmioEmulation failed: HRESULT 0x{hr:08x}")
            }
            EmulationFailed(status) => {
                let reason = match *status {
                    s if s & (1 << 1) != 0 => "internal emulation failure",
                    s if s & (1 << 2) != 0 => "I/O port callback failed",
                    s if s & (1 << 3) != 0 => "memory callback failed",
                    s if s & (1 << 4) != 0 => "translate GVA page callback failed",
                    s if s & (1 << 5) != 0 => "translated GPA page is not aligned",
                    s if s & (1 << 6) != 0 => "get VP registers callback failed",
                    s if s & (1 << 7) != 0 => "set VP registers callback failed",
                    s if s & (1 << 8) != 0 => "interrupt caused intercept",
                    s if s & (1 << 9) != 0 => "guest cannot be faulted",
                    _ => "unknown",
                };
                write!(
                    f,
                    "Instruction emulation failed: {reason} (status 0x{status:08x})"
                )
            }
            GetXsave(hr) => {
                write!(
                    f,
                    "WHvGetVirtualProcessorXsaveState failed: HRESULT 0x{hr:08x}"
                )
            }
            SetXsave(hr) => {
                write!(
                    f,
                    "WHvSetVirtualProcessorXsaveState failed: HRESULT 0x{hr:08x}"
                )
            }
            GetInterruptController(hr) => {
                write!(
                    f,
                    "WHvGetVirtualProcessorInterruptControllerState2 failed: HRESULT 0x{hr:08x}"
                )
            }
            SetInterruptController(hr) => {
                write!(
                    f,
                    "WHvSetVirtualProcessorInterruptControllerState2 failed: HRESULT 0x{hr:08x}"
                )
            }
        }
    }
}

/// Verifies that the Windows Hypervisor Platform is available.
pub fn check_hypervisor() -> Result<(), Error> {
    let mut capability = MaybeUninit::<WHV_CAPABILITY>::uninit();
    let mut written_size: u32 = 0;

    unsafe {
        let hr = WHvGetCapability(
            WHvCapabilityCodeHypervisorPresent,
            capability.as_mut_ptr().cast(),
            mem::size_of::<WHV_CAPABILITY>() as u32,
            &mut written_size,
        );

        if hr != S_OK {
            return Err(Error::CheckCapability(hr));
        }

        if capability.assume_init().HypervisorPresent == 0 {
            return Err(Error::HypervisorNotPresent);
        }
    }

    debug!("WHP hypervisor is present");
    Ok(())
}

fn get_processor_features_banks() -> Result<WHV_PROCESSOR_FEATURES_BANKS, Error> {
    let mut capability = MaybeUninit::<WHV_CAPABILITY>::uninit();
    let mut written_size: u32 = 0;

    unsafe {
        let hr = WHvGetCapability(
            WHvCapabilityCodeProcessorFeaturesBanks,
            capability.as_mut_ptr().cast(),
            mem::size_of::<WHV_CAPABILITY>() as u32,
            &mut written_size,
        );

        if hr != S_OK {
            return Err(Error::CheckCapability(hr));
        }

        Ok(capability.assume_init().ProcessorFeaturesBanks)
    }
}

// CPUID.15H describes TSC frequency as ECX * EBX / EAX. Keep the synthetic
// crystal in the range used by real x86 CPUs: Linux also uses ECX to calibrate
// the count-mode local APIC timer.
const CPUID_0X15_CRYSTAL_HZ: u32 = 25_000_000;
const CPUID_0X15_DENOMINATOR: u32 = 100;

fn cpuid_0x15_values(tsc_freq_hz: u64) -> Option<(u32, u32, u32)> {
    let numerator = ((tsc_freq_hz as u128 * CPUID_0X15_DENOMINATOR as u128)
        + CPUID_0X15_CRYSTAL_HZ as u128 / 2)
        / CPUID_0X15_CRYSTAL_HZ as u128;
    let numerator = u32::try_from(numerator).ok()?;
    if numerator == 0
        || (CPUID_0X15_CRYSTAL_HZ / 1_000)
            .checked_mul(numerator)
            .is_none()
    {
        return None;
    }
    Some((CPUID_0X15_DENOMINATOR, numerator, CPUID_0X15_CRYSTAL_HZ))
}

/// Parsed CPUID exit context returned by [`WhpVcpu::cpuid_exit_info`].
#[derive(Debug, Clone)]
pub struct CpuidExitInfo {
    pub leaf: u64,
    pub subleaf: u64,
    pub default_eax: u64,
    pub default_ebx: u64,
    pub default_ecx: u64,
    pub default_edx: u64,
}

/// Parsed MSR exit context returned by [`WhpVcpu::msr_exit_info`].
#[derive(Debug, Clone)]
pub struct MsrExitInfo {
    pub msr_number: u32,
    pub is_write: bool,
    pub rax: u64,
    pub rdx: u64,
}

pub struct WhpVm {
    handle: WHV_PARTITION_HANDLE,
}

#[repr(C)]
struct WhvInterruptControl {
    type_and_flags: u64,
    destination: u32,
    vector: u32,
}

#[repr(u8)]
#[derive(Debug, Clone, Copy)]
pub enum InterruptType {
    Fixed = 0,
    LowestPriority = 1,
    Nmi = 4,
    Init = 5,
    Sipi = 6,
}

#[repr(u8)]
#[derive(Debug, Clone, Copy)]
pub enum InterruptDestinationMode {
    Physical = 0,
    Logical = 1,
}

#[repr(u8)]
#[derive(Debug, Clone, Copy)]
pub enum InterruptTriggerMode {
    Edge = 0,
    Level = 1,
}

#[derive(Debug, Clone)]
pub struct InterruptRequest {
    pub interrupt_type: InterruptType,
    pub destination_mode: InterruptDestinationMode,
    pub trigger_mode: InterruptTriggerMode,
    pub destination: u32,
    pub vector: u32,
}

impl WhpVm {
    /// Creates a new WHP partition.
    /// WHP has a create → configure → finalize model
    /// WHvCreatePartition — allocates the partition object but it's not yet usable.
    /// WHvSetPartitionProperty — sets properties like processor count, APIC emulation mode, etc.
    ///                           These properties can only be set before finalization.
    /// WHvSetupPartition — finalizes the partition. After this call, configuration is locked and you can start creating vCPUs.
    ///                     You cannot change the config (like processor count) after this call.
    pub fn new(vcpu_count: u32) -> Result<Self, Error> {
        let handle = unsafe {
            let mut h: WHV_PARTITION_HANDLE = 0;
            let hr = WHvCreatePartition(&mut h);
            if hr != S_OK {
                return Err(Error::CreatePartition(hr));
            }
            h
        };

        let tsc_freq_hz = Self::detect_tsc_frequency();

        if let Err(e) = Self::configure_partition(handle, vcpu_count, tsc_freq_hz) {
            let _ = unsafe { WHvDeletePartition(handle) };
            return Err(e);
        }

        debug!("WHP partition created with {vcpu_count} vCPU(s)");
        Ok(WhpVm { handle })
    }

    fn configure_partition(
        handle: WHV_PARTITION_HANDLE,
        vcpu_count: u32,
        tsc_freq_hz: u64,
    ) -> Result<(), Error> {
        Self::set_property(handle, WHvPartitionPropertyCodeProcessorCount, |p| {
            p.ProcessorCount = vcpu_count;
        })?;

        Self::set_property(
            handle,
            WHvPartitionPropertyCodeLocalApicEmulationMode,
            |p| {
                p.LocalApicEmulationMode = WHvX64LocalApicEmulationModeXApic;
            },
        )?;

        // Enable MSR exits (bit 1)
        // https://github.com/google/crosvm/blob/main/hypervisor/src/whpx/whpx_sys/WinHvPlatformDefs.h#L74
        Self::set_property(handle, WHvPartitionPropertyCodeExtendedVmExits, |p| {
            p.ExtendedVmExits.AsUINT64 = 0b10; // bit 1 = X64MsrExit
        })?;

        // Configure how MSRs are handled
        // We just set the bit 0 (UnhandledMsrs) so that any MSR read/write does not automatically fail
        // but it triggers an exit that we can handle
        Self::set_property(handle, WHvPartitionPropertyCodeX64MsrExitBitmap, |p| {
            p.X64MsrExitBitmap.AsUINT64 = 0b01; // bit 0 = UnhandledMsrs
        })?;

        // Set invariant TSC support
        // First we need to retrieve the processor features banks and re-set them with the invariant TSC support
        // otherwise they get lost
        // Whether the partition actually accepts an invariant TSC. What we
        // advertise in CPUID has to match: telling the guest the TSC is
        // invariant while the partition refused to support it leaves the
        // kernel trusting a clock the hypervisor will not hold up.
        let mut tsc_invariant = false;
        let processor_features_banks = get_processor_features_banks()?;
        if processor_features_banks.BanksCount >= 2 {
            // Windows 10 rejects this with E_INVALIDARG: its WHv will not take
            // a ProcessorFeaturesBanks write with TscInvariantSupport set, and
            // because this runs before everything else the partition never gets
            // set up and the guest never boots -- with no output anywhere but
            // an EINVAL from krun_start_enter.
            //
            // Advertising invariant TSC is an optimisation, not a requirement.
            // Where it is refused, carry on without it: the guest picks another
            // clocksource, which is what it would have done before this was
            // ever set. Failing the whole VM to keep a hint is the wrong trade.
            tsc_invariant = Self::set_property(
                handle,
                WHvPartitionPropertyCodeProcessorFeaturesBanks,
                |p| {
                    p.ProcessorFeaturesBanks = processor_features_banks;

                    unsafe {
                        p.ProcessorFeaturesBanks.Anonymous.AsUINT64[1] |= 0x2; // TscInvariantSupport
                    }
                },
            )
            .is_ok();

            // Windows 10 refuses the write above with E_INVALIDARG. Do not
            // give up on the banks entirely: re-set the host's own values
            // unchanged, so the partition still gets its feature banks and
            // only the invariant-TSC hint is lost. Dropping the whole write
            // leaves the banks unset, which is a bigger change than the hint
            // we were trying to add.
            if !tsc_invariant {
                match Self::set_property(
                    handle,
                    WHvPartitionPropertyCodeProcessorFeaturesBanks,
                    |p| {
                        p.ProcessorFeaturesBanks = processor_features_banks;
                    },
                ) {
                    Ok(()) => log::warn!(
                        "invariant TSC not advertised (not supported here); \
                         processor feature banks set unchanged"
                    ),
                    Err(e) => log::warn!(
                        "processor feature banks not set: {e}. The guest will \
                         use another clocksource."
                    ),
                }
            }
        }

        // This unlocks the MSRs you are advertising in CPUID.
        //
        // Windows 10 does not have this property. Its WHv rejects the code
        // outright -- WHvGetPartitionProperty and WHvSetPartitionProperty both
        // return 0x80370302 (unknown property) on build 19045, while every
        // other property we set succeeds -- so setting it unconditionally
        // fails partition setup and the guest never boots. The symptom is a
        // silent one: krun_start_enter returns EINVAL, the console log stays
        // empty, and nebulad reports only that the agent never became healthy.
        //
        // Ask before setting. Where the property does not exist there is
        // nothing to unlock and nothing to do; a Windows that has it behaves
        // exactly as before.
        if Self::property_supported(
            handle,
            WHvPartitionPropertyCodeSyntheticProcessorFeaturesBanks,
        ) {
        Self::set_property(
            handle,
            WHvPartitionPropertyCodeSyntheticProcessorFeaturesBanks,
            |p| {
                p.SyntheticProcessorFeaturesBanks.BanksCount = 1;
                // We use bitwise OR on the AsUINT64 array of the union for safety/clarity.
                // Bit 0: HypervisorPresent
                // Bit 1: Hv1 (Report support for Hv1: CPUID leaves 0x40000000 - 0x40000006)
                // Bit 2: AccessVpRunTimeReg
                // Bit 3: AccessPartitionReferenceCounter
                // Bit 7: Hypercalls
                // Bit 8: AccessVpIndex
                // Bit 9: AccessPartitionReferenceTsc — intentionally OFF:
                //   the TSC page reads through the VIRTUAL TSC, which only
                //   advances while a VP is inside WHvRunVirtualProcessor.
                //   WHP doesn't park HLT'd VPs (we sleep on the host per
                //   HLT exit), so with the TSC page the guest clock froze
                //   while idle (observed 30 min drift in 30 min). The
                //   reference counter MSR advances in real time per the
                //   TLFS, so the guest falls back to the MSR clocksource.
                // Bit 11: AccessFrequencyRegs
                // Bit 3 (AccessPartitionReferenceCounter) is also OFF: WHP's
                // partition reference time has the same only-advances-while-
                // running behavior, so reads of HV_X64_MSR_TIME_REF_COUNT
                // must exit to the VMM (UnhandledMsrs bitmap), which serves
                // them from host QPC (host_reference_time_100ns).
                unsafe {
                    p.SyntheticProcessorFeaturesBanks.Anonymous.AsUINT64[0] = 0x987;
                }
            },
        )?;
        }

        let mut cpuid_results: Vec<WHV_X64_CPUID_RESULT> = Vec::new();

        // WHP does NOT expose Hyper-V CPUID to the guest automatically;
        // we must provide 0x40000000+ via CpuidResultList.
        // More info on Hypervisor Top Level Functional Specification
        // https://learn.microsoft.com/en-us/virtualization/hyper-v-on-windows/tlfs/tlfs

        // 0x40000000 — Hypervisor signature: "Microsoft Hv"
        cpuid_results.push(WHV_X64_CPUID_RESULT {
            Function: 0x40000000,
            Reserved: [0; 3],
            Eax: 0x40000006,
            Ebx: 0x7263694D, // "Micr"
            Ecx: 0x666F736F, // "osof"
            Edx: 0x76482074, // "t Hv"
        });
        // 0x40000001 — Interface identification: "Hv#1"
        cpuid_results.push(WHV_X64_CPUID_RESULT {
            Function: 0x40000001,
            Reserved: [0; 3],
            Eax: 0x31237648, // "Hv#1"
            Ebx: 0,
            Ecx: 0,
            Edx: 0,
        });
        // 0x40000002 — Version (minimal)
        cpuid_results.push(WHV_X64_CPUID_RESULT {
            Function: 0x40000002,
            Reserved: [0; 3],
            Eax: 0x3839,  // build number
            Ebx: 0xa0000, // version
            Ecx: 0,
            Edx: 0,
        });
        // 0x40000003 — Feature identification (Hyper-V TLFS §2.4)
        const ACCESS_VP_RUNTIME: u32 = 1 << 0;
        const ACCESS_REF_COUNTER: u32 = 1 << 1;
        const ACCESS_HYPERCALLS: u32 = 1 << 5;
        const ACCESS_VP_INDEX: u32 = 1 << 6;
        #[allow(dead_code)]
        const ACCESS_REF_TSC: u32 = 1 << 9;
        const ACCESS_FREQ_REGS: u32 = 1 << 11;
        // ACCESS_REF_TSC intentionally absent: the TSC page reads through
        // the virtual TSC, which only advances while a VP is inside
        // WHvRunVirtualProcessor — with our sleep-per-HLT idle handling the
        // guest clock froze while idle. The reference counter MSR advances
        // in real time (TLFS), so the guest uses hyperv_clocksource_msr.
        cpuid_results.push(WHV_X64_CPUID_RESULT {
            Function: 0x40000003,
            Reserved: [0; 3],
            Eax: ACCESS_VP_RUNTIME
                | ACCESS_REF_COUNTER
                | ACCESS_HYPERCALLS
                | ACCESS_VP_INDEX
                | ACCESS_FREQ_REGS,
            Ebx: 0,
            Ecx: 0,
            Edx: 0,
        });
        // 0x40000004 — Recommendations
        cpuid_results.push(WHV_X64_CPUID_RESULT {
            Function: 0x40000004,
            Reserved: [0; 3],
            Eax: 1 << 5, // RelaxedTiming
            Ebx: 0,
            Ecx: 0,
            Edx: 0,
        });
        // 0x40000005 — Implementation limits
        cpuid_results.push(WHV_X64_CPUID_RESULT {
            Function: 0x40000005,
            Reserved: [0; 3],
            Eax: 64, // max virtual processors
            Ebx: 0,
            Ecx: 0,
            Edx: 0,
        });
        // 0x40000006: Hardware Features
        const HV_MSR_BITMAPS: u32 = 1 << 1;
        const HV_SLAT: u32 = 1 << 3;
        cpuid_results.push(WHV_X64_CPUID_RESULT {
            Function: 0x40000006,
            Reserved: [0; 3],
            Eax: HV_MSR_BITMAPS | HV_SLAT,
            Ebx: 0,
            Ecx: 0,
            Edx: 0,
        });

        // invariant tsc -- only when the partition took it above
        if tsc_invariant {
            cpuid_results.push(WHV_X64_CPUID_RESULT {
                Function: 0x80000007,
                Reserved: [0; 3],
                Eax: 0,
                Ebx: 0,
                Ecx: 0,
                Edx: 0x100, // bit 8 (Invariant TSC / nonstop_tsc)
            });
        }

        // Standard Intel CPUID leaves (Intel's SDM Vol. 2A)
        if let Some((eax_val, ebx_val, crystal_hz)) = cpuid_0x15_values(tsc_freq_hz) {
            debug!("Providing TSC frequency to guest: {} Hz", tsc_freq_hz);

            // CPUID 0x15 — TSC / Core Crystal Clock (Intel SDM)
            // Formula: TSC Frequency = ECX (Crystal Hz) * EBX/EAX (Ratio)
            //
            // Do not use a tiny synthetic crystal just to make the ratio fit.
            // With the former 1 kHz value and a HZ=250 guest, Linux derived a
            // four-count LAPIC period and divided it by 16 before writing
            // TMICT. The resulting zero disables the timer. Windows 10 does
            // not expose the TSC-deadline path that masks this on Windows 11,
            // so the guest halted before probing any virtio device.
            //
            // A 25 MHz crystal with a denominator of 100 retains 250 kHz
            // precision and keeps Linux's u32 crystal_khz * numerator
            // intermediate in range for any realistic TSC. Do not substitute
            // WHP's InterruptClockFrequency capability here: it describes a
            // root-partition processor property, not a CPUID core crystal,
            // and using its 200 MHz value resets affected Windows 10 hosts.

            cpuid_results.push(WHV_X64_CPUID_RESULT {
                Function: 0x15,
                Reserved: [0; 3],
                Eax: eax_val,
                Ebx: ebx_val,
                Ecx: crystal_hz,
                Edx: 0,
            });
        } else if tsc_freq_hz > 0 {
            log::warn!("TSC frequency {tsc_freq_hz} cannot be represented safely in CPUID 0x15");
        }

        let hr = unsafe {
            WHvSetPartitionProperty(
                handle,
                WHvPartitionPropertyCodeCpuidResultList,
                cpuid_results.as_ptr() as *const _,
                (cpuid_results.len() * mem::size_of::<WHV_X64_CPUID_RESULT>()) as u32,
            )
        };
        if hr != S_OK {
            return Err(Error::SetPartitionProperty(
                WHvPartitionPropertyCodeCpuidResultList,
                hr,
            ));
        }

        let hr = unsafe { WHvSetupPartition(handle) };
        if hr != S_OK {
            Err(Error::SetupPartition(hr))
        } else {
            Ok(())
        }
    }

    /// Does this Windows know the property at all?
    ///
    /// Reading it is the cheapest probe: a code this build does not implement
    /// fails the same way for get and for set (0x80370302), and a code it does
    /// implement reads back whatever the partition currently holds.
    fn property_supported(
        handle: WHV_PARTITION_HANDLE,
        code: WHV_PARTITION_PROPERTY_CODE,
    ) -> bool {
        let mut prop = unsafe { MaybeUninit::<WHV_PARTITION_PROPERTY>::zeroed().assume_init() };
        let mut written: u32 = 0;
        let hr = unsafe {
            WHvGetPartitionProperty(
                handle,
                code,
                &mut prop as *mut _ as *mut _,
                mem::size_of::<WHV_PARTITION_PROPERTY>() as u32,
                &mut written,
            )
        };
        hr == S_OK
    }

    fn set_property(
        handle: WHV_PARTITION_HANDLE,
        code: WHV_PARTITION_PROPERTY_CODE,
        configure: impl FnOnce(&mut WHV_PARTITION_PROPERTY),
    ) -> Result<(), Error> {
        let mut prop = unsafe { MaybeUninit::<WHV_PARTITION_PROPERTY>::zeroed().assume_init() };
        configure(&mut prop);
        let hr = unsafe {
            WHvSetPartitionProperty(
                handle,
                code,
                &prop as *const _ as *const _,
                mem::size_of::<WHV_PARTITION_PROPERTY>() as u32,
            )
        };
        if hr != S_OK {
            Err(Error::SetPartitionProperty(code, hr))
        } else {
            Ok(())
        }
    }

    /// Detect the host TSC frequency in Hz.
    /// Tries CPUID 0x15, then 0x16 (Intel), then falls back to measuring
    /// via RDTSC over a short sleep (works on AMD and all other x86_64).
    fn detect_tsc_frequency() -> u64 {
        unsafe {
            // Leaf 0 returns the maximum supported standard leaf in EAX.
            // leaves 0x15 and 0x16 may not be supported (like in non-Intel hardware (like AMD)
            // or when using nested virtualization), so it's better to check first.
            let max_leaf = core::arch::x86_64::__cpuid(0x0).eax;

            if max_leaf >= 0x15 {
                let cpuid15 = core::arch::x86_64::__cpuid(0x15);
                if cpuid15.eax != 0 && cpuid15.ebx != 0 && cpuid15.ecx != 0 {
                    let freq = (cpuid15.ecx as u64 * cpuid15.ebx as u64) / cpuid15.eax as u64;
                    debug!("TSC frequency from CPUID 0x15: {} Hz", freq);
                    return freq;
                }
            }

            if max_leaf >= 0x16 {
                let cpuid16 = core::arch::x86_64::__cpuid(0x16);
                if cpuid16.eax != 0 {
                    let freq = cpuid16.eax as u64 * 1_000_000;
                    debug!("TSC frequency from CPUID 0x16: {} Hz", freq);
                    return freq;
                }
            }
        }

        debug!("CPUID 0x15/0x16 unavailable, measuring TSC frequency via QPC");

        // If CPUID 0x15/0x16 is unavailable, we try to measure the CPU's TSC frequency by using the
        // Windows High-Resolution Performance Counter (QPC)
        // We takes a snapshot of both the QPC and the TSC, waits for exactly 10 milliseconds,
        // and then takes another snapshot of both. By comparing how many TSC ticks occurred during those 10 ms
        // of QPC ticks, we can accurately calculate the TSC ticks per second (Hz).

        let mut qpc_freq = 0;
        let mut start_qpc = 0;
        let mut end_qpc = 0;

        unsafe {
            // No need to check the result, it will always succeed
            // https://learn.microsoft.com/en-us/windows/win32/api/profileapi/nf-profileapi-queryperformancefrequency#return-value
            let _ = QueryPerformanceFrequency(&mut qpc_freq);
            // Take a snapshot of the current QPC tick count
            // No need to check the result, it will always succeed.
            // https://learn.microsoft.com/en-us/windows/win32/api/profileapi/nf-profileapi-queryperformancecounter#return-value
            let _ = QueryPerformanceCounter(&mut start_qpc);
        }

        // Take a snapshot of the CPU's TSC
        let start_tsc = unsafe { core::arch::x86_64::_rdtsc() };

        // Calculate how many QPC ticks equal 10 milliseconds and loop over it.
        // If we used `std::thread::sleep(10ms)` instead, the OS might put this thread
        // to sleep for 15ms or 20ms, ruining our highly sensitive calibration interval.
        let target_qpc = start_qpc + (qpc_freq / 100);
        loop {
            unsafe {
                // No need to check the result, it will always succeed.
                let _ = QueryPerformanceCounter(&mut end_qpc);
            }
            if end_qpc >= target_qpc {
                break;
            }
        }

        // Take a snapshot of the CPU's TSC after 10ms
        let end_tsc = unsafe { core::arch::x86_64::_rdtsc() };
        // Calculate the actual elapsed QPC ticks
        let qpc_elapsed = end_qpc - start_qpc;

        if qpc_elapsed > 0 {
            let tsc_elapsed = end_tsc.wrapping_sub(start_tsc);
            // Calculate utilizing u128 to prevent overflow before dividing
            let freq = (tsc_elapsed as u128 * qpc_freq as u128 / qpc_elapsed as u128) as u64;
            debug!(
                "TSC frequency measured: {} Hz ({} MHz)",
                freq,
                freq / 1_000_000
            );
            return freq;
        }

        error!("Could not determine TSC frequency");
        0
    }

    /// Maps a host memory region into the guest physical address space.
    ///
    /// # Safety
    ///
    /// `host_start_addr` must point to a valid, writable memory region of at least `size` bytes
    /// that remains live for the lifetime of this partition.
    pub unsafe fn map_memory(
        &self,
        host_start_addr: *mut c_void,
        guest_start_addr: u64,
        size: u64,
    ) -> Result<(), Error> {
        // WHP requires 4KB alignment
        if (host_start_addr as usize | guest_start_addr as usize | size as usize) & 0xFFF != 0 {
            return Err(Error::MemoryAlignment);
        }

        let hr = unsafe {
            WHvMapGpaRange(
                self.handle,
                host_start_addr as *const _,
                guest_start_addr,
                size,
                WHvMapGpaRangeFlagRead | WHvMapGpaRangeFlagWrite | WHvMapGpaRangeFlagExecute,
            )
        };
        if hr != S_OK {
            Err(Error::MapGpaRange(hr))
        } else {
            Ok(())
        }
    }

    /// Injects an interrupt into a virtual processor's local APIC.
    /// http://learn.microsoft.com/en-us/virtualization/api/hypervisor-platform/funcs/whvrequestinterrupt
    pub fn request_interrupt(&self, req: &InterruptRequest) -> Result<(), Error> {
        let ctrl = WhvInterruptControl {
            type_and_flags: (req.interrupt_type as u64)
                | ((req.destination_mode as u64) << 8)
                | ((req.trigger_mode as u64) << 12),
            destination: req.destination,
            vector: req.vector,
        };

        let hr = unsafe {
            WHvRequestInterrupt(
                self.handle,
                &ctrl as *const _ as *const _,
                mem::size_of::<WhvInterruptControl>() as u32,
            )
        };
        if hr != S_OK {
            Err(Error::RequestInterrupt(hr))
        } else {
            Ok(())
        }
    }

    /// Fire a fixed, edge-triggered interrupt to APIC ID with the given vector.
    pub fn request_fixed_interrupt(&self, vector: u32, apic_id: u32) {
        let result = self.request_interrupt(&InterruptRequest {
            interrupt_type: InterruptType::Fixed,
            destination_mode: InterruptDestinationMode::Physical,
            trigger_mode: InterruptTriggerMode::Edge,
            destination: apic_id,
            vector,
        });
        if result.is_err() {
            error!("inject_vector(0x{vector:02x}) failed: {result:?}");
        }
    }

    /// Cancel a running `WHvRunVirtualProcessor` call so the vCPU thread
    /// exits with `Canceled`. Required after `request_interrupt`/`request_fixed_interrupt` to wake a
    /// vCPU that is blocked in HLT.
    pub fn cancel_vcpu(&self, vp_index: u32) {
        let hr = unsafe { WHvCancelRunVirtualProcessor(self.handle, vp_index, 0) };
        if hr != S_OK {
            error!("WHvCancelRunVirtualProcessor({vp_index}) failed: HRESULT 0x{hr:08x}");
        }
    }

    /// Clear a vector from a VP's local-APIC in-service register.
    ///
    /// A userspace PIC injects its vector through `WHvRequestInterrupt`, so
    /// WHP records it in the LAPIC even though a legacy guest acknowledges it
    /// at the PIC. Call this while the VP is stopped on that PIC port exit.
    pub fn clear_interrupt_in_service(&self, vp_index: u32, vector: u8) -> Result<(), Error> {
        let mut state = vec![0u8; 4096];
        let mut written = 0u32;
        let hr = unsafe {
            WHvGetVirtualProcessorInterruptControllerState2(
                self.handle,
                vp_index,
                state.as_mut_ptr() as *mut c_void,
                state.len() as u32,
                &mut written,
            )
        };
        if hr != S_OK {
            return Err(Error::GetInterruptController(hr));
        }

        let offset = 0x100 + (usize::from(vector) / 32) * 0x10;
        if offset + 4 > written as usize {
            return Ok(());
        }
        let mut word = u32::from_le_bytes(state[offset..offset + 4].try_into().unwrap());
        word &= !(1 << (vector % 32));
        state[offset..offset + 4].copy_from_slice(&word.to_le_bytes());

        let hr = unsafe {
            WHvSetVirtualProcessorInterruptControllerState2(
                self.handle,
                vp_index,
                state.as_ptr() as *const c_void,
                written,
            )
        };
        if hr != S_OK {
            Err(Error::SetInterruptController(hr))
        } else {
            Ok(())
        }
    }

    pub fn partition_handle(&self) -> WHV_PARTITION_HANDLE {
        self.handle
    }
}

impl Drop for WhpVm {
    fn drop(&mut self) {
        let hr = unsafe { WHvDeletePartition(self.handle) };
        if hr != S_OK {
            error!("WHvDeletePartition failed: HRESULT 0x{hr:08x}");
        }
    }
}

unsafe impl Send for WhpVm {}
unsafe impl Sync for WhpVm {}

/// Wraps a `WHV_EMULATOR_HANDLE` (one per vCPU).
///
/// The Windows Hypervisor Platform provides a built-in x86 instruction emulator.
/// When the guest OS tries to read or write to a virtual device (via MMIO or Port I/O),
/// the physical CPU exits to the hypervisor. However, the hypervisor only knows *where*
/// the guest tried to access, not *how* (e.g., was it a 1-byte read or a 4-byte write?).
///
/// We pass the raw VM exit context into this emulator, and it decodes the x86
/// instruction, calls our provided callback functions to perform the actual I/O,
/// updates the guest registers with the result, and advances the instruction pointer.
pub struct WhpEmulator {
    handle: *mut c_void,
}

impl WhpEmulator {
    /// The `callbacks` struct contains function pointers to VMM's I/O
    /// and memory handling routines. When the emulator decodes an instruction
    /// that reads/writes memory, it will call these pointers to get or set the data.
    pub fn new(callbacks: WHV_EMULATOR_CALLBACKS) -> Result<Self, Error> {
        let mut handle: *mut c_void = std::ptr::null_mut();
        let hr = unsafe { WHvEmulatorCreateEmulator(&callbacks, &mut handle) };
        if hr != S_OK {
            Err(Error::CreateEmulator(hr))
        } else {
            Ok(WhpEmulator { handle })
        }
    }

    fn check_emulation_result(
        hr: i32,
        status: WHV_EMULATOR_STATUS,
        hresult_err: fn(i32) -> Error,
    ) -> Result<(), Error> {
        if hr != S_OK {
            return Err(hresult_err(hr));
        }
        // Check the emulator status bitfield.
        // According to Microsoft docs, Bit 0 (EmulationSuccessful) must be 1.
        let bits = unsafe { status.AsUINT32 };
        if bits & 1 == 0 {
            return Err(Error::EmulationFailed(bits));
        }
        Ok(())
    }

    /// Attempts to emulate an x86 Port I/O (PIO) instruction (e.g., `IN`, `OUT`).
    ///
    /// It is called when `WHvRunVirtualProcessor` returns `WHvRunVpExitReasonX64IoPortAccess`.
    ///
    /// * `context`: An opaque pointer that WHP will pass directly into your callbacks.
    ///   This points to the Bus so the callback can route the I/O to the correct device.
    /// * `vp_context`: The state of the vCPU's registers at the time of the exit.
    /// * `io_context`: The details of the I/O port exit (port number, access size, etc.).
    ///
    /// # Safety
    ///
    /// All three pointers must be valid and non-null for the duration of the call.
    // https://learn.microsoft.com/en-us/virtualization/api/hypervisor-instruction-emulator/funcs/whvemulatortryemulation
    pub unsafe fn try_io_emulation(
        &self,
        context: *const c_void,
        vp_context: *const WHV_VP_EXIT_CONTEXT,
        io_context: *const WHV_X64_IO_PORT_ACCESS_CONTEXT,
    ) -> Result<(), Error> {
        let mut status: WHV_EMULATOR_STATUS = mem::zeroed();
        let hr =
            WHvEmulatorTryIoEmulation(self.handle, context, vp_context, io_context, &mut status);
        Self::check_emulation_result(hr, status, Error::IoEmulation)
    }

    /// Attempts to emulate an x86 Memory-Mapped I/O (MMIO) instruction (e.g., `MOV eax, [mem]`).
    ///
    /// It is called when `WHvRunVirtualProcessor` returns `WHvRunVpExitReasonMemoryAccess`
    /// *and* the memory address belongs to a virtual device.
    ///
    /// # Safety
    ///
    /// All three pointers must be valid and non-null for the duration of the call.
    pub unsafe fn try_mmio_emulation(
        &self,
        context: *const c_void,
        vp_context: *const WHV_VP_EXIT_CONTEXT,
        mmio_context: *const WHV_MEMORY_ACCESS_CONTEXT,
    ) -> Result<(), Error> {
        let mut status: WHV_EMULATOR_STATUS = mem::zeroed();
        let hr = WHvEmulatorTryMmioEmulation(
            self.handle,
            context,
            vp_context,
            mmio_context,
            &mut status,
        );
        Self::check_emulation_result(hr, status, Error::MmioEmulation)
    }
}

impl Drop for WhpEmulator {
    fn drop(&mut self) {
        let hr = unsafe { WHvEmulatorDestroyEmulator(self.handle) };
        if hr != S_OK {
            error!("WHvEmulatorDestroyEmulator failed: HRESULT 0x{hr:08x}");
        }
    }
}

unsafe impl Send for WhpEmulator {}

#[derive(Debug)]
pub enum VcpuExitReason {
    IoPortAccess,
    MemoryAccess,
    Halt,
    Canceled,
    CpuidAccess,
    MsrAccess,
    InterruptWindow,
    UnrecoverableException,
    InvalidVpRegisterValue,
    UnsupportedFeature,
    Unknown(u32),
}

pub struct WhpVcpu {
    vm: Arc<WhpVm>,
    index: u32,
    exit_context: WHV_RUN_VP_EXIT_CONTEXT,
}

impl WhpVcpu {
    /// Creates a new virtual processor within the given partition.
    pub fn new(vm: Arc<WhpVm>, index: u32) -> Result<Self, Error> {
        let hr = unsafe { WHvCreateVirtualProcessor(vm.partition_handle(), index, 0) };
        if hr != S_OK {
            return Err(Error::CreateVirtualProcessor(hr));
        }

        debug!("Created WHP vCPU {index}");
        Ok(WhpVcpu {
            vm,
            index,
            exit_context: unsafe { mem::zeroed() },
        })
    }

    /// Runs the virtual processor until a VM exit occurs.
    ///
    /// The raw exit context is stored internally and can be accessed via
    /// [`vp_exit_context`], [`io_port_access_context`], and
    /// [`memory_access_context`] for passing to the instruction emulator.
    pub fn run(&mut self) -> Result<VcpuExitReason, Error> {
        let hr = unsafe {
            WHvRunVirtualProcessor(
                self.vm.partition_handle(),
                self.index,
                &mut self.exit_context as *mut _ as *mut _,
                mem::size_of::<WHV_RUN_VP_EXIT_CONTEXT>() as u32,
            )
        };
        if hr != S_OK {
            return Err(Error::RunVirtualProcessor(hr));
        }

        Ok(Self::decode_reason(&self.exit_context))
    }

    pub fn vp_exit_context(&self) -> *const WHV_VP_EXIT_CONTEXT {
        &self.exit_context.VpContext
    }

    pub fn io_port_access_context(&self) -> *const WHV_X64_IO_PORT_ACCESS_CONTEXT {
        unsafe { &self.exit_context.Anonymous.IoPortAccess }
    }

    pub fn memory_access_context(&self) -> *const WHV_MEMORY_ACCESS_CONTEXT {
        unsafe { &self.exit_context.Anonymous.MemoryAccess }
    }

    /// Returns parsed CPUID exit info. Only valid after a `CpuidAccess` exit.
    pub fn cpuid_exit_info(&self) -> CpuidExitInfo {
        let ctx = unsafe { &self.exit_context.Anonymous.CpuidAccess };
        CpuidExitInfo {
            leaf: ctx.Rax,
            subleaf: ctx.Rcx,
            default_eax: ctx.DefaultResultRax,
            default_ebx: ctx.DefaultResultRbx,
            default_ecx: ctx.DefaultResultRcx,
            default_edx: ctx.DefaultResultRdx,
        }
    }

    /// Returns parsed MSR exit info. Only valid after an `MsrAccess` exit.
    pub fn msr_exit_info(&self) -> MsrExitInfo {
        let ctx = unsafe { &self.exit_context.Anonymous.MsrAccess };
        MsrExitInfo {
            msr_number: ctx.MsrNumber,
            is_write: unsafe { ctx.AccessInfo.Anonymous._bitfield } & 1 != 0,
            rax: ctx.Rax,
            rdx: ctx.Rdx,
        }
    }

    /// RIP at the time of the VM exit.
    pub fn exit_rip(&self) -> u64 {
        self.exit_context.VpContext.Rip
    }

    /// Instruction length from the exit context (lower 4 bits of the packed byte).
    pub fn instruction_length(&self) -> u8 {
        self.exit_context.VpContext._bitfield & 0x0F
    }

    /// Advances RIP past the faulting instruction using the instruction length
    /// from the exit context.
    pub fn advance_rip(&self) -> Result<(), Error> {
        let new_rip = self.exit_context.VpContext.Rip + self.instruction_length() as u64;
        self.set_registers64([(WHvX64RegisterRip, new_rip)])
    }

    /// Sets RAX, RBX, RCX, RDX and advances RIP in a single register write.
    /// Used by CPUID exit handling.
    pub fn complete_cpuid(&self, eax: u64, ebx: u64, ecx: u64, edx: u64) -> Result<(), Error> {
        let new_rip = self.exit_context.VpContext.Rip + self.instruction_length() as u64;

        self.set_registers64([
            (WHvX64RegisterRax, eax),
            (WHvX64RegisterRbx, ebx),
            (WHvX64RegisterRcx, ecx),
            (WHvX64RegisterRdx, edx),
            (WHvX64RegisterRip, new_rip),
        ])
    }

    /// Sets RAX and RDX (MSR read result) then advances RIP.
    pub fn complete_msr_read(&self, rax: u64, rdx: u64) -> Result<(), Error> {
        let new_rip = self.exit_context.VpContext.Rip + self.instruction_length() as u64;

        self.set_registers64([
            (WHvX64RegisterRax, rax),
            (WHvX64RegisterRdx, rdx),
            (WHvX64RegisterRip, new_rip),
        ])
    }

    /// Helper to get multiple registers at once.
    /// Returns a stack-allocated array of results.
    pub fn get_registers<const N: usize>(
        &self,
        names: [WHV_REGISTER_NAME; N],
    ) -> Result<[WHV_REGISTER_VALUE; N], Error> {
        // 16-aligned scratch: see AlignedRegisterValue.
        let mut values: [AlignedRegisterValue; N] = [AlignedRegisterValue::zeroed(); N];

        let hr = unsafe {
            WHvGetVirtualProcessorRegisters(
                self.vm.partition_handle(),
                self.index,
                names.as_ptr(),
                N as u32,
                values.as_mut_ptr() as *mut WHV_REGISTER_VALUE,
            )
        };

        if hr != S_OK {
            Err(Error::GetRegisters(hr))
        } else {
            Ok(values.map(|v| v.0))
        }
    }

    pub fn get_registers64<const N: usize>(
        &self,
        names: [WHV_REGISTER_NAME; N],
    ) -> Result<[u64; N], Error> {
        let values = self.get_registers(names)?;
        Ok(values.map(|v| unsafe { v.Reg64 }))
    }

    fn set_whp_registers(
        &self,
        names: &[WHV_REGISTER_NAME],
        values: &[AlignedRegisterValue],
    ) -> Result<(), Error> {
        assert_eq!(names.len(), values.len());
        let count = names.len() as u32;

        let hr = unsafe {
            WHvSetVirtualProcessorRegisters(
                self.vm.partition_handle(),
                self.index,
                names.as_ptr(),
                count,
                values.as_ptr() as *const WHV_REGISTER_VALUE,
            )
        };
        if hr != S_OK {
            Err(Error::SetRegisters(hr))
        } else {
            Ok(())
        }
    }

    pub fn set_registers<const N: usize>(
        &self,
        pairs: [(WHV_REGISTER_NAME, WHV_REGISTER_VALUE); N],
    ) -> Result<(), Error> {
        let mut names: [WHV_REGISTER_NAME; N] = unsafe { mem::zeroed() };
        let mut values: [AlignedRegisterValue; N] = [AlignedRegisterValue::zeroed(); N];

        for i in 0..N {
            names[i] = pairs[i].0;
            values[i] = AlignedRegisterValue(pairs[i].1);
        }

        self.set_whp_registers(&names, &values)
    }

    pub fn set_registers64<const N: usize>(
        &self,
        pairs: [(WHV_REGISTER_NAME, u64); N],
    ) -> Result<(), Error> {
        let mut names: [WHV_REGISTER_NAME; N] = unsafe { mem::zeroed() };
        let mut values: [AlignedRegisterValue; N] = [AlignedRegisterValue::zeroed(); N];

        for i in 0..N {
            names[i] = pairs[i].0;
            values[i].0.Reg64 = pairs[i].1;
        }

        self.set_whp_registers(&names, &values)
    }

    pub fn vm(&self) -> &Arc<WhpVm> {
        &self.vm
    }

    pub fn partition_handle(&self) -> WHV_PARTITION_HANDLE {
        self.vm.partition_handle()
    }

    pub fn index(&self) -> u32 {
        self.index
    }

    /// Clear the deliverability-notification request so WHP stops exiting
    /// on every STI instruction. Called after an InterruptWindow exit.
    /// https://github.com/google/crosvm/blob/main/hypervisor/src/whpx/whpx_sys/WinHvPlatformDefs.h#L773
    pub fn clear_interrupt_window(&self) -> Result<(), Error> {
        let [notifications] = self.get_registers64([WHvX64RegisterDeliverabilityNotifications])?;
        if notifications & 0x2 != 0 {
            // Clear Bit 1 (InterruptNotification) while preserving NMI and Priority bits
            self.set_registers64([(
                WHvX64RegisterDeliverabilityNotifications,
                notifications & !0x2,
            )])?;
        }
        Ok(())
    }

    #[allow(non_upper_case_globals)]
    fn decode_reason(ctx: &WHV_RUN_VP_EXIT_CONTEXT) -> VcpuExitReason {
        match ctx.ExitReason {
            WHvRunVpExitReasonX64IoPortAccess => VcpuExitReason::IoPortAccess,
            WHvRunVpExitReasonMemoryAccess => VcpuExitReason::MemoryAccess,
            WHvRunVpExitReasonX64Halt => VcpuExitReason::Halt,
            WHvRunVpExitReasonCanceled => VcpuExitReason::Canceled,
            WHvRunVpExitReasonX64Cpuid => VcpuExitReason::CpuidAccess,
            WHvRunVpExitReasonX64MsrAccess => VcpuExitReason::MsrAccess,
            WHvRunVpExitReasonX64InterruptWindow => VcpuExitReason::InterruptWindow,
            WHvRunVpExitReasonUnrecoverableException => VcpuExitReason::UnrecoverableException,
            WHvRunVpExitReasonInvalidVpRegisterValue => VcpuExitReason::InvalidVpRegisterValue,
            WHvRunVpExitReasonUnsupportedFeature => VcpuExitReason::UnsupportedFeature,
            _ => VcpuExitReason::Unknown(ctx.ExitReason as u32),
        }
    }
}

impl Drop for WhpVcpu {
    fn drop(&mut self) {
        let hr = unsafe { WHvDeleteVirtualProcessor(self.vm.partition_handle(), self.index) };
        if hr != S_OK {
            error!(
                "WHvDeleteVirtualProcessor({}) failed: HRESULT 0x{hr:08x}",
                self.index
            );
        }
    }
}

// --- vcpu snapshot state ----------------------------------------------------

/// Every register a snapshot carries: GPRs, RIP/RFLAGS, segments, descriptor
/// tables, control + debug registers, the MSRs WHP models as registers, and
/// the pending-interrupt/deliverability state. Captured and reapplied as one
/// positional batch.
const SNAPSHOT_REGS: [WHV_REGISTER_NAME; 58] = [
    WHvX64RegisterRax,
    WHvX64RegisterRcx,
    WHvX64RegisterRdx,
    WHvX64RegisterRbx,
    WHvX64RegisterRsp,
    WHvX64RegisterRbp,
    WHvX64RegisterRsi,
    WHvX64RegisterRdi,
    WHvX64RegisterR8,
    WHvX64RegisterR9,
    WHvX64RegisterR10,
    WHvX64RegisterR11,
    WHvX64RegisterR12,
    WHvX64RegisterR13,
    WHvX64RegisterR14,
    WHvX64RegisterR15,
    WHvX64RegisterRip,
    WHvX64RegisterRflags,
    WHvX64RegisterEs,
    WHvX64RegisterCs,
    WHvX64RegisterSs,
    WHvX64RegisterDs,
    WHvX64RegisterFs,
    WHvX64RegisterGs,
    WHvX64RegisterLdtr,
    WHvX64RegisterTr,
    WHvX64RegisterIdtr,
    WHvX64RegisterGdtr,
    WHvX64RegisterCr0,
    WHvX64RegisterCr2,
    WHvX64RegisterCr3,
    WHvX64RegisterCr4,
    WHvX64RegisterCr8,
    WHvX64RegisterDr0,
    WHvX64RegisterDr1,
    WHvX64RegisterDr2,
    WHvX64RegisterDr3,
    WHvX64RegisterDr6,
    WHvX64RegisterDr7,
    // XCR0 (with IA32_XSS below) defines the XSAVE area layout — both MUST
    // be applied before WHvSetVirtualProcessorXsaveState or task FPU buffers
    // in the restored RAM misparse ("Bad FPU state detected" guest panics).
    // The register batch runs before the xsave blob, preserving that order.
    WHvX64RegisterXCr0,
    WHvX64RegisterEfer,
    WHvX64RegisterKernelGsBase,
    WHvX64RegisterApicBase,
    WHvX64RegisterPat,
    WHvX64RegisterSysenterCs,
    WHvX64RegisterSysenterEip,
    WHvX64RegisterSysenterEsp,
    WHvX64RegisterStar,
    WHvX64RegisterLstar,
    WHvX64RegisterCstar,
    WHvX64RegisterSfmask,
    WHvX64RegisterTsc,
    WHvX64RegisterTscAux,
    WHvX64RegisterTscAdjust,
    WHvRegisterPendingInterruption,
    WHvRegisterInterruptState,
    WHvX64RegisterDeliverabilityNotifications,
    // Keep last: the armed LAPIC timer deadline (TSC-deadline mode — what
    // Linux uses). restore_state() reapplies it AFTER the interrupt
    // controller blob so the APIC state restore can't clear it; without it
    // the guest never receives another timer interrupt and wedges silently.
    WHvX64RegisterTscDeadline,
];

/// Full per-VP machine state for snapshots. Same-machine only — raw WHP
/// register values, an XSAVE area, and the LAPIC state blob.
pub struct VcpuSavedState {
    /// (register name, raw 16-byte register value) pairs.
    pub regs: Vec<(WHV_REGISTER_NAME, [u8; 16])>,
    /// XSAVE area (`WHvGet/SetVirtualProcessorXsaveState`).
    pub xsave: Vec<u8>,
    /// Local APIC state (`InterruptControllerState2`); valid because the
    /// partition runs with XApic emulation mode.
    pub lapic: Vec<u8>,
}

impl WhpVcpu {
    /// Capture the VP's full state. The VP must be outside
    /// `WHvRunVirtualProcessor` (paused).
    pub fn save_state(&self) -> Result<VcpuSavedState, Error> {
        let values = self.get_registers(SNAPSHOT_REGS)?;
        let mut regs: Vec<(WHV_REGISTER_NAME, [u8; 16])> = SNAPSHOT_REGS
            .iter()
            .zip(values.iter())
            .map(|(n, v)| (*n, unsafe { mem::transmute_copy::<WHV_REGISTER_VALUE, [u8; 16]>(v) }))
            .collect();

        // Host/CPU-dependent registers: fetched singly, skipped when the
        // hypervisor rejects them (one bad name fails a whole batch).
        // IA32_XSS matters for XSAVES-format task FPU buffers; the rest are
        // best-effort. Restore replays exactly what was captured.
        for opt in [WHvX64RegisterXss, WHvX64RegisterXfd, WHvX64RegisterSpecCtrl] {
            match self.get_registers([opt]) {
                Ok([v]) => {
                    regs.push((opt, unsafe {
                        mem::transmute_copy::<WHV_REGISTER_VALUE, [u8; 16]>(&v)
                    }));
                }
                Err(e) => log::debug!("vcpu {}: register {opt} not captured: {e}", self.index),
            }
        }

        // 16K covers every XSAVE layout current hardware reports.
        let mut xsave = vec![0u8; 16384];
        let mut written = 0u32;
        let hr = unsafe {
            WHvGetVirtualProcessorXsaveState(
                self.vm.partition_handle(),
                self.index,
                xsave.as_mut_ptr() as *mut c_void,
                xsave.len() as u32,
                &mut written,
            )
        };
        if hr != S_OK {
            return Err(Error::GetXsave(hr));
        }
        xsave.truncate(written as usize);

        let mut lapic = vec![0u8; 4096];
        let mut written = 0u32;
        let hr = unsafe {
            WHvGetVirtualProcessorInterruptControllerState2(
                self.vm.partition_handle(),
                self.index,
                lapic.as_mut_ptr() as *mut c_void,
                lapic.len() as u32,
                &mut written,
            )
        };
        if hr != S_OK {
            return Err(Error::GetInterruptController(hr));
        }
        lapic.truncate(written as usize);

        Ok(VcpuSavedState { regs, xsave, lapic })
    }

    /// Reapply state captured by [`save_state`](Self::save_state) onto a
    /// freshly created VP (before it ever runs). The TSC deadline is applied
    /// last — after the interrupt-controller blob — so the armed LAPIC timer
    /// survives the APIC state restore.
    pub fn restore_state(&self, st: &VcpuSavedState) -> Result<(), Error> {
        let (late, main): (Vec<(WHV_REGISTER_NAME, [u8; 16])>, Vec<_>) = st
            .regs
            .iter()
            .copied()
            .partition(|(n, _)| *n == WHvX64RegisterTscDeadline);
        let names: Vec<WHV_REGISTER_NAME> = main.iter().map(|(n, _)| *n).collect();
        let values: Vec<AlignedRegisterValue> = main
            .iter()
            .map(|(_, b)| {
                AlignedRegisterValue(unsafe {
                    mem::transmute_copy::<[u8; 16], WHV_REGISTER_VALUE>(b)
                })
            })
            .collect();

        if let Err(batch_err) = self.set_whp_registers(&names, &values) {
            // One rejected register fails the whole batch. Retry singly so an
            // optional straggler (e.g. XCr0 on hosts without XSAVE feature
            // config) doesn't kill the restore; real failures still surface
            // in the log.
            log::warn!("vcpu {} restore: batch set failed ({batch_err}); retrying singly", self.index);
            for i in 0..names.len() {
                if let Err(e) = self.set_whp_registers(&names[i..=i], &values[i..=i]) {
                    log::warn!(
                        "vcpu {} restore: register {} not restored: {e}",
                        self.index,
                        names[i]
                    );
                }
            }
        }

        if !st.xsave.is_empty() {
            let hr = unsafe {
                WHvSetVirtualProcessorXsaveState(
                    self.vm.partition_handle(),
                    self.index,
                    st.xsave.as_ptr() as *const c_void,
                    st.xsave.len() as u32,
                )
            };
            if hr != S_OK {
                return Err(Error::SetXsave(hr));
            }
        }

        if !st.lapic.is_empty() {
            let hr = unsafe {
                WHvSetVirtualProcessorInterruptControllerState2(
                    self.vm.partition_handle(),
                    self.index,
                    st.lapic.as_ptr() as *const c_void,
                    st.lapic.len() as u32,
                )
            };
            if hr != S_OK {
                return Err(Error::SetInterruptController(hr));
            }
        }

        for (n, b) in &late {
            let value = AlignedRegisterValue(unsafe {
                mem::transmute_copy::<[u8; 16], WHV_REGISTER_VALUE>(b)
            });
            if let Err(e) = self.set_whp_registers(&[*n], &[value]) {
                log::warn!(
                    "vcpu {} restore: late register {n} not restored: {e}",
                    self.index
                );
            }
        }

        Ok(())
    }
}

/// Debug aid for snapshot work: sample a VP's execution state. Registers
/// can be read while the VP runs; values are racy but indicative.
pub fn debug_sample_vp(vm: &WhpVm, index: u32) -> String {
    let names = [
        WHvX64RegisterRip,
        WHvX64RegisterRflags,
        WHvX64RegisterTsc,
        WHvX64RegisterTscDeadline,
        WHvRegisterInterruptState,
        WHvRegisterPendingInterruption,
    ];
    let mut values: [AlignedRegisterValue; 6] = [AlignedRegisterValue::zeroed(); 6];
    let hr = unsafe {
        WHvGetVirtualProcessorRegisters(
            vm.partition_handle(),
            index,
            names.as_ptr(),
            names.len() as u32,
            values.as_mut_ptr() as *mut WHV_REGISTER_VALUE,
        )
    };
    if hr != S_OK {
        return format!("sample failed: HRESULT 0x{hr:08x}");
    }
    unsafe {
        format!(
            "rip={:#x} rflags={:#x} tsc={} deadline={} intstate={:#x} pending={:#x}",
            values[0].0.Reg64,
            values[1].0.Reg64,
            values[2].0.Reg64,
            values[3].0.Reg64,
            values[4].0.Reg64,
            values[5].0.Reg64,
        )
    }
}

/// Debug aid: dump key xAPIC registers from a VP's interrupt-controller
/// state blob (ID, version, spurious/SW-enable, ICR, LVT timer, timer counts,
/// timer divisor, and whether any ISR/IRR bits are set).
pub fn debug_sample_apic(vm: &WhpVm, index: u32) -> String {
    let mut buf = vec![0u8; 4096];
    let mut written = 0u32;
    let hr = unsafe {
        WHvGetVirtualProcessorInterruptControllerState2(
            vm.partition_handle(),
            index,
            buf.as_mut_ptr() as *mut c_void,
            buf.len() as u32,
            &mut written,
        )
    };
    if hr != S_OK {
        return format!("apic read failed: HRESULT 0x{hr:08x}");
    }
    buf.truncate(written as usize);
    debug_parse_apic(&buf)
}

/// Debug aid: decode key xAPIC registers from a raw state blob.
pub fn debug_parse_apic(buf: &[u8]) -> String {
    let dw = |off: usize| -> u32 {
        if off + 4 <= buf.len() {
            u32::from_le_bytes(buf[off..off + 4].try_into().unwrap())
        } else {
            0
        }
    };
    let bits = |base: usize| -> u32 { (0..8).map(|i| dw(base + i * 0x10).count_ones()).sum() };
    format!(
        "len={} id={:#x} ver={:#x} tpr={:#x} svr={:#x} icr={:#x}:{:#x} lvt_timer={:#x} tmict={} tmcct={} tdcr={:#x} isr_bits={} irr_bits={}",
        buf.len(),
        dw(0x20),
        dw(0x30),
        dw(0x80),
        dw(0xf0),
        dw(0x310),
        dw(0x300),
        dw(0x320),
        dw(0x380),
        dw(0x390),
        dw(0x3e0),
        bits(0x100),
        bits(0x200),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpuid_0x15_ratio_is_accurate_and_linux_safe() {
        let measured_hz = 2_995_177_500;
        let (denominator, numerator, crystal_hz) = cpuid_0x15_values(measured_hz).unwrap();

        assert_eq!(
            (denominator, numerator, crystal_hz),
            (100, 11_981, 25_000_000)
        );
        let represented_hz = crystal_hz as u64 * numerator as u64 / denominator as u64;
        assert!(represented_hz.abs_diff(measured_hz) <= 125_000);
        assert!((crystal_hz / 1_000).checked_mul(numerator).is_some());
    }

    #[test]
    fn cpuid_0x15_ratio_covers_fast_realistic_tscs() {
        let (denominator, numerator, crystal_hz) = cpuid_0x15_values(6_000_000_000).unwrap();
        assert_eq!(
            (denominator, numerator, crystal_hz),
            (100, 24_000, 25_000_000)
        );
    }

    #[test]
    fn cpuid_0x15_ratio_rejects_unrepresentable_values() {
        assert!(cpuid_0x15_values(10_000).is_none());
        assert!(cpuid_0x15_values(u64::MAX).is_none());
    }
}
