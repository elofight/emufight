//! Z80 CPU wrapper — pure-Rust via the `iz80` crate.
//!
//! Handles the NeoGeo-specific Z80 memory map and I/O ports:
//!
//! # Memory map
//!
//! | Range | Description |
//! |-------|-------------|
//! | `0x0000–0x7FFF` | M1 ROM fixed window (first 32 KB) |
//! | `0x8000–0xBFFF` | M1 ROM 16 KB banked window (port $0B) |
//! | `0xC000–0xDFFF` | M1 ROM  8 KB banked window (port $0A) |
//! | `0xE000–0xEFFF` | M1 ROM  4 KB banked window (port $09) |
//! | `0xF000–0xF7FF` | M1 ROM  2 KB banked window (port $08) |
//! | `0xF800–0xFFFF` | Z80 internal SRAM (2 KB, mirrored) |
//!
//! # I/O ports (full 16-bit address from iz80)
//!
//! | Port (mask) | Direction | Function |
//! |---|---|---|
//! | `$00` (`$0C`) | Read | Sound command + NMI acknowledge |
//! | `$04` (`$0C`) | Read/write | YM2610 register access |
//! | `$08` (`$0C`) | Read | M1 bank switch (bank in high byte) |
//! | `$08` (`$1C`) | Write | NMI enable |
//! | `$18` (`$1C`) | Write | NMI disable |
//! | `$0C` (`$0C`) | Write | Sound reply |
//!
//! The Z80 /INT line is driven by the YM2610 C++ backend via a thread-local
//! Cell, keeping multi-instance emulation safe on separate threads.

use iz80::{Cpu, Machine};
use crate::neogeo::bus::SystemBus;

// ── FFI for YM2610 (still C++) ───────────────────────────────────────────────
extern "C" {
    fn neo_ym2610_write(ptr: *mut std::ffi::c_void, port: i32, data: i32);
    fn neo_ym2610_read(ptr: *mut std::ffi::c_void, port: i32) -> i32;
}

// ── INT line driven by ym2610_glue.cpp ───────────────────────────────────────
//
// Each emulator owns `Z80Inner::int_line`.  YM2610 glue writes through a bound
// pointer (`neo_ym2610_bind_z80_int`) so multiple instances on one thread do not
// share a thread-local (which caused rollback drift in tests).

/// Legacy fallback when glue is not yet bound to an instance.
#[no_mangle]
pub extern "C" fn neo_z80_set_int(level: i32) {
    let _ = level;
}

#[no_mangle]
pub extern "C" fn neo_z80_get_int() -> i32 {
    0
}

pub fn reset_int_line() {}

// ── Inner state (accessible to the Machine trait during execution) ────────────

struct Z80Inner {
    wram: [u8; 0x0800],
    bank_f000: u32,
    bank_e000: u32,
    bank_c000: u32,
    bank_8000: u32,
    nmi_enabled: bool,
    nmi_requested: bool, // set externally (from bus), consumed in execute
    nmi_fire: bool,      // set from port_out when enabling NMI with pending request
    subcycle: u32,
    cycle_target: u64,
    /// YM2610 /INT level (0/1); driven by ym2610 glue via bound pointer.
    int_line: i32,
    m1_rom: Vec<u8>,
}

impl Z80Inner {
    fn new() -> Self {
        Z80Inner {
            wram: [0u8; 0x0800],
            bank_f000: 0,
            bank_e000: 0,
            bank_c000: 0,
            bank_8000: 0,
            nmi_enabled: false,
            nmi_requested: false,
            nmi_fire: false,
            subcycle: 0,
            cycle_target: 0,
            int_line: 0,
            m1_rom: Vec::new(),
        }
    }

    fn reset_banks(&mut self) {
        self.bank_f000 = 0;
        self.bank_e000 = 0;
        self.bank_c000 = 0;
        self.bank_8000 = 0;
    }

    fn m1_byte(&self, off: u32) -> u8 {
        if self.m1_rom.is_empty() { return 0xFF; }
        self.m1_rom[(off as usize) % self.m1_rom.len()]
    }

    fn read_mem(&self, addr: u16) -> u8 {
        match addr {
            0x0000..=0x7FFF => self.m1_byte(addr as u32),
            0x8000..=0xBFFF => self.m1_byte((self.bank_8000 << 14) | (addr as u32 & 0x3FFF)),
            0xC000..=0xDFFF => self.m1_byte((self.bank_c000 << 13) | (addr as u32 & 0x1FFF)),
            0xE000..=0xEFFF => self.m1_byte((self.bank_e000 << 12) | (addr as u32 & 0x0FFF)),
            0xF000..=0xF7FF => self.m1_byte((self.bank_f000 << 11) | (addr as u32 & 0x07FF)),
            0xF800..=0xFFFF => self.wram[(addr & 0x07FF) as usize],
        }
    }

    fn write_mem(&mut self, addr: u16, val: u8) {
        if addr >= 0xF800 {
            self.wram[(addr & 0x07FF) as usize] = val;
        }
    }

    fn do_port_in(&mut self, port: u16, bus: &mut SystemBus, ym2610_ptr: *mut std::ffi::c_void) -> u8 {
        match (port as u8) & 0x0C {
            0x00 => {
                // Sound command read + NMI / sound-cmd acknowledge.
                self.nmi_requested = false;
                bus.sound_status |= 1;
                bus.sound_cmd
            }
            0x04 => unsafe { neo_ym2610_read(ym2610_ptr, (port as u8 & 0x03) as i32) as u8 },
            0x08 => {
                // M1 ROM bank switch: bank number in upper byte of 16-bit port address.
                let bank = ((port >> 8) & 0xFF) as u32;
                match (port as u8) & 0x03 {
                    0 => self.bank_f000 = bank,
                    1 => self.bank_e000 = bank,
                    2 => self.bank_c000 = bank,
                    3 => self.bank_8000 = bank,
                    _ => {}
                }
                0x00
            }
            _ => 0xFF,
        }
    }

    fn do_port_out(&mut self, port: u16, val: u8, bus: &mut SystemBus, ym2610_ptr: *mut std::ffi::c_void) {
        let p = port as u8;
        // NMI control (mask $1C): $08-$0B = enable, $18-$1B = disable.
        if (p & 0x1C) == 0x08 {
            self.nmi_enabled = true;
            // Fire NMI immediately if one was pending while disabled.
            if self.nmi_requested {
                self.nmi_fire = true;
                self.nmi_requested = false;
            }
            return;
        }
        if (p & 0x1C) == 0x18 {
            self.nmi_enabled = false;
            return;
        }
        // YM2610 / sound reply (mask $0C).
        match p & 0x0C {
            0x04 => unsafe { neo_ym2610_write(ym2610_ptr, (p & 0x03) as i32, val as i32); }
            0x0C => { bus.sound_reply = val; }
            _ => {}
        }
    }
}

// ── Machine trait: temporary borrow combining inner state + system bus ────────

struct NeoZ80Machine<'a> {
    inner: &'a mut Z80Inner,
    bus: &'a mut SystemBus,
    ym2610_ptr: *mut std::ffi::c_void,
}

impl<'a> Machine for NeoZ80Machine<'a> {
    fn peek(&mut self, addr: u16) -> u8 {
        self.inner.read_mem(addr)
    }
    fn poke(&mut self, addr: u16, val: u8) {
        self.inner.write_mem(addr, val);
    }
    fn port_in(&mut self, port: u16) -> u8 {
        self.inner.do_port_in(port, self.bus, self.ym2610_ptr)
    }
    fn port_out(&mut self, port: u16, val: u8) {
        self.inner.do_port_out(port, val, self.bus, self.ym2610_ptr);
    }
}

// ── Public Z80 wrapper ────────────────────────────────────────────────────────

pub struct Z80 {
    cpu: Cpu,
    inner: Z80Inner,
}

impl Z80 {
    pub fn new() -> Self {
        Z80 { cpu: Cpu::new(), inner: Z80Inner::new() }
    }

    /// Replace the current M1 ROM and reset bank registers + CPU.
    pub fn set_m1_rom(&mut self, data: &[u8]) {
        self.inner.m1_rom = data.to_vec();
        self.inner.reset_banks();
        self.cpu.signal_reset();
    }

    pub fn reset(&mut self) {
        self.cpu.signal_reset();
        self.inner.wram = [0u8; 0x0800];
        self.inner.nmi_enabled = false;
        self.inner.nmi_requested = false;
        self.inner.nmi_fire = false;
        self.inner.int_line = 0;
        self.inner.reset_banks();
    }

    /// Execute Z80 for a number of M68K master cycles, managing 3:1 clock division correctly.
    pub fn execute(&mut self, m68k_cycles: usize, bus: &mut SystemBus, ym2610: &mut crate::neogeo::sound::YM2610) -> usize {
        // Accumulate M68k cycles to precisely step Z80 at 1/3 speed without dropping remainders
        let total_subcycles = self.inner.subcycle as usize + m68k_cycles;
        let cycles = total_subcycles / 3;
        self.inner.subcycle = (total_subcycles % 3) as u32;

        if cycles == 0 {
            return 0;
        }

        // Apply pending ROM switch (triggered by bus REG_BRDFIX / REG_CRTFIX writes).
        if let Some(rom) = bus.pending_m1_rom.take() {
            self.inner.m1_rom = rom;
            self.inner.reset_banks();
            self.cpu.signal_reset();
        }
        
        self.inner.cycle_target += cycles as u64;

        // Propagate NMI request from bus to inner state.
        if bus.nmi_request {
            bus.nmi_request = false;
            if self.inner.nmi_enabled {
                self.cpu.signal_nmi();
            } else {
                self.inner.nmi_requested = true;
            }
        }

        // Apply sticky INT line (re-assert every batch while held by YM2610).
        let int_active = self.inner.int_line != 0;
        self.cpu.signal_interrupt(int_active);

        let before = self.cpu.cycle_count();
        let target = self.inner.cycle_target;
        
        if before >= target {
            return 0; // Overshot previously, wait until M68k catches up
        }

        {
            let ym2610_ptr = ym2610.ptr();
            let (cpu, inner) = (&mut self.cpu, &mut self.inner);
            let mut machine = NeoZ80Machine { inner, bus, ym2610_ptr };
            while cpu.cycle_count() < target {
                cpu.execute_instruction(&mut machine);
                // NMI fired by port $08 write (NMI-enable with pending request).
                if machine.inner.nmi_fire {
                    cpu.signal_nmi();
                    machine.inner.nmi_fire = false;
                }
            }
        }

        (self.cpu.cycle_count() - before) as usize
    }

    // ── Save-state support ────────────────────────────────────────────────────

    /// Capture the complete Z80 state into a `Z80Snapshot`.
    ///
    /// `iz80::Cpu::serialize()` handles the full CPU register file; the
    /// `Z80Inner` fields (banked memory map, NMI state, WRAM) are captured
    /// directly.
    pub fn cycle_target(&self) -> u64 {
        self.inner.cycle_target
    }

    /// Pointer for ym2610 glue to drive this instance's /INT line.
    pub fn int_line_ptr(&mut self) -> *mut i32 {
        &mut self.inner.int_line
    }

    pub fn active_m1_code(&self, bus: &crate::neogeo::bus::SystemBus) -> u8 {
        use crate::neogeo::bus::ActiveM1Rom;
        if self.inner.m1_rom.is_empty() {
            return ActiveM1Rom::None as u8;
        }
        if !bus.roms.sm1_rom.is_empty() && self.inner.m1_rom == bus.roms.sm1_rom {
            ActiveM1Rom::Sm1 as u8
        } else if !bus.roms.m1_rom.is_empty() && self.inner.m1_rom == bus.roms.m1_rom {
            ActiveM1Rom::Cart as u8
        } else {
            ActiveM1Rom::Cart as u8
        }
    }

    pub fn snapshot(&self, bus: &crate::neogeo::bus::SystemBus) -> crate::save_state::Z80Snapshot {
        crate::save_state::Z80Snapshot {
            cpu_bytes:     self.cpu.serialize(),
            wram:          self.inner.wram.to_vec(),
            bank_f000:     self.inner.bank_f000,
            bank_e000:     self.inner.bank_e000,
            bank_c000:     self.inner.bank_c000,
            bank_8000:     self.inner.bank_8000,
            nmi_enabled:   self.inner.nmi_enabled,
            nmi_requested: self.inner.nmi_requested,
            nmi_fire:      self.inner.nmi_fire,
            subcycle:      self.inner.subcycle,
            cycle_target:  self.inner.cycle_target,
            int_line:      self.inner.int_line,
            active_m1_rom: self.active_m1_code(bus),
        }
    }

    /// Restore from a previously captured `Z80Snapshot`.
    pub fn restore(&mut self, snap: crate::save_state::Z80Snapshot) {
        self.cpu = Cpu::new();
        if let Err(e) = self.cpu.deserialize(&snap.cpu_bytes) {
            log::warn!("Z80 restore: CPU deserialise failed: {}", e);
        }
        if snap.wram.len() == self.inner.wram.len() {
            self.inner.wram.copy_from_slice(&snap.wram);
        }
        self.inner.bank_f000     = snap.bank_f000;
        self.inner.bank_e000     = snap.bank_e000;
        self.inner.bank_c000     = snap.bank_c000;
        self.inner.bank_8000     = snap.bank_8000;
        self.inner.nmi_enabled   = snap.nmi_enabled;
        self.inner.nmi_requested = snap.nmi_requested;
        self.inner.nmi_fire = snap.nmi_fire;
        self.inner.subcycle = snap.subcycle;
        self.inner.cycle_target = snap.cycle_target;
        self.inner.int_line = snap.int_line;
    }

    /// Reconstruct the active M1 ROM vector during save-state restore.
    /// Banks and CPU state come from the snapshot — do not reset here.
    pub fn restore_m1_rom(&mut self, data: &[u8]) {
        self.inner.m1_rom = data.to_vec();
    }
}
