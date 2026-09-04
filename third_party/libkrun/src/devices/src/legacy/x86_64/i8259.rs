// Copyright 2026 Red Hat, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Minimal dual i8259 PIC support for Windows Hypervisor Platform.
//!
//! WHP emulates the local APIC, but Windows 10 does not provide the legacy
//! PIC/PIT path used by Linux during early boot. This device implements the
//! initialization and interrupt-mask registers needed to route PIT IRQ0.

use std::sync::{Arc, Mutex};

use crate::bus::BusDevice;
use crate::legacy::IrqChip;
use whp::{InterruptDestinationMode, InterruptRequest, InterruptTriggerMode, InterruptType, WhpVm};

const ICW1_INIT: u8 = 1 << 4;
const ICW1_SINGLE: u8 = 1 << 1;
const ICW1_ICW4: u8 = 1;

#[derive(Debug)]
struct Controller {
    mask: u8,
    vector_base: u8,
    init_step: u8,
    expect_icw4: bool,
    single: bool,
}

impl Controller {
    fn new(vector_base: u8) -> Self {
        Self {
            mask: u8::MAX,
            vector_base,
            init_step: 0,
            expect_icw4: false,
            single: false,
        }
    }

    fn write_command(&mut self, value: u8) {
        if value & ICW1_INIT != 0 {
            self.init_step = 1;
            self.expect_icw4 = value & ICW1_ICW4 != 0;
            self.single = value & ICW1_SINGLE != 0;
            return;
        }

        // OCW2 EOI and OCW3 register-selection commands need no state while
        // all supported inputs are edge-triggered and forwarded immediately.
    }

    fn write_data(&mut self, value: u8) {
        match self.init_step {
            1 => {
                self.vector_base = value & 0xf8;
                self.init_step = if self.single {
                    if self.expect_icw4 { 3 } else { 0 }
                } else {
                    2
                };
            }
            2 => {
                self.init_step = if self.expect_icw4 { 3 } else { 0 };
            }
            3 => self.init_step = 0,
            _ => self.mask = value,
        }
    }
}

struct PicState {
    controllers: [Controller; 2],
    vm: Option<Arc<WhpVm>>,
    ioapic: Option<IrqChip>,
}

impl Default for PicState {
    fn default() -> Self {
        Self {
            controllers: [Controller::new(0x20), Controller::new(0x28)],
            vm: None,
            ioapic: None,
        }
    }
}

/// One of the two i8259 command/data-port pairs.
pub struct I8259 {
    state: Arc<Mutex<PicState>>,
    controller: usize,
}

/// Interrupt input shared with legacy devices such as the PIT.
#[derive(Clone)]
pub struct I8259Pin {
    state: Arc<Mutex<PicState>>,
}

impl I8259 {
    pub fn new() -> (Self, Self, I8259Pin) {
        let state = Arc::new(Mutex::new(PicState::default()));
        (
            Self {
                state: state.clone(),
                controller: 0,
            },
            Self {
                state: state.clone(),
                controller: 1,
            },
            I8259Pin { state },
        )
    }

    pub fn set_vm(&mut self, vm: Arc<WhpVm>) {
        self.state.lock().unwrap().vm = Some(vm);
    }

    pub fn set_ioapic(&mut self, ioapic: IrqChip) {
        self.state.lock().unwrap().ioapic = Some(ioapic);
    }
}

impl I8259Pin {
    pub fn pulse(&self, irq: u8) {
        if irq >= 16 {
            return;
        }

        let (vm, vector, pic_unmasked, ioapic) = {
            let state = self.state.lock().unwrap();
            let (controller, input) = if irq < 8 {
                (&state.controllers[0], irq)
            } else {
                (&state.controllers[1], irq - 8)
            };
            (
                state.vm.clone(),
                controller.vector_base.wrapping_add(input) as u32,
                controller.mask & (1 << input) == 0,
                state.ioapic.clone(),
            )
        };

        let ioapic_unmasked = ioapic
            .as_ref()
            .is_some_and(|ioapic| !ioapic.lock().unwrap().irq_is_masked(irq as u32));

        // WHP has no userspace ExtINT/PIC input. Deliver the PIT's vector
        // directly whenever either legacy route is open; Linux assigns the
        // same IRQ vector to the PIC and IOAPIC during early timer setup.
        if (pic_unmasked || ioapic_unmasked)
            && let Some(vm) = vm
        {
            let request = InterruptRequest {
                interrupt_type: InterruptType::Fixed,
                destination_mode: InterruptDestinationMode::Physical,
                trigger_mode: InterruptTriggerMode::Edge,
                destination: 0,
                vector,
            };
            if let Err(e) = vm.request_interrupt(&request) {
                warn!("i8259: failed to inject IRQ {irq} as vector 0x{vector:02x}: {e}");
            } else {
                vm.cancel_vcpu(0);
            }
        }
    }
}

impl BusDevice for I8259 {
    fn read(&mut self, _vcpuid: u64, offset: u64, data: &mut [u8]) {
        if data.len() != 1 {
            return;
        }

        let state = self.state.lock().unwrap();
        let controller = &state.controllers[self.controller];
        data[0] = if offset == 1 {
            controller.mask
        } else {
            // No interrupt remains pending in the emulated PIC because edge
            // inputs are forwarded immediately to WHP's LAPIC.
            0
        };
    }

    fn write(&mut self, vcpuid: u64, offset: u64, data: &[u8]) {
        if data.len() != 1 {
            return;
        }

        let (vm, eoi_vector) = {
            let mut state = self.state.lock().unwrap();
            let vm = state.vm.clone();
            let controller = &mut state.controllers[self.controller];
            let eoi_vector = if offset == 0 && data[0] & 0x20 != 0 {
                let input = if data[0] & 0x40 != 0 {
                    data[0] & 0x07
                } else {
                    0
                };
                Some(controller.vector_base.wrapping_add(input))
            } else {
                None
            };
            if offset == 0 {
                controller.write_command(data[0]);
            } else if offset == 1 {
                controller.write_data(data[0]);
            }
            trace!(
                "i8259: {} write port={} value=0x{:02x} mask=0x{:02x} base=0x{:02x}",
                if self.controller == 0 {
                    "master"
                } else {
                    "slave"
                },
                offset,
                data[0],
                controller.mask,
                controller.vector_base
            );
            (vm, eoi_vector)
        };

        if let (Some(vm), Some(vector)) = (vm, eoi_vector)
            && let Err(e) = vm.clear_interrupt_in_service(vcpuid as u32, vector)
        {
            warn!("i8259: failed to acknowledge vector 0x{vector:02x}: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialization_sets_vector_and_mask() {
        let (mut master, _, _) = I8259::new();
        master.write(0, 0, &[0x11]);
        master.write(0, 1, &[0x30]);
        master.write(0, 1, &[0x04]);
        master.write(0, 1, &[0x01]);
        master.write(0, 1, &[0xfe]);

        let state = master.state.lock().unwrap();
        assert_eq!(state.controllers[0].vector_base, 0x30);
        assert_eq!(state.controllers[0].mask, 0xfe);
        assert_eq!(state.controllers[0].init_step, 0);
    }

    #[test]
    fn data_port_reads_current_mask() {
        let (mut master, _, _) = I8259::new();
        master.write(0, 1, &[0xfb]);
        let mut value = [0];
        master.read(0, 1, &mut value);
        assert_eq!(value[0], 0xfb);
    }
}
