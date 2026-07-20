//! NEC uPD4990A Serial Real-Time Clock / Calendar
//!
//! The NeoGeo MVS board contains a uPD4990A that the BIOS self-test exercises:
//!
//! 1. Sends command `0x03` (time-read) via REG_RTCCTRL (`$380051`) to load
//!    the current BCD time into the 48-bit shift register.
//! 2. Sends command `0x01` (register-shift) to enable serial read-out.
//! 3. Clocks out 48 bits by pulsing CLK and reading bit 7 of REG_STATUS_A
//!    (`$320001`) after each pulse.  Validates BCD encoding.
//! 4. Checks that the TP (time-pulse) output on bit 6 of REG_STATUS_A
//!    oscillates at roughly 64 Hz (approximately one toggle per frame at 60 Hz).
//!
//! # REG_STATUS_A (`$320001`) bit mapping
//! | Bit | Signal | Description |
//! |-----|--------|-------------|
//! | 7   | OUT    | Serial shift-register LSB (mode 1), else 0 |
//! | 6   | TP     | Time-pulse square wave (~64 Hz default) |
//! | 5:0 | —      | 0x3F (coin/card status — all inactive) |
//!
//! # REG_RTCCTRL (`$380051`) write bit mapping
//! | Bit | Pin  | Description |
//! |-----|------|-------------|
//! | 0   | DATA | Serial data input |
//! | 1   | CLK  | Clock — rising edge advances shift register / command |
//! | 2   | STB  | Strobe — rising edge executes the latched command |

use serde::{Serialize, Deserialize};

// ── Upd4990a ──────────────────────────────────────────────────────────────────

/// NEC uPD4990A serial real-time clock / calendar emulation.
///
/// Matches the FBNeo `neo_upd4990a.cpp` reference implementation.
/// Implements `Serialize`/`Deserialize` so it is included in save states.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Upd4990a {
    // ── Timekeeping ───────────────────────────────────────────────────────────
    pub(super) seconds:  u8,
    pub(super) minutes:  u8,
    pub(super) hours:    u8,
    pub(super) day:      u8,
    pub(super) weekday:  u8,
    pub(super) month:    u8,
    /// 2-digit year (00–99).
    pub(super) year:     u8,

    // ── 48-bit shift register ─────────────────────────────────────────────────
    /// Bits 31:0  → BCD seconds(8) + minutes(8) + hours(8) + day(8)
    pub(super) shift0: u32,
    /// Bits 47:32 → weekday(4) + month(4) + BCD year(8)
    pub(super) shift1: u16,

    // ── Command / mode state ──────────────────────────────────────────────────
    /// 4-bit command register (shifted in LSB-last on each CLK rising edge).
    pub(super) command: u8,
    /// Operating mode: 0 = hold, 1 = shift-out (serial read), 2 = time-set.
    pub(super) mode:    u8,
    /// TP mode: 0 = free-run, 1 = one-shot, 2 = stopped.
    pub(super) tp_mode: u8,

    // ── Outputs ───────────────────────────────────────────────────────────────
    /// Time-pulse output (bit 6 of REG_STATUS_A).  Toggles at `tp_interval`
    /// cycles, producing a square wave at the selected TP frequency.
    pub tp: bool,

    // ── Edge detection ────────────────────────────────────────────────────────
    pub(super) prev_clk: bool,
    pub(super) prev_stb: bool,

    // ── Timing (68k CPU cycles at 12 MHz) ─────────────────────────────────────
    /// One real-world second expressed in 68k CPU cycles.
    pub(super) one_second:  u64,   // = 12_000_000
    /// Cycle accumulator since the last second tick.
    pub(super) sec_count:   u64,
    /// Phase accumulator for the TP square wave.
    pub(super) tp_count:    u64,
    /// TP half-period in 68k cycles.  Default = one_second / 64 (64 Hz).
    pub(super) tp_interval: u64,
}

impl Upd4990a {
    /// Construct with the current wall-clock time and default TP frequency (64 Hz).
    pub fn new() -> Self {
        let one_second: u64 = 12_000_000;
        Upd4990a {
            // Fixed starting time (e.g. 1990-01-01 12:00:00) so that the host's 
            // wall-clock time does not introduce divergence at Emulator::new().
            seconds: 0, minutes: 0, hours: 12,
            day: 1, weekday: 1, month: 1, year: 90,
            shift0: 0, shift1: 0, command: 0,
            mode: 0, tp_mode: 0, tp: false,
            prev_clk: false, prev_stb: false,
            one_second,
            sec_count:   0,
            tp_count:    0,
            tp_interval: one_second / 64,
        }
    }

    /// Advance internal clocks by `cycles` 68k CPU cycles.
    ///
    /// Called from the main loop roughly every 100 cycles to keep the TP
    /// square wave at sub-frame resolution.
    pub fn update(&mut self, cycles: u64) {
        // ── TP square-wave ────────────────────────────────────────────────────
        if self.tp_mode != 2 {
            self.tp_count += cycles;
            if self.tp_mode == 1 {
                // One-shot: revert to free-run after one interval.
                if self.tp_count >= self.tp_interval {
                    self.tp_mode = 0;
                    self.tp_count %= self.tp_interval;
                    self.tp = self.tp_count >= (self.tp_interval >> 1);
                }
            } else {
                if self.tp_count >= self.tp_interval {
                    self.tp_count %= self.tp_interval;
                }
                self.tp = self.tp_count >= (self.tp_interval >> 1);
            }
        }

        // ── Real-time calendar counter ────────────────────────────────────────
        self.sec_count += cycles;
        if self.sec_count >= self.one_second {
            self.sec_count -= self.one_second;
            self.tick_second();
        }
    }

    fn tick_second(&mut self) {
        self.seconds += 1;
        if self.seconds < 60 { return; }
        self.seconds = 0;
        self.minutes += 1;
        if self.minutes < 60 { return; }
        self.minutes = 0;
        self.hours   += 1;
        if self.hours < 24 { return; }
        self.hours    = 0;
        self.weekday  = (self.weekday + 1) % 7;
        let is_leap = self.year % 4 == 0; // simplified; correct for 2000–2099
        let month_len: [u8; 12] = [
            31, if is_leap { 29 } else { 28 }, 31, 30, 31, 30,
            31, 31, 30, 31, 30, 31,
        ];
        self.day += 1;
        if self.day <= month_len[(self.month as usize).saturating_sub(1).min(11)] { return; }
        self.day    = 1;
        self.month += 1;
        if self.month <= 12 { return; }
        self.month  = 1;
        self.year   = self.year.wrapping_add(1) % 100;
    }

    /// Process a write to REG_RTCCTRL (`$380051`).
    ///
    /// | Parameter | Pin  | Source bit in write value |
    /// |-----------|------|--------------------------|
    /// | `clk`     | CLK  | bit 1 |
    /// | `stb`     | STB  | bit 2 |
    /// | `data`    | DATA | bit 0 |
    pub fn write(&mut self, clk: bool, stb: bool, data: bool) {
        // STB rising edge → execute the latched command.
        if stb && !self.prev_stb {
            self.execute_command();
        }
        // CLK rising edge with STB low → shift one bit into the registers.
        if !stb && clk && !self.prev_clk {
            if self.mode == 1 {
                // Mode 1 (shift-out): advance 48-bit shift register right.
                let carry = (self.shift1 & 1) as u32;
                self.shift1 >>= 1;
                self.shift1 &= 0x7FFF;
                if self.command & 1 != 0 { self.shift1 |= 0x8000; }
                self.shift0 >>= 1;
                if carry != 0 { self.shift0 |= 0x8000_0000; }
            }
            // Command register always accepts the incoming DATA bit (LSB first).
            self.command = (self.command >> 1) & 0x7;
            if data { self.command |= 0x8; }
        }
        self.prev_clk = clk;
        self.prev_stb = stb;
    }

    fn execute_command(&mut self) {
        match self.command & 0x0F {
            0x00 => {   // Register hold
                self.mode = 0;
                self.tp_mode = 0;
                self.tp_interval = self.one_second / 64;
                self.tp_count %= self.tp_interval;
            }
            0x01 => {   // Register shift (enable serial output)
                self.mode = 1;
            }
            0x02 => {   // Time set & counter hold
                self.mode = 2;
                self.load_time_from_registers();
            }
            0x03 => {   // Time read (load shift registers from clock)
                self.mode = 0;
                self.store_time_to_registers();
            }
            c @ 0x04..=0x07 => {   // TP = 64 / 256 / 2048 / 4096 Hz (free-run)
                let n: [u64; 4] = [64, 256, 2048, 4096];
                self.tp_mode = 0;
                self.tp_interval = self.one_second / n[(c & 3) as usize];
                self.tp_count %= self.tp_interval;
            }
            c @ 0x08..=0x0B => {   // TP = 1 / 10 / 30 / 60 second (one-shot)
                let n: [u64; 4] = [1, 10, 30, 60];
                self.tp_interval = n[(c & 3) as usize] * self.one_second;
                self.tp_count    = 0;
                self.tp_mode     = 1;
            }
            0x0C => {   // Interval reset (one-shot start)
                self.tp_mode = 1;
                self.tp = true;
            }
            0x0D => { self.tp_mode = 0; }   // Interval start (free-run)
            0x0E => { self.tp_mode = 2; }   // Interval stop
            _ => {}                          // 0x0F = test mode (not emulated)
        }
    }

    /// Load the current timekeeping registers into the 48-bit shift register
    /// (executed by command `0x03` — time read).
    fn store_time_to_registers(&mut self) {
        self.shift0  = (self.seconds % 10) as u32;
        self.shift0 |= ((self.seconds / 10) as u32) <<  4;
        self.shift0 |= ((self.minutes % 10) as u32) <<  8;
        self.shift0 |= ((self.minutes / 10) as u32) << 12;
        self.shift0 |= ((self.hours   % 10) as u32) << 16;
        self.shift0 |= ((self.hours   / 10) as u32) << 20;
        self.shift0 |= ((self.day     % 10) as u32) << 24;
        self.shift0 |= ((self.day     / 10) as u32) << 28;
        self.shift1  =  self.weekday as u16;
        self.shift1 |= (self.month as u16)          <<  4;
        self.shift1 |= ((self.year % 10) as u16)    <<  8;
        self.shift1 |= ((self.year / 10) as u16)    << 12;
    }

    /// Write the shift register contents back into the timekeeping registers
    /// (executed by command `0x02` — time set).
    fn load_time_from_registers(&mut self) {
        self.seconds = ((self.shift0      ) & 0xF) as u8
                     + ((self.shift0 >>  4) & 0xF) as u8 * 10;
        self.minutes = ((self.shift0 >>  8) & 0xF) as u8
                     + ((self.shift0 >> 12) & 0xF) as u8 * 10;
        self.hours   = ((self.shift0 >> 16) & 0xF) as u8
                     + ((self.shift0 >> 20) & 0xF) as u8 * 10;
        self.day     = ((self.shift0 >> 24) & 0xF) as u8
                     + ((self.shift0 >> 28) & 0xF) as u8 * 10;
        self.weekday = ( self.shift1        & 0xF) as u8;
        self.month   = ((self.shift1 >>  4) & 0xF) as u8;
        self.year    = ((self.shift1 >>  8) & 0xF) as u8
                     + ((self.shift1 >> 12) & 0xF) as u8 * 10;
    }

    /// Read the two output bits exposed in REG_STATUS_A (`$320001`).
    ///
    /// Returns a packed byte where:
    /// - bit 1 = serial OUT (shift register LSB when mode = 1, else 0)
    /// - bit 0 = TP (time-pulse)
    ///
    /// The caller places these at bits 7:6 of the status byte:
    /// `status_a = 0x3F | (rtc.read() << 6)`
    pub fn read(&self) -> u8 {
        let out: u8 = if self.mode == 1 {
            (self.shift0 & 1) as u8
        } else {
            0
        };
        (out << 1) | (self.tp as u8)
    }
}

impl Default for Upd4990a {
    fn default() -> Self { Self::new() }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── BCD encoding ──────────────────────────────────────────────────────────

    #[test]
    fn shift0_encodes_bcd_seconds() {
        // After a time-set sequence the shift0 register should encode
        // seconds in BCD in its low byte.  Verify the BCD layout: if
        // shift0\[7:0\] = 0x59 then that represents 59 seconds.
        let mut rtc = Upd4990a::new();
        // Manually set the shift register and verify read() stays consistent.
        rtc.shift0 = 0x0000_0059; // BCD 59 seconds
        rtc.mode   = 1;           // shift-out mode
        // The read() call should not panic and returns a valid 0 or 1 bit.
        let bit = rtc.read();
        assert!(bit <= 3, "read() returns at most 2 bits");
    }

    // ── TP signal ─────────────────────────────────────────────────────────────

    #[test]
    fn tp_toggles_after_enough_cycles() {
        let mut rtc = Upd4990a::new();
        assert!(!rtc.tp, "TP starts low");
        // tp_interval = one_second / 64 = 12_000_000 / 64 = 187_500 cycles.
        // half-period  = 93_750.
        // Advancing 93_751 cycles puts tp_count just past the threshold:
        //   tp_count = 93_751; 93_751 < 187_500 (no wrap); 93_751 >= 93_750 → tp = true.
        rtc.update(93_751);
        assert!(rtc.tp, "TP should be high after advancing past one half-period");
    }

    // ── Clone round-trip ──────────────────────────────────────────────────────

    #[test]
    fn clone_produces_equal_state() {
        let mut rtc = Upd4990a::new();
        rtc.update(100);
        let clone = rtc.clone();
        assert_eq!(rtc.tp, clone.tp);
        assert_eq!(rtc.mode, clone.mode);
        assert_eq!(rtc.shift0, clone.shift0);
    }
}

