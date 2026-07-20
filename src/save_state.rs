//! Save-state infrastructure.
//!
//! A save state is a complete snapshot of all mutable emulator state at a
//! single frame boundary.  ROM images are **not** included — they are reloaded
//! from disk on startup.
//!
//! # Formats
//!
//! | Method | Format | Use-case |
//! |---|---|---|
//! | `SaveState::to_bytes` / `SaveState::from_bytes` | Binary (bincode) | Rollback netcode, fast snapshots |
//! | `SaveState::write_to_file` / `SaveState::read_from_file` | JSON | Debugging, human-readable inspection |
//!
//! # Rollback-capable workflow
//!
//! ```no_run
//! # use emufight::save_state::SaveState;
//! # let mut emu = emufight::Emulator::new();
//! let blob = emu.save_state_to_bytes().unwrap(); // < 1 ms, ~120–200 KB
//! // ... advance several frames, then roll back:
//! // emu.load_state_from_bytes(&blob).unwrap();
//! ```

use serde::{Serialize, Deserialize};
use crate::neogeo::bus::lspc::Lspc;
use crate::neogeo::bus::rtc::Upd4990a;

// ── Snapshotable trait ────────────────────────────────────────────────────────

/// A hardware component that can produce and restore a serialisable snapshot.
///
/// Implementing this trait is the contract for being included in a save state.
/// Each subsystem owns the definition of its `Snap` type.
pub trait Snapshotable {
    /// The serialisable snapshot type for this component.
    type Snap: Serialize + for<'de> Deserialize<'de>;

    /// Capture the component's current state into a `Snap`.
    fn snapshot(&self) -> Self::Snap;

    /// Restore the component's state from a `Snap`, consuming it.
    fn restore(&mut self, snap: Self::Snap);
}

// ── M68K CPU snapshot ─────────────────────────────────────────────────────────

/// Complete snapshot of the Motorola 68000 CPU register file.
///
/// All fields map 1:1 to public fields in `m68k_cpu::CpuCore`.  The FPU
/// register set is omitted — the NeoGeo uses a plain MC68000 which has no FPU.
#[derive(Serialize, Deserialize)]
pub struct M68kSnapshot {
    /// Data registers D0–D7 and address registers A0–A7.
    pub dar:        [u32; 16],
    pub dar_save:   [u32; 16],
    pub sr_save:    u16,
    pub ppc:        u32,
    pub pc:         u32,
    /// Stack pointers: USP, SSP, and processor-specific extras.
    pub sp:         [u32; 8],
    pub vbr:        u32,
    pub sfc:        u32,
    pub dfc:        u32,
    pub cacr:       u32,
    pub caar:       u32,
    pub ir:         u32,
    // Status register flags (stored as individual u32 to match CpuCore).
    pub t1_flag:    u32,
    pub t0_flag:    u32,
    pub s_flag:     u32,
    pub m_flag:     u32,
    pub x_flag:     u32,
    pub n_flag:     u32,
    pub not_z_flag: u32,
    pub v_flag:     u32,
    pub c_flag:     u32,
    pub int_mask:   u32,
    pub int_level:  u32,
    pub stopped:    u32,
    // ── Additional CPU state required for rollback determinism ──────────────
    pub cycles_remaining: i32,
    pub initial_cycles: i32,
    /// Change-of-flow flag for T0 trace (set by BRA, JMP, JSR, RTS, etc.)
    pub change_of_flow: bool,
    /// Last prefetch address.
    pub pref_addr: u32,
    /// Data in prefetch queue.
    pub pref_data: u32,
    /// Instruction mode (normal vs exception decode).
    pub instr_mode: u32,
    /// Run mode (normal, bus/address error, or reset).
    pub run_mode: u32,
    /// True while processing an exception (double-fault detection).
    pub exception_processing: bool,
    /// Virtual IRQ state.
    pub virq_state: u32,
    /// Pending NMI.
    pub nmi_pending: u32,
}

// ── Z80 CPU snapshot ──────────────────────────────────────────────────────────

/// Snapshot of the Z80 sound-CPU state.
///
/// `cpu_bytes` is the raw serialisation produced by `iz80::Cpu::serialize()`;
/// all other fields come from the `Z80Inner` struct in `z80.rs`.
#[derive(Serialize, Deserialize)]
pub struct Z80Snapshot {
    /// iz80 CPU registers, serialised via `iz80::Cpu::serialize()`.
    pub cpu_bytes:     Vec<u8>,
    /// Z80 internal SRAM (2 KB, mirrored at `$F800–$FFFF`).
    pub wram:          Vec<u8>,
    pub bank_f000:     u32,
    pub bank_e000:     u32,
    pub bank_c000:     u32,
    pub bank_8000:     u32,
    pub nmi_enabled:   bool,
    pub nmi_requested: bool,
    /// Pending NMI edge (fired but not yet acknowledged by the CPU).
    pub nmi_fire:      bool,
    /// Pending INT line state (driven by YM2610, must be restored on rollback).
    pub int_line:      i32,
    /// Accumulated M68k subcycles for Z80 clock division.
    pub subcycle:      u32,
    /// Absolute cycle target to prevent clock drift across batches.
    pub cycle_target:  u64,
    /// Which M1 ROM image is currently mapped into the Z80 address space.
    pub active_m1_rom: u8,
}

// ── Bus snapshot ──────────────────────────────────────────────────────────────

/// Snapshot of all mutable `SystemBus` state.
///
/// ROMs (`RomImages`), the cartridge handler, and the live input state are
/// excluded: ROMs are reloaded from disk; input applies only to the current
/// frame.
///
/// `pending_m1_swap` encodes which M1 ROM (if any) is queued to be loaded
/// into the Z80 address space on the next bank command (see ActiveM1Rom in bus).
///
/// The actual ROM bytes are not stored here; they are reconstructed from the
/// already-loaded `RomImages` on restore.
#[derive(Serialize, Deserialize)]
pub struct BusSnapshot {
    /// 64 KB work RAM (`$100000–$10FFFF`).
    pub work_ram:        Vec<u8>,
    /// 64 KB battery-backed backup SRAM (`$D00000–$D0FFFF`).
    pub backup_ram:      Vec<u8>,
    /// 2 KB memory card.
    pub mem_card:        Vec<u8>,
    /// Complete LSPC-2 state (VRAM, PAL, timer, beam, IRQs).
    pub lspc:            Lspc,
    /// Real-time clock (UPD4990A) register state.
    pub rtc:             Upd4990a,
    /// `true` = cartridge P-ROM mapped to `$000000` (post-boot).
    pub swp_rom:         bool,
    /// 68K→Z80 sound command latch.
    pub sound_cmd:       u8,
    /// Z80→68K sound reply latch.
    pub sound_reply:     u8,
    /// Sound status byte (bit 0 = Z80 ready).
    pub sound_status:    u8,
    /// `true` if the Z80 NMI line is asserted from the 68K side.
    pub nmi_request:     bool,
    /// Active P-ROM bank base offset (bytes into the P-ROM image).
    /// `u32` (not `usize`) so the serialised state is portable across
    /// 64-bit (desktop) and 32-bit (WASM) targets.
    pub p_rom_bank_base: u32,
    /// Pending M1 ROM swap code (see table above).
    pub pending_m1_swap: u8,
    /// SRAM write-protect flag: `true` = writes allowed (`REG_SRAMEN`),
    /// `false` = write-protected (`REG_SRAMLOCK`, boot default).
    pub sram_writable: bool,
    /// Open bus register tracking the last 16-bit access.
    pub open_bus: u16,
    /// Hardware DIP switches.
    pub hw_dips: u8,
    /// Cartridge protection chip state.
    pub cart_state: Vec<u8>,
    /// Latched player input for the upcoming frame.
    pub input: crate::io::InputState,
}

// ── Top-level save state ──────────────────────────────────────────────────────

/// Complete save state: CPU snapshots + bus state + audio chip state.
#[derive(Serialize, Deserialize)]
pub struct SaveState {
    /// Emulator format version; checked on load for compatibility warnings.
    pub version: String,
    /// Monotonic frame counter at the time of the snapshot.
    pub frame:   u64,
    pub m68k:    M68kSnapshot,
    pub z80:     Z80Snapshot,
    pub bus:     BusSnapshot,
    /// ymfm YM2610 chip register state (opaque binary blob).
    pub ym2610:  Vec<u8>,
}

impl SaveState {
    /// Current save-state format version tag.
    pub const VERSION: &'static str = "neo-3.5";

    // ── Binary (rollback-optimised) ───────────────────────────────────────────

    /// Serialise to a compact binary blob using `bincode`.
    ///
    /// Typical size: 120–200 KB.  Round-trip latency: < 1 ms.
    /// Suitable for rollback netcode frame-ring buffers.
    pub fn to_bytes(&self) -> Result<Vec<u8>, String> {
        let body = bincode::serialize(self)
            .map_err(|e| format!("save state binary serialise: {}", e))?;
        Ok(crate::core::with_save_header(crate::core::SAVE_CORE_NEOGEO, body))
    }

    /// Deserialise from a blob produced by `SaveState::to_bytes`.
    pub fn from_bytes(data: &[u8]) -> Result<Self, String> {
        let body = crate::core::strip_save_header(data, crate::core::SAVE_CORE_NEOGEO)?;
        bincode::deserialize(body)
            .map_err(|e| format!("save state binary deserialise: {}", e))
    }

    // ── JSON (human-readable / debug) ─────────────────────────────────────────

    /// Write this save state to `path` as JSON.
    pub fn write_to_file(&self, path: &str) -> Result<(), String> {
        let json = serde_json::to_string(self)
            .map_err(|e| format!("save state JSON serialise: {}", e))?;
        std::fs::write(path, json)
            .map_err(|e| format!("save state write '{}': {}", path, e))?;
        log::info!("Save state written: {}", path);
        Ok(())
    }

    /// Read and deserialise a save state from a JSON file at `path`.
    pub fn read_from_file(path: &str) -> Result<Self, String> {
        let json = std::fs::read_to_string(path)
            .map_err(|e| format!("save state read '{}': {}", path, e))?;
        let state: SaveState = serde_json::from_str(&json)
            .map_err(|e| format!("save state JSON parse: {}", e))?;
        if state.version != Self::VERSION {
            log::warn!(
                "Save state version mismatch: file='{}' expected='{}'",
                state.version, Self::VERSION
            );
        }
        log::info!("Save state loaded: {}", path);
        Ok(state)
    }

    // ── Multisect Checksums (for desync debugging) ────────────────────────────

    /// Computes 8 distinct 16-bit FNV-1a checksums for different emulator subsystems.
    /// This allows rollback netcode to pack them into a single `u128` and identify
    /// exactly which subsystem desynced first.
    pub fn debug_checksums(&self) -> [u16; 8] {
        // 1. M68k
        let m68k_hash = fnv1a_16(&bincode::serialize(&self.m68k).unwrap_or_default());
        // 2. Z80
        let z80_hash = fnv1a_16(&bincode::serialize(&self.z80).unwrap_or_default());
        // 3. YM2610
        let ym2610_hash = fnv1a_16(&self.ym2610);
        // 4. LSPC (Video)
        let lspc_hash = fnv1a_16(&bincode::serialize(&self.bus.lspc).unwrap_or_default());
        // 5. Work RAM
        let work_ram_hash = fnv1a_16(&self.bus.work_ram);
        // 6. Backup RAM + MemCard
        let mut backup_bytes = self.bus.backup_ram.clone();
        backup_bytes.extend_from_slice(&self.bus.mem_card);
        let backup_hash = fnv1a_16(&backup_bytes);
        // 7. Bus Misc (RTC, bank switches, latches, hw config)
        let bus_misc_hash = fnv1a_16(&bincode::serialize(&(
            &self.bus.rtc, &self.bus.swp_rom, &self.bus.sound_cmd, 
            &self.bus.sound_reply, &self.bus.sound_status, &self.bus.nmi_request,
            &self.bus.p_rom_bank_base, &self.bus.pending_m1_swap, &self.bus.sram_writable,
            &self.bus.open_bus, &self.bus.hw_dips, &self.bus.cart_state
        )).unwrap_or_default());
        // 8. Frame Header
        let frame_hash = fnv1a_16(&self.frame.to_le_bytes());

        [
            m68k_hash,
            z80_hash,
            ym2610_hash,
            lspc_hash,
            work_ram_hash,
            backup_hash,
            bus_misc_hash,
            frame_hash
        ]
    }
}

/// Computes a 16-bit FNV-1a hash (by folding a 32-bit hash).
fn fnv1a_16(data: &[u8]) -> u16 {
    let mut hash = 2166136261u32;
    for &byte in data {
        hash ^= byte as u32;
        hash = hash.wrapping_mul(16777619);
    }
    ((hash >> 16) ^ (hash & 0xFFFF)) as u16
}

