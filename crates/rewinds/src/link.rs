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
use std::time::{Duration, Instant};

/// A pluggable serial-link carrier. `send` queues an opaque frame; `recv` flushes
/// pending writes and returns any whole frames that have arrived.
pub trait Transport {
    fn send(&mut self, frame: &[u8]);
    fn recv(&mut self) -> Vec<Vec<u8>>;

    /// Block (up to `timeout`) until at least one frame has arrived, returning all that
    /// are buffered. Empty on timeout. This is the transfer barrier's blocking wait: the
    /// master suspends here until the peer's word for the in-flight round arrives.
    fn recv_blocking(&mut self, timeout: Duration) -> Vec<Vec<u8>> {
        let deadline = Instant::now() + timeout;
        loop {
            let frames = self.recv();
            if !frames.is_empty() {
                return frames;
            }
            if Instant::now() >= deadline {
                return Vec::new();
            }
            std::thread::sleep(Duration::from_micros(100));
        }
    }
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

impl TcpLink {
    /// Flush as much of the outbound queue as the socket will accept (non-blocking).
    fn flush_out(&mut self) {
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
    }

    /// Read whatever is immediately available into the inbound buffer (non-blocking).
    fn drain_available(&mut self) {
        let mut tmp = [0u8; 512];
        loop {
            match self.stream.read(&mut tmp) {
                Ok(0) => break, // peer closed
                Ok(n) => self.inbuf.extend_from_slice(&tmp[..n]),
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }
    }

    /// Split every complete length-prefixed frame out of the inbound buffer.
    fn split_frames(&mut self) -> Vec<Vec<u8>> {
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

impl Transport for TcpLink {
    fn send(&mut self, frame: &[u8]) {
        self.out.push(frame.len() as u8);
        self.out.extend_from_slice(frame);
    }

    fn recv(&mut self) -> Vec<Vec<u8>> {
        self.flush_out();
        self.drain_available();
        self.split_frames()
    }

    /// Block up to `timeout` for at least one frame, **parking** the thread on the socket
    /// (a blocking read with a timeout) rather than busy-polling — so the master waiting
    /// for the peer's word leaves the CPU idle instead of spinning it.
    fn recv_blocking(&mut self, timeout: Duration) -> Vec<Vec<u8>> {
        self.flush_out();
        let ready = self.split_frames();
        if !ready.is_empty() {
            return ready;
        }
        let deadline = Instant::now() + timeout;
        self.stream.set_nonblocking(false).ok();
        let mut tmp = [0u8; 512];
        let frames = loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break Vec::new();
            }
            self.stream.set_read_timeout(Some(remaining)).ok();
            match self.stream.read(&mut tmp) {
                Ok(0) => break Vec::new(), // peer closed
                Ok(n) => {
                    self.inbuf.extend_from_slice(&tmp[..n]);
                    let frames = self.split_frames();
                    if !frames.is_empty() {
                        break frames;
                    }
                }
                Err(_) => break Vec::new(), // timeout or error
            }
        };
        // Restore the non-blocking mode `recv` relies on.
        self.stream.set_nonblocking(true).ok();
        let _ = self.stream.set_read_timeout(None);
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

    /// The transfer-barrier primitive: `recv_blocking` returns empty at the timeout when
    /// nothing arrives, and delivers a frame that shows up within the window.
    #[test]
    fn recv_blocking_times_out_then_delivers() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let child_stream = TcpStream::connect(addr).unwrap();
        let (parent_stream, _) = listener.accept().unwrap();
        let mut parent = TcpLink::wrap(parent_stream, 0).unwrap();
        let mut child = TcpLink::wrap(child_stream, 1).unwrap();

        // Nothing sent: it waits out the timeout and returns empty.
        let start = Instant::now();
        assert!(child.recv_blocking(Duration::from_millis(20)).is_empty());
        assert!(start.elapsed() >= Duration::from_millis(20));

        // A frame in flight is delivered well within the timeout.
        parent.send(&[1, 2, 3, 4, 5, 6, 7, 8]);
        parent.recv(); // flush parent's outbound onto the wire
        assert_eq!(
            child.recv_blocking(Duration::from_millis(500)),
            vec![vec![1, 2, 3, 4, 5, 6, 7, 8]]
        );
    }
}
