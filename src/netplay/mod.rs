//! GGRS P2P netplay session — transport-agnostic online play.
//!
//! [`OnlineSession<A>`] receives a pre-connected socket + opponent address and
//! drives GGRS rollback netcode.  The address type `A` is generic:
//!
//! | Backend | `A` | Socket |
//! |---|---|---|
//! | Tests | `SimAddr` | [`SimSocket`] (in-process latency/loss) |
//! | Native UDP | `SocketAddr` | [`UdpSocket`] (LAN / known endpoint) |
//! | Host-provided | any | Your `ggrs::NonBlockingSocket` impl (WebRTC, …) |
//!
//! The session handles GGRS save/load state requests for rollback, input
//! remapping (two players each pressing P1's buttons → combined P1 + P2
//! input), and spectator input capture via `pull_recent_inputs`.
//!
//! Transport, matchmaking, and signaling live **outside** this crate.

/// Network statistics exposed from the GGRS session to the frontend.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct NetplayStats {
    /// Round-trip time in milliseconds.
    pub ping_ms: u32,
    /// How many frames the local client is behind the remote.
    pub local_frames_behind: i32,
    /// How many frames the remote client is behind the local.
    pub remote_frames_behind: i32,
    /// Length of the outbound packet queue (congestion indicator).
    pub send_queue_len: u32,
    /// Estimated bandwidth in kilobits per second.
    pub kbps_sent: u32,
    /// Number of rollback (catch-up) frames in the last advance() call.
    pub rollback_frames: u32,
    /// Current WaitRecommendation backlog (frames we're intentionally skipping).
    pub frames_to_wait: u32,
}

/// Holds the live GGRS P2P session once both players are connected.
pub struct OnlineSession<A>
where
    A: Clone
        + Send
        + Sync
        + std::fmt::Debug
        + std::fmt::Display
        + std::hash::Hash
        + Eq
        + PartialEq
        + 'static,
{
    pub session:       ggrs::P2PSession<GGRSCfg<A>>,
    pub local_handle:  usize,
    pub remote_handle: usize,
    /// Set when GGRS signals a fatal event (disconnect or desync).
    /// The shell checks this after every advance() and transitions to an error screen.
    pub error: Option<String>,
    /// Number of frames generated since the session reached `Running` state.
    /// Used to transition from the 'SYNCHRONIZING...' overlay to live execution.
    frames_since_running: u32,
    /// Number of frames GGRS recommends we wait to let the remote peer catch up.
    pub frames_to_wait: u32,
    /// Track the current simulated frame (updates on AdvanceFrame / LoadGameState).
    pub current_frame: i32,
    /// Total number of advance() calls made on this session.
    pub total_advance_calls: u64,
    /// Buffer of all simulated inputs, continually overwritten on rollback.
    /// Once a frame is confirmed, its inputs here are final.
    pub all_inputs: Vec<(PackedInput, PackedInput)>,
    /// Last confirmed frame that the shell pulled via `pull_recent_inputs`.
    pub last_polled_frame: i32,
    /// Keep the last 120 frames of local multisected checksums.
    pub local_checksums: std::collections::BTreeMap<i32, [u16; 8]>,
    /// Number of rollback frames in the most recent advance() call.
    pub last_rollback_frames: u32,
}

use crate::EmulatorCore;
use crate::NOMINAL_SAMPLES_PER_FRAME;

use ggrs::{GgrsRequest, NonBlockingSocket, PlayerType, SessionBuilder};

/// Simple FNV-1a 64-bit (used for GGRS state checksums).
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

fn pack_checksums(hashes: [u16; 8]) -> u128 {
    let mut res: u128 = 0;
    for (i, &h) in hashes.iter().enumerate() {
        res |= (h as u128) << (i * 16);
    }
    res
}

fn unpack_checksums(checksum: u128) -> [u16; 8] {
    let mut res = [0; 8];
    for i in 0..8 {
        res[i] = ((checksum >> (i * 16)) & 0xFFFF) as u16;
    }
    res
}

pub mod sim_socket;
pub mod spectator_session;

#[cfg(not(target_arch = "wasm32"))]
pub mod udp_socket;

pub use spectator_session::OnlineSpectatorSession;
pub use sim_socket::{NetworkSimulator, SimAddr, SimConfig, SimSocket};

#[cfg(not(target_arch = "wasm32"))]
pub use udp_socket::UdpSocket;

use crate::io::{PackedInput, unpack_input};

/// GGRS config wrapper so we can be generic over the address type
/// (e.g. `String`, `SocketAddr`, or a host-defined peer id).
#[derive(Debug)]
pub struct GGRSCfg<A>(core::marker::PhantomData<A>);

impl<A> ggrs::Config for GGRSCfg<A>
where
    A: Clone
        + Send
        + Sync
        + std::fmt::Debug
        + std::fmt::Display
        + std::hash::Hash
        + Eq
        + PartialEq
        + 'static,
{
    type Input = PackedInput;
    type State = Vec<u8>;
    type Address = A;
}


impl<A> OnlineSession<A> 
where 
    A: Clone
        + Send
        + Sync
        + std::fmt::Debug
        + std::fmt::Display
        + std::hash::Hash
        + Eq
        + PartialEq
        + 'static,
{
    /// Start a GGRS P2P session from a **pre-bound, non-blocking** socket.
    ///
    /// Call `socket.set_nonblocking(true)` before passing the socket here.
    pub fn start_with_socket<S>(
        socket: S,
        remote_addr: A,
        we_are_player_0: bool,
        input_delay: u32,
    ) -> Result<Self, String> 
    where 
        S: NonBlockingSocket<A> + 'static,
        A: Clone + Send + Sync + std::fmt::Debug + std::hash::Hash + Eq + PartialEq + 'static,
    {
        let (local_handle, remote_handle) =
            if we_are_player_0 { (0, 1) } else { (1, 0) };

        let session = SessionBuilder::<GGRSCfg<A>>::new()
            .with_num_players(2)
            .with_input_delay(input_delay as usize)
            .with_desync_detection_mode(ggrs::DesyncDetection::On { interval: 60 })
            .add_player(PlayerType::Local,               local_handle)
            .map_err(|e| e.to_string())?
            .add_player(PlayerType::Remote(remote_addr.clone()), remote_handle)
            .map_err(|e| e.to_string())?
            .start_p2p_session(socket)
            .map_err(|e| e.to_string())?;

        log::info!(
            "GGRS session started: local_handle={} remote={} input_delay={}",
            local_handle, remote_addr, input_delay
        );
        Ok(Self { 
            session, 
            local_handle, 
            remote_handle, 
            error: None, 
            frames_since_running: 0,
            frames_to_wait: 0,
            current_frame: 0,
            all_inputs: vec![(0, 0); 60 * 60 * 2],
            last_polled_frame: -1,
            total_advance_calls: 0,
            local_checksums: std::collections::BTreeMap::new(),
            last_rollback_frames: 0,
        })
    }

    /// Whether the session has produced at least one real frame.
    /// The frontend can use this to decide when to switch from a placeholder
    /// overlay to live game video.
    pub fn video_ready(&self) -> bool {
        self.frames_since_running > 0
    }

    /// Whether audio output should be unmuted.
    /// Returns `true` once GGRS reaches `Running` state and produces its first frame.
    pub fn audio_ready(&self) -> bool {
        self.frames_since_running > 0
    }

    /// Returns a multiplier [0.0..1.0] to fade the video in from black
    /// over the first 30 frames (0.5 seconds) of live execution.
    pub fn fade_intensity(&self) -> f32 {
        const FADE_FRAMES: u32 = 30;
        if self.frames_since_running >= FADE_FRAMES {
            1.0
        } else {
            self.frames_since_running as f32 / FADE_FRAMES as f32
        }
    }

    /// Drive one GGRS frame with the local player's input.
    /// Handles SaveGameState / LoadGameState / AdvanceFrame requests.
    /// Returns the (framebuffer, audio, had_rollback) from the final AdvanceFrame
    /// in the batch if one occurred. `had_rollback` is true if a LoadGameState was
    /// processed in this batch (i.e. we corrected history). The caller can use
    /// this to retract any optimistically queued recent audio.
    pub fn advance(
        &mut self,
        emulator: &mut dyn EmulatorCore,
        local_input: PackedInput,
    ) -> Option<(Vec<u8>, Vec<f32>, bool)> {
        self.total_advance_calls += 1;
        self.session.poll_remote_clients();

        // Drain GGRS events. Fatal events set self.error; the shell will
        // notice and tear down the session after this advance() returns.
        for event in self.session.events() {
            match event {
                ggrs::GgrsEvent::Disconnected { .. } => {
                    self.error = Some("Remote player disconnected".to_string());
                }
                ggrs::GgrsEvent::DesyncDetected { frame, local_checksum, remote_checksum, .. } => {
                    self.error = Some(format!(
                        "Desync at frame {frame} (local={local_checksum:032x} remote={remote_checksum:032x})"
                    ));

                    log::error!(
                        "================= DESYNC DETECTED (FRAME {frame}) ================="
                    );
                    let loc = unpack_checksums(local_checksum);
                    let rem = unpack_checksums(remote_checksum);
                    // Lane labels are host/debug conventions (NeoGeo layout when
                    // multi-region checksums are used; CPS folds a single blob).
                    let labels = ["lane0", "lane1", "lane2", "lane3", "lane4", "lane5", "lane6", "lane7"];
                    for i in 0..8 {
                        let ok = if loc[i] == rem[i] { "OK" } else { "MISMATCH" };
                        log::error!(
                            "{:<8} local={:04x} remote={:04x} {}",
                            labels[i], loc[i], rem[i], ok
                        );
                    }
                    log::error!("--- LOCAL HISTORY DUMP (Frames {}-{}) ---", frame - 60, frame);
                    for f in (frame - 60)..=frame {
                        if let Some(h) = self.local_checksums.get(&f) {
                            log::error!(
                                "Frame {:<4} | {:04x} {:04x} {:04x} {:04x} {:04x} {:04x} {:04x} {:04x}",
                                f, h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]
                            );
                        }
                    }
                }
                ggrs::GgrsEvent::NetworkInterrupted { disconnect_timeout, .. } => {
                    log::warn!("GGRS: network interrupted, will disconnect in {}ms", disconnect_timeout);
                }
                ggrs::GgrsEvent::WaitRecommendation { skip_frames } => {
                    self.frames_to_wait = (self.frames_to_wait + skip_frames).min(4);
                }
                _other => {},
            }
        }

        // Stop advancing once a fatal event has been recorded.
        if self.error.is_some() {
            return None;
        }

        if self.frames_to_wait > 0 {
            self.frames_to_wait -= 1;
            // Still pump the network so we don't drop the connection while waiting
            self.session.poll_remote_clients();
            return None;
        }

        // We deliberately feed inputs to GGRS even during the initial
        // synchronizing phase (before current_state() == Running). This is
        // required for the peers to exchange the initial inputs over UDP and
        // reach the Running state. Gating add/advance behind Running would
        // starve the sync and leave the window black/unresponsive forever.

        if let Err(e) = self.session.add_local_input(self.local_handle, local_input) {
            // If the error is AlreadyMapped, we still want to pump advance_frame()
            // to allow the session to synchronize or receive remote inputs.
            // Returning early here would permanently deadlock the session.
            log::warn!("GGRS add_local_input: {}", e);
        }

        let requests = match self.session.advance_frame() {
            Ok(r)   => r,
            Err(ggrs::GgrsError::PredictionThreshold) => {
                return None;
            }
            Err(ggrs::GgrsError::NotSynchronized) => {
                return None;
            }
            Err(e)  => { log::warn!("GGRS advance_frame: {}", e); return None; }
        };

        let mut out_fb: Option<Vec<u8>> = None;
        let mut out_audio: Option<Vec<f32>> = None;
        let mut had_rollback = false;
        let mut rollback_frame_count: u32 = 0;

        // Count AdvanceFrame requests so we can identify which is the last
        // (display) frame vs. rollback catch-up frames.  Catch-up uses
        // step_cpu(); only the final advance renders video and drains audio.
        let total_advances = requests.iter()
            .filter(|r| matches!(r, GgrsRequest::AdvanceFrame { .. }))
            .count();
        let mut advance_idx = 0usize;

        for req in requests {
            match req {
                GgrsRequest::SaveGameState { cell, frame } => {
                    match emulator.save_state_and_checksums() {
                        Ok((blob, hashes_opt)) => {
                            let checksum = match hashes_opt {
                                Some(hashes) => {
                                    self.local_checksums.insert(frame, hashes);
                                    // Keep memory clean
                                    if self.local_checksums.len() > 120 {
                                        if let Some(&min) = self.local_checksums.keys().next() {
                                            self.local_checksums.remove(&min);
                                        }
                                    }
                                    pack_checksums(hashes)
                                },
                                None => fnv1a(&blob) as u128,
                            };
                            cell.save(frame, Some(blob), Some(checksum));
                        }
                        Err(e) => {
                            log::error!("save_state: {}", e);
                            self.error = Some(format!("save_state failed: {e}"));
                            return None;
                        }
                    }
                }
                GgrsRequest::LoadGameState { cell, frame } => {
                    had_rollback = true;
                    rollback_frame_count = 0; // reset — we'll count AdvanceFrames after load
                    self.current_frame = frame;
                    match cell.load() {
                        Some(blob) => {
                            if let Err(e) = emulator.load_state_from_bytes(&blob) {
                                log::error!("load_state: {}", e);
                                self.error = Some(format!("load_state failed: {e}"));
                                return None;
                            }
                        }
                        None => {
                            let msg = format!(
                                "load_state: empty GGRS cell at frame {frame}"
                            );
                            log::error!("{msg}");
                            self.error = Some(msg);
                            return None;
                        }
                    }
                }
                GgrsRequest::AdvanceFrame { inputs } => {
                    advance_idx += 1;
                    let is_last_advance = advance_idx == total_advances;

                    // Record inputs for replays
                    if self.current_frame >= 0 {
                        let f = self.current_frame as usize;
                        if self.all_inputs.len() <= f {
                            self.all_inputs.resize(f + 1, (0, 0));
                        }
                        self.all_inputs[f] = (inputs[0].0, inputs[1].0);
                    }
                    self.current_frame += 1;

                    // inputs[i] = (packed_input, InputStatus), indexed by handle.
                    // GGRS handle 0 → emulator P1 controls.
                    // GGRS handle 1 → emulator P2 controls.
                    // Each player packs their keyboard into the `p1` field of
                    // their InputState; we remap the P2-side sys/coin bits here.
                    let p0_inp = unpack_input(inputs[0].0);
                    let p1_inp = unpack_input(inputs[1].0);

                    // ── sys (active-low: 0 = pressed) ────────────────────────
                    // Start with P0's sys (P1 Start=bit0, P1 Select=bit1, P2 Start=bit2).
                    // P1's "Num1" (their P1 Start, bit0=0) → P2 Start (bit2).
                    let mut combined_sys = p0_inp.sys;
                    if (p1_inp.sys & 0x01) == 0 { combined_sys &= !0x04u8; } // P2 Start

                    // ── coin (active-low: 0 = inserted) ──────────────────────
                    // P0's Num5 → coin slot 1 (bit0).
                    // P1's Num5 (their bit0=0) → coin slot 2 (bit1).
                    let mut combined_coin = p0_inp.coin;
                    if (p1_inp.coin & 0x01) == 0 { combined_coin &= !0x02u8; }

                    let combined = crate::io::InputState {
                        p1:   p0_inp.p1,   // P0's joystick/buttons → P1
                        p2:   p1_inp.p1,   // P1's joystick/buttons → P2
                        sys:  combined_sys,
                        coin: combined_coin,
                        // 6-button E/F kicks: each peer carries its own E/F in
                        // ext bits 0–1; merge into the P1/P2 combined layout.
                        ext:  crate::io::combine_ext(p0_inp.ext, p1_inp.ext),
                    };
                    emulator.set_input(combined);

                    if is_last_advance {
                        let out = emulator.step(NOMINAL_SAMPLES_PER_FRAME);
                        out_fb = Some(out.framebuffer.to_vec());
                        out_audio = Some(out.audio.to_vec());
                    } else {
                        emulator.step_cpu();
                        if had_rollback {
                            rollback_frame_count += 1;
                        }
                    }
                }
            }
        }

        // Track how many frames we've produced while in Running state.
        // This drives the audio_ready() / video_ready() gates.
        let is_running = self.session.current_state() == ggrs::SessionState::Running;
        if is_running && out_fb.is_some() {
            self.frames_since_running = self.frames_since_running.saturating_add(1);
            if self.frames_since_running == 1 {
                log::info!("ONLINE: first GGRS Running frame produced — video & audio ready");
            }
        }

        self.last_rollback_frames = rollback_frame_count;

        match (out_fb, out_audio) {
            (Some(fb), Some(au)) => Some((fb, au, had_rollback)),
            _ => None,
        }
    }

    /// Query GGRS network stats for the remote peer and combine with local metrics.
    pub fn get_stats(&self) -> NetplayStats {
        match self.session.network_stats(self.remote_handle) {
            Ok(stats) => NetplayStats {
                ping_ms: stats.ping as u32,
                local_frames_behind: stats.local_frames_behind,
                remote_frames_behind: stats.remote_frames_behind,
                send_queue_len: stats.send_queue_len as u32,
                kbps_sent: stats.kbps_sent as u32,
                rollback_frames: self.last_rollback_frames,
                frames_to_wait: self.frames_to_wait,
            },
            Err(_) => NetplayStats {
                rollback_frames: self.last_rollback_frames,
                frames_to_wait: self.frames_to_wait,
                ..Default::default()
            },
        }
    }

    /// Returns frame inputs since the last poll (for live broadcasting).
    ///
    /// Streams only inputs GGRS has **confirmed** (received from both peers up
    /// to `confirmed_frame()`).  Confirmed inputs are final and never rolled
    /// back, so live spectators — which have no rollback of their own — stay in
    /// perfect sync.  Streaming the *predicted* `current_frame` instead would
    /// permanently bake any host misprediction into the spectator stream,
    /// desyncing watchers for the remainder of the match.  Confirmation lags
    /// the live frame only by the input delay plus network RTT under normal
    /// play; severe packet loss adds latency but never incorrect frames.

    pub fn poll_remote_clients(&mut self) {
        self.session.poll_remote_clients();
    }

    pub fn pull_recent_inputs(&mut self) -> Vec<(PackedInput, PackedInput)> {
        // confirmed_frame() asserts at least one connected peer, so only query
        // it once the session is actually running.
        if self.session.current_state() != ggrs::SessionState::Running {
            return Vec::new();
        }
        let confirmed = self.session.confirmed_frame();
        if confirmed < 0 {
            return Vec::new();
        }
        // confirmed_frame() is inclusive; stream up to and including it.
        let end = ((confirmed as usize) + 1).min(self.all_inputs.len());
        let last = self.last_polled_frame.max(0) as usize;
        if end <= last {
            return Vec::new();
        }

        let mut res = Vec::with_capacity(end - last);
        for i in last..end {
            res.push(self.all_inputs[i]);
        }
        self.last_polled_frame = end as i32;
        res
    }

    /// Returns the entire input history (for saving replays).
    /// Uses `current_frame` (the actual number of frames advanced) rather than
    /// GGRS `confirmed_frame()`, which can lag behind by tens of frames and
    /// would truncate the final moments of the match.
    pub fn get_full_replay_data(&self) -> Vec<(PackedInput, PackedInput)> {
        let end = self.current_frame.max(0) as usize;
        let end = end.min(self.all_inputs.len());
        self.all_inputs[..end].to_vec()
    }

    pub fn total_advance_calls(&self) -> u64 {
        self.total_advance_calls
    }

    /// Returns the packed input pair for the most recently advanced frame.
    /// This reflects the actual inputs used by the emulator after any rollback,
    /// so the HUD can show both local and remote players' real inputs.
    /// Returns `None` before the first frame has been produced.
    pub fn last_inputs(&self) -> Option<(PackedInput, PackedInput)> {
        let frame = self.current_frame.saturating_sub(1).max(0) as usize;
        if frame < self.all_inputs.len() {
            Some(self.all_inputs[frame])
        } else {
            None
        }
    }
}
