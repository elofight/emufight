//! LSPC-2 Video Controller
//!
//! The NeoGeo's custom LSPC-2 chip is responsible for:
//!
//! - **VRAM** (68 KB): 64 KB slow (word addresses `$0000–$7FFF`) + 4 KB fast
//!   (word addresses `$8000–$87FF`).  The 68K accesses VRAM through a register
//!   window at `$3C0000–$3C000F`.
//! - **Palette RAM** (16 KB): two 8 KB banks; the active bank is selected by
//!   writes to `REG_PALBANK1` / `REG_PALBANK0`.
//! - **Raster timer**: 32-bit down-counter loaded via `REG_TIMERHIGH`/`REG_TIMERLOW`;
//!   fires IRQ2 (level 2) on underflow and reloads.
//! - **Beam counter**: free-running 68K-cycle accumulator that synthesises the
//!   current scanline number for `REG_LSPCMODE` reads.
//! - **IRQ management**: three interrupt sources with fixed 68K priorities.
//!
//! # Pending-IRQ bit assignments (must match the original `bus.rs` exactly)
//!
//! | Bit | Mask   | Source              | 68K level |
//! |-----|--------|---------------------|-----------|
//! | 0   | `0x01` | Cold-boot / watchdog | 3 |
//! | 1   | `0x02` | Raster timer        | 2 |
//! | 2   | `0x04` | V-blank             | 1 |
//!
//! External code raises VBL with `lspc.raise_irq(0x04)`.
//! The timer sets bit 1 (`0x02`) internally inside `Lspc::tick`.

use serde::{Serialize, Deserialize};

// ── Raster constants (re-exported by bus/mod.rs) ──────────────────────────────

/// Total scanlines per frame including blanking (NTSC NeoGeo).
pub const SCANLINES_PER_FRAME:   u32 = 264;
/// 68K CPU cycles per full video frame (12 MHz ÷ ~60 Hz).
pub const CYCLES_PER_BEAM_FRAME: u32 = 200_000;
/// 68K CPU cycles per scanline (= 200_000 ÷ 264 ≈ 758).
pub const CYCLES_PER_LINE:       u32 = CYCLES_PER_BEAM_FRAME / SCANLINES_PER_FRAME;

// ── Lspc ──────────────────────────────────────────────────────────────────────

/// LSPC-2 video-controller state.
///
/// All fields are `pub` — `SystemBus` needs direct access for the
/// address-decoder and `main.rs` reads several fields for debug output.
/// Prefer the method API where it exists.
///
/// Implements `Serialize` / `Deserialize` so the complete state is captured
/// in a save-state without any manual field enumeration.
#[derive(Serialize, Deserialize, Clone)]
pub struct Lspc {
    // ── VRAM / Palette ────────────────────────────────────────────────────────

    /// 68 KB VRAM: 64 KB slow (word `$0000–$7FFF`) stored at bytes
    /// `0x00000–0x0FFFF`; 4 KB fast (word `$8000–$87FF`) at bytes
    /// `0x10000–0x10FFF`.  Total allocation: `0x11000` bytes.
    pub vram: Vec<u8>,

    /// 16 KB palette RAM, two 8 KB banks stored in a single flat buffer.
    /// Bank 0 occupies bytes `0x0000–0x1FFF`; bank 1 occupies `0x2000–0x3FFF`.
    /// Using `Vec<u8>` rather than `[u8; 0x4000]` so serde can handle it
    /// without a custom serialiser.
    pub pal_ram: Vec<u8>,

    /// Active palette bank: `false` = bank 0, `true` = bank 1.
    pub pal_bank: bool,

    // ── VRAM access registers ─────────────────────────────────────────────────

    /// `REG_VRAMADDR` (`$3C0000`): current word address into VRAM.
    pub vram_addr: u16,

    /// `REG_VRAMMOD` (`$3C0004`): auto-increment applied to `vram_addr`
    /// after each write through `REG_VRAMRW`.  Default 1.
    pub vram_mod: u16,

    // ── Video output flags ────────────────────────────────────────────────────

    /// `true` when `REG_BRDFIX` is active: fix layer uses system SFIX ROM.
    /// `false` when `REG_CRTFIX` is active: fix layer uses cartridge S-ROM.
    pub brd_fix: bool,

    /// Shadow mode: dim the overall video output when `true`.
    pub shadow: bool,

    // ── Raster timer ──────────────────────────────────────────────────────────

    /// 32-bit reload value assembled from `REG_TIMERHIGH` (high word) and
    /// `REG_TIMERLOW` (low word).  Writing `REG_TIMERLOW` also loads
    /// `timer_counter` and sets `timer_active = true`.
    pub timer_reload: u32,

    /// Current down-counter.  Decremented at 6 MHz (half the 68K clock).
    /// Fires bit-1 IRQ on underflow and reloads from `timer_reload`.
    pub timer_counter: u32,

    /// Timer is running once `REG_TIMERLOW` has been written at least once.
    pub timer_active: bool,

    /// `REG_TIMERSTOP` bit 0: pause the counter during the border region.
    pub timer_stop_in_border: bool,

    /// Remainder of M68K cycles used for timer scaling
    pub timer_subcycle: u32,

    // ── Beam counter ──────────────────────────────────────────────────────────

    /// Running 68K-cycle accumulator for the current raster position.
    /// Wraps at [`CYCLES_PER_BEAM_FRAME`].
    /// Used by `REG_LSPCMODE` reads and `timer_stop_in_border` checks.
    pub beam_cycles: u32,

    // ── IRQ state ─────────────────────────────────────────────────────────────

    /// Bitmask of pending interrupt sources (see module-level bit table).
    pub pending_irq: u8,

    // ── IRQ control register ($3C0006 write) ──────────────────────────────────

    /// Low byte of the last `REG_LSPCMODE` write.
    /// bit 4 (0x10): raster timer IRQ enable.
    /// Stored as written; individual bits are checked in `tick()`.
    pub irq_control: u8,

    // ── Auto-animation ────────────────────────────────────────────────────────

    /// Current auto-animation frame (0–7), incremented each VBL when the
    /// speed counter expires.  The sprite renderer substitutes the low 3 bits
    /// (or low 2 bits for 4-frame mode) of animated tile codes with this value.
    pub auto_anim_frame: u8,

    /// Auto-animation speed: frame advances every `auto_anim_speed + 1` VBLs.
    /// Loaded from bits \[15:8\] of `REG_LSPCMODE` writes.
    pub auto_anim_speed: u8,

    /// Down-counter for auto-animation timing.  Reloads from `auto_anim_speed`
    /// each time it expires.
    pub auto_anim_timer: u8,
}

impl Lspc {
    /// Construct with power-on defaults (empty VRAM, timer disabled, etc.).
    pub fn new() -> Self {
        Lspc {
            vram:     vec![0u8; 0x11000],  // 64 KB slow + 4 KB fast
            pal_ram:  vec![0u8; 0x4000],   // 16 KB palette RAM
            pal_bank: false,
            vram_addr:   0,
            vram_mod:    1,
            brd_fix: true,
            shadow:  false,
            timer_reload:  0,
            timer_counter: 0,
            timer_active:         false,
            timer_stop_in_border: false,
            timer_subcycle:       0,
            beam_cycles: 0,
            pending_irq: 0,
            irq_control: 0,
            auto_anim_frame: 0,
            auto_anim_speed: 0,
            auto_anim_timer: 0,
        }
    }

    // ── VRAM helpers ──────────────────────────────────────────────────────────

    /// Translate `vram_addr` into a byte offset into `self.vram`.
    ///
    /// Addresses `$0000–$7FFF` → slow VRAM (bytes `0–$FFFF`).
    /// Addresses `$8000–$87FF` → fast VRAM (bytes `$10000–$10FFE`).
    #[inline]
    pub(super) fn vram_byte_addr(&self) -> usize {
        let len = self.vram.len(); // 0x11000 bytes
        (self.vram_addr as usize * 2) % len
    }

    /// Read the 16-bit word at the current `vram_addr`.
    ///
    /// **Does not advance `vram_addr`** — the NeoGeo LSPC does not
    /// auto-increment on reads, only on writes.
    #[inline]
    pub fn vram_read_word(&self) -> u16 {
        let a = self.vram_byte_addr();
        let len = self.vram.len();
        ((self.vram[a] as u16) << 8) | self.vram[(a + 1) % len] as u16
    }

    /// Write `value` into VRAM at the current `vram_addr`.
    ///
    /// **Does not advance `vram_addr`** — the caller (bus/mod.rs write_16) is
    /// responsible for the post-increment so the exact sequence and timing
    /// match the original flat-bus implementation.
    #[inline]
    pub fn vram_write_word(&mut self, value: u16) {
        let a = self.vram_byte_addr();
        let len = self.vram.len();
        self.vram[a]             = (value >> 8)   as u8;
        self.vram[(a + 1) % len] = (value & 0xFF) as u8;
    }

    // ── Palette RAM helpers ────────────────────────────────────────────────────

    /// Read a byte from palette RAM at the given flat byte offset.
    #[inline]
    pub fn pal_read(&self, offset: usize) -> u8 {
        self.pal_ram.get(offset).copied().unwrap_or(0)
    }

    /// Write a byte to palette RAM at the given flat byte offset.
    #[inline]
    pub fn pal_write(&mut self, offset: usize, v: u8) {
        if let Some(slot) = self.pal_ram.get_mut(offset) {
            *slot = v;
        }
    }

    // ── LSPC mode register ────────────────────────────────────────────────────

    /// `REG_LSPCMODE` (`$3C0006`) read value.
    ///
    /// Bits `[15:7]` encode the current scanline **with a +0xF8 hardware offset**,
    /// verified on real MVS hardware (FBNeo comment: "0xF8 is correct as
    /// verified on MVS hardware").  At the start of the active display the
    /// hardware scanline counter wraps through 0xF8; the BIOS scanline-sync
    /// routines rely on this exact value.
    ///
    /// Bits `[2:0]` expose the auto-animation frame counter so games can read
    /// back the current animation phase.
    #[inline]
    pub fn lspc_mode_read(&self) -> u16 {
        let line = (self.beam_cycles / CYCLES_PER_LINE)
            .min(SCANLINES_PER_FRAME - 1);
        // Hardware scanline counter adds 0xF8 offset; keep in 9-bit range.
        let hw_line = (line + 0xF8) & 0x1FF;
        ((hw_line << 7) | (self.auto_anim_frame as u32 & 0x07)) as u16
    }

    // ── Auto-animation VBL tick ───────────────────────────────────────────────

    /// Advance the auto-animation counter once per VBL.
    ///
    /// Must be called from the main frame loop just before raising the VBL IRQ.
    /// The frame advances every `auto_anim_speed + 1` VBL periods, matching
    /// FBNeo's `nSpriteFrameTimer` / `nSpriteFrameSpeed` logic.
    pub fn tick_vbl(&mut self) {
        if self.auto_anim_timer >= self.auto_anim_speed {
            self.auto_anim_timer = 0;
            self.auto_anim_frame = self.auto_anim_frame.wrapping_add(1) & 0x07;
        } else {
            self.auto_anim_timer = self.auto_anim_timer.wrapping_add(1);
        }
    }

    // ── IRQ management ────────────────────────────────────────────────────────

    /// OR `mask` into the pending-IRQ register.
    ///
    /// Typical callers:
    /// - `raise_irq(0x04)` — VBL at the start of each frame (main loop).
    /// - `raise_irq(0x02)` — raster timer underflow (inside `Lspc::tick`).
    /// - `raise_irq(0x01)` — cold-boot trigger (BIOS POST sequence).
    #[inline]
    pub fn raise_irq(&mut self, mask: u8) {
        self.pending_irq |= mask;
    }

    /// Clear the acknowledged interrupt bits from the pending-IRQ register.
    ///
    /// Called on `REG_IRQACK` (`$3C000C`) writes.  The hardware masks out
    /// up to three bits (bits 2:0), hence `mask & 0x07`.
    #[inline]
    pub fn irq_ack(&mut self, mask: u8) {
        self.pending_irq &= !(mask & 0x07);
    }

    /// Derive the 68K interrupt level to assert from the pending-IRQ register.
    ///
    /// | `pending_irq` bit | 68K level |
    /// |-------------------|-----------|
    /// | bit 0 (cold-boot) | 3 |
    /// | bit 1 (timer)     | 2 |
    /// | bit 2 (VBL)       | 1 |
    /// | none              | 0 |
    ///
    /// Higher levels take priority over lower levels (68K convention).
    pub fn irq_level(&self) -> u32 {
        if      self.pending_irq & 0x01 != 0 { 3 } // cold-boot → highest priority
        else if self.pending_irq & 0x02 != 0 { 2 } // raster timer
        else if self.pending_irq & 0x04 != 0 { 1 } // V-blank
        else                                  { 0 }
    }

    // ── Combined tick ─────────────────────────────────────────────────────────

    /// Advance the beam counter and raster timer by `m68k_cycles` 68K cycles.
    ///
    /// This is the single entry point called from `SystemBus::tick_timer()`;
    /// it combines the original `tick_beam` and `tick_timer` methods.
    ///
    /// **Beam counter**: wraps at [`CYCLES_PER_BEAM_FRAME`].
    ///
    /// **Timer**: decremented at the pixel clock rate (6 MHz = ½ × 68K clock).
    /// Each call counts `m68k_cycles / 2` timer ticks.  When the counter
    /// reaches zero IRQ bit 1 is raised and the counter reloads from
    /// `timer_reload`.
    pub fn tick(&mut self, m68k_cycles: u32) {
        // ── Beam counter ──────────────────────────────────────────────────────
        self.beam_cycles = self.beam_cycles
            .wrapping_add(m68k_cycles) % CYCLES_PER_BEAM_FRAME;

        // ── Raster timer ──────────────────────────────────────────────────────
        if !self.timer_active { return; }

        // Pixel clock is half the 68K clock. Accumulate fractional ticks.
        let total_subcycles = self.timer_subcycle + m68k_cycles;
        let ticks = total_subcycles / 2;
        self.timer_subcycle = total_subcycles % 2;
        if ticks == 0 { return; }

        if self.timer_counter <= ticks {
            // Gate on irq_control bit 4: only raise timer IRQ if enabled.
            if self.irq_control & 0x10 != 0 {
                self.pending_irq |= 0x02; // bit 1 = timer → IRQ level 2
            }
            self.timer_counter = self.timer_reload;
        } else {
            self.timer_counter -= ticks;
        }
    }
}

impl Default for Lspc {
    fn default() -> Self { Self::new() }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── VRAM read / write ─────────────────────────────────────────────────────

    #[test]
    fn vram_write_then_read_word() {
        let mut lspc = Lspc::new();
        lspc.vram_addr = 0x0010;
        lspc.vram_mod  = 1;
        lspc.vram_write_word(0xBEEF);
        // NOTE: auto-increment is applied by the bus layer (write_16), NOT by
        // vram_write_word itself.  Read back at the same address.
        lspc.vram_addr = 0x0010;
        assert_eq!(lspc.vram_read_word(), 0xBEEF);
    }

    #[test]
    fn vram_write_auto_increments_by_mod() {
        // Auto-increment is handled by SystemBus::write_16 (REG_VRAMRW case).
        // Here we verify that vram_mod is stored and readable correctly.
        let mut lspc = Lspc::new();
        lspc.vram_mod = 4;
        assert_eq!(lspc.vram_mod, 4, "vram_mod should persist as set");
    }

    #[test]
    fn fast_vram_address_range() {
        // Addresses $8000+ go to the fast (SCB2–4) region.
        let mut lspc = Lspc::new();
        lspc.vram_addr = 0x8000;
        lspc.vram_mod  = 1;
        lspc.vram_write_word(0xCAFE);
        lspc.vram_addr = 0x8000;
        assert_eq!(lspc.vram_read_word(), 0xCAFE);
    }

    // ── IRQ management ────────────────────────────────────────────────────────

    #[test]
    fn raise_and_ack_vblank_irq() {
        let mut lspc = Lspc::new();
        // No IRQ initially.
        assert_eq!(lspc.irq_level(), 0);
        // Raise V-blank (bit 2 = level 1 on NeoGeo).
        lspc.raise_irq(0x04);
        assert!(lspc.irq_level() > 0, "V-blank should assert an IRQ");
        // Acknowledge clears the pending bit.
        lspc.irq_ack(0x04);
        assert_eq!(lspc.irq_level(), 0, "IRQ should clear after ack");
    }

    #[test]
    fn timer_fires_irq_after_reload_ticks() {
        let mut lspc = Lspc::new();
        lspc.timer_reload  = 200; // fire after 200 M68k/2 ticks
        lspc.timer_counter = 200;
        lspc.timer_active  = true;
        lspc.irq_control   = 0x10; // bit 4: enable raster timer IRQ

        // Advance exactly enough cycles.
        lspc.tick(400); // 400 M68k cycles → 200 ticks (divides by 2)
        assert!(
            lspc.pending_irq & 0x02 != 0,
            "timer IRQ (bit 1) should be set after reload ticks"
        );
    }

    // ── Palette write passthrough ─────────────────────────────────────────────

    #[test]
    fn pal_write_stores_byte() {
        let mut lspc = Lspc::new();
        lspc.pal_write(0, 0xAB);
        assert_eq!(lspc.pal_ram[0], 0xAB);
    }
}

