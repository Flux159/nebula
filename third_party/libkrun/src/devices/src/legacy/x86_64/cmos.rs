// Copyright 2025 Red Hat, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::cmp::min;

use crate::bus::BusDevice;

const INDEX_MASK: u8 = 0x7f;
const INDEX_OFFSET: u64 = 0x0;
const DATA_OFFSET: u64 = 0x1;
const DATA_LEN: usize = 128;

pub struct Cmos {
    index: u8,
    data: [u8; DATA_LEN],
}

impl Cmos {
    pub fn new(mem_below_4g: u64, mem_above_4g: u64) -> Cmos {
        debug!("cmos: mem_below_4g={mem_below_4g} mem_above_4g={mem_above_4g}");

        let mut data = [0u8; DATA_LEN];

        // Extended memory from 16 MB to 4 GB in units of 64 KB
        let ext_mem = min(
            0xFFFF,
            mem_below_4g.saturating_sub(16 * 1024 * 1024) / (64 * 1024),
        );
        data[0x34] = ext_mem as u8;
        data[0x35] = (ext_mem >> 8) as u8;

        // High memory (> 4GB) in units of 64 KB
        let high_mem = min(0xFFFFFF, mem_above_4g / (64 * 1024));
        data[0x5b] = high_mem as u8;
        data[0x5c] = (high_mem >> 8) as u8;
        data[0x5d] = (high_mem >> 16) as u8;

        Cmos { index: 0, data }
    }
}

/// Pack a 0-99 value as BCD (the kernel reads the RTC in BCD mode unless
/// status register B says otherwise).
fn bcd(v: u64) -> u8 {
    (((v / 10) << 4) | (v % 10)) as u8
}

/// Civil date from a unix timestamp (Howard Hinnant's algorithm), plus
/// weekday (1 = Sunday per RTC convention) and time of day.
#[allow(clippy::many_single_char_names)]
fn civil_from_unix(secs: u64) -> (u64, u64, u64, u64, u64, u64, u64) {
    let days = secs / 86400;
    let tod = secs % 86400;
    let (hh, mm, ss) = (tod / 3600, (tod % 3600) / 60, tod % 60);
    // 1970-01-01 was a Thursday; RTC weekday is 1-based starting Sunday.
    let wd = (days + 4) % 7 + 1;

    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z % 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d, wd, hh, mm, ss)
}

impl Cmos {
    /// Live RTC registers (time/date/status); None for everything else.
    ///
    /// Without these the guest kernel reads zeros, decides the RTC is
    /// garbage, and boots with a 1999 wall clock — which breaks every TLS
    /// handshake (registry pulls). KVM guests never noticed because
    /// kvmclock supplies time; WHP has no such fallback.
    fn rtc_register(&self, index: u8) -> Option<u8> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_secs();
        let (y, mo, d, wd, hh, mm, ss) = civil_from_unix(now);
        match index {
            0x00 => Some(bcd(ss)),
            0x02 => Some(bcd(mm)),
            0x04 => Some(bcd(hh)),
            0x06 => Some(bcd(wd)),
            0x07 => Some(bcd(d)),
            0x08 => Some(bcd(mo)),
            0x09 => Some(bcd(y % 100)),
            // Alarm registers: never set.
            0x01 | 0x03 | 0x05 => Some(0),
            // Status A: UIP clear (time always consistent to read).
            0x0a => Some(0),
            // Status B: 24-hour mode, BCD encoding.
            0x0b => Some(0x02),
            // Status C/D: no pending interrupts; battery good.
            0x0c => Some(0),
            0x0d => Some(0x80),
            0x32 => Some(bcd(y / 100)),
            _ => None,
        }
    }
}

impl BusDevice for Cmos {
    fn read(&mut self, _vcpuid: u64, offset: u64, data: &mut [u8]) {
        if data.len() != 1 {
            error!("cmos: unsupported read length");
            return;
        }

        data[0] = match offset {
            INDEX_OFFSET => {
                debug!("cmos: read index offset");
                self.index
            }
            DATA_OFFSET => {
                let index = self.index & INDEX_MASK;
                debug!("cmos: read data offset from index={index:x}");
                self.rtc_register(index)
                    .unwrap_or(self.data[index as usize])
            }
            _ => {
                debug!("cmos: unsupported read offset");
                0
            }
        };
    }

    fn write(&mut self, _vcpuid: u64, offset: u64, data: &[u8]) {
        if data.len() != 1 {
            error!("cmos: unsupported write length");
            return;
        }

        match offset {
            INDEX_OFFSET => {
                debug!("cmos: update index");
                self.index = data[0] & INDEX_MASK;
            }
            _ => debug!("cmos: ignoring unsupported write to CMOS"),
        }
    }
}
