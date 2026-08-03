//! UDP transport for GGRS (`std::net::UdpSocket`).
//!
//! Peer of [`crate::netplay::SimSocket`]: real OS datagrams instead of in-process
//! simulation.  No STUN, hole-punching, or matchmaking — the host supplies
//! bind/connect addresses.
//!
//! Available on native targets only (`cfg(not(target_arch = "wasm32"))`).

use ggrs::{Message, NonBlockingSocket};
use std::io;
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket as StdUdpSocket};

/// Non-blocking UDP socket implementing [`NonBlockingSocket`] for GGRS.
///
/// Messages are serialized with `bincode` (same as typical GGRS UDP hosts).
pub struct UdpSocket {
    inner: StdUdpSocket,
    buffer: [u8; 4096],
}

impl UdpSocket {
    /// Wrap an already-bound, non-blocking [`StdUdpSocket`].
    pub fn from_std(sock: StdUdpSocket) -> io::Result<Self> {
        sock.set_nonblocking(true)?;
        Ok(Self {
            inner: sock,
            buffer: [0; 4096],
        })
    }

    /// Bind `addr` (e.g. `0.0.0.0:7000` or `127.0.0.1:0` for ephemeral).
    pub fn bind(addr: impl ToSocketAddrs) -> io::Result<Self> {
        let sock = StdUdpSocket::bind(addr)?;
        Self::from_std(sock)
    }

    /// Local socket address after bind.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }

    /// Borrow the underlying std socket (e.g. for `connect` or options).
    pub fn std(&self) -> &StdUdpSocket {
        &self.inner
    }

    /// Mutable access to the underlying std socket.
    pub fn std_mut(&mut self) -> &mut StdUdpSocket {
        &mut self.inner
    }
}

impl NonBlockingSocket<SocketAddr> for UdpSocket {
    fn send_to(&mut self, msg: &Message, addr: &SocketAddr) {
        if let Ok(buf) = bincode::serialize(msg) {
            if let Err(e) = self.inner.send_to(&buf, addr) {
                if e.kind() != io::ErrorKind::WouldBlock {
                    log::warn!("UDP send_to {addr}: {e}");
                }
            }
        }
    }

    fn receive_all_messages(&mut self) -> Vec<(SocketAddr, Message)> {
        let mut out = Vec::new();
        loop {
            match self.inner.recv_from(&mut self.buffer) {
                Ok((n, src)) => {
                    if let Ok(msg) = bincode::deserialize(&self.buffer[..n]) {
                        out.push((src, msg));
                    }
                }
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => {
                    log::warn!("UDP recv_from: {e}");
                    break;
                }
            }
        }
        out
    }
}
