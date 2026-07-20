//! Rollback / save-state determinism tests for KOF98 charselect.
//!
//! Serial execution is required when tests construct many `Emulator` instances
//! on one thread (`--test-threads=1`).

use super::*;
use serial_test::serial;
use std::sync::OnceLock;

/// Full-machine tests need real ROM dumps. Set EMUFIGHT_RUN_ROM_TESTS=1 and
/// provide roms/ + system BIOS to run them; otherwise they skip so CI stays fast.
fn rom_tests_enabled() -> bool {
    std::env::var_os("EMUFIGHT_RUN_ROM_TESTS").is_some()
}


const CHARSELECT_FRAMES: usize = 60;
const FRAME_45_AT: usize = 44;

fn charselect_input(frame: usize) -> InputState {
    let mut inp = InputState::default();
    let f = frame as u8;
    if f % 7 == 0 { inp.p1 &= !0x10; }
    if f % 11 == 0 { inp.p1 &= !0x20; }
    if f % 13 == 0 { inp.p1 &= !0x40; }
    if f % 17 == 0 { inp.p1 &= !0x80; }
    if f % 5 == 0 { inp.p1 &= !0x01; }
    if f % 6 == 0 { inp.p1 &= !0x02; }
    if f % 9 == 0 { inp.p2 &= !0x10; }
    if f % 14 == 0 { inp.p2 &= !0x20; }
    if f % 19 == 0 { inp.p2 &= !0x40; }
    if frame == 0 || frame == 1 {
        inp.coin &= !0x01;
        inp.coin &= !0x02;
    }
    if frame == 2 || frame == 3 {
        inp.sys &= !0x01;
        inp.sys &= !0x04;
    }
    inp
}

/// Returns `None` when kof98 ROMs or host boot state are unavailable.
fn try_kof98_charselect() -> Option<Emulator> {
    if !rom_tests_enabled() {
        return None;
    }
    let mut emu = Emulator::new();
    if emu.load_roms(Some("kof98")).is_err() {
        eprintln!("skip: kof98 ROMs not available");
        return None;
    }
    emu.reset();
    if !emu.load_initial_match_state() {
        eprintln!("skip: no host charselect boot state");
        return None;
    }
    Some(emu)
}

fn try_load_kof98_checkpoint(checkpoint: &[u8]) -> Option<Emulator> {
    let mut emu = Emulator::new();
    if emu.load_roms(Some("kof98")).is_err() {
        eprintln!("skip: kof98 ROMs not available");
        return None;
    }
    emu.reset();
    if emu.load_state_from_bytes(checkpoint).is_err() {
        return None;
    }
    Some(emu)
}



/// Reset per-thread YM2610 glue residue before tests that construct many
/// emulator instances. Production rollback uses a single live emulator.
fn ym2610_multi_instance_test_hygiene() {
    let mut guard = Emulator::new();
    let _ = guard.load_roms(Some("kof98"));
    guard.reset();

    static FRAME_45_CHECKPOINT: OnceLock<Vec<u8>> = OnceLock::new();
    let checkpoint = FRAME_45_CHECKPOINT.get_or_init(|| {
        let Some(mut emu) = try_kof98_charselect() else { return Vec::new(); };
        for f in 0..FRAME_45_AT {
            emu.set_input(charselect_input(f));
            emu.step(NOMINAL_SAMPLES_PER_FRAME);
        }
        emu.save_state_to_bytes().unwrap()
    });

    let Some(mut burn) = try_load_kof98_checkpoint(checkpoint) else { return; };
    burn.set_input(charselect_input(FRAME_45_AT));
    burn.step(NOMINAL_SAMPLES_PER_FRAME);
}

fn assert_single_frame_rollback(at: usize, inputs: &[InputState]) {
    let Some(mut runner) = try_kof98_charselect() else { return; };
    for f in 0..at {
        runner.set_input(inputs[f].clone());
        runner.step(NOMINAL_SAMPLES_PER_FRAME);
    }
    let checkpoint = runner.save_state_to_bytes().unwrap();
    let step_input = inputs[at].clone();
    drop(runner);

    let Some(mut expected) = try_load_kof98_checkpoint(&checkpoint) else { return; };
    expected.set_input(step_input.clone());
    expected.step(NOMINAL_SAMPLES_PER_FRAME);
    let expected_bytes = expected.save_state_to_bytes().unwrap();
    drop(expected);

    let Some(mut replay) = try_load_kof98_checkpoint(&checkpoint) else { return; };
    replay.set_input(step_input);
    replay.step(NOMINAL_SAMPLES_PER_FRAME);
    assert_eq!(
        replay.save_state_to_bytes().unwrap(),
        expected_bytes,
        "rollback desync at frame {} -> {}",
        at,
        at + 1
    );
}

#[test]
fn save_restore_determinism() {
    if !rom_tests_enabled() { return; }
    let mut emu = Emulator::new();
    if emu.load_roms(Some("kof98")).is_err() { eprintln!("skip: no kof98 ROMs"); return; }
    emu.reset();

    for _ in 0..10 {
        emu.set_input(InputState::default());
        emu.step(NOMINAL_SAMPLES_PER_FRAME);
    }

    let checkpoint = emu.save_state_to_bytes().unwrap();
    let checkpoint_frame = emu.frame();

    for _ in 0..5 {
        emu.set_input(InputState::default());
        emu.step(NOMINAL_SAMPLES_PER_FRAME);
    }
    let ref_fb = emu.framebuffer().to_vec();
    let ref_audio = emu.audio_samples().to_vec();
    let ref_frame = emu.frame();

    emu.load_state_from_bytes(&checkpoint).unwrap();
    assert_eq!(emu.frame(), checkpoint_frame);
    for _ in 0..5 {
        emu.set_input(InputState::default());
        emu.step(NOMINAL_SAMPLES_PER_FRAME);
    }

    assert_eq!(emu.frame(), ref_frame);
    assert_eq!(emu.framebuffer(), ref_fb.as_slice());
    assert_eq!(emu.audio_samples(), ref_audio.as_slice());
}

#[test]
fn rollback_determinism_every_frame() {
    if !rom_tests_enabled() { return; }
    let mut emu = Emulator::new();
    if emu.load_roms(Some("kof98")).is_err() { eprintln!("skip: no kof98 ROMs"); return; }
    emu.reset();
    for _ in 0..300 {
        emu.step(NOMINAL_SAMPLES_PER_FRAME);
    }

    const TOTAL: usize = 120;
    let inputs: Vec<InputState> = (0..TOTAL).map(charselect_input).collect();

    let mut reference: Vec<Vec<u8>> = Vec::with_capacity(TOTAL + 1);
    reference.push(emu.save_state_to_bytes().unwrap());
    for f in 0..TOTAL {
        emu.set_input(inputs[f].clone());
        emu.step(NOMINAL_SAMPLES_PER_FRAME);
        reference.push(emu.save_state_to_bytes().unwrap());
    }

    for start in 0..TOTAL {
        let mut replay = Emulator::new();
        if replay.load_roms(Some("kof98")).is_err() { return; }
        replay.reset();
        replay.load_state_from_bytes(&reference[start]).unwrap();
        replay.set_input(inputs[start].clone());
        replay.step(NOMINAL_SAMPLES_PER_FRAME);
        assert_eq!(
            replay.save_state_to_bytes().unwrap(),
            reference[start + 1],
            "rollback desync at frame {}",
            start + 1
        );
    }
}

#[test]
#[serial]
fn charselect_rollback_determinism_every_frame() {
    ym2610_multi_instance_test_hygiene();

    let inputs: Vec<InputState> = (0..CHARSELECT_FRAMES).map(charselect_input).collect();

    let Some(mut emu) = try_kof98_charselect() else { return; };
    let mut reference: Vec<Vec<u8>> = Vec::with_capacity(CHARSELECT_FRAMES + 1);
    reference.push(emu.save_state_to_bytes().unwrap());
    for f in 0..CHARSELECT_FRAMES {
        emu.set_input(inputs[f].clone());
        emu.step(NOMINAL_SAMPLES_PER_FRAME);
        reference.push(emu.save_state_to_bytes().unwrap());
    }

    for start in 0..CHARSELECT_FRAMES {
        let mut replay = Emulator::new();
        if replay.load_roms(Some("kof98")).is_err() { return; }
        replay.reset();
        replay.load_state_from_bytes(&reference[start]).unwrap();
        replay.set_input(inputs[start].clone());
        replay.step(NOMINAL_SAMPLES_PER_FRAME);
        assert_eq!(
            replay.save_state_to_bytes().unwrap(),
            reference[start + 1],
            "rollback desync at frame {} -> {}",
            start,
            start + 1
        );
    }
}

#[test]
#[serial]
fn charselect_frame45_rollback_regression() {
    ym2610_multi_instance_test_hygiene();
    let inputs: Vec<InputState> = (0..=FRAME_45_AT + 1).map(charselect_input).collect();
    assert_single_frame_rollback(FRAME_45_AT, &inputs);
}

/// CPS invariant 3: reaching a frame via display `step()` must checksum-match
/// reaching it via GGRS catch-up `step_cpu()` (ring is transient, not in save state).
#[test]
#[serial]
fn charselect_display_vs_catchup_invariant() {
    ym2610_multi_instance_test_hygiene();

    const N: usize = 120;
    const CHECKPOINT_AT: usize = 30;
    let inputs: Vec<InputState> = (0..N).map(charselect_input).collect();

    let Some(mut display) = try_kof98_charselect() else { return; };
    for f in 0..N {
        display.set_input(inputs[f].clone());
        display.step(NOMINAL_SAMPLES_PER_FRAME);
    }
    let cs_display = display.build_save_state().debug_checksums();

    let Some(mut runner) = try_kof98_charselect() else { return; };
    for f in 0..CHECKPOINT_AT {
        runner.set_input(inputs[f].clone());
        runner.step(NOMINAL_SAMPLES_PER_FRAME);
    }
    let blob = runner.save_state_to_bytes().unwrap();

    let Some(mut reload_display) = try_load_kof98_checkpoint(&blob) else { return; };
    for f in CHECKPOINT_AT..N {
        reload_display.set_input(inputs[f].clone());
        reload_display.step(NOMINAL_SAMPLES_PER_FRAME);
    }
    let cs_reload_display = reload_display.build_save_state().debug_checksums();

    let Some(mut catchup) = try_load_kof98_checkpoint(&blob) else { return; };
    for f in CHECKPOINT_AT..N {
        catchup.set_input(inputs[f].clone());
        catchup.step_cpu();
    }
    let cs_catchup = catchup.build_save_state().debug_checksums();

    assert_eq!(
        cs_display, cs_reload_display,
        "reload + display path diverged from continuous display"
    );
    assert_eq!(
        cs_display, cs_catchup,
        "display path diverged from catch-up step_cpu (GGRS rollback schedule)"
    );
}

#[test]
#[serial]
fn charselect_apply_matches_bytes_load() {
    const AT: usize = 43;
    let inputs: Vec<InputState> = (0..=AT + 1).map(charselect_input).collect();

    let Some(mut emu) = try_kof98_charselect() else { return; };
    for f in 0..AT {
        emu.set_input(inputs[f].clone());
        emu.step(NOMINAL_SAMPLES_PER_FRAME);
    }
    let checkpoint = emu.save_state_to_bytes().unwrap();
    let snap = SaveState::from_bytes(&checkpoint).unwrap();

    emu.set_input(inputs[AT].clone());
    emu.step(NOMINAL_SAMPLES_PER_FRAME);
    let reference_next = emu.save_state_to_bytes().unwrap();

    let Some(mut via_bytes) = try_load_kof98_checkpoint(&checkpoint) else { return; };
    via_bytes.set_input(inputs[AT].clone());
    via_bytes.step(NOMINAL_SAMPLES_PER_FRAME);
    assert_eq!(via_bytes.save_state_to_bytes().unwrap(), reference_next);

    let mut via_apply = Emulator::new();
    if via_apply.load_roms(Some("kof98")).is_err() { return; }
    via_apply.reset();
    via_apply.apply_save_state(snap).unwrap();
    via_apply.set_input(inputs[AT].clone());
    via_apply.step(NOMINAL_SAMPLES_PER_FRAME);
    assert_eq!(via_apply.save_state_to_bytes().unwrap(), reference_next);
}

#[test]
#[serial]
fn charselect_ym2610_roundtrip_identity() {
    let inputs: Vec<InputState> = (0..CHARSELECT_FRAMES).map(charselect_input).collect();
    let Some(mut emu) = try_kof98_charselect() else { return; };

    for f in 0..CHARSELECT_FRAMES {
        emu.set_input(inputs[f].clone());
        emu.step(NOMINAL_SAMPLES_PER_FRAME);
        let original = emu.ym2610.snapshot();
        let Some(probe) = try_load_kof98_checkpoint(&emu.save_state_to_bytes().unwrap()) else { return; };
        assert_eq!(
            original,
            probe.ym2610.snapshot(),
            "ym2610 roundtrip not identity after frame {}",
            f + 1
        );
    }
}

#[test]
#[serial]
fn seek_replay_determinism_regression() {
    ym2610_multi_instance_test_hygiene();

    const TOTAL: usize = 44;
    const ANCHOR: usize = 10;
    let inputs: Vec<InputState> = (0..TOTAL).map(charselect_input).collect();

    let Some(mut emu) = try_kof98_charselect() else { return; };
    let mut ref_cs: Vec<[u16; 8]> = Vec::with_capacity(TOTAL);
    let mut anchor = Vec::new();

    for f in 0..TOTAL {
        if f == ANCHOR {
            anchor = emu.save_state_to_bytes().unwrap();
        }
        emu.set_input(inputs[f].clone());
        emu.step(NOMINAL_SAMPLES_PER_FRAME);
        ref_cs.push(emu.build_save_state().debug_checksums());
    }

    let Some(mut replay) = try_load_kof98_checkpoint(&anchor) else { return; };
    for f in ANCHOR..TOTAL {
        replay.set_input(inputs[f].clone());
        replay.step(NOMINAL_SAMPLES_PER_FRAME);
        assert_eq!(
            replay.build_save_state().debug_checksums(),
            ref_cs[f],
            "seek replay drift at frame {} (anchor={})",
            f,
            ANCHOR
        );
    }
}

#[test]
#[cfg(feature = "netplay")]
fn ggrs_synctest_desync_check() {
    use crate::io::{pack_input, PackedInput};
    use ggrs::{GgrsError, GgrsRequest, SessionBuilder};

    fn fnv1a(data: &[u8]) -> u64 {
        const FNV_OFFSET: u64 = 0xcbf29ce484222325;
        const FNV_PRIME: u64 = 0x100000001b3;
        let mut hash = FNV_OFFSET;
        for &b in data {
            hash ^= b as u64;
            hash = hash.wrapping_mul(FNV_PRIME);
        }
        hash
    }

    let Some(mut emu) = try_kof98_charselect() else { return; };
    const TOTAL_FRAMES: usize = 360;
    const CHECK_DISTANCE: usize = 7;

    type Cfg = crate::netplay::GGRSCfg<u64>;
    let mut session: ggrs::SyncTestSession<Cfg> = SessionBuilder::<Cfg>::new()
        .with_num_players(2)
        .with_max_prediction_window(8)
        .expect("max_prediction_window")
        .with_check_distance(CHECK_DISTANCE)
        .with_input_delay(1)
        .start_synctest_session()
        .expect("SyncTestSession creation");

    let inputs: Vec<PackedInput> = (0..TOTAL_FRAMES)
        .map(|f| pack_input(&charselect_input(f)))
        .collect();

    for frame in 0..TOTAL_FRAMES {
        session.add_local_input(0, inputs[frame]).unwrap();
        session.add_local_input(1, inputs[frame]).unwrap();

        let requests = match session.advance_frame() {
            Ok(r) => r,
            Err(GgrsError::MismatchedChecksum { current_frame, mismatched_frames }) => {
                panic!(
                    "DESYNC at frame {} (mismatched frames: {:?})",
                    current_frame, mismatched_frames
                );
            }
            Err(e) => panic!("SyncTestSession error at frame {}: {}", frame, e),
        };

        let total_advances = requests
            .iter()
            .filter(|r| matches!(r, GgrsRequest::AdvanceFrame { .. }))
            .count();
        let mut advance_idx = 0usize;

        for req in requests {
            match req {
                GgrsRequest::SaveGameState { cell, frame } => {
                    let blob = emu.save_state_to_bytes().unwrap();
                    let checksum = fnv1a(&blob) as u128;
                    cell.save(frame, Some(blob), Some(checksum));
                }
                GgrsRequest::LoadGameState { cell, frame: _ } => {
                    if let Some(blob) = cell.load() {
                        emu.load_state_from_bytes(&blob).unwrap();
                    }
                }
                GgrsRequest::AdvanceFrame { .. } => {
                    advance_idx += 1;
                    if advance_idx == total_advances {
                        emu.step(NOMINAL_SAMPLES_PER_FRAME);
                    } else {
                        emu.step_cpu();
                    }
                }
            }
        }
    }
}

// Session boot profiles (AES/training overlays) live in the host product.



#[test]
fn kof98_cart_initial_match_state_is_optional() {
    let cart = crate::neogeo::cart::cart_for("kof98");
    // Core does not embed boot blobs; host may inject via load_state_from_bytes.
    assert!(cart.initial_match_state().is_none());
}