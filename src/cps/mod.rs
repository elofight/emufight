//! CPS1 emulator core.
//!
//! Ties together the 68000 main CPU, the CPS-A/B custom-chip bus, the tile /
//! sprite video renderer and the Z80 + OKI + YM2151 sound board, exposing the
//! shared `crate::core::EmulatorCore` frame-level interface.
//!
//! Supported games: Street Fighter II World Warrior (`sf2`) and Champion
//! Edition (`sf2ce`).

mod bus;
mod config;
mod romset;
pub mod save_state;
mod sound;
mod video;

use m68k_cpu::{CpuCore, CpuType};

use crate::core::{EmulatorCore, FrameOutput};
use crate::io::InputState;
use bus::CpsBus;
use config::CpsGame;
use sound::CpsSound;
use video::CpsVideo;

/// Fold a serialised state blob into 8 packed 16-bit FNV-1a lanes for GGRS
/// rollback desync debugging (mirrors the NeoGeo core's checksum layout).
fn fold_checksums(bytes: &[u8]) -> [u16; 8] {
    let mut lanes = [0xcbf2_9ce4_8422_2325u64; 8];
    for (i, &b) in bytes.iter().enumerate() {
        let l = &mut lanes[i & 7];
        *l ^= b as u64;
        *l = l.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let mut out = [0u16; 8];
    for (o, l) in out.iter_mut().zip(lanes.iter()) {
        *o = (*l ^ (*l >> 16) ^ (*l >> 32) ^ (*l >> 48)) as u16;
    }
    out
}

/// A CPS1 arcade machine.
pub struct CpsEmulator {
    cpu: CpuCore,
    bus: CpsBus,
    video: CpsVideo,
    sound: CpsSound,
    audio_buf: Vec<f32>,
    frame: u64,
    audio_total: u64,
    game: &'static CpsGame,
}

impl CpsEmulator {
    /// Create a CPS1 machine defaulting to the `sf2` configuration until
    /// `load_roms`(EmulatorCore::load_roms) selects a game.
    pub fn new() -> Self {
        let game = config::find("sf2").expect("sf2 config present");
        CpsEmulator {
            cpu: Self::make_cpu(),
            bus: CpsBus::new(game),
            video: CpsVideo::new(),
            sound: CpsSound::new(),
            audio_buf: Vec::with_capacity(1024),
            frame: 0,
            audio_total: 0,
            game,
        }
    }

    /// Build a plain 68000 core.
    ///
    /// Unlike `m68k.rs` (NeoGeo) we do *not* fake an EC020 to suppress
    /// address-error checks: CPS1 boot code relies on true 68000 extension-word
    /// decoding (brief format).  On the 68020 an extension word with bit 8 set
    /// selects the full format (base displacement / scaling / memory indirect),
    /// which changes instruction length and derails execution.
    fn make_cpu() -> CpuCore {
        let mut cpu = CpuCore::new();
        cpu.set_cpu_type(CpuType::M68000);
        cpu
    }

    fn apply_input(&mut self, s: &InputState) {
        // `InputState` uses the NeoGeo active-low bit layout:
        //   p1/p2: bit0=Up 1=Down 2=Left 3=Right 4=A 5=B 6=C 7=D
        //   ext:   bit0=P1E 1=P1F 2=P2E 3=P2F
        // CPS1 IN1/IN2 use a *different* order (MAME `cps1_6b`):
        //   IN1 per player: bit0=Right 1=Left 2=Down 3=Up 4=B1 5=B2 6=B3
        //   IN2 per player: bit0=B4 1=B5 2=B6  (P2 shifted to bits 4-6)
        // Buttons map A/B/C = punches (B1/B2/B3) and D/E/F = kicks (B4/B5/B6),
        // (light→medium→heavy each).  All active-low.
        fn to_cps_in1(p: u8) -> u8 {
            let bit = |n: u8| (p >> n) & 1; // 1 = released (active-low)
            (bit(3) << 0)   // Right
                | (bit(2) << 1) // Left
                | (bit(1) << 2) // Down
                | (bit(0) << 3) // Up
                | (bit(4) << 4) // B1 (A / jab)
                | (bit(5) << 5) // B2 (B / strong)
                | (bit(6) << 6) // B3 (C / fierce)
                | (1 << 7) // unused, held high
        }
        // Extra buttons for one player: D (from p bit7) + E/F (from ext bits).
        fn to_cps_in2(p_d: u8, e: u8, f: u8) -> u8 {
            (p_d << 0)      // B4 (D / short kick)
                | (e << 1)  // B5 (E / forward kick)
                | (f << 2)  // B6 (F / roundhouse kick)
        }

        let p1 = to_cps_in1(s.p1);
        let p2 = to_cps_in1(s.p2);
        self.bus.in1 = ((p2 as u16) << 8) | p1 as u16;

        // System port (coins / start / service), all active-low.
        let mut in0 = 0xffu8;
        if s.coin & 0x01 == 0 {
            in0 &= !0x01; // coin 1
        }
        if s.coin & 0x02 == 0 {
            in0 &= !0x02; // coin 2
        }
        if s.coin & 0x04 == 0 {
            in0 &= !0x08; // service / test
        }
        if s.sys & 0x01 == 0 {
            in0 &= !0x10; // start 1
        }
        if s.sys & 0x04 == 0 {
            in0 &= !0x20; // start 2
        }
        self.bus.in0 = in0;

        // Extra-button register (IN2) for 6-button games.  Bits held high when
        // released; P1 in the low nibble, P2 in the high nibble.
        let p1_ext = to_cps_in2((s.p1 >> 7) & 1, s.ext & 1, (s.ext >> 1) & 1);
        let p2_ext = to_cps_in2((s.p2 >> 7) & 1, (s.ext >> 2) & 1, (s.ext >> 3) & 1);
        self.bus.in2 = 0xff00 | (p1_ext as u16) | ((p2_ext as u16) << 4) | 0x88;
    }

    fn run_frame(&mut self) {
        // CPS1 frame timing (MAME): the 68000 runs the active display, then a        // VBLANK interrupt (IRQ level 2 / IPL1) is asserted at scanline 240 and
        // held until the CPU acknowledges it (bus.interrupt_acknowledge, i.e.
        // MAME irqack_r). The handler runs during the ~22-scanline blanking
        // period. Asserting the IRQ once per frame near the *end* — rather than
        // for the whole frame — is essential: the boot task scheduler polls a
        // vblank-synchronised flag set by this handler, so the phase of the IRQ
        // relative to the main loop must match hardware.
        const SCANLINES: u32 = 262;
        const VBLANK_SCANLINE: u32 = 240;
        let cycles_per_frame = self.game.cpu_clock / 60;
        let active = (cycles_per_frame as u64 * VBLANK_SCANLINE as u64 / SCANLINES as u64) as i32;
        let vblank = cycles_per_frame as i32 - active;

        // Latch the sprite list at the vblank boundary (MAME screen_vblank_cps1
        // memcpy to m_buffered_obj).  Snapshotting here — before this frame's
        // vblank handler rebuilds the OBJ table — makes rendered sprites stable
        // and one frame delayed, as on hardware, eliminating flicker/garbage
        // from reading a half-built list.
        self.video.buffer_sprites(&self.bus);

        // Active display — no maskable IRQ pending yet.
        self.run_cpu(active);

        // Assert VBLANK (IPL1 / IRQ2); held until acknowledged.
        self.bus.irq_pending |= 0x01;
        self.run_cpu(vblank);

        // Forward any pending sound command to the Z80 board and run it.
        if self.bus.sound_latch_pending {
            self.sound.set_latch(self.bus.sound_latch);
            self.bus.sound_latch_pending = false;
        }
        self.sound.run_frame();
    }

    /// Run the 68000 for `cycles`, re-asserting the currently-held interrupt
    /// level before every batch (the crate clears `int_level` once it services
    /// an interrupt, so a level-triggered line must be re-applied until the
    /// game acknowledges it via `interrupt_acknowledge`).
    fn run_cpu(&mut self, cycles: i32) {
        let mut remaining = cycles;
        while remaining > 0 {
            self.cpu.int_level = self.bus.irq_level();
            let ran = self.cpu.execute(&mut self.bus, remaining.min(4000)).max(1);
            remaining -= ran;
        }
    }

    /// Capture the full machine state into a serialisable [`CpsSaveState`].
    fn build_save_state(&self) -> save_state::CpsSaveState {
        save_state::CpsSaveState {
            version: save_state::CPS_SAVE_VERSION,
            cpu: save_state::cpu_snapshot(&self.cpu),
            bus: self.bus.snapshot(),
            sound: self.sound.snapshot(),
            buffered_obj: self.video.snapshot_obj(),
            frame: self.frame,
            audio_total: self.audio_total,
        }
    }

    /// Restore the full machine state from a [`CpsSaveState`].
    fn apply_save_state(&mut self, state: &save_state::CpsSaveState) {
        save_state::cpu_restore(&mut self.cpu, &state.cpu);
        self.bus.restore(&state.bus);
        self.sound.restore(&state.sound);
        self.video.restore_obj(&state.buffered_obj);
        self.frame = state.frame;
        self.audio_total = state.audio_total;
    }
}

impl Default for CpsEmulator {
    fn default() -> Self {
        Self::new()
    }
}

impl EmulatorCore for CpsEmulator {
    fn load_roms(&mut self, name: Option<&str>) -> Result<(), String> {
        let name = name.unwrap_or("sf2");
        let game = config::find(name)
            .ok_or_else(|| format!("unsupported CPS1 game '{}'", name))?;
        let roms = romset::load(name)?;

        self.game = game;
        self.bus = CpsBus::new(game);
        self.bus.program = roms.program; // ROM_LOAD16_BYTE already gives correct 68k byte order
        self.video.set_gfx(roms.gfx);
        self.sound = CpsSound::new();
        self.sound.load_z80_rom(&roms.z80);
        self.sound.load_oki_rom(&roms.oki);
        Ok(())
    }

    fn reset(&mut self) {
        self.cpu = Self::make_cpu();
        self.cpu.reset(&mut self.bus);
        self.sound.reset();
        self.frame = 0;
        self.audio_total = 0;
    }

    fn set_input(&mut self, state: InputState) {
        self.apply_input(&state);
    }

    fn step(&mut self, n_audio_samples: usize) -> FrameOutput<'_> {
        self.run_frame();
        self.video.render(&self.bus);

        self.audio_buf.clear();
        let n = if n_audio_samples == 0 {
            crate::NOMINAL_SAMPLES_PER_FRAME
        } else {
            n_audio_samples
        };
        self.sound.generate(&mut self.audio_buf, n);

        self.frame += 1;
        // Advance by a fixed per-frame count (NOT audio_buf.len()) so the value
        // is a pure function of the frame number and stays identical to the
        // catch-up path below — otherwise confirmed frames desync depending on
        // each peer's display/catch-up rollback schedule.
        self.audio_total += crate::NOMINAL_SAMPLES_PER_FRAME as u64;
        FrameOutput {
            framebuffer: &self.video.framebuffer,
            audio: &self.audio_buf,
        }
    }

    fn framebuffer(&self) -> &[u8] {
        &self.video.framebuffer
    }

    fn audio_samples(&self) -> &[f32] {
        &self.audio_buf
    }

    fn frame(&self) -> u64 {
        self.frame
    }

    fn step_cpu(&mut self) {
        self.run_frame();
        self.frame += 1;
        // Mirror the display-step increment so audio_total advances identically
        // on rollback catch-up frames (keeps it out of the desync surface).
        self.audio_total += crate::NOMINAL_SAMPLES_PER_FRAME as u64;
    }

    fn save_state_to_bytes(&mut self) -> Result<Vec<u8>, String> {
        self.build_save_state().to_bytes()
    }

    fn load_state_from_bytes(&mut self, data: &[u8]) -> Result<(), String> {
        let state = save_state::CpsSaveState::from_bytes(data)?;
        self.apply_save_state(&state);
        Ok(())
    }

    fn save_state_to_file(&self, path: &str) -> Result<(), String> {
        self.build_save_state().write_to_file(path)
    }

    fn load_state_from_file(&mut self, path: &str) -> Result<(), String> {
        let state = save_state::CpsSaveState::read_from_file(path)?;
        self.apply_save_state(&state);
        Ok(())
    }

    fn state_debug_checksums(&self) -> Option<[u16; 8]> {
        // FNV-1a over the serialised state, folded into 8 lanes for GGRS
        // desync debugging (mirrors the NeoGeo core).
        let bytes = self.build_save_state().to_bytes().ok()?;
        Some(fold_checksums(&bytes))
    }

    fn save_state_and_checksums(&mut self) -> Result<(Vec<u8>, Option<[u16; 8]>), String> {
        // Serialise once and derive the checksums from the same blob — rollback
        // saves this every frame and the CPS state is ~260 KB, so the default
        // (which serialises twice) would double the per-frame cost.
        let bytes = self.build_save_state().to_bytes()?;
        let hashes = fold_checksums(&bytes);
        Ok((bytes, Some(hashes)))
    }

    fn work_ram(&self) -> &[u8] {
        self.bus.work_ram()
    }

    /// Load netplay boot savestate from `boot/<game>/charselect.bin` (etc.).
    fn load_initial_match_state(&mut self) -> bool {
        let name = self.game.name;
        for path in crate::boot::initial_match_state_paths(name) {
            let path_s = path.to_string_lossy();
            match std::fs::read(&path) {
                Ok(bytes) => match self.load_state_from_bytes(&bytes) {
                    Ok(()) => {
                        log::info!("loaded CPS initial match state from {path_s}");
                        return true;
                    }
                    Err(e) => log::warn!("initial match state from {path_s} failed: {e}"),
                },
                Err(_) => {}
            }
        }
        false
    }

    fn backup_ram(&self) -> &[u8] {
        &[]
    }

    fn load_sram(&mut self, _data: &[u8]) -> bool {
        false
    }

    fn resolution(&self) -> (u32, u32) {
        (video::WIDTH as u32, video::HEIGHT as u32)
    }

    fn audio_sample_rate(&self) -> u32 {
        crate::AUDIO_SAMPLE_RATE
    }

    fn refresh_rate(&self) -> f64 {
        59.637405 // CPS-1 field rate
    }

    fn debug_pc(&self) -> u32 {
        self.cpu.pc
    }

    fn video_frame_count(&self) -> usize {
        self.frame as usize
    }

    fn audio_sample_count(&self) -> u64 {
        self.audio_total
    }
}
