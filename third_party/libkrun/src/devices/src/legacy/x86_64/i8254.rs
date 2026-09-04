// Copyright 2026 Red Hat, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Minimal i8254 PIT support for Windows Hypervisor Platform.
//!
//! Windows 10 WHP does not provide a guest PIT. Linux on AMD needs PIT
//! channel 2 to calibrate the TSC before it can set up the local APIC timer.
//! Channel 0 supplies the early periodic timer through the companion i8259.

use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use crate::bus::BusDevice;

use super::i8259::I8259Pin;

const PIT_TICK_RATE: u128 = 1_193_182;

const CHANNEL_2_OFFSET: u64 = 2;
const COMMAND_OFFSET: u64 = 3;

const ACCESS_LATCH: u8 = 0;
const ACCESS_LOW: u8 = 1;
const ACCESS_HIGH: u8 = 2;
const ACCESS_LOW_HIGH: u8 = 3;

#[derive(Debug)]
struct Channel0 {
    access: u8,
    mode: u8,
    reload: u32,
    pending_low: Option<u8>,
    generation: u64,
}

impl Default for Channel0 {
    fn default() -> Self {
        Self {
            access: ACCESS_LOW_HIGH,
            mode: 0,
            reload: 0,
            pending_low: None,
            generation: 0,
        }
    }
}

impl Channel0 {
    fn program(&mut self, command: u8) {
        let access = (command >> 4) & 0x3;
        if access == ACCESS_LATCH {
            return;
        }

        let raw_mode = (command >> 1) & 0x7;
        self.mode = match raw_mode {
            6 => 2,
            7 => 3,
            mode => mode,
        };
        self.access = access;
        self.reload = 0;
        self.pending_low = None;
        self.generation = self.generation.wrapping_add(1);
        trace!(
            "i8254: programmed channel 0 access={access} mode={}",
            self.mode
        );
    }

    fn load(&mut self, value: u16) {
        self.reload = if value == 0 { 65_536 } else { value as u32 };
        self.generation = self.generation.wrapping_add(1);
        trace!("i8254: loaded channel 0 count={}", self.reload);
    }

    fn write_count_byte(&mut self, value: u8) -> bool {
        match self.access {
            ACCESS_LOW => self.load(value as u16),
            ACCESS_HIGH => self.load((value as u16) << 8),
            ACCESS_LOW_HIGH => {
                if let Some(low) = self.pending_low.take() {
                    self.load(u16::from_le_bytes([low, value]));
                } else {
                    self.pending_low = Some(value);
                    return false;
                }
            }
            _ => return false,
        }
        true
    }
}

#[derive(Debug)]
struct Channel2 {
    access: u8,
    mode: u8,
    reload: u32,
    pending_low: Option<u8>,
    read_high_next: bool,
    latched_count: Option<u16>,
    started_at: Option<Instant>,
    elapsed_before_start: u64,
    gate: bool,
    programmed: bool,
}

impl Default for Channel2 {
    fn default() -> Self {
        Self {
            access: ACCESS_LOW_HIGH,
            mode: 0,
            reload: 0,
            pending_low: None,
            read_high_next: false,
            latched_count: None,
            started_at: None,
            elapsed_before_start: 0,
            gate: false,
            programmed: false,
        }
    }
}

impl Channel2 {
    fn elapsed_ticks_at(&self, now: Instant) -> u64 {
        let running_ticks = self.started_at.map_or(0, |start| {
            now.saturating_duration_since(start)
                .as_nanos()
                .saturating_mul(PIT_TICK_RATE)
                .checked_div(1_000_000_000)
                .unwrap_or(0) as u64
        });
        self.elapsed_before_start.saturating_add(running_ticks)
    }

    fn current_count_at(&self, now: Instant) -> u16 {
        if self.reload == 0 {
            return 0;
        }

        let elapsed = self.elapsed_ticks_at(now);
        if elapsed >= self.reload as u64 {
            0
        } else {
            (self.reload as u64 - elapsed) as u16
        }
    }

    fn output_at(&self, now: Instant) -> bool {
        if !self.programmed {
            return true;
        }

        // Linux calibrates with mode 0. Treat unsupported modes as inactive
        // high rather than leaving early boot in an endless polling loop.
        self.mode != 0 || (self.reload != 0 && self.elapsed_ticks_at(now) >= self.reload as u64)
    }

    fn set_gate(&mut self, enabled: bool, now: Instant) {
        if self.gate == enabled {
            return;
        }

        if enabled {
            if self.reload != 0 && self.elapsed_before_start < self.reload as u64 {
                self.started_at = Some(now);
            }
        } else if self.started_at.is_some() {
            self.elapsed_before_start = self.elapsed_ticks_at(now);
            self.started_at = None;
        }
        self.gate = enabled;
    }

    fn program(&mut self, command: u8) {
        let access = (command >> 4) & 0x3;
        if access == ACCESS_LATCH {
            if self.latched_count.is_none() {
                self.latched_count = Some(self.current_count_at(Instant::now()));
                self.read_high_next = false;
            }
            return;
        }

        let raw_mode = (command >> 1) & 0x7;
        self.mode = match raw_mode {
            6 => 2,
            7 => 3,
            mode => mode,
        };
        self.access = access;
        self.reload = 0;
        self.pending_low = None;
        self.read_high_next = false;
        self.latched_count = None;
        self.started_at = None;
        self.elapsed_before_start = 0;
        self.programmed = true;
        trace!(
            "i8254: programmed channel 2 access={access} mode={}",
            self.mode
        );
    }

    fn load(&mut self, value: u16) {
        self.reload = if value == 0 { 65_536 } else { value as u32 };
        self.elapsed_before_start = 0;
        self.started_at = self.gate.then(Instant::now);
        self.latched_count = None;
        trace!("i8254: loaded channel 2 count={}", self.reload);
    }

    fn write_count_byte(&mut self, value: u8) {
        match self.access {
            ACCESS_LOW => self.load(value as u16),
            ACCESS_HIGH => self.load((value as u16) << 8),
            ACCESS_LOW_HIGH => {
                if let Some(low) = self.pending_low.take() {
                    self.load(u16::from_le_bytes([low, value]));
                } else {
                    self.pending_low = Some(value);
                }
            }
            _ => {}
        }
    }

    fn read_count_byte(&mut self) -> u8 {
        let count = self
            .latched_count
            .unwrap_or_else(|| self.current_count_at(Instant::now()));
        match self.access {
            ACCESS_LOW => count as u8,
            ACCESS_HIGH => (count >> 8) as u8,
            ACCESS_LOW_HIGH => {
                let high = self.read_high_next;
                self.read_high_next = !self.read_high_next;
                if high {
                    self.latched_count = None;
                    (count >> 8) as u8
                } else {
                    count as u8
                }
            }
            _ => 0,
        }
    }
}

struct PitState {
    channel0: Channel0,
    channel2: Channel2,
    speaker_control: u8,
    timer_irq: I8259Pin,
}

/// PIT counter and command ports (`0x40` through `0x43`).
pub struct I8254 {
    state: Arc<Mutex<PitState>>,
}

/// System-control port B (`0x61`), including PIT channel 2 gate and output.
pub struct I8254Speaker {
    state: Arc<Mutex<PitState>>,
}

impl I8254 {
    pub fn new(timer_irq: I8259Pin) -> (Self, I8254Speaker) {
        let state = Arc::new(Mutex::new(PitState {
            channel0: Channel0::default(),
            channel2: Channel2::default(),
            speaker_control: 0,
            timer_irq,
        }));
        (
            Self {
                state: state.clone(),
            },
            I8254Speaker { state },
        )
    }

    fn arm_channel0(&self) {
        let state = Arc::downgrade(&self.state);
        let (generation, period, periodic) = {
            let state = self.state.lock().unwrap();
            if state.channel0.reload == 0 {
                return;
            }
            let nanos = ((state.channel0.reload as u128 * 1_000_000_000)
                .saturating_add(PIT_TICK_RATE - 1)
                / PIT_TICK_RATE) as u64;
            (
                state.channel0.generation,
                Duration::from_nanos(nanos.max(1)),
                matches!(state.channel0.mode, 2 | 3),
            )
        };

        std::thread::spawn(move || run_channel0(state, generation, period, periodic));
    }
}

fn run_channel0(state: Weak<Mutex<PitState>>, generation: u64, period: Duration, periodic: bool) {
    loop {
        std::thread::sleep(period);

        let Some(state) = state.upgrade() else {
            return;
        };
        let timer_irq = {
            let state = state.lock().unwrap();
            if state.channel0.generation != generation {
                return;
            }
            state.timer_irq.clone()
        };
        timer_irq.pulse(0);
        if !periodic {
            return;
        }
    }
}

impl BusDevice for I8254 {
    fn read(&mut self, _vcpuid: u64, offset: u64, data: &mut [u8]) {
        if data.len() != 1 {
            return;
        }

        data[0] = if offset == CHANNEL_2_OFFSET {
            self.state.lock().unwrap().channel2.read_count_byte()
        } else {
            0
        };
    }

    fn write(&mut self, _vcpuid: u64, offset: u64, data: &[u8]) {
        if data.len() != 1 {
            return;
        }

        let arm_channel0 = {
            let mut state = self.state.lock().unwrap();
            match offset {
                0 => state.channel0.write_count_byte(data[0]),
                CHANNEL_2_OFFSET => {
                    state.channel2.write_count_byte(data[0]);
                    false
                }
                COMMAND_OFFSET => match data[0] >> 6 {
                    0 => {
                        state.channel0.program(data[0]);
                        false
                    }
                    2 => {
                        state.channel2.program(data[0]);
                        false
                    }
                    _ => false,
                },
                _ => false,
            }
        };
        if arm_channel0 {
            self.arm_channel0();
        }
    }
}

impl BusDevice for I8254Speaker {
    fn read(&mut self, _vcpuid: u64, _offset: u64, data: &mut [u8]) {
        if data.len() != 1 {
            return;
        }

        let state = self.state.lock().unwrap();
        data[0] = state.speaker_control
            | if state.channel2.output_at(Instant::now()) {
                1 << 5
            } else {
                0
            };
    }

    fn write(&mut self, _vcpuid: u64, _offset: u64, data: &[u8]) {
        if data.len() != 1 {
            return;
        }

        let mut state = self.state.lock().unwrap();
        state.speaker_control = data[0] & 0x3;
        state.channel2.set_gate(data[0] & 0x1 != 0, Instant::now());
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn program_mode0(pit: &mut I8254, speaker: &mut I8254Speaker, count: u16) {
        speaker.write(0, 0, &[1]);
        pit.write(0, COMMAND_OFFSET, &[0xb0]);
        pit.write(0, CHANNEL_2_OFFSET, &[count as u8]);
        pit.write(0, CHANNEL_2_OFFSET, &[(count >> 8) as u8]);
    }

    fn new_pit() -> (I8254, I8254Speaker) {
        let (_, _, timer_irq) = super::super::i8259::I8259::new();
        I8254::new(timer_irq)
    }

    #[test]
    fn channel2_loads_low_then_high_and_starts() {
        let (mut pit, mut speaker) = new_pit();
        program_mode0(&mut pit, &mut speaker, 0x12a5);

        let state = pit.state.lock().unwrap();
        assert_eq!(state.channel2.reload, 0x12a5);
        assert!(state.channel2.started_at.is_some());
        assert!(!state.channel2.output_at(Instant::now()));
    }

    #[test]
    fn channel2_output_goes_high_at_terminal_count() {
        let (mut pit, mut speaker) = new_pit();
        program_mode0(&mut pit, &mut speaker, 1_193);
        pit.state.lock().unwrap().channel2.started_at =
            Some(Instant::now() - Duration::from_millis(2));

        let mut value = [0];
        speaker.read(0, 0, &mut value);
        assert_ne!(value[0] & (1 << 5), 0);
    }

    #[test]
    fn channel2_gate_pauses_countdown() {
        let (mut pit, mut speaker) = new_pit();
        program_mode0(&mut pit, &mut speaker, 60_000);
        pit.state.lock().unwrap().channel2.started_at =
            Some(Instant::now() - Duration::from_millis(2));
        speaker.write(0, 0, &[0]);

        let state = pit.state.lock().unwrap();
        assert!(state.channel2.started_at.is_none());
        assert!(state.channel2.elapsed_before_start >= 2_000);
    }

    #[test]
    fn channel0_loads_periodic_timer() {
        let (mut pit, _) = new_pit();
        pit.write(0, COMMAND_OFFSET, &[0x34]);
        pit.write(0, 0, &[0xa9]);
        pit.write(0, 0, &[0x12]);

        let state = pit.state.lock().unwrap();
        assert_eq!(state.channel0.mode, 2);
        assert_eq!(state.channel0.reload, 0x12a9);
    }
}
