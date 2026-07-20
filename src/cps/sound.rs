//! CPS1 audio subsystem.
//!
//! * Z80 sound CPU (via the `iz80` crate) running the CPS1 sound driver.
//! * OKI MSM6295 ADPCM sample player (pure Rust) — drives sound effects/voices.
//! * YM2151 (OPM) FM synthesiser — the vendored `ymfm` OPM core (via
//!   `src/ym2151_glue.cpp`), stepped in lockstep with the Z80 so its timer
//!   IRQs drive the sound driver.  Provides the FM music and most SFX.
//!
//! Z80 memory map (MAME `sub_map`):
//!
//! | Range         | Function                          |
//! |---------------|-----------------------------------|
//! | `0000–7FFF`   | Sound ROM (fixed)                 |
//! | `8000–BFFF`   | Sound ROM (banked, 0x4000 window) |
//! | `D000–D7FF`   | Work RAM                          |
//! | `F000–F001`   | YM2151                            |
//! | `F002`        | OKI MSM6295                       |
//! | `F004`        | ROM bank select                   |
//! | `F006`        | OKI pin 7 (sample rate)           |
//! | `F008`        | Sound-command latch (read)        |

use iz80::{Cpu, Machine};

const Z80_CLOCK: u32 = 3_579_545;
const OUTPUT_RATE: u32 = crate::AUDIO_SAMPLE_RATE;

// ── OKI MSM6295 ──────────────────────────────────────────────────────────────

const ADPCM_STEP_TABLE: [i32; 49] = [
    16, 17, 19, 21, 23, 25, 28, 31, 34, 37, 41, 45, 50, 55, 60, 66, 73, 80, 88, 97, 107, 118, 130,
    143, 157, 173, 190, 209, 230, 253, 279, 307, 337, 371, 408, 449, 494, 544, 598, 658, 724, 796,
    876, 963, 1060, 1166, 1282, 1411, 1552,
];
const ADPCM_INDEX_SHIFT: [i32; 8] = [-1, -1, -1, -1, 2, 4, 6, 8];

#[derive(Clone, Copy)]
struct OkiVoice {
    playing: bool,
    position: u32, // nibble position (ROM byte*2)
    stop: u32,     // nibble position end
    signal: i32,
    step: i32,
    volume: i32,
    phase: u32, // fractional resampling accumulator
}

impl OkiVoice {
    const fn new() -> Self {
        OkiVoice {
            playing: false,
            position: 0,
            stop: 0,
            signal: 0,
            step: 0,
            volume: 8,
            phase: 0,
        }
    }
}

struct Oki {
    rom: Vec<u8>,
    voices: [OkiVoice; 4],
    command: i32,
    rate: u32, // effective sample rate (pin7)
    /// Transient decimated output ring — **NOT** part of the save state (the
    /// `snapshot`/`restore` path copies fields explicitly and omits this).
    /// Filled deterministically once per frame by `advance` and drained by
    /// `CpsSound::generate`; keeping sample generation out of the checksummed
    /// path is what makes rollback catch-up frames leave identical OKI voice
    /// state (mirrors the YM2151 ring model in `ym2151_glue.cpp`).
    ring: std::collections::VecDeque<f32>,
}

// OKI volume attenuation (approx. -1.5 dB per step, index 0..8).
const OKI_VOL: [i32; 9] = [32, 27, 22, 18, 14, 11, 8, 6, 4];

impl Oki {
    fn new() -> Self {
        Oki {
            rom: Vec::new(),
            voices: [OkiVoice::new(); 4],
            command: -1,
            rate: 7575, // pin7 = high, 1 MHz / 132
            ring: std::collections::VecDeque::new(),
        }
    }

    /// Maximum buffered output samples before the oldest are dropped.  Rollback
    /// clears the ring on restore, so this only bounds growth across a single
    /// prediction-window catch-up burst.
    const RING_CAP: usize = crate::NOMINAL_SAMPLES_PER_FRAME * 16;

    /// Clock the OKI for `n` output-rate samples, buffering the decimated
    /// result.  Called once per frame from the deterministic sound step so the
    /// voice state advances identically on display and rollback catch-up frames.
    fn advance(&mut self, n: usize) {
        for _ in 0..n {
            let s = self.next_sample();
            if self.ring.len() >= Self::RING_CAP {
                self.ring.pop_front();
            }
            self.ring.push_back(s);
        }
    }

    /// Drain one buffered output sample (silence on underflow).
    fn pop_sample(&mut self) -> f32 {
        self.ring.pop_front().unwrap_or(0.0)
    }

    /// Discard buffered output — called on rollback restore, as the samples
    /// belong to a mispredicted timeline and are not part of deterministic state.
    fn clear_ring(&mut self) {
        self.ring.clear();
    }

    fn read_byte(&self, addr: u32) -> u8 {
        self.rom.get(addr as usize).copied().unwrap_or(0)
    }

    fn set_pin7(&mut self, high: bool) {
        self.rate = if high { 7575 } else { 6060 };
    }

    fn write_command(&mut self, data: u8) {
        // MSM6295 two-write protocol.  Whether a byte is a phrase-select or a
        // key-on/off byte is determined by internal STATE (are we armed with a
        // pending phrase?), NOT by bit 7 of the data — bit 7 is the channel-3
        // select bit in the key-on byte.  Deciding on `data & 0x80` (as before)
        // mis-read every channel-3 trigger as a new phrase latch and silently
        // dropped it, so SFX the driver routed to voice 3 (e.g. normal-attack
        // impacts) never played while voices 0–2 (announcer/specials) did.
        if self.command >= 0 {
            // Second (key-on) byte: bits 4–7 select the voices to start,
            // bits 0–3 are the attenuation.
            let cmd = self.command as u32;
            for v in 0..4 {
                if data & (0x10 << v) != 0 && !self.rom.is_empty() {
                    let base = cmd * 8;
                    let start = ((self.read_byte(base) as u32) << 16)
                        | ((self.read_byte(base + 1) as u32) << 8)
                        | self.read_byte(base + 2) as u32;
                    let stop = ((self.read_byte(base + 3) as u32) << 16)
                        | ((self.read_byte(base + 4) as u32) << 8)
                        | self.read_byte(base + 5) as u32;
                    let voice = &mut self.voices[v];
                    voice.playing = start < stop;
                    voice.position = start * 2;
                    voice.stop = stop * 2;
                    voice.signal = 0;
                    voice.step = 0;
                    voice.volume = OKI_VOL[(data & 0x0f).min(8) as usize];
                    voice.phase = 0;
                }
            }
            self.command = -1;
        } else if data & 0x80 != 0 {
            // First byte: latch the phrase (sample) number.
            self.command = (data & 0x7f) as i32;
        } else {
            // Key-off byte (no phrase armed): bits 4–7 select voices to halt.
            for v in 0..4 {
                if data & (0x10 << v) != 0 {
                    self.voices[v].playing = false;
                }
            }
        }
    }

    fn decode_nibble(voice: &mut OkiVoice, nibble: u8) -> i32 {
        let step = ADPCM_STEP_TABLE[voice.step.clamp(0, 48) as usize];
        let mut diff = step >> 3;
        if nibble & 1 != 0 {
            diff += step >> 2;
        }
        if nibble & 2 != 0 {
            diff += step >> 1;
        }
        if nibble & 4 != 0 {
            diff += step;
        }
        if nibble & 8 != 0 {
            diff = -diff;
        }
        voice.signal = (voice.signal + diff).clamp(-2048, 2047);
        voice.step = (voice.step + ADPCM_INDEX_SHIFT[(nibble & 7) as usize]).clamp(0, 48);
        voice.signal
    }

    /// Produce one mixed output sample in the range roughly ±1.0.
    fn next_sample(&mut self) -> f32 {
        let mut acc = 0i32;
        let output_rate = OUTPUT_RATE;
        for i in 0..4 {
            let rate = self.rate;
            let mut voice = self.voices[i];
            if voice.playing {
                voice.phase += rate;
                while voice.phase >= output_rate {
                    voice.phase -= output_rate;
                    if voice.position >= voice.stop {
                        voice.playing = false;
                        break;
                    }
                    let byte = self.read_byte(voice.position / 2);
                    let nibble = if voice.position & 1 == 0 {
                        byte >> 4
                    } else {
                        byte & 0x0f
                    };
                    voice.position += 1;
                    Oki::decode_nibble(&mut voice, nibble);
                }
                acc += voice.signal * voice.volume;
            }
            self.voices[i] = voice;
        }
        // Scale: 12-bit signal * vol(≤32) * 4 voices → normalise.
        (acc as f32) / (2048.0 * 32.0)
    }
}

// ── YM2151 (OPM) — vendored `ymfm` OPM core via C++ glue ─────────────────────

#[cfg(not(target_arch = "wasm32"))]
extern "C" {
    fn neo_ym2151_create() -> *mut std::ffi::c_void;
    fn neo_ym2151_destroy(p: *mut std::ffi::c_void);
    fn neo_ym2151_reset(p: *mut std::ffi::c_void);
    fn neo_ym2151_write_addr(p: *mut std::ffi::c_void, data: i32);
    fn neo_ym2151_write_data(p: *mut std::ffi::c_void, data: i32);
    fn neo_ym2151_read_status(p: *mut std::ffi::c_void) -> i32;
    fn neo_ym2151_irq(p: *mut std::ffi::c_void) -> i32;
    fn neo_ym2151_advance(p: *mut std::ffi::c_void, chip_samples: i32);
    fn neo_ym2151_generate(p: *mut std::ffi::c_void, buf: *mut f32, n: i32);
    fn neo_ym2151_save_state(p: *mut std::ffi::c_void, out_buf: *mut *mut u8, out_size: *mut i32);
    fn neo_ym2151_load_state(p: *mut std::ffi::c_void, buf: *const u8, size: i32);
    fn neo_ym2151_free_state_buf(buf: *mut u8);
}

#[cfg(target_arch = "wasm32")]
unsafe fn neo_ym2151_create() -> *mut std::ffi::c_void { std::ptr::null_mut() }
#[cfg(target_arch = "wasm32")]
unsafe fn neo_ym2151_destroy(_p: *mut std::ffi::c_void) {}
#[cfg(target_arch = "wasm32")]
unsafe fn neo_ym2151_reset(_p: *mut std::ffi::c_void) {}
#[cfg(target_arch = "wasm32")]
unsafe fn neo_ym2151_write_addr(_p: *mut std::ffi::c_void, _data: i32) {}
#[cfg(target_arch = "wasm32")]
unsafe fn neo_ym2151_write_data(_p: *mut std::ffi::c_void, _data: i32) {}
#[cfg(target_arch = "wasm32")]
unsafe fn neo_ym2151_read_status(_p: *mut std::ffi::c_void) -> i32 { 0 }
#[cfg(target_arch = "wasm32")]
unsafe fn neo_ym2151_irq(_p: *mut std::ffi::c_void) -> i32 { 0 }
#[cfg(target_arch = "wasm32")]
unsafe fn neo_ym2151_advance(_p: *mut std::ffi::c_void, _chip_samples: i32) {}
#[cfg(target_arch = "wasm32")]
unsafe fn neo_ym2151_generate(_p: *mut std::ffi::c_void, _buf: *mut f32, _n: i32) {}
#[cfg(target_arch = "wasm32")]
unsafe fn neo_ym2151_save_state(_p: *mut std::ffi::c_void, _out_buf: *mut *mut u8, _out_size: *mut i32) {}
#[cfg(target_arch = "wasm32")]
unsafe fn neo_ym2151_load_state(_p: *mut std::ffi::c_void, _buf: *const u8, _size: i32) {}
#[cfg(target_arch = "wasm32")]
unsafe fn neo_ym2151_free_state_buf(_buf: *mut u8) {}

/// Safe wrapper around the ymfm OPM (YM2151) instance.
struct Ym2151 {
    handle: *mut std::ffi::c_void,
}

// The handle is exclusively owned; access is serialised through `&mut self`.
unsafe impl Send for Ym2151 {}

impl Ym2151 {
    fn new() -> Self {
        Ym2151 {
            handle: unsafe { neo_ym2151_create() },
        }
    }
    fn reset(&mut self) {
        unsafe { neo_ym2151_reset(self.handle) }
    }
    fn write_addr(&mut self, v: u8) {
        unsafe { neo_ym2151_write_addr(self.handle, v as i32) }
    }
    fn write_data(&mut self, v: u8) {
        unsafe { neo_ym2151_write_data(self.handle, v as i32) }
    }
    fn read_status(&self) -> u8 {
        unsafe { neo_ym2151_read_status(self.handle) as u8 }
    }
    fn irq(&self) -> bool {
        unsafe { neo_ym2151_irq(self.handle) != 0 }
    }
    /// Advance the chip by `chip_samples` native (clock/64) samples.
    fn advance(&mut self, chip_samples: i32) {
        unsafe { neo_ym2151_advance(self.handle, chip_samples) }
    }
    /// Drain `buf.len()` output-rate mono samples.
    fn generate(&mut self, buf: &mut [f32]) {
        unsafe { neo_ym2151_generate(self.handle, buf.as_mut_ptr(), buf.len() as i32) }
    }

    /// Capture the ymfm OPM chip + timer/IRQ + decimation state for rollback.
    fn snapshot(&self) -> Vec<u8> {
        let mut buf: *mut u8 = std::ptr::null_mut();
        let mut size: i32 = 0;
        unsafe {
            neo_ym2151_save_state(self.handle, &mut buf, &mut size);
            if buf.is_null() || size <= 0 {
                return Vec::new();
            }
            let bytes = std::slice::from_raw_parts(buf, size as usize).to_vec();
            neo_ym2151_free_state_buf(buf);
            bytes
        }
    }

    /// Restore chip state from a blob produced by `snapshot`.
    fn restore(&mut self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        unsafe { neo_ym2151_load_state(self.handle, data.as_ptr(), data.len() as i32) }
    }
}

impl Drop for Ym2151 {
    fn drop(&mut self) {
        unsafe { neo_ym2151_destroy(self.handle) }
    }
}

// ── Z80 machine ──────────────────────────────────────────────────────────────

struct SoundInner {
    rom: Vec<u8>,
    ram: [u8; 0x800],
    bank: usize,
    oki: Oki,
    ym: Ym2151,
    soundlatch: u8,
}

impl SoundInner {
    fn read_mem(&self, addr: u16) -> u8 {
        match addr {
            0x0000..=0x7fff => self.rom.get(addr as usize).copied().unwrap_or(0xff),
            0x8000..=0xbfff => {
                let off = 0x8000 + self.bank * 0x4000 + (addr as usize - 0x8000);
                self.rom.get(off).copied().unwrap_or(0xff)
            }
            0xd000..=0xd7ff => self.ram[(addr - 0xd000) as usize],
            0xf000..=0xf001 => self.ym.read_status(),
            0xf002 => 0x00,
            0xf008 => self.soundlatch,
            _ => 0xff,
        }
    }

    fn write_mem(&mut self, addr: u16, val: u8) {
        match addr {
            0xd000..=0xd7ff => self.ram[(addr - 0xd000) as usize] = val,
            0xf000 => self.ym.write_addr(val),
            0xf001 => self.ym.write_data(val),
            0xf002 => self.oki.write_command(val),
            0xf004 => self.bank = (val & 0x0f) as usize,
            0xf006 => self.oki.set_pin7(val & 1 != 0),
            _ => {}
        }
    }
}

struct SoundMachine<'a> {
    inner: &'a mut SoundInner,
}

impl<'a> Machine for SoundMachine<'a> {
    fn peek(&mut self, addr: u16) -> u8 {
        self.inner.read_mem(addr)
    }
    fn poke(&mut self, addr: u16, val: u8) {
        self.inner.write_mem(addr, val);
    }
    fn port_in(&mut self, _port: u16) -> u8 {
        0xff
    }
    fn port_out(&mut self, _port: u16, _val: u8) {}
}

/// The complete CPS1 sound board.
pub struct CpsSound {
    cpu: Cpu,
    inner: SoundInner,
}

impl CpsSound {
    pub fn new() -> Self {
        CpsSound {
            cpu: Cpu::new(),
            inner: SoundInner {
                rom: Vec::new(),
                ram: [0u8; 0x800],
                bank: 0,
                oki: Oki::new(),
                ym: Ym2151::new(),
                soundlatch: 0,
            },
        }
    }

    pub fn load_z80_rom(&mut self, rom: &[u8]) {
        self.inner.rom = rom.to_vec();
        self.cpu.signal_reset();
    }

    pub fn load_oki_rom(&mut self, rom: &[u8]) {
        self.inner.oki.rom = rom.to_vec();
    }

    pub fn reset(&mut self) {
        self.cpu.signal_reset();
        self.inner.ram = [0u8; 0x800];
        self.inner.bank = 0;
        self.inner.oki = Oki {
            rom: std::mem::take(&mut self.inner.oki.rom),
            ..Oki::new()
        };
        self.inner.ym.reset();
    }

    pub fn set_latch(&mut self, value: u8) {
        self.inner.soundlatch = value;
    }

    /// Run the Z80 for one video frame, stepping the YM2151 in lockstep so its
    /// timer-A/B IRQs drive the Z80 sound driver with the correct phase.
    pub fn run_frame(&mut self) {
        let cycles_per_frame = (Z80_CLOCK / 60) as u64;
        let start = self.cpu.cycle_count();
        // YM2151 native sample = 64 master clocks (= 64 Z80 clocks here).
        const CLOCKS_PER_CHIP_SAMPLE: u64 = 64;
        let mut chip_accum: u64 = 0;
        {
            let mut machine = SoundMachine {
                inner: &mut self.inner,
            };
            while self.cpu.cycle_count() - start < cycles_per_frame {
                let before = self.cpu.cycle_count();
                self.cpu.execute_instruction(&mut machine);
                let elapsed = self.cpu.cycle_count().wrapping_sub(before);
                chip_accum += elapsed;
                while chip_accum >= CLOCKS_PER_CHIP_SAMPLE {
                    chip_accum -= CLOCKS_PER_CHIP_SAMPLE;
                    machine.inner.ym.advance(1);
                }
                // Deliver the YM timer IRQ to the Z80 as soon as it asserts.
                self.cpu.signal_interrupt(machine.inner.ym.irq());
            }
        }

        // Clock the OKI ADPCM player for one frame's worth of output samples
        // *inside* the deterministic sound step, buffering the result.
        // Previously the OKI was only advanced from generate() (display frames),
        // so rollback catch-up frames left its voice state unadvanced and the
        // two peers desynced whenever their rollback schedules differed.
        self.inner.oki.advance(crate::NOMINAL_SAMPLES_PER_FRAME);
    }

    /// Append `n` mixed mono samples to `out` (YM2151 FM + OKI ADPCM).  Both
    /// sources are drained from ring buffers filled during `run_frame`, so
    /// this display-only call never mutates the checksummed sound state.
    pub fn generate(&mut self, out: &mut Vec<f32>, n: usize) {
        let mut ym_buf = vec![0.0f32; n];
        self.inner.ym.generate(&mut ym_buf);
        for ym in ym_buf.into_iter().take(n) {
            let oki = self.inner.oki.pop_sample();
            out.push((ym + oki).clamp(-1.0, 1.0));
        }
    }

    /// Capture the full sound-board state (Z80 + banking + OKI voices + YM2151)
    /// for rollback / save states.  ROM images are excluded — they are reloaded
    /// via `load_z80_rom` / `load_oki_rom`.
    pub fn snapshot(&self) -> super::save_state::SoundSnap {
        super::save_state::SoundSnap {
            z80_bytes: self.cpu.serialize(),
            ram: self.inner.ram.to_vec(),
            bank: self.inner.bank as u32,
            soundlatch: self.inner.soundlatch,
            oki_voices: self
                .inner
                .oki
                .voices
                .iter()
                .map(|v| super::save_state::OkiVoiceSnap {
                    playing: v.playing,
                    position: v.position,
                    stop: v.stop,
                    signal: v.signal,
                    step: v.step,
                    volume: v.volume,
                    phase: v.phase,
                })
                .collect(),
            oki_command: self.inner.oki.command,
            oki_rate: self.inner.oki.rate,
            ym_bytes: self.inner.ym.snapshot(),
        }
    }

    /// Restore sound-board state from a [`SoundSnap`] (ROMs are left intact).
    pub fn restore(&mut self, snap: &super::save_state::SoundSnap) {
        if let Err(e) = self.cpu.deserialize(&snap.z80_bytes) {
            log::warn!("CPS1 Z80 deserialize failed: {:?}", e);
        }
        let n = snap.ram.len().min(self.inner.ram.len());
        self.inner.ram[..n].copy_from_slice(&snap.ram[..n]);
        self.inner.bank = snap.bank as usize;
        self.inner.soundlatch = snap.soundlatch;
        for (v, s) in self.inner.oki.voices.iter_mut().zip(snap.oki_voices.iter()) {
            v.playing = s.playing;
            v.position = s.position;
            v.stop = s.stop;
            v.signal = s.signal;
            v.step = s.step;
            v.volume = s.volume;
            v.phase = s.phase;
        }
        self.inner.oki.command = snap.oki_command;
        self.inner.oki.rate = snap.oki_rate;
        // Buffered output is transient and belongs to the pre-rollback timeline;
        // drop it so it can be regenerated deterministically (matches YM2151).
        self.inner.oki.clear_ring();
        self.inner.ym.restore(&snap.ym_bytes);
    }
}

impl Default for CpsSound {
    fn default() -> Self {
        Self::new()
    }
}
