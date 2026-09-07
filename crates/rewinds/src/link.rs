//! Serial-link carrier for the desktop host — one pluggable transport that
//! relays the emulator's *opaque* serial frames between two `rewinds` instances.
//!
//! The emulator core is carrier-blind: it emits and consumes frame bytes and
//! knows nothing about how they travel. This TCP transport is one carrier; a
//! Swift Bonjour or web WebRTC host would implement the same length-prefixed
//! relay in its own language against the same frames. Frames are length-prefixed
//! (1-byte length + payload) so the carrier never needs to know their contents.

use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};

/// A pluggable serial-link carrier. `send` queues an opaque frame; `recv` flushes
/// pending writes and returns any whole frames that have arrived.
pub trait Transport {
    fn send(&mut self, frame: &[u8]);
    fn recv(&mut self) -> Vec<Vec<u8>>;
}

/// A two-unit TCP link: the parent listens, the child connects. (Three- and
/// four-unit links would extend this into a parent-hub star relay.)
pub struct TcpLink {
    stream: TcpStream,
    out: Vec<u8>,
    inbuf: Vec<u8>,
    id: u8,
    count: u8,
}

impl TcpLink {
    /// Parent: wait for one child to connect on `port`. This unit becomes id 0.
    pub fn listen(port: u16) -> std::io::Result<Self> {
        let listener = TcpListener::bind(("0.0.0.0", port))?;
        log::info!("serial link: waiting for a peer on port {port}…");
        let (stream, addr) = listener.accept()?;
        log::info!("serial link: peer connected from {addr}");
        Self::wrap(stream, 0)
    }

    /// Child: connect to a parent at `addr` (host:port). This unit becomes id 1.
    pub fn connect(addr: &str) -> std::io::Result<Self> {
        let stream = TcpStream::connect(addr)?;
        log::info!("serial link: connected to {addr}");
        Self::wrap(stream, 1)
    }

    fn wrap(stream: TcpStream, id: u8) -> std::io::Result<Self> {
        stream.set_nonblocking(true)?;
        stream.set_nodelay(true).ok();
        Ok(Self { stream, out: Vec::new(), inbuf: Vec::new(), id, count: 2 })
    }

    pub fn id(&self) -> u8 {
        self.id
    }
    pub fn count(&self) -> u8 {
        self.count
    }
}

impl Transport for TcpLink {
    fn send(&mut self, frame: &[u8]) {
        self.out.push(frame.len() as u8);
        self.out.extend_from_slice(frame);
    }

    fn recv(&mut self) -> Vec<Vec<u8>> {
        // Flush as much of the outbound queue as the socket will accept.
        while !self.out.is_empty() {
            match self.stream.write(&self.out) {
                Ok(0) => break,
                Ok(n) => {
                    self.out.drain(..n);
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }
        // Drain the socket into the inbound buffer.
        let mut tmp = [0u8; 512];
        loop {
            match self.stream.read(&mut tmp) {
                Ok(0) => break, // peer closed
                Ok(n) => self.inbuf.extend_from_slice(&tmp[..n]),
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }
        // Split off complete length-prefixed frames.
        let mut frames = Vec::new();
        while let Some(&len) = self.inbuf.first() {
            let len = len as usize;
            if self.inbuf.len() < 1 + len {
                break;
            }
            frames.push(self.inbuf[1..1 + len].to_vec());
            self.inbuf.drain(..1 + len);
        }
        frames
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Frames survive a real loopback round-trip, length-framing intact, in both
    /// directions. Uses an OS-assigned port so the test never collides.
    #[test]
    fn relays_frames_both_ways() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let child_stream = TcpStream::connect(addr).unwrap();
        let (parent_stream, _) = listener.accept().unwrap();
        let mut parent = TcpLink::wrap(parent_stream, 0).unwrap();
        let mut child = TcpLink::wrap(child_stream, 1).unwrap();
        assert_eq!((parent.id(), child.id(), parent.count()), (0, 1, 2));

        let pump = |from: &mut TcpLink, to: &mut TcpLink| -> Vec<Vec<u8>> {
            for _ in 0..100 {
                from.recv(); // flush `from`'s outbound queue onto the wire
                let got = to.recv();
                if !got.is_empty() {
                    return got;
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            Vec::new()
        };

        parent.send(&[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(pump(&mut parent, &mut child), vec![vec![1, 2, 3, 4, 5, 6, 7, 8]]);
        child.send(&[9, 10, 11, 12, 13, 14, 15, 16]);
        assert_eq!(pump(&mut child, &mut parent), vec![vec![9, 10, 11, 12, 13, 14, 15, 16]]);
    }
}
