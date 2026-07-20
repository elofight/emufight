//! Capture a netplay boot savestate (`boot/<game>/charselect.bin`).
//!
//! Requires **your** licensed ROM set under `roms/<game>/` (and system ROMs for
//! NeoGeo). Never ships ROM dumps — only writes the small charselect snapshot.
//!
//! ```sh
//! cargo run --bin emufight-capture-boot -- kof98
//! cargo run --bin emufight-capture-boot -- sf2ce
//! cargo run --bin emufight-capture-boot -- kof98 --out boot/kof98/charselect.bin
//! ```

use emufight::boot::default_capture_path;
use emufight::core::EmulatorCore;
use emufight::cps::CpsEmulator;
use emufight::io::InputState;
use emufight::neogeo::Emulator;
use emufight::{create_emulator_for_platform, RomCatalog};
use std::path::{Path, PathBuf};

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let mut args = std::env::args().skip(1);
    let mut game = None;
    let mut out: Option<PathBuf> = None;

    while let Some(a) = args.next() {
        match a.as_str() {
            "-h" | "--help" => {
                eprintln!(
                    "Usage: emufight-capture-boot <game> [--out path]\n\
                     Writes boot/<game>/charselect.bin (not ROMs).\n\
                     Supported recipes: kof98, sf2ce (others: generic idle after reset)."
                );
                return;
            }
            "--out" => {
                out = Some(PathBuf::from(args.next().expect("--out needs path")));
            }
            s if s.starts_with('-') => {
                eprintln!("unknown option: {s}");
                std::process::exit(2);
            }
            s => game = Some(s.to_string()),
        }
    }

    let game = game.unwrap_or_else(|| {
        eprintln!("missing <game>");
        std::process::exit(2);
    });
    let out = out.unwrap_or_else(|| default_capture_path(&game));

    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent).expect("create boot dir");
    }

    let platform = RomCatalog::from_path("roms.json")
        .ok()
        .and_then(|c| c.platform_for(&game).map(|s| s.to_string()))
        .or_else(|| {
            RomCatalog::from_path("roms.example.json")
                .ok()
                .and_then(|c| c.platform_for(&game).map(|s| s.to_string()))
        })
        .unwrap_or_else(|| {
            if game.starts_with("sf2") {
                "cps1".into()
            } else {
                "neogeo".into()
            }
        });

    match platform.as_str() {
        "cps1" | "cps" => capture_sf2ce_style(&game, &out),
        _ => {
            if game == "kof98" {
                capture_kof98(&out);
            } else {
                capture_generic_neogeo(&game, &out);
            }
        }
    }
}

fn capture_kof98(out: &Path) {
    let mut emu = Emulator::new();
    emu.load_roms(Some("kof98")).expect("load kof98 (need roms/kof98 + system ROMs)");
    emu.reset();

    // Coin both + P2 start → 2P → skip instructions → settle on char select.
    for i in 0..2300 {
        let mut input = InputState::default();
        if i == 500 {
            input.coin &= !1;
            input.coin &= !2;
        }
        if i == 550 {
            input.sys &= !4;
        }
        emu.set_input(input);
        emu.step(735);
    }
    {
        let mut input = InputState::default();
        input.p1 &= !0x10;
        emu.set_input(input);
        emu.step(735);
    }
    emu.set_input(InputState::default());
    emu.step(735);
    {
        let mut input = InputState::default();
        input.sys &= !4;
        emu.set_input(input);
        emu.step(735);
    }
    for _ in 0..10 {
        emu.set_input(InputState::default());
        emu.step(735);
    }

    write_state(&mut emu, out);
}

fn capture_sf2ce_style(game: &str, out: &Path) {
    let mut emu = CpsEmulator::new();
    emu.load_roms(Some(game))
        .unwrap_or_else(|e| panic!("load {game}: {e} (need roms/{game})"));
    emu.reset();

    // Same recipe as product cps_capture: nav then idle so no held inputs bake in.
    const NAV_END: usize = 900;
    const IDLE_FRAMES: usize = 120;
    for f in 0..=(NAV_END + IDLE_FRAMES) {
        let coin1 = (300..306).contains(&f);
        let coin2 = (320..326).contains(&f);
        let start2 = (600..NAV_END).contains(&f) && (f % 40) < 6;
        emu.set_input(cps_input(coin1, coin2, start2));
        emu.step(0);
    }

    if let Some(parent) = out.parent() {
        let shot = parent.join("charselect.bin.ppm");
        write_ppm(&emu, &shot);
        println!("screenshot {}", shot.display());
    }

    write_state(&mut emu, out);
}

fn capture_generic_neogeo(game: &str, out: &Path) {
    let mut emu = create_emulator_for_platform("neogeo").expect("neogeo");
    emu.load_roms(Some(game))
        .unwrap_or_else(|e| panic!("load {game}: {e}"));
    emu.reset();
    // Minimal settle — game-specific recipes should replace this.
    for _ in 0..600 {
        emu.set_input(InputState::default());
        emu.step(735);
    }
    write_state(emu.as_mut(), out);
    eprintln!(
        "note: generic NeoGeo capture for '{game}' — prefer a dedicated recipe for online use"
    );
}

fn cps_input(coin1: bool, coin2: bool, start2: bool) -> InputState {
    let mut s = InputState::default();
    s.coin = 0xFF;
    if coin1 {
        s.coin &= !0x01;
    }
    if coin2 {
        s.coin &= !0x02;
    }
    if start2 {
        s.sys &= !0x04;
    }
    s
}

fn write_ppm(emu: &CpsEmulator, path: &Path) {
    let fb = emu.framebuffer();
    let (w, h) = (384usize, 224usize);
    let mut out = format!("P6\n{w} {h}\n255\n").into_bytes();
    out.extend_from_slice(&fb[..w * h * 3]);
    let _ = std::fs::write(path, &out);
}

fn write_state(emu: &mut dyn EmulatorCore, out: &Path) {
    let blob = emu.save_state_to_bytes().expect("save_state_to_bytes");
    std::fs::write(out, &blob).expect("write charselect.bin");
    println!("wrote {} bytes → {}", blob.len(), out.display());
}
