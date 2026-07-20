//! CPS1 save-state serialisation.
//!
//! A save state is a complete snapshot of all mutable CPS1 state at a frame
//! boundary — the 68000 CPU register file, the bus (work RAM, graphics RAM,
//! CPS-A/B registers, inputs, sound latches, interrupts), the sound board
//! (Z80 + OKI MSM6295 voices + the ymfm YM2151 chip), and the video sprite
//! double-buffer.  ROM images are **not** included; they are reloaded on
//! startup via `EmulatorCore::load_roms`.
//!
//! The binary form (bincode) is used for GGRS rollback; the JSON form is for
//! human-readable inspection.  Both mirror the NeoGeo `crate::save_state`
//! design so the netplay / replay plumbing works unchanged.

use serde::{Deserialize, Serialize};

use crate::save_state::M68kSnapshot;

/// One OKI MSM6295 ADPCM voice (mid-playback position + decoder accumulators).
#[derive(Serialize, Deserialize)]
pub struct OkiVoiceSnap {
    pub playing: bool,
    pub position: u32,
    pub stop: u32,
    pub signal: i32,
    pub step: i32,
    pub volume: i32,
    pub phase: u32,
}

/// Complete sound-board snapshot (Z80 + OKI + YM2151).
#[derive(Serialize, Deserialize)]
pub struct SoundSnap {
    /// iz80 CPU registers, serialised via `iz80::Cpu::serialize()`.
    pub z80_bytes: Vec<u8>,
    /// Z80 internal work RAM (`$D000–$D7FF`).
    pub ram: Vec<u8>,
    /// Current ROM bank window (`$8000–$BFFF`).
    pub bank: u32,
    /// Latched sound command from the 68000.
    pub soundlatch: u8,
    /// The four OKI voices.
    pub oki_voices: Vec<OkiVoiceSnap>,
    /// OKI pending phrase-latch (`-1` when idle).
    pub oki_command: i32,
    /// OKI effective sample rate (pin 7).
    pub oki_rate: u32,
    /// ymfm YM2151 opaque chip blob (registers + timers + IRQ + decimation).
    pub ym_bytes: Vec<u8>,
}

/// Snapshot of the CPS1 68000 bus (all mutable RAM + register files + inputs).
#[derive(Serialize, Deserialize)]
pub struct BusSnap {
    pub work_ram: Vec<u8>,
    pub gfxram: Vec<u16>,
    pub cps_a: Vec<u16>,
    pub cps_b: Vec<u16>,
    pub in0: u8,
    pub in1: u16,
    pub in2: u16,
    pub dswa: u8,
    pub dswb: u8,
    pub dswc: u8,
    pub sound_latch: u8,
    pub sound_latch2: u8,
    pub sound_latch_pending: bool,
    pub irq_pending: u8,
}

/// Top-level CPS1 save state.
#[derive(Serialize, Deserialize)]
pub struct CpsSaveState {
    /// Format version (bump on any breaking layout change).
    pub version: u32,
    pub cpu: M68kSnapshot,
    pub bus: BusSnap,
    pub sound: SoundSnap,
    /// Latched sprite OBJ table (mirrors MAME `m_buffered_obj`).
    pub buffered_obj: Vec<u16>,
    pub frame: u64,
    pub audio_total: u64,
}

/// Current save-state format version.
pub const CPS_SAVE_VERSION: u32 = 1;

impl CpsSaveState {
    /// Serialise to a compact binary blob (bincode) for rollback / save files.
    pub fn to_bytes(&self) -> Result<Vec<u8>, String> {
        let body = bincode::serialize(self).map_err(|e| format!("CPS1 save serialize: {e}"))?;
        Ok(crate::core::with_save_header(crate::core::SAVE_CORE_CPS1, body))
    }

    /// Deserialise from a binary blob produced by `to_bytes`.
    pub fn from_bytes(data: &[u8]) -> Result<Self, String> {
        let body = crate::core::strip_save_header(data, crate::core::SAVE_CORE_CPS1)?;
        bincode::deserialize(body).map_err(|e| format!("CPS1 save deserialize: {e}"))
    }

    /// Write a human-readable JSON save state to `path`.
    pub fn write_to_file(&self, path: &str) -> Result<(), String> {
        let json = serde_json::to_string(self).map_err(|e| format!("CPS1 save json: {e}"))?;
        std::fs::write(path, json).map_err(|e| format!("CPS1 save write {path}: {e}"))
    }

    /// Read a JSON save state written by `write_to_file`.
    pub fn read_from_file(path: &str) -> Result<Self, String> {
        let json = std::fs::read_to_string(path).map_err(|e| format!("CPS1 save read {path}: {e}"))?;
        serde_json::from_str(&json).map_err(|e| format!("CPS1 save parse {path}: {e}"))
    }
}

/// Capture the mutable 68000 register file into an [`M68kSnapshot`].
///
/// The FPU set is omitted — CPS1 uses a plain MC68000.
pub fn cpu_snapshot(cpu: &m68k_cpu::CpuCore) -> M68kSnapshot {
    M68kSnapshot {
        dar: cpu.dar,
        dar_save: cpu.dar_save,
        sr_save: cpu.sr_save,
        ppc: cpu.ppc,
        pc: cpu.pc,
        sp: cpu.sp,
        vbr: cpu.vbr,
        sfc: cpu.sfc,
        dfc: cpu.dfc,
        cacr: cpu.cacr,
        caar: cpu.caar,
        ir: cpu.ir,
        t1_flag: cpu.t1_flag,
        t0_flag: cpu.t0_flag,
        s_flag: cpu.s_flag,
        m_flag: cpu.m_flag,
        x_flag: cpu.x_flag,
        n_flag: cpu.n_flag,
        not_z_flag: cpu.not_z_flag,
        v_flag: cpu.v_flag,
        c_flag: cpu.c_flag,
        int_mask: cpu.int_mask,
        int_level: cpu.int_level,
        stopped: cpu.stopped,
        cycles_remaining: cpu.cycles_remaining,
        initial_cycles: cpu.initial_cycles,
        change_of_flow: cpu.change_of_flow,
        pref_addr: cpu.pref_addr,
        pref_data: cpu.pref_data,
        instr_mode: cpu.instr_mode,
        run_mode: cpu.run_mode,
        exception_processing: cpu.exception_processing,
        virq_state: cpu.virq_state,
        nmi_pending: cpu.nmi_pending,
    }
}

/// Restore the 68000 register file from an [`M68kSnapshot`].
pub fn cpu_restore(cpu: &mut m68k_cpu::CpuCore, snap: &M68kSnapshot) {
    cpu.dar = snap.dar;
    cpu.dar_save = snap.dar_save;
    cpu.sr_save = snap.sr_save;
    cpu.ppc = snap.ppc;
    cpu.pc = snap.pc;
    cpu.sp = snap.sp;
    cpu.vbr = snap.vbr;
    cpu.sfc = snap.sfc;
    cpu.dfc = snap.dfc;
    cpu.cacr = snap.cacr;
    cpu.caar = snap.caar;
    cpu.ir = snap.ir;
    cpu.t1_flag = snap.t1_flag;
    cpu.t0_flag = snap.t0_flag;
    cpu.s_flag = snap.s_flag;
    cpu.m_flag = snap.m_flag;
    cpu.x_flag = snap.x_flag;
    cpu.n_flag = snap.n_flag;
    cpu.not_z_flag = snap.not_z_flag;
    cpu.v_flag = snap.v_flag;
    cpu.c_flag = snap.c_flag;
    cpu.int_mask = snap.int_mask;
    cpu.int_level = snap.int_level;
    cpu.stopped = snap.stopped;
    cpu.cycles_remaining = snap.cycles_remaining;
    cpu.initial_cycles = snap.initial_cycles;
    cpu.change_of_flow = snap.change_of_flow;
    cpu.pref_addr = snap.pref_addr;
    cpu.pref_data = snap.pref_data;
    cpu.instr_mode = snap.instr_mode;
    cpu.run_mode = snap.run_mode;
    cpu.exception_processing = snap.exception_processing;
    cpu.virq_state = snap.virq_state;
    cpu.nmi_pending = snap.nmi_pending;
}
