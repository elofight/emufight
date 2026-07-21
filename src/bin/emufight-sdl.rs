//! Reference SDL2 host for emufight — offline or UDP netplay.
//!
//! Host / join (joiner only needs the host address):
//! - `--listen <addr>`   → player **0** (host room)
//! - `--connect <addr>`  → player **1** (join that host)
//!
//! Optional: joiner may also pass `--listen` for a fixed local bind.

use emufight::io::{pack_input, InputState};
use emufight::netplay::{OnlineSession, UdpSocket};
use emufight::{
    create_emulator_for_platform, EmulatorCore, RomCatalog, NOMINAL_SAMPLES_PER_FRAME,
};
use log::{error, info, warn};
use sdl2::audio::{AudioQueue, AudioSpecDesired};
use sdl2::event::Event;
use sdl2::keyboard::Keycode;
use sdl2::pixels::PixelFormatEnum;
use sdl2::render::{Canvas, Texture};
use sdl2::video::Window;
use sdl2::EventPump;
use std::net::{SocketAddr, UdpSocket as StdUdpSocket};
use std::path::Path;
use std::time::{Duration, Instant};

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args = match Args::parse(std::env::args().skip(1)) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}\n\n{}", Args::usage());
            std::process::exit(2);
        }
    };

    if let Err(e) = run(args) {
        error!("{e}");
        std::process::exit(1);
    }
}

struct Args {
    rom: String,
    /// Host (P0): room bind address.
    listen: Option<SocketAddr>,
    /// Join (P1): host address to connect to.
    connect: Option<SocketAddr>,
    input_delay: u32,
    scale: u32,
}

impl Args {
    fn usage() -> &'static str {
        "Usage: emufight-sdl [OPTIONS] <rom-name>\n\
         \n\
         Offline:\n\
           emufight-sdl kof98\n\
         \n\
         Online (host / join — joiner only needs host address):\n\
           host:  --listen <host:port>\n\
           join:  --connect <host:port>\n\
         \n\
         Local example:\n\
           host:  … --listen 127.0.0.1:7000\n\
           join:  … --connect 127.0.0.1:7000\n\
         \n\
         Join may also pass --listen <local> for a fixed local UDP port.\n\
         \n\
         Options:\n\
           --input-delay <n>   GGRS delay frames (default 2)\n\
           --scale <n>         Window scale (default 2)\n\
         \n\
         Keys (product defaults): WASD + IJOK (NeoGeo) or IOP/JKL (6-btn);\n\
           1 Start, 3 Select, 5 Coin, Esc quit.\n\
         ROMs: roms/<name>/  boot: boot/<name>/charselect.bin"
    }

    fn parse(mut argv: impl Iterator<Item = String>) -> Result<Self, String> {
        let mut rom = None;
        let mut listen = None;
        let mut connect = None;
        let mut input_delay = 2u32;
        let mut scale = 2u32;

        while let Some(a) = argv.next() {
            match a.as_str() {
                "-h" | "--help" => return Err(Self::usage().into()),
                "--listen" => {
                    let v = argv.next().ok_or("--listen needs ADDR")?;
                    listen = Some(parse_addr(&v)?);
                }
                "--connect" => {
                    let v = argv.next().ok_or("--connect needs ADDR")?;
                    connect = Some(parse_addr(&v)?);
                }
                "--input-delay" => {
                    let v = argv.next().ok_or("--input-delay needs N")?;
                    input_delay = v.parse().map_err(|_| "bad --input-delay")?;
                }
                "--scale" => {
                    let v = argv.next().ok_or("--scale needs N")?;
                    scale = v.parse::<u32>().map_err(|_| "bad --scale")?.max(1);
                }
                s if s.starts_with('-') => return Err(format!("unknown option: {s}")),
                s => {
                    if rom.is_some() {
                        return Err("multiple ROM names".into());
                    }
                    rom = Some(s.to_string());
                }
            }
        }

        let rom = rom.ok_or_else(|| "missing <rom-name>".to_string())?;
        match (listen.is_some(), connect.is_some()) {
            (false, false) => {} // offline
            (true, false) => {}  // host
            (false, true) | (true, true) => {} // join (optional fixed local bind)
        }
        Ok(Self {
            rom,
            listen,
            connect,
            input_delay,
            scale,
        })
    }
}

fn parse_addr(s: &str) -> Result<SocketAddr, String> {
    s.parse()
        .map_err(|_| format!("invalid address '{s}' (expected host:port)"))
}

fn run(args: Args) -> Result<(), String> {
    let catalog = load_catalog();
    let platform = catalog
        .as_ref()
        .and_then(|c| c.platform_for(&args.rom).map(|s| s.to_string()))
        .unwrap_or_else(|| "neogeo".into());

    let mut emu = create_emulator_for_platform(&platform)?;
    emu.load_roms(Some(&args.rom))?;
    emu.reset();
    if args.listen.is_some() || args.connect.is_some() {
        if emu.load_initial_match_state() {
            info!("loaded host initial match state");
        }
    }

    let (w, h) = emu.resolution();
    let mut ui = SdlHost::new(args.scale, w as usize, h as usize, &args.rom)?;

    match (args.listen, args.connect) {
        (None, None) => run_offline(&mut emu, &mut ui),
        // Host (P0): bind room, latch joiner from first UDP, then GGRS.
        (Some(bind), None) => run_host(&mut emu, &mut ui, bind, args.input_delay),
        // Join (P1): only needs host address; optional fixed local bind.
        (local, Some(host)) => {
            let bind = local.unwrap_or_else(|| "0.0.0.0:0".parse().unwrap());
            run_join(&mut emu, &mut ui, bind, host, args.input_delay)
        }
    }
}

/// Player 0: listen on a fixed address; first inbound packet sets the remote peer.
fn run_host(
    emu: &mut Box<dyn EmulatorCore>,
    ui: &mut SdlHost,
    bind: SocketAddr,
    input_delay: u32,
) -> Result<(), String> {
    let std_sock = bind_udp(bind)?;
    info!("host (player 0) listening on {bind} — waiting for joiner…");

    // One-shot: learn joiner address. That datagram is not fed into GGRS; the
    // joiner's session will retransmit handshake. Then both use exact endpoints.
    let joiner = wait_for_peer(&std_sock, ui, emu)?;
    info!("host: joiner={joiner} — starting GGRS");

    let sock = UdpSocket::from_std(std_sock).map_err(|e| e.to_string())?;
    let mut session = OnlineSession::start_with_socket(sock, joiner, true, input_delay)?;
    run_online(emu, ui, &mut session)
}

/// Player 1: connect to host address (bind ephemeral unless --listen given).
fn run_join(
    emu: &mut Box<dyn EmulatorCore>,
    ui: &mut SdlHost,
    bind: SocketAddr,
    host: SocketAddr,
    input_delay: u32,
) -> Result<(), String> {
    let std_sock = bind_udp(bind)?;
    let local = std_sock.local_addr().map_err(|e| e.to_string())?;
    info!("join (player 1) local={local} → host={host}");

    let sock = UdpSocket::from_std(std_sock).map_err(|e| e.to_string())?;
    let mut session = OnlineSession::start_with_socket(sock, host, false, input_delay)?;
    run_online(emu, ui, &mut session)
}

fn bind_udp(addr: SocketAddr) -> Result<StdUdpSocket, String> {
    let sock = StdUdpSocket::bind(addr).map_err(|e| format!("UDP bind {addr}: {e}"))?;
    sock.set_nonblocking(true)
        .map_err(|e| format!("UDP set_nonblocking: {e}"))?;
    // Faster restart of the host room on the same port.
    let _ = sock.set_broadcast(false);
    Ok(sock)
}

/// Block until a UDP packet arrives; return its source address (packet consumed).
fn wait_for_peer(
    sock: &StdUdpSocket,
    ui: &mut SdlHost,
    emu: &mut Box<dyn EmulatorCore>,
) -> Result<SocketAddr, String> {
    let mut buf = [0u8; 2048];
    loop {
        if !ui.running {
            return Err("quit while waiting for joiner".into());
        }
        let (_input, quit) = ui.poll();
        if quit {
            return Err("quit while waiting for joiner".into());
        }
        ui.present(emu.framebuffer());

        match sock.recv_from(&mut buf) {
            Ok((n, src)) => {
                info!("host: first packet from {src} ({n} bytes)");
                return Ok(src);
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(e) => return Err(format!("UDP recv while waiting for joiner: {e}")),
        }
    }
}

fn load_catalog() -> Option<RomCatalog> {
    for p in ["roms.json", "crates/emufight/roms.example.json", "roms.example.json"] {
        if Path::new(p).is_file() {
            match RomCatalog::from_path(p) {
                Ok(c) => {
                    info!("catalog: {p}");
                    return Some(c);
                }
                Err(e) => warn!("catalog {p}: {e}"),
            }
        }
    }
    None
}

fn run_offline(emu: &mut Box<dyn EmulatorCore>, ui: &mut SdlHost) -> Result<(), String> {
    let frame_dt = Duration::from_secs_f64(1.0 / emu.refresh_rate());
    let mut next = Instant::now() + frame_dt;

    while ui.running {
        let (input, quit) = ui.poll();
        if quit {
            break;
        }
        emu.set_input(input);
        let frame = emu.step(NOMINAL_SAMPLES_PER_FRAME);
        ui.present(frame.framebuffer);
        ui.queue_audio(frame.audio);

        let now = Instant::now();
        if now < next {
            std::thread::sleep(next - now);
        }
        next += frame_dt;
        if Instant::now() > next + frame_dt {
            next = Instant::now() + frame_dt;
        }
    }
    Ok(())
}

fn run_online(
    emu: &mut Box<dyn EmulatorCore>,
    ui: &mut SdlHost,
    session: &mut OnlineSession<SocketAddr>,
) -> Result<(), String> {
    let frame_dt = Duration::from_secs_f64(1.0 / emu.refresh_rate());
    let mut next = Instant::now() + frame_dt;

    while ui.running {
        let (input, quit) = ui.poll();
        if quit {
            break;
        }
        let packed = pack_input(&input);

        // While synchronizing, pump GGRS hard (network + handshake). After
        // Running, one advance per display frame.
        let pumps = if session.video_ready() { 1 } else { 16 };
        let mut got_frame = false;
        for _ in 0..pumps {
            session.poll_remote_clients();
            if let Some((fb, audio, _rb)) = session.advance(emu.as_mut(), packed) {
                ui.present(&fb);
                ui.queue_audio(&audio);
                got_frame = true;
                break;
            }
            if session.error.is_some() {
                break;
            }
        }
        if let Some(err) = session.error.as_ref() {
            return Err(err.clone());
        }
        if !got_frame {
            // Keep last framebuffer on screen during sync.
            ui.present(emu.framebuffer());
        }

        if session.video_ready() {
            let now = Instant::now();
            if now < next {
                std::thread::sleep(next - now);
            }
            next += frame_dt;
            if Instant::now() > next + frame_dt {
                next = Instant::now() + frame_dt;
            }
        } else {
            std::thread::sleep(Duration::from_millis(1));
            next = Instant::now() + frame_dt;
        }
    }
    Ok(())
}

// ── Minimal SDL host ─────────────────────────────────────────────────────────
//
// Default keys match the Elofight product registry (`elofight::input::default_keymap`):
//   WASD stick · face buttons per game · 1 Start · 3 Select · 5 Coin
//   NeoGeo 4-button (kof98): I/J/O/K = A/B/C/D
//   6-button (sf2…):         I/O/P punches, J/K/L kicks

struct SdlHost {
    canvas: Canvas<Window>,
    texture: Texture,
    audio: AudioQueue<f32>,
    events: EventPump,
    running: bool,
    screen_w: usize,
    screen_h: usize,
    /// Six-button face layout (CPS); false = NeoGeo-style IJOK.
    six_button: bool,
    keys: Keys,
}

#[derive(Default)]
struct Keys {
    up: bool,
    down: bool,
    left: bool,
    right: bool,
    a: bool,
    b: bool,
    c: bool,
    d: bool,
    e: bool,
    f: bool,
    start: bool,
    select: bool,
    coin: bool,
}

impl SdlHost {
    fn new(scale: u32, screen_w: usize, screen_h: usize, game: &str) -> Result<Self, String> {
        let sdl = sdl2::init().map_err(|e| e.to_string())?;
        let video = sdl.video().map_err(|e| e.to_string())?;
        let audio_sys = sdl.audio().map_err(|e| e.to_string())?;

        let six_button = !matches!(game, "kof98");
        info!(
            "keymap: WASD + {} (Start=1 Select=3 Coin=5)",
            if six_button {
                "IOP/JKL six-button"
            } else {
                "IJOK four-button"
            }
        );

        let window = video
            .window(
                "emufight-sdl",
                screen_w as u32 * scale,
                screen_h as u32 * scale,
            )
            .position_centered()
            .resizable()
            .build()
            .map_err(|e| e.to_string())?;

        let canvas = window
            .into_canvas()
            .accelerated()
            .present_vsync()
            .build()
            .map_err(|e| e.to_string())?;

        let creator = canvas.texture_creator();
        let texture = creator
            .create_texture_streaming(PixelFormatEnum::RGB24, screen_w as u32, screen_h as u32)
            .map_err(|e| e.to_string())?;

        let desired = AudioSpecDesired {
            freq: Some(44_100),
            channels: Some(1),
            samples: Some(512),
        };
        let audio: AudioQueue<f32> = audio_sys
            .open_queue(None, &desired)
            .map_err(|e| e.to_string())?;
        audio.resume();

        let events = sdl.event_pump().map_err(|e| e.to_string())?;

        Ok(Self {
            canvas,
            texture,
            audio,
            events,
            running: true,
            screen_w,
            screen_h,
            six_button,
            keys: Keys::default(),
        })
    }

    fn poll(&mut self) -> (InputState, bool) {
        let events: Vec<Event> = self.events.poll_iter().collect();
        for event in events {
            match event {
                Event::Quit { .. } => {
                    self.running = false;
                    return (self.input_state(), true);
                }
                Event::KeyDown {
                    keycode: Some(k),
                    repeat: false,
                    ..
                } => {
                    if k == Keycode::Escape {
                        self.running = false;
                        return (self.input_state(), true);
                    }
                    self.set_key(k, true);
                }
                Event::KeyUp {
                    keycode: Some(k),
                    repeat: false,
                    ..
                } => self.set_key(k, false),
                _ => {}
            }
        }
        (self.input_state(), false)
    }

    fn set_key(&mut self, k: Keycode, down: bool) {
        // Directions: WASD (same as product). Arrow keys also accepted.
        match k {
            Keycode::W | Keycode::Up => self.keys.up = down,
            Keycode::S | Keycode::Down => self.keys.down = down,
            Keycode::A | Keycode::Left => self.keys.left = down,
            Keycode::D | Keycode::Right => self.keys.right = down,
            Keycode::Num1 => self.keys.start = down,
            Keycode::Num3 => self.keys.select = down,
            Keycode::Num5 => self.keys.coin = down,
            _ => {}
        }
        if self.six_button {
            // Product CORE_KEYMAP: I O P punches, J K L kicks.
            match k {
                Keycode::I => self.keys.a = down,
                Keycode::O => self.keys.b = down,
                Keycode::P => self.keys.c = down,
                Keycode::J => self.keys.d = down,
                Keycode::K => self.keys.e = down,
                Keycode::L => self.keys.f = down,
                _ => {}
            }
        } else {
            // Product KOF98_KEYMAP: I J O K = A B C D.
            match k {
                Keycode::I => self.keys.a = down,
                Keycode::J => self.keys.b = down,
                Keycode::O => self.keys.c = down,
                Keycode::K => self.keys.d = down,
                _ => {}
            }
        }
    }

    fn input_state(&self) -> InputState {
        // Active-low: cleared bit = pressed.
        let bit = |pressed: bool, n: u8| if pressed { 0 } else { 1 << n };
        let p1 = bit(self.keys.up, 0)
            | bit(self.keys.down, 1)
            | bit(self.keys.left, 2)
            | bit(self.keys.right, 3)
            | bit(self.keys.a, 4)
            | bit(self.keys.b, 5)
            | bit(self.keys.c, 6)
            | bit(self.keys.d, 7);
        let mut s = InputState::default();
        s.p1 = p1;
        s.p2 = 0xFF;
        let mut sys = 0xFFu8;
        if self.keys.start {
            sys &= !0x01;
        }
        if self.keys.select {
            sys &= !0x02;
        }
        s.sys = sys;
        let mut coin = 0x3Fu8;
        if self.keys.coin {
            coin &= !0x01;
        }
        s.coin = coin;
        // ext: P1 E/F in bits 0–1 (active-low).
        let mut ext = 0x0Fu8;
        if self.keys.e {
            ext &= !0x01;
        }
        if self.keys.f {
            ext &= !0x02;
        }
        s.ext = ext;
        s
    }

    fn present(&mut self, rgb: &[u8]) {
        let pitch = self.screen_w * 3;
        if rgb.len() < pitch * self.screen_h {
            return;
        }
        let _ = self.texture.update(None, rgb, pitch);
        self.canvas.clear();
        let _ = self.canvas.copy(&self.texture, None, None);
        self.canvas.present();
    }

    fn queue_audio(&mut self, samples: &[f32]) {
        if samples.is_empty() {
            return;
        }
        // Avoid unbounded growth if the device is stuck.
        if self.audio.size() > 44_100 {
            return;
        }
        let _ = self.audio.queue_audio(samples);
    }
}
