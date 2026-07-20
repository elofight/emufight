//! NeoGeo System Bus — Backplane, memory map and I/O routing for the MC68000.
//!
//! # Architecture overview
//!
//! The bus is split into well-defined components rather than a single flat
//! struct.  Each component owns the state it is responsible for and exposes a
//! focused API; `SystemBus` acts as the backplane that wires them together and
//! implements the full 68K address-decoder.
//!
//! ```text
//!  ┌─────────────────────────────────────────────────────────────────────┐
//!  │                         SystemBus (backplane)                       │
//!  │                                                                     │
//!  │  RomImages  ──── immutable during gameplay (not in save-state)      │
//!  │    p_rom / bios_rom / sfix_rom / sm1_rom / m1_rom / s_rom / c_rom  │
//!  │                                                                     │
//!  │  Lspc  ──── LSPC-2 video controller (fully serialisable)           │
//!  │    vram / pal_ram / timer / beam / irq                              │
//!  │                                                                     │
//!  │  Upd4990a  ──── serial real-time clock (fully serialisable)         │
//!  │                                                                     │
//!  │  work_ram / backup_ram / mem_card ── general-purpose RAM            │
//!  │  sound_cmd / sound_reply / sound_status / nmi_request ── Z80 IPC   │
//!  │  input / cart / hw_dips ── peripherals                              │
//!  └─────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Memory map (NeoGeo AES/MVS, 24-bit address bus)
//!
//! | Range                | Description                              |
//! |----------------------|------------------------------------------|
//! | `$000000–$0FFFFF`    | P-ROM (or BIOS mirror at boot)           |
//! | `$100000–$10FFFF`    | 64 KB Work RAM                           |
//! | `$200000–$2FFFFF`    | Banked P-ROM window                      |
//! | `$2FFFF0–$2FFFFF`    | P-ROM bank select register               |
//! | `$300000–$3BFFFF`    | Memory-mapped I/O                        |
//! | `$3C0000–$3C000F`    | LSPC-2 video registers                   |
//! | `$400000–$401FFF`    | Palette RAM (8 KB active bank)           |
//! | `$800000–$800FFF`    | Memory card (2 KB)                       |
//! | `$C00000–$C7FFFF`    | BIOS ROM                                 |
//! | `$D00000–$D0FFFF`    | Battery-backed backup SRAM               |

use std::fs;
use serde::{Serialize, Deserialize};
use crate::io::InputState;
use crate::neogeo::cart::{self, Cartridge};

pub mod lspc;
pub mod rtc;

pub use lspc::Lspc;
pub use rtc::Upd4990a;


// ── Hardware register addresses ───────────────────────────────────────────────

const REG_P1CNT:    u32 = 0x300000; // Player 1 input register
const REG_DIPSW:    u32 = 0x300001; // Hardware DIP switches (active-low)

/// Work RAM offset for `BIOS_MVS_FLAG` (`$10FD82`).
const BIOS_MVS_FLAG_OFF: usize = 0xFD82;

/// AES training `REG_DIPSW`: freeplay on (bit 6 low), other switches off.
/// Matches FBNeo `neoForceAES` defaults for home-only game modes.
pub(crate) const AES_TRAINING_HW_DIPS: u8 = 0xFF & !0x40;
const REG_SOUND:    u32 = 0x320000; // 68K→Z80 command / Z80→68K reply latch
const REG_STATUS_A: u32 = 0x320001; // RTC serial out + TP + coin inputs
const REG_P2CNT:    u32 = 0x340000; // Player 2 input register
const REG_STATUS_B: u32 = 0x380000; // System input register (start, select…)
const REG_RTCCTRL:  u32 = 0x380051; // uPD4990A serial control (CLK/STB/DATA)
const REG_NOSHADOW: u32 = 0x3A0001; // Disable shadow (dim) mode
const REG_SHADOW:   u32 = 0x3A0011; // Enable shadow mode
const REG_BRDFIX:   u32 = 0x3A000B; // Select board SFIX + SM1 (REG_BRDFIX)
const REG_CRTFIX:   u32 = 0x3A001B; // Select cartridge S-ROM + M1 (REG_CRTFIX)
const REG_PALBANK1: u32 = 0x3A000F; // Activate palette bank 1
const REG_PALBANK0: u32 = 0x3A001F; // Activate palette bank 0
const REG_SWPBIOS:  u32 = 0x3A0003; // Map BIOS to $000000 (boot default)
const REG_SWPROM:   u32 = 0x3A0013; // Map cartridge P-ROM to $000000
const REG_SRAMLOCK: u32 = 0x3A000D; // Write-protect backup SRAM
const REG_SRAMEN:   u32 = 0x3A001D; // Write-enable backup SRAM
const REG_VRAMADDR: u32 = 0x3C0000; // VRAM word-address register
const REG_VRAMRW:   u32 = 0x3C0002; // VRAM data port (read/write)
const REG_VRAMMOD:  u32 = 0x3C0004; // VRAM auto-increment step
const REG_LSPCMODE: u32 = 0x3C0006; // LSPC mode / scanline read-back
const REG_TIMERHIGH:u32 = 0x3C0008; // Raster timer reload — high word
const REG_TIMERLOW: u32 = 0x3C000A; // Raster timer reload — low word (also starts timer)
const REG_IRQACK:   u32 = 0x3C000C; // IRQ acknowledge (clears pending bits)
const REG_TIMERSTOP:u32 = 0x3C000E; // Timer-stop-in-border control

// ── Address-range constants ───────────────────────────────────────────────────

const PROM_START: u32 = 0x000000;
const PROM_END:   u32 = 0x0FFFFF;

/// Banked P-ROM window — second megabyte of cartridge program ROM.
/// Games with >1 MB of code dynamically remap this window at runtime.
const PROM2_START:       u32 = 0x200000;
const PROM2_END:         u32 = 0x2FFFFF;

/// Writing any 16-bit value in this range selects the P2-ROM bank
/// (`N → p_rom[$100000 + N × $100000]`).
const REG_BANKSEL_START: u32 = 0x2FFFF0;
const REG_BANKSEL_END:   u32 = 0x2FFFFF;

pub const WRAM_START: u32 = 0x100000;
pub const WRAM_END:   u32 = 0x10FFFF;  // 64 KB

const IO_START: u32 = 0x300000;
const IO_END:   u32 = 0x3FFFFF;

/// Palette RAM window: maps to the active 8 KB bank of `Lspc::pal_ram`.
const PAL_START: u32 = 0x400000;
const PAL_END:   u32 = 0x401FFF; // 8 KB

/// Memory card: 2 KB exposed over a 4 KB word-addressed window.
/// Only odd bytes carry data; even bytes read as `$FF` (open bus).
const MEMCARD_START: u32 = 0x800000;
const MEMCARD_END:   u32 = 0x800FFF;

const BIOS_START: u32 = 0xC00000;
const BIOS_END:   u32 = 0xC7FFFF; // up to 512 KB

/// Battery-backed backup SRAM (high-score / soft-dip storage).
const SRAM_START: u32 = 0xD00000;
const SRAM_END:   u32 = 0xD0FFFF; // 64 KB

// ── Address space region abstraction (Phase 1 decomposition) ──────────────────

/// Internal trait for a 24-bit address region. Callers have already masked
/// the address to 24 bits.
trait BusRegion {
    fn read8(&self, offset: u32) -> u8;
    fn write8(&mut self, offset: u32, val: u8);
}

/// Work RAM ($100000–$10FFFF).
struct WramRegion {
    data: Vec<u8>,
}

impl WramRegion {
    fn new() -> Self { Self { data: vec![0u8; 0x10000] } }
}

impl BusRegion for WramRegion {
    fn read8(&self, offset: u32) -> u8 {
        self.data[offset as usize]
    }
    fn write8(&mut self, offset: u32, val: u8) {
        self.data[offset as usize] = val;
    }
}

/// Battery-backed SRAM with write-protect ($D00000–$D0FFFF).
struct SramRegion {
    data: Vec<u8>,
    /// Controlled by REG_SRAMLOCK / REG_SRAMEN on the parent bus.
    pub(crate) writable: bool,
}

impl SramRegion {
    fn new() -> Self {
        Self { data: vec![0xFF; 0x10000], writable: false }
    }
}

impl BusRegion for SramRegion {
    fn read8(&self, offset: u32) -> u8 {
        self.data[offset as usize]
    }
    fn write8(&mut self, offset: u32, val: u8) {
        if self.writable {
            self.data[offset as usize] = val;
        }
    }
}

/// Memory card (2 KB, odd bytes only; even bytes are open-bus).
struct MemcardRegion {
    data: Vec<u8>,
}

impl MemcardRegion {
    fn new() -> Self { Self { data: vec![0u8; 0x800] } }

    fn read_odd(&self, word_idx: usize, _open_bus: u16) -> u8 {
        self.data.get(word_idx).copied().unwrap_or(0xFF)
    }

    fn write_odd(&mut self, word_idx: usize, val: u8) {
        if let Some(slot) = self.data.get_mut(word_idx) {
            *slot = val;
        }
    }
}

/// PROM region handling both the fixed low window ($000000–$0FFFFF when swp_rom)
/// and the banked window ($200000–$2FFFFF). Also responsible for cart intercepts
/// in those ranges and the bank select register.
///
/// Owns the bank base so that bank switching logic is encapsulated here
/// rather than mutating a flat field on the bus.
struct PromRegion {
    /// Current base offset into roms.p_rom for the $200000 window.
    bank_base: usize,
}

impl PromRegion {
    fn new() -> Self {
        Self { bank_base: 0x100000 }
    }

    fn read8(&self, address: u32, bus: &SystemBus, mode: AddressMode) -> u8 {
        let off = address as usize;
        if off < 0x80 {
            if mode.swp_rom && !bus.roms.p_rom.is_empty() {
                if let Some(byte) = bus.cart.intercept_read_8(address) {
                    return byte;
                }
                bus.roms.p_rom.get(off).copied().unwrap_or(0xFF)
            } else {
                let blen = bus.roms.bios_rom.len();
                if blen > 0 { bus.roms.bios_rom[off % blen] } else { 0xFF }
            }
        } else if !bus.roms.p_rom.is_empty() {
            if let Some(byte) = bus.cart.intercept_read_8(address) {
                return byte;
            }
            bus.roms.p_rom[off % bus.roms.p_rom.len()]
        } else {
            let blen = bus.roms.bios_rom.len();
            if blen > 0 { bus.roms.bios_rom[off % blen] } else { 0xFF }
        }
    }

    fn read_banked(&self, address: u32, bus: &SystemBus) -> u8 {
        if bus.roms.p_rom.is_empty() { return 0xFF; }
        let bank_off = self.bank_base + (address - PROM2_START) as usize;
        bus.roms.p_rom[bank_off % bus.roms.p_rom.len()]
    }

    fn handle_bank_select(&mut self, rom_len: usize, value: u16) {
        // P-ROM bank switch (FBNeo `Bankswitch()`):
        //   nBank = 0x100000 + ((value & 7) << 20)
        let bank = (value as usize) & 7;
        let bank_addr = 0x100000usize + (bank << 20);
        if rom_len > 0 {
            self.bank_base = if bank_addr >= rom_len {
                0x100000
            } else {
                bank_addr
            };
        }
    }

    fn bank_base(&self) -> usize {
        self.bank_base
    }

    fn set_bank_base(&mut self, base: usize) {
        self.bank_base = base;
    }
}

/// BIOS region for $C00000–$C7FFFF with the special vector overlay logic
/// (cart vectors visible in BIOS area when !swp_rom).
struct BiosRegion;

impl BiosRegion {
    fn read8(&self, address: u32, bus: &SystemBus, mode: AddressMode) -> u8 {
        if bus.roms.bios_rom.is_empty() { return 0xFF; }
        let off = (address - BIOS_START) as usize;
        // FBNeo NeoBiosVector overlay: in BIOS mode (swp_rom=false),
        // $C00000..$C0007F serves the cart's own vectors.
        if !mode.swp_rom && off < 0x80 && !bus.roms.p_rom.is_empty() {
            if let Some(byte) = bus.cart.intercept_read_8(address) {
                return byte;
            }
            return bus.roms.p_rom[off % bus.roms.p_rom.len()];
        }
        bus.roms.bios_rom[off % bus.roms.bios_rom.len()]
    }
}

/// I/O region ($300000–$3FFFFF). Contains all memory-mapped registers,
/// the 74HC259 system latch, sound IPC, RTC control, DIPs, etc.
/// Delegates to subcomponents (lspc, rtc, cart, input) on the parent bus.
struct IoRegion;

impl IoRegion {
    fn read8(address: u32, bus: &SystemBus) -> u8 {
        match address {
            a if a == REG_P1CNT => bus.input.p1,
            a if a == REG_DIPSW => bus.effective_hw_dips(),
            a if a == REG_SOUND => {
                if bus.sound_status & 1 != 0 {
                    bus.sound_reply
                } else {
                    bus.sound_reply & 0x7F
                }
            }
            a if a == REG_STATUS_A => {
                (bus.input.coin & 0x3F) | (bus.rtc.read() << 6)
            }
            a if a == REG_P2CNT   => bus.input.p2,
            a if a == REG_STATUS_B => bus.input.sys,
            _ => 0xFF,
        }
    }

    fn write8(address: u32, value: u8, bus: &mut SystemBus) {
        match address {
            a if a == REG_SOUND => {
                bus.sound_cmd     = value;
                bus.sound_status &= !1;
                bus.nmi_request   = true;
                // log::trace!("[68K→Z80] sound_cmd=${:02X}", value);
            }
            a if a == REG_NOSHADOW => bus.lspc.shadow = false,
            a if a == REG_SHADOW   => bus.lspc.shadow = true,
            a if a == REG_SWPBIOS  => { bus.swp_rom = false; }
            a if a == REG_SWPROM   => {
                bus.swp_rom = true;
                // Cart is about to run — force BIOS_MVS_FLAG for AES mode
                // right now so the game's first read (which can happen inside
                // the same frame) already sees "home".
                bus.sync_aes_bios_ram();
            }
            a if a == REG_BRDFIX   => {
                bus.lspc.brd_fix = true;
                if !bus.roms.sm1_rom.is_empty() {
                    bus.pending_m1_rom = Some(bus.roms.sm1_rom.clone());
                }
            }
            a if a == REG_CRTFIX => {
                bus.lspc.brd_fix = false;
                if !bus.roms.m1_rom.is_empty() {
                    bus.pending_m1_rom = Some(bus.roms.m1_rom.clone());
                }
            }
            a if a == REG_PALBANK0 => bus.lspc.pal_bank = false,
            a if a == REG_PALBANK1 => bus.lspc.pal_bank = true,
            a if a == REG_SRAMLOCK => bus.sram.writable = false,
            a if a == REG_SRAMEN   => bus.sram.writable = true,
            a if a == REG_RTCCTRL  => {
                bus.rtc.write(
                    (value & 0x02) != 0,
                    (value & 0x04) != 0,
                    (value & 0x01) != 0,
                );
            }
            a if a == REG_DIPSW => {
                // log::trace!("Watchdog fed via write to 300001");
            }
            _ => {}
        }
    }
}

// ── ROM image collection ──────────────────────────────────────────────────────

/// All ROM images used during a session.
///
/// ROMs are read-only during emulation and are **not** included in save states
/// (they are reloaded from disk on startup).
pub struct RomImages {
    /// Cartridge P-ROM (game program code, mapped at `$000000` when `swp_rom = true`).
    pub p_rom:    Vec<u8>,
    /// BIOS ROM (sp-u2.sp1 / sp-s2.sp1), mapped at `$C00000` and at `$000000`
    /// during the initial boot sequence before `REG_SWPROM` is written.
    pub bios_rom: Vec<u8>,
    /// System fix-layer tile ROM (sfix.sfix), used when `brd_fix = true`.
    pub sfix_rom: Vec<u8>,
    /// System Z80 BIOS ROM (sm1.sm1), loaded into Z80 address space on
    /// `REG_BRDFIX` writes (board mode).
    pub sm1_rom:  Vec<u8>,
    /// Z80 sound-program ROM — starts as `sm1.sm1`, then swapped to the game
    /// `.m1` when `REG_CRTFIX` is written.
    pub m1_rom:   Vec<u8>,
    /// Cartridge fix-layer tile ROM (.s1 / s_rom), used when `brd_fix = false`.
    pub s_rom:    Vec<u8>,
    /// Cartridge sprite tile ROMs (C1+C2 interleaved per byte).
    pub c_rom:    Vec<u8>,
    /// V-zoom hardware lookup table ROM (`000-lo.lo`, 128 KB).
    /// Indexed as `lo_rom[vshrink * 256 + dest_y]`; value encodes the source
    /// tile row: `src_tile = value >> 4`, `src_row = value & 0x0F`.
    /// `0xFF` means skip that destination line.
    pub lo_rom:   Vec<u8>,
}

impl RomImages {
    pub fn new() -> Self {
        RomImages {
            p_rom:    Vec::new(),
            bios_rom: Vec::new(),
            sfix_rom: Vec::new(),
            sm1_rom:  Vec::new(),
            m1_rom:   Vec::new(),
            s_rom:    Vec::new(),
            c_rom:    Vec::new(),
            lo_rom:   Vec::new(),
        }
    }
}

impl Default for RomImages {
    fn default() -> Self { Self::new() }
}

/// Zero-copy view of the bus fields required by `VideoController::render()`.
///
/// Constructed by `SystemBus::video_snapshot`.  Passing this struct rather
/// than seven separate parameters keeps the render call-site readable and
/// prevents the renderer from accidentally mutating bus state.
pub struct VideoSnapshot<'a> {
    /// 68 KB VRAM (SCB1 + fix map + SCB2–4 fast area).
    pub vram:     &'a [u8],
    /// 16 KB palette RAM (both banks).  `pal_bank` indicates the active one.
    pub pal_ram:  &'a [u8],
    /// System fix tile ROM (sfix.sfix), active when `brd_fix = true`.
    pub sfix_rom: &'a [u8],
    /// Cartridge S-ROM (game fix tiles), active when `brd_fix = false`.
    pub s_rom:    &'a [u8],
    /// Cartridge C-ROM (sprite tiles, C1+C2 interleaved).
    pub c_rom:    &'a [u8],
    /// Active palette bank: `false` = bank 0, `true` = bank 1.
    pub pal_bank: bool,
    /// `true` → use system SFIX; `false` → use cartridge S-ROM.
    pub brd_fix:  bool,
    /// Current LSPC auto-animation frame counter (0–7).
    /// Substituted into tile codes for sprites with auto-animation enabled.
    pub auto_anim_frame: u8,
    /// V-zoom hardware LUT (`000-lo.lo`).
    pub lo_rom: &'a [u8],
    /// Global shadow mode (REG_SHADOW / 0x3A0011): when `true`, all displayed
    /// colours are rendered at ~55.7% brightness via a 150Ω DAC pulldown.
    pub shadow: bool,
}

// ── SystemBus ─────────────────────────────────────────────────────────────────

/// NeoGeo system backplane.
///
/// Owns every hardware component and implements the full 68K address-decode
/// for read/write/IO accesses.  The two main entry points for the 68K core
/// are `read_8` / `write_8` / `read_16` / `write_16` / `read_32` /
/// `write_32`; Z80 sound firmware communicates through the IPC latches
/// (`sound_cmd`, `sound_reply`, `sound_status`, `nmi_request`).
pub struct SystemBus {
    // ── ROM images (not in save state) ────────────────────────────────────────
    /// All ROM data for the current session.
    pub roms: RomImages,

    // ── LSPC-2 video controller ────────────────────────────────────────────────
    /// Video controller: VRAM, palette RAM, raster timer, beam counter, IRQs.
    pub lspc: Lspc,

    // ── Real-time clock ────────────────────────────────────────────────────────
    /// NEC uPD4990A serial real-time clock / calendar.
    pub rtc: Upd4990a,

    // ── General-purpose RAM (now behind region objects for decode encapsulation) ─
    /// Work RAM region ($100000–$10FFFF).  Use `.work_ram()` for read access
    /// from game observers / debug code.
    wram: WramRegion,

    /// Battery-backed SRAM region with write protect ($D00000–$D0FFFF).
    sram: SramRegion,

    /// Memory card region (owns the 2 KB; odd-byte access rules live here).
    mem_card: MemcardRegion,

    /// PROM address space (owns banked window state and cart intercept logic
    /// for the cartridge P-ROM ranges).
    prom: PromRegion,

    /// BIOS address space (owns the vector overlay logic for $C00000 area).
    bios: BiosRegion,

    // ── Hardware DIP switches ──────────────────────────────────────────────────
    /// Active-low hardware DIP switch byte exposed on `REG_DIPSW` (`$300001`).
    ///
    /// Default `0xFF` = all switches OFF = normal MVS operation.
    /// Bit 0 (SW1) = `0` → BIOS enters the operator service menu on boot.
    pub hw_dips: u8,

    // ── 74HC259 system-latch flags ────────────────────────────────────────────
    /// `true` = cartridge P-ROM maps to `$000000`.
    /// `false` = BIOS mirrors to `$000000` (power-on default).
    pub swp_rom: bool,

    // ── Z80 inter-processor communication ─────────────────────────────────────
    /// Byte written by the 68K to `REG_SOUND`; read by the Z80 on port `$00`.
    pub sound_cmd: u8,

    /// Byte written by the Z80 on port `$0C`; read by the 68K from `REG_SOUND`.
    pub sound_reply: u8,

    /// Sound status register: bit 0 = Z80 ready (has ack'd the last command).
    /// When bit 0 = `0`, bit 7 of `sound_reply` is masked on 68K reads.
    pub sound_status: u8,

    /// Set when the 68K writes `REG_SOUND`; cleared by `Z80::execute` on NMI
    /// delivery.  Drives the NMI edge sent to the sound CPU.
    pub nmi_request: bool,

    /// Pending M1-ROM swap queued by `REG_BRDFIX` / `REG_CRTFIX` writes.
    /// Drained (and applied) by `Z80::execute` at the start of each tick.
    pub pending_m1_rom: Option<Vec<u8>>,

    // ── P-ROM bank ────────────────────────────────────────────────────────────
    /// Byte offset into `roms.p_rom` that the `$200000–$2FFFFF` window maps to.
    /// Default `0x100000` (bank 1 = first byte of the extension ROM).
    pub p_rom_bank_base: usize,

    // ── Cartridge handler ─────────────────────────────────────────────────────
    /// Game-specific ROM post-processing and protection-chip emulation.
    /// Defaults to a plain `cart::ComposedCart`; `romset` overrides this
    /// before calling `load_p_rom_bytes`.
    pub cart: Box<dyn Cartridge>,

    // ── Input state ────────────────────────────────────────────────────────────
    /// When set, forces REG_STATUS_B bit 7 each frame: `true` = AES (bit low),
    /// `false` = MVS (bit high).  KOF98 practice mode reads this at runtime.
    pub presentation_aes: Option<bool>,
    /// Current controller / system input state, updated each frame by the
    /// platform layer via `apply_input`.
    pub input: InputState,
    /// Open bus value (last 16-bit access).
    pub open_bus: u16,


}

/// Which M1 ROM is currently (or pending) active for the Z80.
/// Used for clean save/restore reconstruction instead of magic numbers + pointer compares.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum ActiveM1Rom {
    None = 0,
    Sm1 = 1,
    Cart = 2,
}

impl Default for ActiveM1Rom {
    fn default() -> Self { ActiveM1Rom::None }
}

/// Lightweight view of mode state that affects address decoding (vector overlays,
/// etc.). Regions receive this instead of the full bus where possible, reducing
/// coupling and making the "mode" explicit.
#[derive(Clone, Copy, Default)]
pub struct AddressMode {
    pub swp_rom: bool,
}

impl SystemBus {
    pub fn mode(&self) -> AddressMode {
        AddressMode { swp_rom: self.swp_rom }
    }
}

impl SystemBus {
    /// Construct a fully reset bus with no ROMs loaded.
    pub fn new() -> Self {
        Self {
            roms:         RomImages::new(),
            lspc:         Lspc::new(),
            rtc:          Upd4990a::new(),
            wram:         WramRegion::new(),
            sram:         SramRegion::new(),
            mem_card:     MemcardRegion::new(),
            prom:         PromRegion::new(),
            bios:         BiosRegion,
            hw_dips:      0xFF,
            swp_rom:      false,
            sound_cmd:    0,
            sound_reply:  0,
            sound_status: 1,   // 1 = Z80 ready (no pending command)
            nmi_request:     false,
            pending_m1_rom:  None,
            p_rom_bank_base: 0x100000, // kept synced from prom for pub API compat
            cart:         Box::new(cart::ComposedCart::new(None, None)),
            presentation_aes: None,
            input:        InputState::default(),
            open_bus:     0,

        }
    }

    /// Reset internal bus state (called on system reset).
    pub fn reset(&mut self) {
        self.lspc = Lspc::new();
        self.swp_rom = false;
        self.sound_cmd = 0;
        self.sound_reply = 0;
        self.sound_status = 1;
        self.nmi_request = false;
        self.pending_m1_rom = None;
        self.prom = PromRegion::new();
        self.p_rom_bank_base = 0x100000;
        self.cart.reset();
        self.open_bus = 0;
    }

    // ── ROM load helpers ──────────────────────────────────────────────────────

    /// Load and validate the system BIOS ROM from a filesystem path.
    ///
    /// Prefer [`Self::load_host_system_roms`] for the standard host layout.
    /// Auto-detects and corrects byte-swapped images.
    pub fn load_bios(&mut self, path: &str) -> Result<(), String> {
        self.roms.bios_rom = fs::read(path)
            .map_err(|e| format!("cannot read BIOS '{}': {}", path, e))?;

        // Auto-detect and correct byte-swapped BIOS images.
        if self.roms.bios_rom.len() >= 4
            && (self.roms.bios_rom[0] == 0x10 || self.roms.bios_rom[0] == 0x11)
            && self.roms.bios_rom[1] == 0x00
        {
            log::debug!("BIOS appears byteswapped — correcting");
            for i in (0..self.roms.bios_rom.len() - 1).step_by(2) {
                self.roms.bios_rom.swap(i, i + 1);
            }
        }

        log::info!("BIOS loaded: {} ({} bytes)", path, self.roms.bios_rom.len());
        Ok(())
    }

    /// Load BIOS from a host-supplied byte slice (e.g. after reading a dump).
    /// Performs the same auto byteswap correction as the file version.
    pub fn load_bios_bytes(&mut self, data: &[u8]) -> Result<(), String> {
        self.roms.bios_rom = data.to_vec();

        // Auto-detect and correct byte-swapped BIOS images.
        if self.roms.bios_rom.len() >= 4
            && (self.roms.bios_rom[0] == 0x10 || self.roms.bios_rom[0] == 0x11)
            && self.roms.bios_rom[1] == 0x00
        {
            log::debug!("BIOS appears byteswapped — correcting");
            for i in (0..self.roms.bios_rom.len() - 1).step_by(2) {
                self.roms.bios_rom.swap(i, i + 1);
            }
        }

        log::info!("BIOS loaded from bytes ({} bytes)", self.roms.bios_rom.len());
        Ok(())
    }

    pub fn load_p_rom(&mut self, path: &str) -> Result<(), String> {
        self.roms.p_rom = fs::read(path)
            .map_err(|e| format!("cannot read P-ROM '{}': {}", path, e))?;
        cart::byteswap_p_rom_if_needed(&mut self.roms.p_rom);
        log::info!("P-ROM loaded: {} ({} bytes)", path, self.roms.p_rom.len());
        Ok(())
    }

    pub fn load_sfix(&mut self, path: &str) -> Result<(), String> {
        self.roms.sfix_rom = fs::read(path)
            .map_err(|e| format!("cannot read SFIX '{}': {}", path, e))?;
        log::info!("SFIX loaded: {} ({} bytes)", path, self.roms.sfix_rom.len());
        Ok(())
    }

    pub fn load_m1(&mut self, path: &str) -> Result<(), String> {
        self.roms.m1_rom = fs::read(path)
            .map_err(|e| format!("cannot read M1   '{}': {}", path, e))?;
        log::info!("M1 loaded: {} ({} bytes)", path, self.roms.m1_rom.len());
        Ok(())
    }

    /// Load M1 (Z80 program) from bytes. Used for game M1 or SM1 fallback.
    pub fn load_m1_bytes(&mut self, data: Vec<u8>) {
        self.roms.m1_rom = data;
        log::info!("M1 loaded ({} bytes)", self.roms.m1_rom.len());
    }

    /// Load the system Z80 BIOS ROM (sm1.sm1).
    pub fn load_sm1(&mut self, path: &str) -> Result<(), String> {
        self.roms.sm1_rom = fs::read(path)
            .map_err(|e| format!("cannot read SM1  '{}': {}", path, e))?;
        log::info!("SM1 loaded: {} ({} bytes)", path, self.roms.sm1_rom.len());
        Ok(())
    }

    /// Load the game cartridge S-ROM (fix-layer tiles).
    pub fn load_s_rom(&mut self, path: &str) -> Result<(), String> {
        self.roms.s_rom = fs::read(path)
            .map_err(|e| format!("cannot read S-ROM '{}': {}", path, e))?;
        log::info!("S-ROM (cart) loaded: {} ({} bytes)", path, self.roms.s_rom.len());
        Ok(())
    }



    /// Load the V-zoom hardware LUT ROM (`000-lo.lo`, 128 KB).
    pub fn load_lo_rom(&mut self, path: &str) -> Result<(), String> {
        self.roms.lo_rom = fs::read(path)
            .map_err(|e| format!("cannot read LO-ROM '{}': {}", path, e))?;
        log::info!("LO-ROM loaded: {} ({} bytes)", path, self.roms.lo_rom.len());
        Ok(())
    }

    /// Load SFIX from a host-supplied byte slice.
    pub fn load_sfix_bytes(&mut self, data: &[u8]) {
        self.roms.sfix_rom = data.to_vec();
        log::info!("SFIX loaded from bytes ({} bytes)", self.roms.sfix_rom.len());
    }

    /// Load SM1 (Z80 system BIOS) from a host-supplied byte slice.
    pub fn load_sm1_bytes(&mut self, data: &[u8]) {
        self.roms.sm1_rom = data.to_vec();
        log::info!("SM1 loaded from bytes ({} bytes)", self.roms.sm1_rom.len());
    }

    /// Load LO (V-zoom LUT) from a host-supplied byte slice.
    pub fn load_lo_rom_bytes(&mut self, data: &[u8]) {
        self.roms.lo_rom = data.to_vec();
        log::info!("LO-ROM loaded from bytes ({} bytes)", self.roms.lo_rom.len());
    }

    /// Load system ROMs (BIOS, sfix, sm1, lo) from host-provided directories.
    ///
    /// Searches `data/neogeo/` then `roms/neogeo/`. Never compiles dumps into
    /// the library — the host must place licensed dumps on disk.
    pub fn load_host_system_roms(&mut self) {
        for dir in [
            std::path::Path::new("data/neogeo"),
            std::path::Path::new("roms/neogeo"),
        ] {
            if self.try_load_system_roms_from_dir(dir).is_ok() {
                return;
            }
        }
        log::warn!(
            "no NeoGeo system ROMs found under data/neogeo/ or roms/neogeo/; \
             host must supply BIOS (and usually sfix/sm1/lo)"
        );
    }

    /// Load BIOS / sfix / sm1 / lo from a directory (user-supplied dumps).
    pub fn try_load_system_roms_from_dir(&mut self, dir: &std::path::Path) -> Result<(), String> {
        use std::fs;
        if !dir.is_dir() {
            return Err(format!("not a directory: {}", dir.display()));
        }
        let bios_names = ["sp-s2.sp1", "sp-e.sp1", "uni-bios.rom", "uni-bios_4_0.rom"];
        let mut loaded_bios = false;
        for name in bios_names {
            let p = dir.join(name);
            if p.is_file() {
                let bytes = fs::read(&p).map_err(|e| e.to_string())?;
                self.load_bios_bytes(&bytes)?;
                loaded_bios = true;
                break;
            }
        }
        if !loaded_bios {
            return Err(format!("no BIOS found in {}", dir.display()));
        }
        for (file, kind) in [
            ("sfix.sfix", "sfix"),
            ("sm1.sm1", "sm1"),
            ("000-lo.lo", "lo"),
        ] {
            let p = dir.join(file);
            if p.is_file() {
                let bytes = fs::read(&p).map_err(|e| e.to_string())?;
                match kind {
                    "sfix" => self.load_sfix_bytes(&bytes),
                    "sm1" => self.load_sm1_bytes(&bytes),
                    "lo" => self.load_lo_rom_bytes(&bytes),
                    _ => {}
                }
            }
        }
        Ok(())
    }


    /// Load a P-ROM from an already-read byte vector.
    ///
    /// Delegates post-processing (byteswap, decryption) to `self.cart`.
    pub fn load_p_rom_bytes(&mut self, data: Vec<u8>) {
        log::debug!("P-ROM: {} bytes (pre-processing)", data.len());
        self.roms.p_rom = self.cart.process_p_rom(data);
        log::info!("P-ROM: {} bytes loaded", self.roms.p_rom.len());
    }

    /// Load a combined C-ROM from an already-interleaved byte vector.
    pub fn load_c_rom_bytes(&mut self, data: Vec<u8>) {
        log::debug!("C-ROM: {} bytes (from ROM set)", data.len());
        self.roms.c_rom = data;
    }

    // ── 68K address decoder ───────────────────────────────────────────────────

    /// 8-bit read — the fundamental bus access.
    #[inline]
    pub fn read_8(&self, address: u32) -> u8 {
        let v = self.read_8_impl(address);
        crate::trace::check_read(address & 0x00FF_FFFF, v as u32, 8);
        v
    }

    fn read_8_impl(&self, address: u32) -> u8 {
        let address = address & 0x00FF_FFFF; // 68000 has a 24-bit address bus
        match address {
            PROM_START..=PROM_END => self.prom.read8(address, self, self.mode()),
            PROM2_START..=PROM2_END => self.prom.read_banked(address, self),
            WRAM_START..=WRAM_END => {
                let off = address - WRAM_START;
                self.wram.read8(off)
            }
            IO_START..=IO_END => IoRegion::read8(address, self),
            PAL_START..=PAL_END => {
                // The active 8 KB bank starts at byte offset bank * $2000.
                let bank_off = (self.lspc.pal_bank as usize) * 0x2000;
                self.lspc.pal_ram[bank_off + (address - PAL_START) as usize]
            }
            MEMCARD_START..=MEMCARD_END => {
                // Only odd byte addresses carry data; even bytes return open bus.
                if address & 1 == 1 {
                    let idx = ((address - MEMCARD_START) >> 1) as usize;
                    self.mem_card.read_odd(idx, self.open_bus)
                } else {
                    (self.open_bus >> 8) as u8
                }
            }
            BIOS_START..=BIOS_END => self.bios.read8(address, self, self.mode()),
            SRAM_START..=SRAM_END => {
                let off = address - SRAM_START;
                self.sram.read8(off)
            }
            _ => if address & 1 == 0 { (self.open_bus >> 8) as u8 } else { (self.open_bus & 0xFF) as u8 }, // unmapped — open bus
        }
    }

    /// 16-bit big-endian read.
    ///
    /// LSPC registers are handled here directly because they are always
    /// accessed as 16-bit words by the BIOS and game code.
    pub fn read_16(&mut self, address: u32) -> u16 {
        let address = address & 0x00FF_FFFF;
        let value = match address {
            a if a == REG_VRAMRW   => {
                // Reading VRAM does not auto-increment the address pointer.
                self.lspc.vram_read_word()
            }
            a if a == REG_LSPCMODE => self.lspc.lspc_mode_read(),
            _ => {
                let hi = self.read_8(address)     as u16;
                let lo = self.read_8(address + 1) as u16;
                (hi << 8) | lo
            }
        };
        self.open_bus = value;
        value
    }

    /// 32-bit big-endian read (two consecutive 16-bit reads).
    pub fn read_32(&mut self, address: u32) -> u32 {
        let hi = self.read_16(address)     as u32;
        let lo = self.read_16(address + 2) as u32;
        (hi << 16) | lo
    }

    // ── Write ─────────────────────────────────────────────────────────────────

    /// 8-bit write.
    #[inline]
    pub fn write_8(&mut self, address: u32, value: u8) {
        crate::trace::check_write(address & 0x00FF_FFFF, value as u32, 8);
        self.write_8_impl(address, value);
    }

    fn write_8_impl(&mut self, address: u32, value: u8) {
        let address = address & 0x00FF_FFFF;
        match address {
            WRAM_START..=WRAM_END => {
                let off = address - WRAM_START;
                self.wram.write8(off, value);
            }
            SRAM_START..=SRAM_END => {
                let off = address - SRAM_START;
                self.sram.write8(off, value);
            }
            MEMCARD_START..=MEMCARD_END => {
                if address & 1 == 1 {
                    let idx = ((address - MEMCARD_START) >> 1) as usize;
                    self.mem_card.write_odd(idx, value);
                }
            }
            IO_START..=IO_END => IoRegion::write8(address, value, self),
            PAL_START..=PAL_END => {
                let bank_off = (self.lspc.pal_bank as usize) * 0x2000;
                let idx      = bank_off + (address - PAL_START) as usize;
                self.lspc.pal_write(idx, value);
            }
            a if (REG_BANKSEL_START..=REG_BANKSEL_END).contains(&a) => {
                // Many games (like KOF '97) bankswitch using byte-writes (move.b)
                self.write_16(address, value as u16);
            }
            _ => {
                self.cart.on_write_8(address, value);
            }
        }
    }

    /// 16-bit big-endian write.
    ///
    /// LSPC register writes are handled here directly for efficiency and
    /// to avoid the overhead of splitting into two byte writes.
    #[inline]
    pub fn write_16(&mut self, address: u32, value: u16) {
        crate::trace::check_write(address & 0x00FF_FFFF, value as u32, 16);
        self.write_16_impl(address, value);
    }

    fn write_16_impl(&mut self, address: u32, value: u16) {
        let address = address & 0x00FF_FFFF;
        match address {
            a if a == REG_VRAMADDR => {
                // Full 16-bit address: $0000–$7FFF = slow VRAM; $8000+ = fast VRAM.
                self.lspc.vram_addr = value;
            }
            a if a == REG_VRAMRW => {
                self.lspc.vram_write_word(value);
                self.lspc.vram_addr = self.lspc.vram_addr
                    .wrapping_add(self.lspc.vram_mod);
            }
            a if a == REG_VRAMMOD => {
                self.lspc.vram_mod = value;
            }
            a if a == REG_LSPCMODE => {
                // bits\[15:8\] = auto-animation speed (frame advances every speed+1 VBLs)
                // bits\[7:0\]  = IRQ control (bit 4 = raster timer IRQ enable)
                self.lspc.auto_anim_speed = (value >> 8) as u8;
                self.lspc.irq_control     = (value & 0xFF) as u8;
            }
            a if a == REG_TIMERHIGH => {
                self.lspc.timer_reload =
                    (self.lspc.timer_reload & 0x0000_FFFF) | ((value as u32) << 16);
            }
            a if a == REG_TIMERLOW => {
                self.lspc.timer_reload =
                    (self.lspc.timer_reload & 0xFFFF_0000) | (value as u32);
                // Writing the low word loads the counter and starts the timer.
                self.lspc.timer_counter = self.lspc.timer_reload;
                self.lspc.timer_active  = true;
            }
            a if a == REG_IRQACK => {
                // Acknowledge interrupt sources: clear the indicated bits.
                self.lspc.irq_ack(value as u8);
            }
            a if a == REG_TIMERSTOP => {
                self.lspc.timer_stop_in_border = (value & 1) != 0;
            }
            a if (REG_BANKSEL_START..=REG_BANKSEL_END).contains(&a) => {
                self.prom.handle_bank_select(self.roms.p_rom.len(), value);
                self.p_rom_bank_base = self.prom.bank_base(); // keep flat in sync for compat
            }
            _ => {
                // Let the cart handler try game-specific registers first
                // (e.g. KOF98 ALTERA write at $20AAAA).
                if !self.cart.on_write_16(address, value) {
                    self.write_8(address,     (value >> 8)   as u8);
                    self.write_8(address + 1, (value & 0xFF) as u8);
                }
            }
        }
        self.open_bus = value;
    }

    /// 32-bit big-endian write (two consecutive 16-bit writes).
    pub fn write_32(&mut self, address: u32, value: u32) {
        self.write_16(address,     (value >> 16)    as u16);
        self.write_16(address + 2, (value & 0xFFFF) as u16);
    }

    // ── Input ─────────────────────────────────────────────────────────────────

    /// Latch AES/MVS presentation for the session (survives `reset`).
    ///
    /// Must be called **before** `reset`; after reset the BIOS will write the
    /// correct value into `BIOS_MVS_FLAG` based on `REG_STATUS_B` bit 7,
    /// and `sync_aes_bios_ram` will then keep it clear once the cart takes over.
    pub fn set_presentation_aes(&mut self, aes: bool) {
        self.presentation_aes = Some(aes);
        if aes {
            self.input.sys &= !0x80;
        } else {
            self.input.sys |= 0x80;
        }
    }

    /// `REG_DIPSW` byte presented to the 68K (latched per presentation mode).
    pub fn effective_hw_dips(&self) -> u8 {
        match self.presentation_aes {
            Some(true) => AES_TRAINING_HW_DIPS,
            Some(false) | None => self.hw_dips,
        }
    }

    fn merge_presentation_sys(&self, sys: u8) -> u8 {
        match self.presentation_aes {
            Some(true) => sys & !0x80,
            Some(false) => sys | 0x80,
            None => sys,
        }
    }

    /// Force `BIOS_MVS_FLAG` to 0 while presenting AES so games see "home".
    ///
    /// Only runs after the BIOS handed control to the cart (`swp_rom = true`);
    /// during POST the BIOS uses that byte as a RAM test target and would
    /// error out if we clamped it.
    fn sync_aes_bios_ram(&mut self) {
        if self.presentation_aes == Some(true)
            && self.swp_rom
            && self.wram.data.len() > BIOS_MVS_FLAG_OFF
        {
            self.wram.data[BIOS_MVS_FLAG_OFF] = 0;
        }
    }

    /// Apply one frame of player input from the platform layer.
    pub fn apply_input(&mut self, state: InputState) {
        let mut state = state;
        state.sys = self.merge_presentation_sys(state.sys);
        self.input = state;
        self.sync_aes_bios_ram();
    }

    // ── Region accessors (for observers, debug, and game-specific code) ───────

    /// Borrow the 64 KB work RAM.
    pub fn work_ram(&self) -> &[u8] {
        &self.wram.data
    }

    /// Mutable borrow of the 64 KB work RAM (e.g. for BIOS credit patching after state load).
    pub fn work_ram_mut(&mut self) -> &mut [u8] {
        &mut self.wram.data
    }

    /// Borrow the 64 KB backup SRAM.
    pub fn backup_ram(&self) -> &[u8] {
        &self.sram.data
    }

    /// Mutable borrow for loading SRAM (e.g. from .sram files).
    pub fn backup_ram_mut(&mut self) -> &mut [u8] {
        &mut self.sram.data
    }

    // ── IRQ facade (delegates to Lspc) ────────────────────────────────────────

    /// Raise one or more interrupt sources.  See `Lspc::raise_irq` for the
    /// bit-to-level mapping.
    ///
    /// Typical call from the main loop: `bus.raise_irq(0x04)` for V-blank.
    #[inline]
    pub fn raise_irq(&mut self, mask: u8) {
        self.lspc.raise_irq(mask);
    }

    /// Return the 68K interrupt level the bus wants to assert (0 = none).
    #[inline]
    pub fn irq_level(&self) -> u32 {
        self.lspc.irq_level()
    }

    // ── Timing facade (delegates to Lspc) ─────────────────────────────────────

    /// Advance beam counter and raster timer by `m68k_cycles` 68K cycles.
    ///
    /// Called from the main loop after each batch of CPU cycles.
    #[inline]
    pub fn tick_timer(&mut self, m68k_cycles: u32) {
        self.lspc.tick(m68k_cycles);
    }

    // ── Video snapshot ────────────────────────────────────────────────────────

    /// Build a zero-copy view of the bus fields needed by `VideoController::render()`.
    pub fn video_snapshot(&self) -> VideoSnapshot<'_> {
        VideoSnapshot {
            vram:     &self.lspc.vram,
            pal_ram:  &self.lspc.pal_ram,
            sfix_rom: &self.roms.sfix_rom,
            s_rom:    &self.roms.s_rom,
            c_rom:    &self.roms.c_rom,
            pal_bank: self.lspc.pal_bank,
            brd_fix:  self.lspc.brd_fix,
            auto_anim_frame: self.lspc.auto_anim_frame,
            lo_rom: &self.roms.lo_rom,
            shadow: self.lspc.shadow,
        }
    }

    // ── Save state ────────────────────────────────────────────────────────────

    /// Encode which M1 ROM (if any) is queued for the next Z80 bank command.
    fn pending_m1_swap_code(&self) -> ActiveM1Rom {
        match &self.pending_m1_rom {
            None => ActiveM1Rom::None,
            Some(rom) if !self.roms.sm1_rom.is_empty()
                && rom.as_ptr() == self.roms.sm1_rom.as_ptr() => ActiveM1Rom::Sm1,
            Some(rom) if !self.roms.m1_rom.is_empty()
                && rom.as_ptr() == self.roms.m1_rom.as_ptr() => ActiveM1Rom::Cart,
            // Fallback: compare by content.
            Some(rom) => {
                if *rom == self.roms.sm1_rom { ActiveM1Rom::Sm1 }
                else if *rom == self.roms.m1_rom { ActiveM1Rom::Cart }
                else { ActiveM1Rom::None }
            }
        }
    }

    /// Capture all mutable bus state into a `BusSnapshot`.
    pub fn snapshot(&self) -> crate::save_state::BusSnapshot {
        use crate::save_state::BusSnapshot;
        BusSnapshot {
            work_ram:        self.wram.data.clone(),
            backup_ram:      self.sram.data.clone(),
            mem_card:        self.mem_card.data.clone(),
            lspc:            self.lspc.clone(),
            rtc:             self.rtc.clone(),
            swp_rom:         self.swp_rom,
            sound_cmd:       self.sound_cmd,
            sound_reply:     self.sound_reply,
            sound_status:    self.sound_status,
            nmi_request:     self.nmi_request,
            p_rom_bank_base: self.prom.bank_base() as u32,
            pending_m1_swap: self.pending_m1_swap_code() as u8,
            sram_writable:   self.sram.writable,
            open_bus:        self.open_bus,
            hw_dips:         self.hw_dips,
            cart_state:      self.cart.snapshot(),
            input:           self.input.clone(),
        }
    }

    /// Restore all mutable bus state from a `BusSnapshot`.
    ///
    /// ROM images (already loaded) are used to reconstruct `pending_m1_rom`.
    pub fn restore(&mut self, snap: crate::save_state::BusSnapshot) {
        self.wram.data       = snap.work_ram;
        self.sram.data       = snap.backup_ram;
        self.mem_card.data   = snap.mem_card;
        self.lspc            = snap.lspc;
        self.rtc             = snap.rtc;
        self.swp_rom         = snap.swp_rom;
        self.sound_cmd       = snap.sound_cmd;
        self.sound_reply     = snap.sound_reply;
        self.sound_status    = snap.sound_status;
        self.nmi_request     = snap.nmi_request;
        self.p_rom_bank_base = snap.p_rom_bank_base as usize;
        self.prom.set_bank_base(snap.p_rom_bank_base as usize); // region owns the authoritative copy
        self.sram.writable   = snap.sram_writable;
        self.open_bus        = snap.open_bus;
        self.hw_dips         = snap.hw_dips;
        self.cart.restore(&snap.cart_state);
        // Active ROM is reconstructed from `Z80Snapshot::active_m1_rom`; an
        // outstanding swap must not survive restore or the next Z80 batch will
        // `signal_reset()` and destroy the deserialized CPU state.
        self.pending_m1_rom = None;
        self.input = snap.input;
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_bus() -> SystemBus {
        let mut bus = SystemBus::new();
        // Minimal BIOS — 512 KB, all 0xFF (open bus pattern).
        bus.roms.bios_rom = vec![0xFF; 0x80000];
        bus
    }

    // ── Work RAM ──────────────────────────────────────────────────────────────

    #[test]
    fn wram_write_read_roundtrip() {
        let mut bus = make_bus();
        bus.write_8(0x100042, 0xAB);
        assert_eq!(bus.read_8(0x100042), 0xAB);
    }

    #[test]
    fn wram_16bit_roundtrip() {
        let mut bus = make_bus();
        bus.write_16(0x100100, 0xDEAD);
        assert_eq!(bus.read_16(0x100100), 0xDEAD);
    }

    // ── BIOS mirror at boot ───────────────────────────────────────────────────

    #[test]
    fn bios_mirrors_to_000000_before_swprom() {
        let mut bus = make_bus();
        bus.roms.bios_rom[0] = 0x11;
        bus.roms.bios_rom[1] = 0x22;
        // swp_rom = false (boot default) → BIOS mirrors to $000000.
        assert_eq!(bus.read_8(0x000000), 0x11);
        assert_eq!(bus.read_8(0x000001), 0x22);
    }

    #[test]
    fn prom_maps_after_swprom() {
        let mut bus = make_bus();
        bus.roms.p_rom    = vec![0x55; 0x10000];
        bus.roms.p_rom[0] = 0xAA;
        bus.swp_rom = true;
        assert_eq!(bus.read_8(0x000000), 0xAA, "P-ROM byte 0 should be visible after swp_rom");
    }

    // ── Backup SRAM ───────────────────────────────────────────────────────────

    #[test]
    fn sram_default_is_ff() {
        let bus = make_bus();
        // An uninitialised SRAM should read $FF (dead-battery default).
        assert_eq!(bus.backup_ram()[0], 0xFF);
        assert_eq!(bus.read_8(SRAM_START), 0xFF);
    }

    #[test]
    fn sram_write_read_roundtrip() {
        let mut bus = make_bus();
        // SRAM is write-protected at boot; must enable via REG_SRAMEN first.
        bus.sram.writable = true;
        bus.write_8(0xD00010, 0x42);
        assert_eq!(bus.read_8(0xD00010), 0x42);
    }

    #[test]
    fn sram_write_protect_blocks_writes() {
        let mut bus = make_bus();
        // Default: write-protected. Writes must be silently ignored.
        bus.write_8(0xD00010, 0x42);
        assert_eq!(bus.read_8(0xD00010), 0xFF, "SRAM should still be 0xFF when write-protected");
    }

    // ── Memory card ───────────────────────────────────────────────────────────

    #[test]
    fn memcard_odd_bytes_accessible() {
        let mut bus = make_bus();
        bus.write_8(0x800001, 0x7E); // odd address
        assert_eq!(bus.read_8(0x800001), 0x7E);
    }

    #[test]
    fn memcard_even_bytes_are_open_bus() {
        let mut bus = make_bus();
        bus.open_bus = 0xABCD;
        assert_eq!(bus.read_8(0x800000), 0xAB, "even memcard byte = open bus");
    }

    // ── VRAM auto-increment via bus write_16 ─────────────────────────────────

    #[test]
    fn vram_write_16_auto_increments_addr() {
        let mut bus = make_bus();
        bus.write_16(REG_VRAMADDR, 0x0000); // set VRAM address
        bus.write_16(REG_VRAMMOD,  4);      // set step
        bus.write_16(REG_VRAMRW,   0x1234); // write → auto-increments by 4
        // After the write, reading VRAMADDR should return 4.
        assert_eq!(bus.lspc.vram_addr, 4, "VRAM address should have advanced by vram_mod");
    }

    // ── IRQ / sound IPC ──────────────────────────────────────────────────────

    #[test]
    fn sound_cmd_write_sets_nmi_request() {
        let mut bus = make_bus();
        bus.write_8(REG_SOUND, 0x5A);
        assert_eq!(bus.sound_cmd, 0x5A);
        assert!(bus.nmi_request, "sound cmd write must assert nmi_request");
    }

    // ── P-ROM bank switching ─────────────────────────────────────────────────

    #[test]
    fn prom_bank_select_affects_200000_window() {
        let mut bus = make_bus();
        // 3 MB image so we have room for bank 0 (0x100000) and bank 1 (0x200000) views
        bus.roms.p_rom = vec![0u8; 0x300000];
        // Marker visible when bank base = 0x100000 (typical "bank 0" selectable)
        bus.roms.p_rom[0x100000] = 0xAA;
        bus.roms.p_rom[0x100001] = 0x55;
        // Marker for next bank base 0x200000
        bus.roms.p_rom[0x200000] = 0xBB;
        bus.roms.p_rom[0x200001] = 0x66;

        // Default is 0x100000
        assert_eq!(bus.read_8(0x200000), 0xAA, "default bank base should expose 0x100000 image offset");

        // Write to banksel with value whose low bits = 1 → 0x100000 + 1<<20 = 0x200000
        bus.write_16(0x2FFFF0, 1);
        assert_eq!(bus.read_8(0x200000), 0xBB);
        assert_eq!(bus.read_8(0x200001), 0x66);

        // Back to "bank 0" view
        bus.write_16(0x2FFFF2, 0);
        assert_eq!(bus.read_8(0x200000), 0xAA);
    }

    // ── Palette banking ──────────────────────────────────────────────────────

    #[test]
    fn palette_bank_switch_affects_400000_reads() {
        let mut bus = make_bus();
        // Write distinct values into both banks via the active mapping
        bus.lspc.pal_bank = false;
        bus.write_8(0x400000, 0x11);
        bus.lspc.pal_bank = true;
        bus.write_8(0x400000, 0x22);

        bus.lspc.pal_bank = false;
        assert_eq!(bus.read_8(0x400000), 0x11, "bank 0 visible");
        bus.lspc.pal_bank = true;
        assert_eq!(bus.read_8(0x400000), 0x22, "bank 1 visible");
    }

    // ── Open bus behavior for unmapped areas ─────────────────────────────────

    #[test]
    fn unmapped_areas_return_open_bus() {
        let mut bus = make_bus();
        bus.open_bus = 0xBEEF;
        // Some high unmapped address (outside all defined ranges)
        assert_eq!(bus.read_8(0xE00000), 0xBE);
        assert_eq!(bus.read_8(0xE00001), 0xEF);
    }

    #[test]
    fn presentation_latch_forces_aes_mvs_on_status_b() {
        use crate::io::InputState;

        let mut bus = make_bus();
        bus.set_presentation_aes(true);
        bus.apply_input(InputState::default());
        assert_eq!(bus.input.sys & 0x80, 0, "AES: bit 7 low");
        assert_eq!(bus.read_8(REG_STATUS_B), bus.input.sys);
        assert_eq!(bus.read_8(REG_DIPSW), AES_TRAINING_HW_DIPS, "training freeplay dip");

        bus.set_presentation_aes(false);
        bus.apply_input(InputState::default());
        assert_eq!(bus.input.sys & 0x80, 0x80, "MVS: bit 7 high");
        assert_eq!(bus.read_8(REG_STATUS_B), bus.input.sys);
        assert_eq!(bus.read_8(REG_DIPSW), 0xFF, "MVS default dips");
    }

    #[test]
    fn aes_presentation_clears_bios_mvs_flag_after_cart_swap() {
        use crate::io::InputState;

        let mut bus = make_bus();
        bus.wram.data[BIOS_MVS_FLAG_OFF] = 0x80;
        bus.set_presentation_aes(true);
        // While in BIOS mode (swp_rom = false) the flag must be left alone so
        // the BIOS RAM POST can use $10FD82 as a scratch target.
        bus.apply_input(InputState::default());
        assert_eq!(bus.wram.data[BIOS_MVS_FLAG_OFF], 0x80);

        // Once the BIOS hands control to the cart the flag is forced to 0.
        bus.swp_rom = true;
        bus.apply_input(InputState::default());
        assert_eq!(bus.wram.data[BIOS_MVS_FLAG_OFF], 0);
    }
}
