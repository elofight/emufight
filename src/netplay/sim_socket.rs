//! Simulated network transport for headless netcode testing.
//!
//! Provides [`NetworkSimulator`], which creates a pair of [`SimSocket`]s that
//! implement `ggrs::NonBlockingSocket`.  Packets routed between the two
//! endpoints are delayed by a configurable base latency plus random jitter, and
//! can be randomly dropped.
//!
//! This lets the netplay code run against realistic network conditions without
//! leaving the build tree, and without needing two physical machines.
//!
//! Because the YM2610 C++ backend uses thread-local state, each peer in a
//! simulation must run on its own OS thread.  The harness in
//! `src/bin/netplay_sim.rs` does exactly that.

use ggrs::{Message, NonBlockingSocket};
use rand::{rngs::StdRng, Rng, SeedableRng};
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Address type for the simulated network.  Only two endpoints exist.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SimAddr(pub u8);

impl fmt::Display for SimAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "sim:{}", self.0)
    }
}

/// Configuration for the simulated link between the two endpoints.
///
/// Delay is one-way: a packet sent at time `t` is deliverable at
/// `t + latency + jitter_sample`, where `jitter_sample` is uniformly
/// distributed in `[-jitter/2, +jitter/2]`.
#[derive(Clone, Copy, Debug)]
pub struct SimConfig {
    /// Base one-way latency.
    pub latency: Duration,
    /// Total jitter range (peak-to-peak).  Half is subtracted or added.
    pub jitter: Duration,
    /// Probability of dropping any given packet, in `[0.0, 1.0]`.
    pub loss: f64,
}

impl Default for SimConfig {
    fn default() -> Self {
        Self {
            latency: Duration::from_millis(20),
            jitter: Duration::from_millis(5),
            loss: 0.0,
        }
    }
}

struct SimPacket {
    from: SimAddr,
    to: SimAddr,
    msg: Message,
    deliver_at: Instant,
}

struct SimShared {
    packets: Vec<SimPacket>,
    config: SimConfig,
    rng: StdRng,
    start: Instant,
}

/// One endpoint of the simulated network.  Clone it to share the same
/// underlying simulator (for example, to keep a handle while giving another
/// clone to a GGRS session).
pub struct SimSocket {
    local_addr: SimAddr,
    shared: Arc<Mutex<SimShared>>,
}

impl Clone for SimSocket {
    fn clone(&self) -> Self {
        Self {
            local_addr: self.local_addr,
            shared: self.shared.clone(),
        }
    }
}

impl SimSocket {
    fn new(local_addr: SimAddr, shared: Arc<Mutex<SimShared>>) -> Self {
        Self {
            local_addr,
            shared,
        }
    }

    /// Local address of this endpoint.
    pub fn local_addr(&self) -> SimAddr {
        self.local_addr
    }
}

/// Pair of connected sockets plus the shared simulator state.
///
/// Create with `NetworkSimulator::new`, then hand each [`SimSocket`] to a
/// peer.  The simulator uses real time for packet delivery, so the harness
/// should advance both peers at roughly the same rate (e.g. 60 Hz).
pub struct NetworkSimulator {
    shared: Arc<Mutex<SimShared>>,
    pub sockets: [SimSocket; 2],
}

impl NetworkSimulator {
    /// Create a simulator with the given link parameters.
    pub fn new(config: SimConfig) -> Self {
        let shared = Arc::new(Mutex::new(SimShared {
            packets: Vec::new(),
            config,
            rng: StdRng::from_entropy(),
            start: Instant::now(),
        }));
        Self {
            shared: shared.clone(),
            sockets: [
                SimSocket::new(SimAddr(0), shared.clone()),
                SimSocket::new(SimAddr(1), shared),
            ],
        }
    }

    /// Wall-clock time since the simulator was created.
    pub fn elapsed(&self) -> Duration {
        self.shared.lock().unwrap().start.elapsed()
    }

    /// Copy of the current link configuration.
    pub fn config(&self) -> SimConfig {
        let guard = self.shared.lock().unwrap();
        guard.config
    }

    /// Number of packets currently in flight.
    pub fn packets_in_flight(&self) -> usize {
        self.shared.lock().unwrap().packets.len()
    }
}

impl NonBlockingSocket<SimAddr> for SimSocket {
    fn send_to(&mut self, msg: &Message, addr: &SimAddr) {
        let mut guard = self.shared.lock().unwrap();

        if guard.config.loss > 0.0 && guard.rng.gen::<f64>() < guard.config.loss {
            return;
        }

        let now = Instant::now();
        let delay = if guard.config.jitter == Duration::ZERO {
            guard.config.latency
        } else {
            let half = guard.config.jitter.as_secs_f64() / 2.0;
            let jitter_s = guard.rng.gen_range(-half..half);
            guard
                .config
                .latency
                .saturating_add(Duration::from_secs_f64(jitter_s.max(0.0)))
        };

        guard.packets.push(SimPacket {
            from: self.local_addr,
            to: *addr,
            msg: msg.clone(),
            deliver_at: now + delay,
        });
    }

    fn receive_all_messages(&mut self) -> Vec<(SimAddr, Message)> {
        let mut guard = self.shared.lock().unwrap();
        let now = Instant::now();
        let mut out = Vec::new();
        guard.packets.retain(|p| {
            if p.to == self.local_addr && p.deliver_at <= now {
                out.push((p.from, p.msg.clone()));
                false
            } else {
                true
            }
        });
        out
    }
}
