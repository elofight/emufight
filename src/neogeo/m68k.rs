//! M68000 CPU wrapper — pure-Rust via the `m68k_cpu` crate.
//!
//! Replaces the original Musashi C library (libmusashi.a) and its link-time
//! global callbacks.  `SystemBus` implements `AddressBus` so the CPU reads
//! and writes memory through the bus directly, without raw pointers or
//! global state.  Address-error checks are disabled to match the original
//! Musashi build and real NeoGeo behaviour (word/long accesses to odd
//! addresses do not trap).

use m68k_cpu::{CpuCore, CpuType, AddressBus};
use crate::neogeo::bus::SystemBus;

// ── AddressBus implementation ────────────────────────────────────────────────

impl AddressBus for SystemBus {
    fn read_byte(&mut self, address: u32) -> u8   { self.read_8(address) }
    fn read_word(&mut self, address: u32) -> u16  { self.read_16(address) }
    fn read_long(&mut self, address: u32) -> u32  { self.read_32(address) }
    fn write_byte(&mut self, address: u32, value: u8)  { self.write_8(address, value); }
    fn write_word(&mut self, address: u32, value: u16) { self.write_16(address, value); }
    fn write_long(&mut self, address: u32, value: u32) { self.write_32(address, value); }
}

// ── M68k wrapper ─────────────────────────────────────────────────────────────

pub struct M68k {
    cpu: CpuCore,
}

impl M68k {
    pub fn new() -> Self {
        let cpu = Self::create_68000_core_without_address_errors();
        M68k { cpu }
    }

    /// Create a 68000 core with address error checks disabled.
    ///
    /// The real 68000 raises an address-error exception for any word or long
    /// read/write to an odd address.  Our previous Musashi build had
    /// M68K_EMULATE_ADDRESS_ERROR = OPT_OFF. NeoGeo games rely on this.
    ///
    /// We set the proper M68000 derived fields (address_mask, sr_mask, etc.)
    /// then override the cpu_type tag to M68EC020 (the only tag that disables
    /// the crate's unconditional alignment checks for this version).
    ///
    /// All other behaviour (decode, exceptions, interrupts, 24-bit wrap) remains
    /// driven by the M68000-derived fields.
    fn create_68000_core_without_address_errors() -> m68k_cpu::CpuCore {
        let mut cpu = CpuCore::new();
        cpu.set_cpu_type(CpuType::M68000);
        cpu.cpu_type = CpuType::M68EC020;
        cpu
    }

    /// Reset the CPU and read the initial SSP/PC vectors from the bus.
    pub fn init(&mut self, bus: &mut SystemBus) {
        self.cpu.reset(bus);
    }

    /// Execute up to `cycles` M68000 clock cycles; returns cycles actually run.
    pub fn execute(&mut self, bus: &mut SystemBus, cycles: i32) -> i32 {
        self.cpu.execute(bus, cycles)
    }

    /// Set the pending interrupt level (0 = none, 1–7 = priority).
    /// Checked by the CPU before each instruction; auto-clears after acknowledge.
    pub fn set_irq(&mut self, level: u32) {
        self.cpu.int_level = level;
    }

    pub fn get_pc(&self) -> u32 {
        self.cpu.pc
    }

    // ── Save-state support ────────────────────────────────────────────────────

    /// Capture the complete M68K register file into an `M68kSnapshot`.
    pub fn snapshot(&self) -> crate::save_state::M68kSnapshot {
        crate::save_state::M68kSnapshot {
            dar:        self.cpu.dar,
            dar_save:   self.cpu.dar_save,
            sr_save:    self.cpu.sr_save,
            ppc:        self.cpu.ppc,
            pc:         self.cpu.pc,
            sp:         self.cpu.sp,
            vbr:        self.cpu.vbr,
            sfc:        self.cpu.sfc,
            dfc:        self.cpu.dfc,
            cacr:       self.cpu.cacr,
            caar:       self.cpu.caar,
            ir:         self.cpu.ir,
            t1_flag:    self.cpu.t1_flag,
            t0_flag:    self.cpu.t0_flag,
            s_flag:     self.cpu.s_flag,
            m_flag:     self.cpu.m_flag,
            x_flag:     self.cpu.x_flag,
            n_flag:     self.cpu.n_flag,
            not_z_flag: self.cpu.not_z_flag,
            v_flag:     self.cpu.v_flag,
            c_flag:     self.cpu.c_flag,
            int_mask:   self.cpu.int_mask,
            int_level:  self.cpu.int_level,
            stopped:    self.cpu.stopped,
            cycles_remaining: self.cpu.cycles_remaining,
            initial_cycles: self.cpu.initial_cycles,
            change_of_flow: self.cpu.change_of_flow,
            pref_addr:  self.cpu.pref_addr,
            pref_data:  self.cpu.pref_data,
            instr_mode: self.cpu.instr_mode,
            run_mode:   self.cpu.run_mode,
            exception_processing: self.cpu.exception_processing,
            virq_state: self.cpu.virq_state,
            nmi_pending: self.cpu.nmi_pending,
        }
    }

    /// Restore from a previously captured `M68kSnapshot`.
    pub fn restore(&mut self, snap: crate::save_state::M68kSnapshot) {
        // Rebuild the core so fields outside the snapshot cannot leak across
        // rollback restores on a long-lived emulator instance.
        self.cpu = Self::create_68000_core_without_address_errors();
        self.cpu.dar        = snap.dar;
        self.cpu.dar_save   = snap.dar_save;
        self.cpu.sr_save    = snap.sr_save;
        self.cpu.ppc        = snap.ppc;
        self.cpu.pc         = snap.pc;
        self.cpu.sp         = snap.sp;
        self.cpu.vbr        = snap.vbr;
        self.cpu.sfc        = snap.sfc;
        self.cpu.dfc        = snap.dfc;
        self.cpu.cacr       = snap.cacr;
        self.cpu.caar       = snap.caar;
        self.cpu.ir         = snap.ir;
        self.cpu.t1_flag    = snap.t1_flag;
        self.cpu.t0_flag    = snap.t0_flag;
        self.cpu.s_flag     = snap.s_flag;
        self.cpu.m_flag     = snap.m_flag;
        self.cpu.x_flag     = snap.x_flag;
        self.cpu.n_flag     = snap.n_flag;
        self.cpu.not_z_flag = snap.not_z_flag;
        self.cpu.v_flag     = snap.v_flag;
        self.cpu.c_flag     = snap.c_flag;
        self.cpu.int_mask   = snap.int_mask;
        self.cpu.int_level  = snap.int_level;
        self.cpu.stopped    = snap.stopped;
        self.cpu.cycles_remaining = snap.cycles_remaining;
        self.cpu.initial_cycles = snap.initial_cycles;
        self.cpu.change_of_flow = snap.change_of_flow;
        self.cpu.pref_addr  = snap.pref_addr;
        self.cpu.pref_data  = snap.pref_data;
        self.cpu.instr_mode = snap.instr_mode;
        self.cpu.run_mode   = snap.run_mode;
        self.cpu.exception_processing = snap.exception_processing;
        self.cpu.virq_state = snap.virq_state;
        self.cpu.nmi_pending = snap.nmi_pending;
    }
}

impl Default for M68k {
    fn default() -> Self { Self::new() }
}
