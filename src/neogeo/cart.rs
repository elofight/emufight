//! Game cartridge handler abstraction.
//!
//! NeoGeo cartridges can require two independent kinds of game-specific
//! behaviour:
//!
//! 1. **P-ROM post-processing** — decryption or byte-rearrangement applied
//!    once at load time ([`PRomDecryptor`]).
//! 2. **Runtime protection chip** — CPLD/ALTERA registers that intercept reads
//!    and writes at specific addresses ([`ProtectionChip`]).
//!
//! These two concerns are kept in separate component traits so that any
//! combination is possible — e.g. two games may share the same decryptor but
//! use different protection chips, or one game may have protection but no
//! decryption.
//!
//! [`ComposedCart`] wires an optional `PRomDecryptor` with an optional
//! `ProtectionChip` into a single [`Cartridge`] that `SystemBus` holds.
//!
//! # Adding a new game
//!
//! * If the game reuses an existing decryptor or protection chip, just add a
//!   `match` arm in `cart_for` and compose the existing components.
//! * If new logic is needed, implement [`PRomDecryptor`] and/or
//!   [`ProtectionChip`] and compose them in `cart_for`.
//! * `SystemBus` and `romset` never need to change.

use serde::{Serialize, Deserialize};

// ── Top-level Cartridge trait ─────────────────────────────────────────────────

/// Per-cartridge hardware abstraction seen by `SystemBus`.
///
/// In almost all cases you should not implement this directly — create
/// component structs ([`PRomDecryptor`], [`ProtectionChip`]) and compose them
/// via [`ComposedCart`].  Direct implementation is available for one-off games
/// that don't fit the two-component model.
pub trait Cartridge: Send + 'static {
    /// Post-process raw P-ROM bytes after they are read from disk.
    ///
    /// The default implementation applies only the universal byteswap heuristic.
    fn process_p_rom(&mut self, mut data: Vec<u8>) -> Vec<u8> {
        byteswap_p_rom_if_needed(&mut data);
        data
    }

    /// Reset internal cartridge state (e.g., protection chips, bank switching).
    fn reset(&mut self) {}

    /// Optionally intercept an 8-bit read from the P-ROM address space.
    ///
    /// Return `Some(byte)` to substitute the value; `None` to read from `p_rom`.
    fn intercept_read_8(&self, _address: u32) -> Option<u8> {
        None
    }

    /// Optionally consume a 16-bit bus write.
    ///
    /// Return `true` if consumed (bus skips the byte-pair fallback).
    fn on_write_16(&mut self, _address: u32, _value: u16) -> bool {
        false
    }

    /// Optionally consume an 8-bit bus write.
    fn on_write_8(&mut self, _address: u32, _value: u8) -> bool {
        false
    }

    /// Capture mutable cartridge state.
    fn snapshot(&self) -> Vec<u8> {
        Vec::new()
    }

    /// Restore mutable cartridge state.
    fn restore(&mut self, _data: &[u8]) {}

    /// Optional static initial match state (legacy hook). Prefer host disk
    /// paths via [`crate::neogeo::Emulator::load_initial_match_state`].
    /// Default is `None` — this crate never compiles game states in.
    fn initial_match_state(&self) -> Option<&'static [u8]> {
        None
    }
}

// ── Component trait: PRomDecryptor ────────────────────────────────────────────

/// Decrypts (or otherwise rearranges) P-ROM bytes at load time.
///
/// Called by [`ComposedCart`] after the universal byteswap heuristic.
/// Multiple games can share the same `PRomDecryptor` implementation.
pub trait PRomDecryptor: Send + 'static {
    fn decrypt(&self, data: &mut Vec<u8>);
}

// ── Component trait: ProtectionChip ──────────────────────────────────────────

/// Emulates a hardware protection chip (CPLD / ALTERA) at runtime.
///
/// The three hooks map directly to the three points where the bus interacts
/// with protection hardware.  Multiple games can share one implementation.
pub trait ProtectionChip: Send + 'static {
    /// Called once after P-ROM has been (optionally) decrypted.
    ///
    /// Allows the chip to cache values it needs from the final ROM image
    /// (e.g. default register read-back values embedded in the ROM).
    fn after_p_rom_load(&mut self, _data: &[u8]) {}

    /// Optionally intercept a P-ROM 8-bit read.  See `Cartridge::intercept_read_8`.
    fn intercept_read_8(&self, address: u32) -> Option<u8>;

    /// Optionally consume a 16-bit write.  See `Cartridge::on_write_16`.
    fn on_write_16(&mut self, address: u32, value: u16) -> bool;

    /// Optionally consume an 8-bit write.
    fn on_write_8(&mut self, _address: u32, _value: u8) -> bool { false }

    /// Capture mutable protection chip state.
    fn snapshot(&self) -> Vec<u8> { Vec::new() }

    /// Restore mutable protection chip state.
    fn restore(&mut self, _data: &[u8]) {}
}

// ── ComposedCart ──────────────────────────────────────────────────────────────

/// A cartridge composed of an optional [`PRomDecryptor`] and an optional
/// [`ProtectionChip`].
///
/// This covers every known NeoGeo configuration:
/// * Neither component → plain cart (same as the old `StandardCart`).
/// * Decryptor only    → encrypted ROM, no runtime protection.
/// * Protection only   → unencrypted ROM with a protection chip.
/// * Both              → encrypted ROM with a protection chip (e.g. KoF '98).
///
/// To share a decryptor between two games, just pass the same type to both
/// arms of `cart_for`.
pub struct ComposedCart {
    decryptor:  Option<Box<dyn PRomDecryptor>>,
    protection: Option<Box<dyn ProtectionChip>>,
}

impl ComposedCart {
    pub fn new(
        decryptor:  Option<Box<dyn PRomDecryptor>>,
        protection: Option<Box<dyn ProtectionChip>>,
    ) -> Self {
        Self { decryptor, protection }
    }
}

impl Cartridge for ComposedCart {
    fn process_p_rom(&mut self, mut data: Vec<u8>) -> Vec<u8> {
        byteswap_p_rom_if_needed(&mut data);
        if let Some(d) = &self.decryptor {
            d.decrypt(&mut data);
        }
        if let Some(p) = &mut self.protection {
            p.after_p_rom_load(&data);
        }
        data
    }

    fn intercept_read_8(&self, address: u32) -> Option<u8> {
        self.protection.as_ref()?.intercept_read_8(address)
    }

    fn on_write_16(&mut self, address: u32, value: u16) -> bool {
        self.protection.as_mut().map_or(false, |p| p.on_write_16(address, value))
    }

    fn on_write_8(&mut self, address: u32, value: u8) -> bool {
        self.protection.as_mut().map_or(false, |p| p.on_write_8(address, value))
    }

    fn snapshot(&self) -> Vec<u8> {
        self.protection.as_ref().map_or(Vec::new(), |p| p.snapshot())
    }

    fn restore(&mut self, data: &[u8]) {
        if let Some(p) = &mut self.protection {
            p.restore(data);
        }
    }

    fn initial_match_state(&self) -> Option<&'static [u8]> {
        None
    }
}

// ── KoF '98 components ────────────────────────────────────────────────────────

/// KoF '98 P-ROM decryptor.
///
/// `242-p1.p1` stores code in a scrambled layout in the lower 1 MB, with the
/// decryption key embedded in the upper 1 MB.  After decryption the lower half
/// holds valid 68k code and the sp2 extension is moved to the banked window at
/// $100000 (see MAME `prot_kof98.cpp` `decrypt_68k`).
pub struct Kof98Decryptor;

impl PRomDecryptor for Kof98Decryptor {
    fn decrypt(&self, data: &mut Vec<u8>) {
        decrypt_68k_kof98(data);

        // Anti-piracy WARNING-screen workaround.
        //
        // After decryption, the cart contains two copies of an anti-piracy gate
        // at $9F38 and $9FAA that test runtime markers ($10FD82 byte and
        // $D00100 word) and `JMP $B4CC` to a "WARNING" lockup at $B4E8 if
        // either is non-zero. In FBNeo with the same ROM and BIOS, the gate
        // also reads non-zero markers but does NOT lock up — meaning some
        // bus/CPU/timing detail FBNeo gets right makes the resulting branch
        // either skipped or harmless. We have not been able to reproduce that
        // behaviour and the divergence is not in the parts we can confirm
        // against FBNeo source: SRAM-protection gate, kof98Decrypt,
        // kof98Protection ($20AAAA → forge "NEO-" at $100), bank-switch.
        //
        // Suspected (untested) root causes, in order of likelihood:
        //   1. M68K cycle counts / interrupt timing differ from Musashi (FBNeo
        //      uses Musashi; we use the `m68k_cpu` crate). The cart may rely
        //      on an IRQ firing between marker-write and gate-read.
        //   2. ROM byte order in the M68K bus path. Decrypted bytes match
        //      FBNeo byte-for-byte, but our M68K may read them at a different
        //      effective offset due to a missing addr^1 swap somewhere.
        //   3. Vector-table overlay (cart-mode vs BIOS-mode) timing — the
        //      cart's reset path may diverge before reaching the gate.
        //
        // Until the real cause is identified, NOP both `JMP $B4CC` (6 bytes
        // each: 4E F9 00 00 B4 CC → 4E 71 4E 71 4E 71). This patches only the
        // two known gate sites and changes no other cart behaviour.
        for off in [0x9F48usize, 0x9FBEusize] {
            if off + 6 <= data.len() && &data[off..off + 6] == [0x4E, 0xF9, 0x00, 0x00, 0xB4, 0xCC] {
                data[off..off + 6].copy_from_slice(&[0x4E, 0x71, 0x4E, 0x71, 0x4E, 0x71]);
            }
        }
    }
}

/// KoF '98 ALTERA copy-protection chip.
///
/// Writes to $20AAAA configure the CPLD state machine; subsequent reads at
/// $000100–$000103 return forged values instead of ROM data:
/// * state `0x0090` → $000100 = $00C2, $000102 = $00FD
/// * state `0x00F0` → $000100 = $4E45 ("NE"), $000102 = $4F2D ("O-")
/// * state `0x0000` (default) → return actual ROM values
///
/// FBNeo writes the protection magic to BOTH $100 AND $400 (the BIOS vector
/// block is 1024 bytes, mirrored at $400). We must forge reads at both.
#[derive(Serialize, Deserialize)]
pub struct Kof98Protection {
    prot_state:  u8,
    prot_reg:    u16,
    default_rom: [u16; 2],
}

impl Default for Kof98Protection {
    fn default() -> Self {
        Self::new()
    }
}

impl Kof98Protection {
    pub fn new() -> Self {
        Self { prot_state: 0, prot_reg: 0, default_rom: [0; 2] }
    }

    fn update_state(&mut self) {
        self.prot_state = match self.prot_reg {
            0x0090 => 1,
            0x00F0 => 2,
            _ => 0,
        };
    }
}

impl ProtectionChip for Kof98Protection {
    fn after_p_rom_load(&mut self, data: &[u8]) {
        if data.len() >= 0x104 {
            self.default_rom[0] = ((data[0x100] as u16) << 8) | data[0x101] as u16;
            self.default_rom[1] = ((data[0x102] as u16) << 8) | data[0x103] as u16;
        }
    }

    fn intercept_read_8(&self, address: u32) -> Option<u8> {
        // Forge magic at $100..$103 AND its mirror at $400..$403 (FBNeo writes both).
        let local = match address {
            0x100..=0x103 => address - 0x100,
            0x400..=0x403 => address - 0x400,
            _ => return None,
        };

        if self.prot_state == 0 {
            return None;
        }

        let (w0, w1) = match self.prot_state {
            1 => (0x00C2u16, 0x00FDu16),  // state 0x0090
            2 => (0x4E45u16, 0x4F2Du16),  // state 0x00F0
            _ => (self.default_rom[0], self.default_rom[1]),
        };

        let word = if local < 2 { w0 } else { w1 };
        Some(if local & 1 == 0 { (word >> 8) as u8 } else { (word & 0xFF) as u8 })
    }

    fn on_write_16(&mut self, address: u32, value: u16) -> bool {
        if address != 0x20AAAA { return false; }
        self.prot_reg = value;
        self.update_state();
        true
    }

    fn on_write_8(&mut self, address: u32, value: u8) -> bool {
        match address {
            0x20AAAA => {
                self.prot_reg = (self.prot_reg & 0x00FF) | ((value as u16) << 8);
                self.update_state();
                true
            }
            0x20AAAB => {
                self.prot_reg = (self.prot_reg & 0xFF00) | (value as u16);
                self.update_state();
                true
            }
            _ => false
        }
    }

    fn snapshot(&self) -> Vec<u8> {
        bincode::serialize(self).unwrap_or_default()
    }

    fn restore(&mut self, data: &[u8]) {
        if let Ok(state) = bincode::deserialize(data) {
            *self = state;
        }
    }
}

// ── KOF98 game cartridge adapter ─────────────────────────────────────────────

/// KOF '98 cartridge adapter.
///
/// Composes the KOF98-specific decryptor + protection chip. Netplay boot
/// savestates are host-supplied on disk (see
/// [`crate::neogeo::Emulator::load_initial_match_state`]).
pub struct Kof98Cart {
    composed: ComposedCart,
}

impl Kof98Cart {
    pub fn new() -> Self {
        Self {
            composed: ComposedCart::new(
                Some(Box::new(Kof98Decryptor)),
                Some(Box::new(Kof98Protection::new())),
            ),
        }
    }
}

impl Cartridge for Kof98Cart {
    fn process_p_rom(&mut self, data: Vec<u8>) -> Vec<u8> {
        self.composed.process_p_rom(data)
    }

    fn intercept_read_8(&self, address: u32) -> Option<u8> {
        self.composed.intercept_read_8(address)
    }

    fn on_write_16(&mut self, address: u32, value: u16) -> bool {
        self.composed.on_write_16(address, value)
    }

    fn on_write_8(&mut self, address: u32, value: u8) -> bool {
        self.composed.on_write_8(address, value)
    }

    fn snapshot(&self) -> Vec<u8> {
        self.composed.snapshot()
    }

    fn restore(&mut self, data: &[u8]) {
        self.composed.restore(data);
    }

    fn initial_match_state(&self) -> Option<&'static [u8]> {
        // Boot blobs are host-supplied (see Emulator::load_initial_match_state_from_path).
        None
    }
}

// ── SMA Protection (KOF '99, etc.) ────────────────────────────────────────────

/// Super Magic Ass (SMA) protection chip.
/// Provides RNG at specific addresses to pass anti-piracy checks.
#[derive(Serialize, Deserialize)]
pub struct SmaProtection {
    rng: u32,
}

impl SmaProtection {
    pub fn new() -> Self {
        Self { rng: 0 }
    }
}

impl ProtectionChip for SmaProtection {
    fn intercept_read_8(&self, address: u32) -> Option<u8> {
        // Mock RNG read for SMA
        if address == 0x2FFC00 || address == 0x2FFCC0 {
            // Very simplified RNG return for now; some games just need it not to be constant 0xFF
            Some(((self.rng >> 8) & 0xFF) as u8)
        } else {
            None
        }
    }

    fn on_write_16(&mut self, _address: u32, _value: u16) -> bool {
        // SMA bankswitching usually writes to 0x2FFC00 region, handled by bus.
        // Update RNG on each cycle/write roughly.
        let new_bit = ((self.rng >> 0) ^ (self.rng >> 2) ^ (self.rng >> 3) ^ (self.rng >> 5)) & 1;
        self.rng = ((self.rng << 1) | new_bit) & 0x0FFFFF;
        false
    }

    fn snapshot(&self) -> Vec<u8> {
        bincode::serialize(self).unwrap_or_default()
    }

    fn restore(&mut self, data: &[u8]) {
        if let Ok(state) = bincode::deserialize(data) {
            *self = state;
        }
    }
}

// ── PVC Protection (SVC Chaos, KOF 2003, Metal Slug 5) ─────────────────────────

/// PVC protection chip.
/// Uses a RAM block at 0x2FE000 for its state and protection logic.
#[derive(Serialize, Deserialize)]
pub struct PvcProtection {
    ram: Vec<u8>,
}

impl PvcProtection {
    pub fn new() -> Self {
        Self { ram: vec![0; 0x2000] }
    }
}

impl ProtectionChip for PvcProtection {
    fn intercept_read_8(&self, address: u32) -> Option<u8> {
        if (0x2FE000..=0x2FFFFF).contains(&address) {
            let offset = (address - 0x2FE000) as usize;
            if offset < self.ram.len() {
                return Some(self.ram[offset]);
            }
        }
        None
    }

    fn on_write_16(&mut self, address: u32, value: u16) -> bool {
        if (0x2FE000..=0x2FFFFF).contains(&address) {
            let offset = (address - 0x2FE000) as usize;
            if offset < self.ram.len() {
                self.ram[offset] = (value >> 8) as u8;
                if offset + 1 < self.ram.len() {
                    self.ram[offset + 1] = (value & 0xFF) as u8;
                }
            }
            return true;
        }
        false
    }

    fn snapshot(&self) -> Vec<u8> {
        bincode::serialize(self).unwrap_or_default()
    }

    fn restore(&mut self, data: &[u8]) {
        if let Ok(state) = bincode::deserialize(data) {
            *self = state;
        }
    }
}

// ── Factory ───────────────────────────────────────────────────────────────────

/// Return the [`Cartridge`] for `game_name`.
///
/// Game-specific behaviour (decryptors, protection chips, initial states for
/// netplay fast-start, etc.) is provided entirely by Rust adapters.  No external
/// JSON configuration is used.
///
/// ## Adding a new game
///
/// * If the game reuses an existing decryptor or protection chip, just add a
///   `match` arm in `cart_for` and compose the existing components (usually
///   via [`ComposedCart`]).
/// * For games that need a dedicated adapter, implement [`Cartridge`]
///   (e.g. [`Kof98Cart`]) and add a match arm.
/// * `SystemBus` and `romset` never need to change.
///
/// If a game is not listed, a plain pass-through cart is used.
pub fn cart_for(game_name: &str) -> Box<dyn Cartridge> {
    match game_name {
        "kof98" => {
            // KOF98: dedicated decryptor + protection chip adapter.
            Box::new(Kof98Cart::new())
        }
        // SMA protection games (KOF '99 family, some Metal Slug, etc.)
        "kof99" | "kof2000" | "kof2001" | "mslug4" => Box::new(ComposedCart::new(
            None,
            Some(Box::new(SmaProtection::new())),
        )),
        // PVC protection games (SVC Chaos, KOF 2003, Metal Slug 5, etc.)
        "svc" | "kof2003" | "ms5" | "mslug5" => Box::new(ComposedCart::new(
            None,
            Some(Box::new(PvcProtection::new())),
        )),
        _ => Box::new(ComposedCart::new(None, None)),
    }
}

// ── Shared helpers ────────────────────────────────────────────────────────────

/// Detect and correct a byteswapped P-ROM in place.
///
/// Standard NeoGeo carts have SSP ≈ `$0010_F300` at the first 4 bytes.
/// If the raw SSP is implausibly large but the byteswapped form fits the
/// expected range, every 16-bit word is swapped.
pub(crate) fn byteswap_p_rom_if_needed(rom: &mut Vec<u8>) {
    if rom.len() < 8 { return; }
    let ssp_be = u32::from_be_bytes([rom[0], rom[1], rom[2], rom[3]]);
    let ssp_sw = u32::from_be_bytes([rom[1], rom[0], rom[3], rom[2]]);
    if ssp_be > 0x0020_0000 && ssp_sw <= 0x0020_0000 && (ssp_sw & 1) == 0 {
        log::debug!("P-ROM appears byteswapped (SSP=${:08X}); byteswapping in place", ssp_be);
        for i in (0..rom.len() - 1).step_by(2) {
            rom.swap(i, i + 1);
        }
    }
}

// ── KoF '98 decryption ────────────────────────────────────────────────────────

/// KoF '98 P-ROM decryption (MAME `prot_kof98.cpp` `decrypt_68k`).
///
/// `242-p1.p1` stores encrypted code in the lower 1 MB; the upper 1 MB holds
/// the decryption key data.  After rearrangement the lower half contains valid
/// 68k code.  The sp2 extension (if present at offset $200000) is shifted down
/// to $100000 (the banked window base).
fn decrypt_68k_kof98(p_rom: &mut Vec<u8>) {
    if p_rom.len() < 0x200000 {
        log::warn!(
            "kof98 P-ROM too small for decryption ({} bytes, need >= 2 MB)",
            p_rom.len()
        );
        return;
    }

    let mut dst = vec![0u8; 0x200000];
    dst.copy_from_slice(&p_rom[0..0x200000]);

    let sec = [0x000000u32, 0x100000, 0x000004, 0x100004,
               0x10000a, 0x00000a, 0x10000e, 0x00000e];
    let pos = [0x000u32, 0x004, 0x00a, 0x00e];

    for i in (0x800..0x100000).step_by(0x200) {
        for j in (0..0x100).step_by(0x10) {
            for k in (0..16).step_by(2) {
                let src_offset = i as usize + j as usize + sec[(k / 2) as usize] as usize + 0x100;
                let dst_offset = i as usize + j as usize + k as usize;
                p_rom[dst_offset..dst_offset+2].copy_from_slice(&dst[src_offset..src_offset+2]);

                let src_offset = i as usize + j as usize + sec[(k / 2) as usize] as usize;
                let dst_offset = i as usize + j as usize + k as usize + 0x100;
                p_rom[dst_offset..dst_offset+2].copy_from_slice(&dst[src_offset..src_offset+2]);
            }

            if i >= 0x080000 && i < 0x0c0000 {
                for k in 0..4 {
                    let offset = pos[k] as usize;
                    let src_off = i as usize + j as usize + offset;
                    let dst_off = i as usize + j as usize + offset;
                    p_rom[dst_off..dst_off+2].copy_from_slice(&dst[src_off..src_off+2]);

                    let src_off = i as usize + j as usize + offset + 0x100;
                    let dst_off = i as usize + j as usize + offset + 0x100;
                    p_rom[dst_off..dst_off+2].copy_from_slice(&dst[src_off..src_off+2]);
                }
            } else if i >= 0x0c0000 {
                for k in 0..4 {
                    let offset = pos[k] as usize;
                    let src_off = i as usize + j as usize + offset + 0x100;
                    let dst_off = i as usize + j as usize + offset;
                    p_rom[dst_off..dst_off+2].copy_from_slice(&dst[src_off..src_off+2]);

                    let src_off = i as usize + j as usize + offset;
                    let dst_off = i as usize + j as usize + offset + 0x100;
                    p_rom[dst_off..dst_off+2].copy_from_slice(&dst[src_off..src_off+2]);
                }
            }
        }

        let i = i as usize;
        p_rom[i+0x000000..i+0x000002].copy_from_slice(&dst[i+0x000000..i+0x000002]);
        p_rom[i+0x000002..i+0x000004].copy_from_slice(&dst[i+0x100000..i+0x100002]);
        p_rom[i+0x000100..i+0x000102].copy_from_slice(&dst[i+0x000100..i+0x000102]);
        p_rom[i+0x000102..i+0x000104].copy_from_slice(&dst[i+0x100100..i+0x100102]);
    }

   if p_rom.len() > 0x200000 {
        p_rom.copy_within(0x200000.., 0x100000);
        p_rom.truncate(0x100000 + (p_rom.len() - 0x200000));
    }

    // Diagnostic dump: vector bytes ($0..$F), header signature ($100..$108), and $400..$408.
    if p_rom.len() >= 0x410 {
        let v0 = u32::from_be_bytes([p_rom[0], p_rom[1], p_rom[2], p_rom[3]]);
        let v4 = u32::from_be_bytes([p_rom[4], p_rom[5], p_rom[6], p_rom[7]]);
        let sig100 = &p_rom[0x100..0x108];
        let sig400 = &p_rom[0x400..0x408];
        log::warn!(
            "[KOF98 decrypt] SSP=${:08X} PC=${:08X} sig@$100={:02X?} ('{}') sig@$400={:02X?} ('{}')",
            v0, v4, sig100,
            sig100.iter().map(|&b| if (0x20..0x7F).contains(&b) { b as char } else { '.' }).collect::<String>(),
            sig400,
            sig400.iter().map(|&b| if (0x20..0x7F).contains(&b) { b as char } else { '.' }).collect::<String>()
        );
    }

    log::debug!("kof98: P-ROM decrypted ({} bytes total)", p_rom.len());
}


#[cfg(test)]
mod kof98_decrypt_tests {
    use std::path::PathBuf;

    /// FBNeo `kof98Decrypt` (d_neogeo.cpp:6910) — bit-XOR address permutation.
    /// Expects `p_rom` to be the raw concatenation of p1(2 MB) + sp2(4 MB) = 6 MB.
    /// Output layout is [decrypted_p1(1 MB) | sp2(4 MB at $100000)] = 5 MB.
    fn fbneo_decrypt(p_rom: &mut Vec<u8>) {
        if p_rom.len() < 0x200000 { return; }
        let mut tmp = vec![0u8; 0x200000];
        for i in 0..0x100000usize {
            let mut j = i;
            if (i & 0x0000fc) == 0x000000 { j ^= 0x000100; }
            if (i & 0x0c0000) != 0x080000 { j ^= 0x000100; }
            if (i & 0x0c0008) == 0x080008 { j ^= 0x000100; }
            if (i & 0x0c00fe) == 0x080000 { j ^= 0x000100; }
            if (i & 0x0c0002) == 0x080002 { j ^= 0x000100; }
            if (i & 0x100000) == 0x100000 { j ^= 0x000102; }
            if (i & 0x000002) == 0x000002 { j ^= 0x100002; }
            if (i & 0x000008) == 0x000008 { j ^= 0x100002; }
            tmp[i] = p_rom[j];
        }
        // memmove(ROM+0x800, pTemp+0x800, 0x200000-0x800)
        p_rom[0x800..0x200000].copy_from_slice(&tmp[0x800..0x200000]);
        // memmove(ROM+0x100000, ROM+0x200000, 0x400000) — shift sp2 down
        if p_rom.len() > 0x200000 {
            p_rom.copy_within(0x200000.., 0x100000);
            p_rom.truncate(0x100000 + (p_rom.len() - 0x200000));
        }
    }

    fn load_raw_kof98() -> Option<Vec<u8>> {
        let root = std::env::var("CARGO_MANIFEST_DIR").ok()?;
        let dir  = PathBuf::from(root).join("roms/kof98");
        let p1   = std::fs::read(dir.join("242-p1.p1")).ok()?;
        let sp2  = std::fs::read(dir.join("242-p2.sp2")).ok()?;
        let mut v = Vec::with_capacity(p1.len() + sp2.len());
        v.extend_from_slice(&p1);
        v.extend_from_slice(&sp2);
        Some(v)
    }

    #[test]
    fn kof98_decrypt_matches_fbneo() {
        let Some(raw) = load_raw_kof98() else { eprintln!("kof98 ROM missing — skipping"); return; };
        let mut ours = raw.clone();
        super::decrypt_68k_kof98(&mut ours);
        let mut fbn = raw.clone();
        fbneo_decrypt(&mut fbn);
        assert_eq!(ours.len(), fbn.len(), "decrypt output length mismatch");
        let differ = (0..ours.len()).filter(|&i| ours[i] != fbn[i]).count();
        assert_eq!(differ, 0, "ours vs fbn differ in {} bytes", differ);
    }

}
