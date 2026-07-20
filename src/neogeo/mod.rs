//! NeoGeo (MVS/AES) system core.
//!
//! Houses the NeoGeo-specific hardware modules (68000, Z80, LSPC video,
//! YM2610 sound, cartridge/bus) and the [`Emulator`] type that ties them into
//! a single `crate::core::EmulatorCore` implementation.  Kept parallel to
//! `crate::cps` so each supported system lives in its own module tree.

pub mod cart;
pub mod bus;
pub mod sound;
pub mod video;
pub mod m68k;
pub mod z80;

use m68k::M68k;
use bus::SystemBus;
use sound::{YM2610, AudioController};
use video::VideoController;
use z80::Z80;

use crate::io::InputState;
use crate::core::{EmulatorCore, FrameOutput};
use crate::save_state::SaveState;
use crate::replay::InputDriver;
use crate::{AUDIO_SAMPLE_RATE, NOMINAL_SAMPLES_PER_FRAME};

use crate::romset;

/// 68k active-display cycles per frame (12 MHz / ~60 Hz, split into
/// 170 000 active + 30 000 VBL = 200 000 total so RTC POST passes).
const CYCLES_PER_FRAME: i32 = 170_000;

// ── Timing abstraction (Phase 2 start) ────────────────────────────────────────

/// Encapsulates the CPU/subsystem interleaving policy.
/// Currently uses fixed 100-cycle batches (easy to tune or replace with
/// an event queue later).
struct TimingController {
    batch_size: i32,
}

impl TimingController {
    fn new() -> Self {
        Self { batch_size: 100 }
    }

    fn advance(
        &self,
        m68k: &mut M68k,
        bus: &mut SystemBus,
        z80: &mut Z80,
        ym2610: &mut YM2610,
        total: i32,
    ) {
        let mut rem = total;
        while rem > 0 {
            let batch = rem.min(self.batch_size);
            m68k.set_irq(bus.irq_level());
            crate::trace::check_pc(m68k.get_pc());
            let ran = m68k.execute(bus, batch);
            rem -= ran.max(1);

            bus.tick_timer(ran as u32);
            bus.rtc.update(ran as u64);
            z80.execute(ran as usize, bus, ym2610);
            ym2610.update(ran as usize);
        }
    }
}

// ── Emulator ──────────────────────────────────────────────────────────────────

/// Complete NeoGeo system — the embeddable unit.
///
/// Owns all hardware state.  ROM images are loaded at startup and are NOT
/// included in save states (they are reloaded from disk on each run).
///
/// # Thread safety
///
/// `Emulator` is `Send`. The YM2610 C++ backend binds IRQ output per instance;
/// avoid keeping multiple live instances on one thread during rollback tests.
pub struct Emulator {
    pub(crate) m68k:   M68k,
    pub(crate) z80:    Z80,
    pub(crate) bus:    SystemBus,
    pub(crate) ym2610: YM2610,
    audio:             AudioController,
    video:             VideoController,
    framebuffer:       Vec<u8>,
    audio_buf:         Vec<f32>,
    /// Active input mode: live play, recording, or replay.
    pub input_driver:  InputDriver,
    /// Monotonic frame counter, incremented by each `step` call.
    frame:             u64,
    _debug_framebuffer: Vec<u8>,

    // Phase 2 timing abstraction (owns batch policy and interleaving).
    timing:            TimingController,
    /// Cached ADPCM ROM bytes so YM2610 can be recreated on save-state restore.
    ym2610_adpcm_a:    Vec<u8>,
    ym2610_adpcm_b:    Vec<u8>,
    /// Last successfully loaded set name (for boot savestate path lookup).
    rom_set_name:      Option<String>,
}

impl Emulator {
    pub fn bus_mut(&mut self) -> &mut SystemBus {
        &mut self.bus
    }

    /// Create a new emulator with no ROMs loaded and all RAM zeroed.
    pub fn new() -> Self {
        let mut emu = Self {
            m68k:         M68k::new(),
            z80:          Z80::new(),
            bus:          SystemBus::new(),
            ym2610:       YM2610::new(),
            audio:        AudioController::new(),
            video:        VideoController::new(),
            framebuffer:  vec![0u8; video::FRAMEBUFFER_BYTES],
            audio_buf:    Vec::with_capacity(NOMINAL_SAMPLES_PER_FRAME + 64),
            input_driver: InputDriver::live(),
            frame:        0,
            _debug_framebuffer: vec![0; 304 * 224 * 3],
            timing:            TimingController::new(),
            ym2610_adpcm_a:    Vec::new(),
            ym2610_adpcm_b:    Vec::new(),
            rom_set_name:      None,
        };
        emu.sync_irq_binding();
        emu
    }

    fn cache_and_load_adpcm(&mut self, adpcm_a: &[u8], adpcm_b: &[u8]) {
        self.ym2610_adpcm_a = adpcm_a.to_vec();
        self.ym2610_adpcm_b = adpcm_b.to_vec();
        if !adpcm_a.is_empty() {
            self.ym2610.load_adpcm_a(adpcm_a);
        }
        if !adpcm_b.is_empty() {
            self.ym2610.load_adpcm_b(adpcm_b);
        }
    }

    fn recreate_ym2610_from_snapshot(&mut self, data: &[u8]) {
        self.ym2610 = YM2610::new();
        if !self.ym2610_adpcm_a.is_empty() {
            self.ym2610.load_adpcm_a(&self.ym2610_adpcm_a);
        }
        if !self.ym2610_adpcm_b.is_empty() {
            self.ym2610.load_adpcm_b(&self.ym2610_adpcm_b);
        }
        self.ym2610.load_snapshot(data);
    }

    // ── ROM loading ───────────────────────────────────────────────────────────

    /// Load ROMs for a set name (`roms/<name>/`) or a direct P-ROM path.
    ///
    /// Named sets load from disk under `roms/<name>/` (no network).  Optional
    /// catalog download lives behind feature `native-romset`
    /// (`romset::ensure_roms_dir` / `prepare_and_load`) and is host-driven.
    /// System BIOS / sfix / sm1 / lo are loaded from host paths via
    /// [`SystemBus::load_host_system_roms`](bus::SystemBus::load_host_system_roms).
    ///
    /// A bare P-ROM file path loads only that program ROM plus host system ROMs
    /// (not full C/S/M1/ADPCM sets). Prefer a prepared `roms/<name>/` directory
    /// for complete games.
    pub fn load_roms(&mut self, name: Option<&str>) -> Result<(), String> {
        let Some(arg) = name else {
            return Err("No ROM name given".to_string());
        };

        let is_name = !arg.contains('/')
            && !arg.contains('\\')
            && !std::path::Path::new(arg).exists();

        if is_name {
            match romset::load_prepared_game(&mut self.bus, arg) {
                Ok(()) => {
                    let (adpcm_a, adpcm_b) = romset::collect_adpcm_roms_for(arg);
                    self.cache_and_load_adpcm(&adpcm_a, &adpcm_b);
                    self.rom_set_name = Some(arg.to_string());
                    return Ok(());
                }
                Err(e) => {
                    return Err(format!("Could not load ROMs for '{arg}': {e}"));
                }
            }
        }

        // Treat argument as a direct P-ROM file path (program ROM only + system).
        self.bus.load_p_rom(arg).map_err(|e| format!("{e}"))?;
        self.bus.load_host_system_roms();
        self.rom_set_name = None;
        Ok(())
    }

    /// Load battery-backed SRAM from a raw byte slice.
    ///
    /// Returns `true` if `data` matches the expected 64 KB size and was
    /// copied in.  High-scores and soft-DIPs survive between sessions this way.
    pub fn load_sram(&mut self, data: &[u8]) -> bool {
        if data.len() == self.bus.backup_ram().len() {
            self.bus.backup_ram_mut().copy_from_slice(data);
            true
        } else {
            log::warn!(
                "SRAM size mismatch: got {} bytes, expected {}",
                data.len(), self.bus.backup_ram().len()
            );
            false
        }
    }

    // ── Reset ─────────────────────────────────────────────────────────────────

    /// Cold-reset the system.
    ///
    /// Must be called after `load_roms` and before the first `step`.
    pub fn reset(&mut self) {
        if !self.bus.roms.sm1_rom.is_empty() {
            self.z80.set_m1_rom(&self.bus.roms.sm1_rom.clone());
        }
        self.bus.reset();
        self.m68k.init(&mut self.bus);
        self.z80.reset();
        self.ym2610.reset();
        self.sync_irq_binding();
    }

    // ── Per-frame API ─────────────────────────────────────────────────────────

    /// Set the controller and system input state for the upcoming frame.
    ///
    /// Call this before `step` each frame.  The bus latches the state and
    /// holds it for the duration of the frame.
    pub fn set_input(&mut self, state: InputState) {
        self.bus.apply_input(state);
    }

    /// Advance one video frame.
    ///
    /// Executes active-display cycles, fires V-blank, renders the sprite and
    /// fix layers, and produces `n_audio_samples` PCM samples.
    ///
    /// Returns borrowed references into the emulator's internal framebuffer
    /// and audio buffer.  The data is valid until the next call to `step`.
    pub fn step(&mut self, n_audio_samples: usize) -> FrameOutput<'_> {
        self.frame += 1;
        self.ym2610.begin_frame();

        // Active-display cycles.
        self.run_cycles(CYCLES_PER_FRAME);

        // V-blank interrupt (IRQ level 1) then blanking-period cycles.
        // Advance auto-animation before raising VBL so the frame counter is
        // up-to-date when games read REG_LSPCMODE in the VBL handler.
        self.bus.lspc.tick_vbl();
        self.bus.raise_irq(0x04);
        self.run_cycles(30_000);

        // Render completed frame (clean video only).
        let snap = self.bus.video_snapshot();
        self.video.render(&mut self.framebuffer, &snap);

        // Produce audio.
        self.audio_buf.clear();
        self.audio.generate_samples(&mut self.audio_buf, &mut self.ym2610, n_audio_samples);

        FrameOutput {
            framebuffer: &self.framebuffer,
            audio:       &self.audio_buf,
        }
    }

    /// Fast step — runs one frame of CPU + video logic but skips framebuffer
    /// composition and PCM drain.  Chip timers advance identically to
    /// `step`; only the display path drains the transient glue ring.
    pub fn step_cpu(&mut self) {
        self.frame += 1;
        self.ym2610.begin_frame();
        self.run_cycles(CYCLES_PER_FRAME);
        self.bus.lspc.tick_vbl();
        self.bus.raise_irq(0x04);
        self.run_cycles(30_000);
    }

    // ── Save state ────────────────────────────────────────────────────────────

    /// Serialize all mutable state to a compact binary blob.
    ///
    /// Suitable for rollback netcode: typical size is < 200 KB; round-trip
    /// latency is < 1 ms on modern hardware.
    pub fn save_state_to_bytes(&mut self) -> Result<Vec<u8>, String> {
        let bytes = self.build_save_state().to_bytes()?;
        self.load_state_from_bytes(&bytes)?;
        Ok(bytes)
    }

    /// Restore state from a blob produced by `save_state_to_bytes`.
    pub fn load_state_from_bytes(&mut self, data: &[u8]) -> Result<(), String> {
        let state = SaveState::from_bytes(data)?;
        self.apply_save_state(state)
    }

    /// Write save state to a human-readable JSON file (for debugging).
    pub fn save_state_to_file(&self, path: &str) -> Result<(), String> {
        self.build_save_state().write_to_file(path)
    }

    /// Load save state from a JSON file written by `save_state_to_file`.
    pub fn load_state_from_file(&mut self, path: &str) -> Result<(), String> {
        let state = SaveState::read_from_file(path)?;
        self.apply_save_state(state)
    }

    /// Load a netplay boot savestate (character select / match-ready).
    ///
    /// After `load_roms` + `reset`, peers jump to a shared frame-0. Search order
    /// is defined by [`crate::boot::initial_match_state_paths`] (`boot/` first).
    /// **ROM dumps are never embedded** — only optional `charselect.bin` files.
    pub fn load_initial_match_state(&mut self) -> bool {
        let names: Vec<String> = match self.rom_set_name.clone() {
            Some(n) => vec![n],
            None => vec!["kof98".into()], // legacy default when name unknown
        };
        for name in &names {
            for path in crate::boot::initial_match_state_paths(name) {
                if self.load_initial_match_state_from_path(path.to_str().unwrap_or("")) {
                    return true;
                }
            }
        }
        if let Some(bytes) = self.bus.cart.initial_match_state() {
            return match self.load_state_from_bytes(bytes) {
                Ok(()) => true,
                Err(e) => {
                    log::warn!("cart initial match state load failed: {e}");
                    false
                }
            };
        }
        false
    }

    /// Load a netplay boot savestate from an explicit filesystem path.
    pub fn load_initial_match_state_from_path(&mut self, path: &str) -> bool {
        match std::fs::read(path) {
            Ok(bytes) => match self.load_state_from_bytes(&bytes) {
                Ok(()) => {
                    log::info!("loaded initial match state from {path}");
                    true
                }
                Err(e) => {
                    log::warn!("initial match state from {path} failed: {e}");
                    false
                }
            },
            Err(_) => false,
        }
    }

    // ── Accessors ─────────────────────────────────────────────────────────────

    /// Borrow the rendered RGB24 framebuffer from the last `step`.
    pub fn framebuffer(&self) -> &[u8] { &self.framebuffer }

    /// Borrow the PCM audio from the last `step`.
    pub fn audio_samples(&self) -> &[f32] { &self.audio_buf }

    /// Borrow the battery-backed SRAM (64 KB).
    pub fn backup_ram(&self) -> &[u8] { self.bus.backup_ram() }

    /// Borrow the main work RAM (2 MB).
    pub fn work_ram(&self) -> &[u8] { self.bus.work_ram() }

    /// Current 68000 program counter (diagnostics).
    pub fn m68k_pc(&self) -> u32 { self.m68k.get_pc() }

    /// Absolute frame counter since construction.
    pub fn frame(&self) -> u64 { self.frame }

    /// Total audio samples produced (diagnostics).
    pub fn audio_sample_count(&self) -> u64 { self.audio.sample_count }

    /// Total video frames rendered (diagnostics).
    pub fn video_frame_count(&self) -> usize { self.video.frame_count }

    /// Flush a recording session to disk (no-op if not recording).
    pub fn save_recording(&mut self) -> Result<(), String> {
        self.input_driver.save_if_recording()
    }
    
    /// Enable the BIOS operator service menu at next boot.
    ///
    /// Clears HW DIP switch 1 (active-low) — the BIOS checks this bit during
    /// POST and enters the operator menu when it is `0`.  Only meaningful when
    /// called **before** `reset`.
    pub fn enable_operator_menu(&mut self) {
        self.bus.hw_dips &= !0x01;
    }

    /// Present as AES/home cartridge (REG_STATUS_B bit 7 = 0).
    ///
    /// Keeps the host-loaded MVS BIOS (typically sp-s2.sp1) — swapping to sp-e.sp1 breaks
    /// the auto-boot path (AES BIOS waits for user input) and its RAM POST
    /// clashes with `SystemBus::sync_aes_bios_ram`.  MVS BIOS reads
    /// REG_STATUS_B bit 7 and stores the corresponding value into
    /// `BIOS_MVS_FLAG`, which is what games such as KOF98 read to expose
    /// Practice mode.
    pub fn enable_aes_home(&mut self) {
        self.bus.set_presentation_aes(true);
    }

    /// Present as MVS/arcade cabinet (REG_STATUS_B bit 7 = 1).
    pub fn enable_mvs_presentation(&mut self) {
        self.bus.set_presentation_aes(false);
    }

    // ── Diagnostics ───────────────────────────────────────────────────────────

    /// One-line BIOS boot-progress snapshot (for debug logging).
    ///
    /// Reports palette/VRAM fill status, BIOS state-machine progress, and the
    /// POST failure registers.  Only meaningful while the BIOS is running.
    pub fn bios_debug_info(&self) -> String {
        let bus = &self.bus;
        let pal_nz  = bus.lspc.pal_ram.iter().filter(|&&b| b != 0).count();
        let vram_nz = bus.lspc.vram.iter().filter(|&&b| b != 0).count();
        let bg_word = ((bus.lspc.pal_ram[0] as u16) << 8) | bus.lspc.pal_ram[1] as u16;
        let fix00   = ((bus.lspc.vram[0x7000 * 2] as u16) << 8)
                    | bus.lspc.vram[0x7000 * 2 + 1] as u16;
        let sm_state = if bus.work_ram().len() > 0xFCD9 {
            ((bus.work_ram()[0xFCD8] as u16) << 8) | bus.work_ram()[0xFCD9] as u16
        } else { 0xFFFF };
        let d00038 = bus.backup_ram().get(0x38).copied().unwrap_or(0xFF);
        let s47    = bus.backup_ram().get(0x47).copied().unwrap_or(0);
        let s124   = ((bus.backup_ram().get(0x124).copied().unwrap_or(0) as u16) << 8)
                   | bus.backup_ram().get(0x125).copied().unwrap_or(0) as u16;
        let fa     = u32::from_be_bytes(
            bus.work_ram()[0xFD00..0xFD04].try_into().unwrap_or([0; 4])
        );
        let fexp   = u16::from_be_bytes(
            bus.work_ram()[0xFD04..0xFD06].try_into().unwrap_or([0; 2])
        );
        let fact   = u16::from_be_bytes(
            bus.work_ram()[0xFD06..0xFD08].try_into().unwrap_or([0; 2])
        );
        let ftest  = u16::from_be_bytes(
            bus.work_ram()[0xFD08..0xFD0A].try_into().unwrap_or([0; 2])
        );
        format!(
            "PC=${:06X}  pal_nz={:4}  vram_nz={:5}  bg=${:04X}  fix[0]=${:04X}  \
             sm={:2}  swp={}  d00038=${:02X}  s47=${:02X}  s124=${:04X}  \
             FAIL: test={} a0=${:08X} exp=${:04X} act=${:04X}",
            self.m68k_pc(), pal_nz, vram_nz, bg_word, fix00,
            sm_state, bus.swp_rom as u8, d00038, s47, s124,
            ftest, fa, fexp, fact
        )
    }

    // ── Internal helpers ──────────────────────────────────────────────────────

    /// Wire ym2610 glue IRQ output to this emulator's Z80 (per-instance).
    fn sync_irq_binding(&mut self) {
        let int_ptr = self.z80.int_line_ptr();
        self.ym2610.bind_z80_int(int_ptr);
    }

    /// Run `total` M68k cycles. The interleaving policy lives in TimingController.
    fn run_cycles(&mut self, total: i32) {
        self.timing.advance(
            &mut self.m68k,
            &mut self.bus,
            &mut self.z80,
            &mut self.ym2610,
            total,
        );
    }

    pub fn ym2610_irq_assert_count(&self) -> u64 {
        self.ym2610.irq_assert_count()
    }

    pub fn ym2610_timer_expire_count(&self, idx: usize) -> u64 {
        self.ym2610.timer_expire_count(idx)
    }

    fn build_save_state(&self) -> SaveState {
        // Do not call begin_frame() here — see save_state_to_bytes().
        let ym2610 = self.ym2610.snapshot();
        SaveState {
            version: SaveState::VERSION.to_owned(),
            frame:   self.frame,
            m68k:    self.m68k.snapshot(),
            z80:     self.z80.snapshot(&self.bus),
            bus:     self.bus.snapshot(),
            ym2610,
        }
    }

    fn apply_save_state(&mut self, state: SaveState) -> Result<(), String> {
        if state.version != SaveState::VERSION {
            log::warn!(
                "Save state version mismatch: file='{}' expected='{}'",
                state.version, SaveState::VERSION
            );
        }
        let active_m1 = state.z80.active_m1_rom;
        self.m68k.restore(state.m68k);
        self.bus.restore(state.bus);
        self.z80.restore(state.z80);
        // Recreate the C++ backend from scratch so a long-lived instance matches
        // a fresh emulator after the same blob (reset+load on one NeoYmState
        // left rollback drift at frame 44→45).
        self.recreate_ym2610_from_snapshot(&state.ym2610);
        self.frame = state.frame;
        // Presentation-layer counters are not snapshotted; zero them so a
        // long-lived instance matches a fresh emulator after the same blob.
        self.audio = AudioController::new();
        self.video = VideoController::new();
        self.sync_irq_binding();

        // Restore the active Z80 M1 ROM from the snapshot (not brd_fix alone).
        use crate::neogeo::bus::ActiveM1Rom;
        match active_m1 {
            x if x == ActiveM1Rom::Sm1 as u8 && !self.bus.roms.sm1_rom.is_empty() => {
                self.z80.restore_m1_rom(&self.bus.roms.sm1_rom);
            }
            x if x == ActiveM1Rom::Cart as u8 && !self.bus.roms.m1_rom.is_empty() => {
                self.z80.restore_m1_rom(&self.bus.roms.m1_rom);
            }
            _ => {}
        }

        Ok(())
    }
}

impl Default for Emulator {
    fn default() -> Self { Self::new() }
}

impl EmulatorCore for Emulator {
    fn load_roms(&mut self, name: Option<&str>) -> Result<(), String> {
        self.load_roms(name)
    }
    fn reset(&mut self) { self.reset(); }
    fn set_input(&mut self, state: InputState) { self.set_input(state); }
    fn step(&mut self, n: usize) -> FrameOutput<'_> { self.step(n) }
    fn framebuffer(&self) -> &[u8] { self.framebuffer() }
    fn audio_samples(&self) -> &[f32] { self.audio_samples() }
    fn frame(&self) -> u64 { self.frame }
    fn step_cpu(&mut self) { self.step_cpu(); }
    fn work_ram(&self) -> &[u8] { Emulator::work_ram(self) }
    fn save_state_to_bytes(&mut self) -> Result<Vec<u8>, String> { self.save_state_to_bytes() }
    fn state_debug_checksums(&self) -> Option<[u16; 8]> {
        Some(self.build_save_state().debug_checksums())
    }

    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }

    fn save_state_and_checksums(&mut self) -> Result<(Vec<u8>, Option<[u16; 8]>), String> {
        let state = self.build_save_state();
        Ok((state.to_bytes()?, Some(state.debug_checksums())))
    }
    fn load_state_from_bytes(&mut self, data: &[u8]) -> Result<(), String> { self.load_state_from_bytes(data) }
    fn save_state_to_file(&self, path: &str) -> Result<(), String> { self.save_state_to_file(path) }
    fn load_state_from_file(&mut self, path: &str) -> Result<(), String> { self.load_state_from_file(path) }
    fn save_recording(&mut self) -> Result<(), String> { self.input_driver.save_if_recording() }
    fn backup_ram(&self) -> &[u8] { Emulator::backup_ram(self) }
    fn load_sram(&mut self, data: &[u8]) -> bool { self.load_sram(data) }
    fn resolution(&self) -> (u32, u32) { (304, 224) }
    fn audio_sample_rate(&self) -> u32 { AUDIO_SAMPLE_RATE }
    fn refresh_rate(&self) -> f64 { 59.185606 } // NeoGeo LSPC field rate
    fn debug_pc(&self) -> u32 { self.m68k.get_pc() }
    fn video_frame_count(&self) -> usize { self.video.frame_count }
    fn audio_sample_count(&self) -> u64 { self.audio.sample_count }
    fn enable_operator_menu(&mut self) { self.bus.hw_dips &= !0x01; }
    fn enable_aes_home(&mut self) { self.enable_aes_home(); }
    fn enable_mvs_presentation(&mut self) { self.enable_mvs_presentation(); }
    fn load_initial_match_state(&mut self) -> bool { self.load_initial_match_state() }
}

#[cfg(test)]
mod rollback_tests;
