//! CPS1 68000 address bus.
//!
//! Implements the memory map used by CPS1 A-board games (see MAME
//! `cps1.cpp main_map`).  The bus owns work RAM, the 192 KB graphics RAM,
//! and the CPS-A / CPS-B custom-chip register files.  Input ports and the
//! sound-command latch are also serviced here.

use m68k_cpu::AddressBus;

use super::config::{CpsGame, GFXTYPE_SPRITES};

pub const GFXRAM_WORDS: usize = 0x18000; // 0x30000 bytes @ 0x900000
pub const WORKRAM_BYTES: usize = 0x10000; // @ 0xff0000
pub const CPS_REG_WORDS: usize = 0x20; // 0x40-byte register windows

/// The CPS1 main-CPU bus.
pub struct CpsBus {
    /// 68000 program ROM (big-endian words).
    pub program: Vec<u8>,
    pub work_ram: Vec<u8>,
    /// Graphics RAM, word-addressed (native order).
    pub gfxram: Vec<u16>,
    pub cps_a: [u16; CPS_REG_WORDS],
    pub cps_b: [u16; CPS_REG_WORDS],

    game: &'static CpsGame,

    // ── Inputs (active-low) ──────────────────────────────────────────────────
    pub in0: u8, // system: coins / start / service
    pub in1: u16, // players 1 & 2 (P1 low byte, P2 high byte)
    pub in2: u16, // extra buttons (6-button games)
    pub dswa: u8,
    pub dswb: u8,
    pub dswc: u8,

    // ── Sound interface ──────────────────────────────────────────────────────
    pub sound_latch: u8,
    pub sound_latch2: u8,
    /// Set when the 68K writes a new sound command (consumed by the Z80).
    pub sound_latch_pending: bool,

    // ── Interrupt state (MAME cps1.cpp) ──────────────────────────────────────
    /// Pending maskable interrupts, held (level-triggered) until the CPU
    /// acknowledges them by entering interrupt-acknowledge (CPU space) — MAME
    /// `irqack_r` clears both lines. bit0 = IPL1 (IRQ2, VBLANK), bit1 = IPL2
    /// (IRQ4, raster — unused by sf2/sf2ce).
    pub irq_pending: u8,
}

impl CpsBus {
    pub fn new(game: &'static CpsGame) -> Self {
        CpsBus {
            program: Vec::new(),
            work_ram: vec![0u8; WORKRAM_BYTES],
            gfxram: vec![0u16; GFXRAM_WORDS],
            cps_a: [0u16; CPS_REG_WORDS],
            cps_b: [0u16; CPS_REG_WORDS],
            game,
            in0: 0xff,
            in1: 0xffff,
            in2: 0xffff,
            dswa: 0xff,
            dswb: 0xff,
            dswc: 0xff,
            sound_latch: 0,
            sound_latch2: 0,
            sound_latch_pending: false,
            irq_pending: 0,
        }
    }

    pub fn game(&self) -> &'static CpsGame {
        self.game
    }

    /// Capture the mutable bus state (RAM + register files + inputs + IRQ) for
    /// save states.  ROM (`program`) and the static `game` pointer are excluded.
    pub fn snapshot(&self) -> super::save_state::BusSnap {
        super::save_state::BusSnap {
            work_ram: self.work_ram.clone(),
            gfxram: self.gfxram.clone(),
            cps_a: self.cps_a.to_vec(),
            cps_b: self.cps_b.to_vec(),
            in0: self.in0,
            in1: self.in1,
            in2: self.in2,
            dswa: self.dswa,
            dswb: self.dswb,
            dswc: self.dswc,
            sound_latch: self.sound_latch,
            sound_latch2: self.sound_latch2,
            sound_latch_pending: self.sound_latch_pending,
            irq_pending: self.irq_pending,
        }
    }

    /// Restore mutable bus state from a [`BusSnap`] (ROM/game left intact).
    pub fn restore(&mut self, snap: &super::save_state::BusSnap) {
        let wn = snap.work_ram.len().min(self.work_ram.len());
        self.work_ram[..wn].copy_from_slice(&snap.work_ram[..wn]);
        let gn = snap.gfxram.len().min(self.gfxram.len());
        self.gfxram[..gn].copy_from_slice(&snap.gfxram[..gn]);
        let an = snap.cps_a.len().min(self.cps_a.len());
        self.cps_a[..an].copy_from_slice(&snap.cps_a[..an]);
        let bn = snap.cps_b.len().min(self.cps_b.len());
        self.cps_b[..bn].copy_from_slice(&snap.cps_b[..bn]);
        self.in0 = snap.in0;
        self.in1 = snap.in1;
        self.in2 = snap.in2;
        self.dswa = snap.dswa;
        self.dswb = snap.dswb;
        self.dswc = snap.dswc;
        self.sound_latch = snap.sound_latch;
        self.sound_latch2 = snap.sound_latch2;
        self.sound_latch_pending = snap.sound_latch_pending;
        self.irq_pending = snap.irq_pending;
    }

    /// Borrow the 68000 work RAM (`$FF0000–$FFFFFF`) for game-data inspection.
    pub fn work_ram(&self) -> &[u8] {
        &self.work_ram
    }

    /// Current asserted 68000 interrupt level derived from the held IRQ lines.
    /// IPL2 (IRQ4) has priority over IPL1 (IRQ2); returns 0 when idle.
    /// Port of the CPS1 interrupt-mixer behaviour (set_interrupt_mixer(false)).
    pub fn irq_level(&self) -> u32 {
        if self.irq_pending & 0x02 != 0 {
            4
        } else if self.irq_pending & 0x01 != 0 {
            2
        } else {
            0
        }
    }

    fn rom_word(&self, addr: u32) -> u16 {
        let i = addr as usize;
        if i + 1 < self.program.len() {
            ((self.program[i] as u16) << 8) | self.program[i + 1] as u16
        } else {
            0xffff
        }
    }

    /// CPS-B register read — handles self-test, multiply protection and the
    /// extra-input ports (port of MAME `cps1_cps_b_r`).
    fn cps_b_read(&self, off: usize) -> u16 {
        let cfg = &self.game.cpsb;
        if cfg.cpsb_addr >= 0 && off == (cfg.cpsb_addr / 2) as usize && cfg.cpsb_value >= 0 {
            return cfg.cpsb_value as u16;
        }
        if cfg.mult_result_lo >= 0 && off == (cfg.mult_result_lo / 2) as usize {
            let a = self.cps_b[(cfg.mult_factor1 / 2) as usize] as u32;
            let b = self.cps_b[(cfg.mult_factor2 / 2) as usize] as u32;
            return (a.wrapping_mul(b) & 0xffff) as u16;
        }
        if cfg.mult_result_hi >= 0 && off == (cfg.mult_result_hi / 2) as usize {
            let a = self.cps_b[(cfg.mult_factor1 / 2) as usize] as u32;
            let b = self.cps_b[(cfg.mult_factor2 / 2) as usize] as u32;
            return (a.wrapping_mul(b) >> 16) as u16;
        }
        if self.game.in2_addr != 0 && off == (self.game.in2_addr / 2) as usize {
            return self.in2;
        }
        if self.game.in3_addr != 0 && off == (self.game.in3_addr / 2) as usize {
            return 0xffff;
        }
        self.cps_b[off & (CPS_REG_WORDS - 1)]
    }

    fn dsw_read(&self, off: usize) -> u16 {
        let v = match off {
            0 => self.in0,
            1 => self.dswa,
            2 => self.dswb,
            _ => self.dswc,
        };
        // MAME `cps1_dsw_r`: system/DSW value in the high byte, low byte 0xff.
        ((v as u16) << 8) | 0x00ff
    }

    // ── 16-bit primary accessors ─────────────────────────────────────────────

    pub fn read_16(&mut self, addr: u32) -> u16 {
        match addr {
            0x000000..=0x3fffff => self.rom_word(addr),
            0x800000..=0x800001 => self.in1,
            0x800018..=0x80001f => self.dsw_read(((addr - 0x800018) / 2) as usize),
            0x800140..=0x80017f => self.cps_b_read(((addr - 0x800140) / 2) as usize),
            0x900000..=0x92ffff => self.gfxram[((addr - 0x900000) / 2) as usize],
            0xff0000..=0xffffff => {
                // Mask to even and wrap within the buffer to prevent OOB on
                // the last byte (0xffffff → index 65535, [i+1] would be 65536).
                let i = ((addr - 0xff0000) as usize) & (WORKRAM_BYTES - 2);
                ((self.work_ram[i] as u16) << 8) | self.work_ram[i + 1] as u16
            }
            _ => 0xffff,
        }
    }

    pub fn write_16(&mut self, addr: u32, val: u16) {
        match addr {
            0x800030..=0x800037 => { /* coin counters / lockout — ignored */ }
            0x800100..=0x80013f => {
                self.cps_a[(((addr - 0x800100) / 2) as usize) & (CPS_REG_WORDS - 1)] = val;
            }
            0x800140..=0x80017f => {
                self.cps_b[(((addr - 0x800140) / 2) as usize) & (CPS_REG_WORDS - 1)] = val;
            }
            0x800180..=0x800187 => {
                self.sound_latch = val as u8;
                self.sound_latch_pending = true;
            }
            0x800188..=0x80018f => {
                self.sound_latch2 = val as u8;
            }
            0x900000..=0x92ffff => {
                self.gfxram[((addr - 0x900000) / 2) as usize] = val;
            }
            0xff0000..=0xffffff => {
                let i = ((addr - 0xff0000) as usize) & (WORKRAM_BYTES - 2);
                self.work_ram[i] = (val >> 8) as u8;
                self.work_ram[i + 1] = val as u8;
            }
            _ => {}
        }
    }

    // ── Video helpers ────────────────────────────────────────────────────────

    /// Compute a video-base pointer offset (in gfxram words) for a CPS-A base
    /// register, aligned down to `boundary` bytes. Port of MAME `cps1_base`.
    pub fn video_base(&self, reg_index: usize, boundary: u32) -> usize {
        let mut base = (self.cps_a[reg_index] as u32) * 256;
        base &= !(boundary - 1);
        ((base & 0x3ffff) / 2) as usize
    }

    /// Remap a tile code via the active game's GFX bank mapper.
    pub fn map_gfx(&self, gfxtype: i32, code: i32) -> i32 {
        self.game.gfxrom_bank_mapper(gfxtype, code)
    }
}

// ── Sanity check that the sprite type constant is wired ──────────────────────
const _: i32 = GFXTYPE_SPRITES;

// ── m68k_cpu::AddressBus (big-endian 68000) ──────────────────────────────────

impl AddressBus for CpsBus {
    fn read_byte(&mut self, address: u32) -> u8 {
        let w = self.read_16(address & !1);
        if address & 1 == 0 { (w >> 8) as u8 } else { w as u8 }
    }
    fn read_word(&mut self, address: u32) -> u16 {
        self.read_16(address)
    }
    fn read_long(&mut self, address: u32) -> u32 {
        ((self.read_16(address) as u32) << 16) | self.read_16(address + 2) as u32
    }
    fn write_byte(&mut self, address: u32, value: u8) {
        let aligned = address & !1;
        let old = self.read_16(aligned);
        let new = if address & 1 == 0 {
            (old & 0x00ff) | ((value as u16) << 8)
        } else {
            (old & 0xff00) | value as u16
        };
        self.write_16(aligned, new);
    }
    fn write_word(&mut self, address: u32, value: u16) {
        self.write_16(address, value);
    }
    fn write_long(&mut self, address: u32, value: u32) {
        self.write_16(address, (value >> 16) as u16);
        self.write_16(address + 2, value as u16);
    }

    /// Interrupt acknowledge — MAME `cps_state::irqack_r` (cpu_space_map at
    /// 0xfffff2..0xffffff). Reading the acknowledge vector clears *both*
    /// maskable IRQ lines (IPL1 and IPL2) and returns autovector, so a held
    /// VBLANK/raster interrupt fires exactly once per assertion.
    fn interrupt_acknowledge(&mut self, _level: u8) -> u32 {
        self.irq_pending = 0;
        0xFFFF_FFFF // autovector
    }
}
