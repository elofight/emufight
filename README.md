# emufight

Rust library for **NeoGeo** and **CPS-1** arcade cores: frame step, full save-states, and GGRS rollback netplay.

`0.1.x` — API can still move. Edition 2021, MSRV 1.74.

<p align="center">
  <img src="assets/emufight-sdl-kof98.png" alt="emufight-sdl running KOF '98" width="720" />
</p>

<p align="center"><em><code>emufight-sdl</code> · KOF '98 (bring your own ROMs)</em></p>

---

## What it is

A small **rlib** you embed. You get machines and a netplay session; you keep the window, keys, matchmaking, and any product stuff.

The core idea is simple: **same inputs + same ROM + same boot state ⇒ same frames on both peers.** Everything else is built around that.

We are not trying to be MAME. Two boards, done in a shape that is pleasant to call from Rust and to run under rollback—not a driver database for every PCB ever made.

---

## Quick start

```sh
git clone --recurse-submodules https://github.com/elofight/emufight.git
cd emufight

cargo test -p emufight --lib
cargo run -p emufight-sdl --release -- kof98

# local 2P — joiner only needs the host address
cargo run -p emufight-sdl --release -- kof98 --listen 127.0.0.1:7000
cargo run -p emufight-sdl --release -- kof98 --connect 127.0.0.1:7000
```

You will need a C++ toolchain (ymfm), SDL2 for the reference host, dumps under `roms/<name>/`, and for NeoGeo system ROMs under `data/neogeo/` or `roms/neogeo/`. Boot snapshots for netplay live in `boot/` (shipped for kof98 / sf2ce).

**Keys** (same idea as Elofight): WASD · I/J/O/K for A–D on NeoGeo · I/O/P + J/K/L on six-button · 1 start · 3 select · 5 coin · Esc quit.

---

## How it’s modeled

Everything interesting goes through one trait:

```text
set_input → step(samples) → RGB + PCM
            step_cpu()    → catch-up without drawing
```

Hosts own the clock. The lib does not open a window or an audio device. NeoGeo and CPS-1 both implement `EmulatorCore`; netplay and the SDL host only care about that surface (plus a host-supplied catalog JSON if you want platform dispatch by name).

**Netplay** is GGRS on that same trait. Transports are just sockets:

```text
input → OnlineSession → EmulatorCore
              │
     NonBlockingSocket
        ╱         ╲
  SimSocket     UdpSocket   (+ whatever you implement)
```

`SimSocket` is for tests (fake latency/loss). `UdpSocket` is plain native UDP. STUN, WebRTC, lobbies—your problem (in a good way).

**Saves** are meant to be taken every rollback frame: compact bincode, header so you cannot load a NeoGeo blob into CPS by accident, sound chip state included. Idle inputs pack to zero so GGRS blank frames stay “nothing pressed.”

**Sound** is the one intentional C++ island—[ymfm](https://github.com/elofight/ymfm)—via thin glue. 68k is pure Rust ([m68k](https://github.com/elofight/m68k-rs) fork); Z80 is `iz80`. Feature flags keep SDL out of the library crate entirely.

Rough numbers if you care: 44100 Hz mono, 735 samples/frame nominal; NeoGeo 304×224, CPS 384×224 RGB24; snapshots on the order of a couple hundred KB.

We aim for *playable + lockstep-stable*, not silicon transcripts of every custom chip.

---

## Embed

```rust
use emufight::{create_emulator_for_platform, InputState, NOMINAL_SAMPLES_PER_FRAME};

let mut emu = create_emulator_for_platform("neogeo")?;
emu.load_roms(Some("kof98"))?;
emu.reset();
let _ = emu.load_initial_match_state(); // boot/<game>/charselect.bin

loop {
    emu.set_input(InputState::default());
    let frame = emu.step(NOMINAL_SAMPLES_PER_FRAME);
    // frame.framebuffer, frame.audio
}
```

Online sketch: bind `UdpSocket`, `OnlineSession::start_with_socket(...)`, each tick `session.advance(&mut *emu, pack_input(&input))`.

---

## Boot states (not ROMs)

Game/system dumps never ship in this repo. Small **charselect** savestates under `boot/<game>/` do—so peers can start mid-flow instead of cold BIOS.

```sh
cargo run -p emufight-sdl --bin emufight-capture-boot -- kof98
```

See [`boot/README.md`](boot/README.md).

---

## Layout

```text
crates/emufight/      library
crates/emufight-sdl/  reference host + capture tool
boot/                 charselect.bin only
vendor/ymfm/          submodule
```

Poke around: `core.rs` (trait), `netplay/` (session), `neogeo/`, `cps/`, `io.rs` (input packing).

---

## Features

- `netplay` (default) — GGRS session, sim + UDP sockets  
- `zip` — load sets from zip on disk  
- `native-romset` — optional download helpers; you still own licensing  

---

## Contributing

New game? Board support + a capture recipe + `boot/<id>/charselect.bin` if it should go online.  
Heavy ROM tests: `EMUFIGHT_RUN_ROM_TESTS=1 cargo test -p emufight --lib --features netplay`.  
Keep `emufight-sdl` boring; fancy product code stays elsewhere.

CI runs tests/docs with `-D warnings`.

---

## License

MIT OR Apache-2.0. ymfm / GGRS / m68k: see `NOTICE`.

ROMs are on you.
