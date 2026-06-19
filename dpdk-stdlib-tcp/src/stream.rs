//! Sync TCP socket: engine loop, connect, and DpdkTcpStream.
//!
//! This module implements:
//! - `engine_loop`: the runtime driving the TcpEngine on a dedicated thread
//! - `connect_v4` / `connect_timeout`: socket-layer connection establishment
//! - `DpdkTcpStream`: blocking io::Read / io::Write over SPSC rings

use std::io;
use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dpdk_stdlib_net::backend::{PacketBackend, RxReadiness};
use dpdk_stdlib_net::neighbor::NeighborResolver;

use crate::codec::parse_tcp_packet;
use crate::contract::{
    CommandSender, ConnectionHandle, EngineCommand, EngineWakeup,
    oneshot_channel,
};
use crate::engine::TcpEngine;
use crate::state::FourTuple;

// ============================================================
// Engine Loop
// ============================================================

/// Run the TCP engine loop on a dedicated thread.
///
/// Selects on rx_readiness | engine_wakeup | timer deadline, dispatching:
/// - Inbound frames → `parse_tcp_packet` → `engine.on_segment`
/// - Commands from `cmd_rx` → `engine.on_command`
/// - Timer ticks → `engine.on_tick`
///
/// All outbound frames are sent via `backend.send_frame`.
pub fn engine_loop(
    backend: Arc<dyn PacketBackend>,
    engine: &mut TcpEngine,
    cmd_rx: Receiver<EngineCommand>,
    wakeup: Arc<EngineWakeup>,
) {
    loop {
        // Determine the next timer deadline for bounded wait.
        let now = engine.clock().now();
        let deadline = engine.next_timer_deadline(now);
        let timeout = deadline
            .map(|d| d.saturating_duration_since(now))
            .unwrap_or(Duration::from_millis(100));

        // Wait for an event based on backend rx_readiness type.
        match backend.rx_readiness() {
            RxReadiness::Condvar(pair) => {
                // Stub/test: wait on condvar or engine_wakeup signal.
                // Check wakeup first (command may already be pending).
                if !wakeup.try_recv() {
                    let (lock, cv) = &*pair;
                    let guard = lock.lock().unwrap();
                    if !*guard && !wakeup.try_recv() {
                        let _ = cv.wait_timeout(guard, timeout).unwrap();
                    }
                }
            }
            RxReadiness::PollOnly => {
                // DPDK: busy-poll with short sleep to check wakeup/timer.
                // In practice this is a dedicated core; we yield briefly.
                if !wakeup.try_recv() {
                    std::thread::sleep(timeout.min(Duration::from_millis(1)));
                }
            }
            RxReadiness::Fd(_fd) => {
                // AF_PACKET: in production would epoll; for now use wakeup wait.
                wakeup.wait(timeout);
            }
        }

        // --- Process inbound frames ---
        if let Ok(frames) = backend.recv_frames(32) {
            for frame in &frames {
                if let Ok(seg) = parse_tcp_packet(frame) {
                    let outbound = engine.on_segment(&seg);
                    for out in outbound {
                        let _ = backend.send_frame(&out);
                    }
                }
            }
        }

        // --- Process pending commands ---
        while let Ok(cmd) = cmd_rx.try_recv() {
            let outbound = engine.on_command(cmd);
            for out in outbound {
                let _ = backend.send_frame(&out);
            }
        }

        // --- Service timers + drain tx_rings ---
        let now = engine.clock().now();
        let outbound = engine.on_tick(now);
        for out in outbound {
            let _ = backend.send_frame(&out);
        }
    }
}

// ============================================================
// Connect helpers (socket layer)
// ============================================================

/// Resolve destination MAC and issue `EngineCommand::Connect`, parking until
/// the engine completes the three-way handshake or returns an error.
pub fn connect_v4(
    remote: SocketAddr,
    local: SocketAddr,
    backend: &Arc<dyn PacketBackend>,
    resolver: &Arc<dyn NeighborResolver>,
    cmd_tx: &CommandSender,
    _wakeup: &Arc<EngineWakeup>,
) -> io::Result<(FourTuple, Arc<ConnectionHandle>)> {
    let dst_mac = resolver.resolve(remote.ip())?;
    let src_mac = backend.mac_address();

    let key = FourTuple { local, remote };
    let handle = Arc::new(ConnectionHandle::new(65536, 65536, cmd_tx.clone(), key));

    let (resp_tx, resp_rx) = oneshot_channel();
    cmd_tx
        .send(EngineCommand::Connect {
            local,
            remote,
            src_mac,
            dst_mac,
            handle: handle.clone(),
            response: resp_tx,
        })
        .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "engine channel closed"))?;

    // Park until Established or error.
    let result = resp_rx.recv();
    match result {
        Ok(_key) => Ok((key, handle)),
        Err(e) => Err(e.into()),
    }
}

/// Same as `connect_v4` but with a timeout. Returns `TimedOut` if the
/// handshake does not complete within the given duration.
pub fn connect_timeout(
    remote: SocketAddr,
    local: SocketAddr,
    timeout: Duration,
    backend: &Arc<dyn PacketBackend>,
    resolver: &Arc<dyn NeighborResolver>,
    cmd_tx: &CommandSender,
    _wakeup: &Arc<EngineWakeup>,
) -> io::Result<(FourTuple, Arc<ConnectionHandle>)> {
    let dst_mac = resolver.resolve(remote.ip())?;
    let src_mac = backend.mac_address();

    let key = FourTuple { local, remote };
    let handle = Arc::new(ConnectionHandle::new(65536, 65536, cmd_tx.clone(), key));

    let (resp_tx, resp_rx) = oneshot_channel();
    cmd_tx
        .send(EngineCommand::Connect {
            local,
            remote,
            src_mac,
            dst_mac,
            handle: handle.clone(),
            response: resp_tx,
        })
        .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "engine channel closed"))?;

    match resp_rx.recv_timeout(timeout) {
        Some(Ok(_key)) => Ok((key, handle)),
        Some(Err(e)) => Err(e.into()),
        None => Err(io::Error::new(io::ErrorKind::TimedOut, "connect timed out")),
    }
}

// ============================================================
// DpdkTcpStream
// ============================================================

/// A blocking TCP stream backed by DPDK. Reads/writes go through SPSC rings
/// shared with the engine thread.
pub struct DpdkTcpStream {
    pub handle: Arc<ConnectionHandle>,
    pub key: FourTuple,
    // Per-stream settings (mirrored from handle for fast access).
    nonblocking: bool,
    read_timeout: Option<Duration>,
    write_timeout: Option<Duration>,
}

impl DpdkTcpStream {
    /// Create a new stream from an established connection handle.
    pub fn new(handle: Arc<ConnectionHandle>, key: FourTuple) -> Self {
        Self {
            handle,
            key,
            nonblocking: false,
            read_timeout: None,
            write_timeout: None,
        }
    }

    /// Set non-blocking mode.
    pub fn set_nonblocking(&mut self, nonblocking: bool) {
        self.nonblocking = nonblocking;
    }

    /// Get non-blocking mode.
    pub fn nonblocking(&self) -> bool {
        self.nonblocking
    }

    /// Set read timeout.
    pub fn set_read_timeout(&mut self, timeout: Option<Duration>) {
        self.read_timeout = timeout;
    }

    /// Get read timeout.
    pub fn read_timeout(&self) -> Option<Duration> {
        self.read_timeout
    }

    /// Set write timeout.
    pub fn set_write_timeout(&mut self, timeout: Option<Duration>) {
        self.write_timeout = timeout;
    }

    /// Get write timeout.
    pub fn write_timeout(&self) -> Option<Duration> {
        self.write_timeout
    }

    /// Get the local address.
    pub fn local_addr(&self) -> SocketAddr {
        self.key.local
    }

    /// Get the peer address.
    pub fn peer_addr(&self) -> SocketAddr {
        self.key.remote
    }
}

impl io::Read for DpdkTcpStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let _read_guard = self.handle.read_mutex.lock().unwrap();
        let deadline = self.read_timeout.map(|d| Instant::now() + d);

        loop {
            // 1. Check sticky error.
            if let Some(err) = self.handle.peek_error() {
                return Err(err.into());
            }

            // 2. Try non-blocking read from rx_ring.
            let n = self.handle.rx_ring.read(buf);
            if n > 0 {
                return Ok(n);
            }

            // 3. Check explicit EOF.
            if self.handle.eof.load(Ordering::Acquire) {
                return Ok(0);
            }

            // 4. Non-blocking mode → WouldBlock.
            if self.nonblocking {
                return Err(io::Error::new(io::ErrorKind::WouldBlock, "would block"));
            }

            // 5. P0-B: Lock notify_lock, recheck under it, then wait.
            let guard = self.handle.notify_lock.lock().unwrap();

            // Recheck under the lock.
            if self.handle.rx_ring.available_read() > 0
                || self.handle.eof.load(Ordering::Acquire)
                || self.handle.peek_error().is_some()
            {
                drop(guard);
                continue;
            }

            // 6. Park with optional timeout.
            match deadline {
                Some(dl) => {
                    let remaining = dl.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "read timed out",
                        ));
                    }
                    let (_guard, _) = self.handle.condvar.wait_timeout(guard, remaining).unwrap();
                }
                None => {
                    let _guard = self.handle.condvar.wait(guard).unwrap();
                }
            }
        }
    }
}

impl io::Write for DpdkTcpStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let _write_guard = self.handle.write_mutex.lock().unwrap();
        let deadline = self.write_timeout.map(|d| Instant::now() + d);

        loop {
            // 1. Check sticky error.
            if let Some(err) = self.handle.peek_error() {
                return Err(err.into());
            }

            // 2. Try non-blocking write to tx_ring.
            let n = self.handle.tx_ring.write(buf);
            if n > 0 {
                // Signal engine to pick up data.
                self.handle.cmd_tx.wakeup().signal();
                return Ok(n);
            }

            // 3. Non-blocking mode → WouldBlock.
            if self.nonblocking {
                return Err(io::Error::new(io::ErrorKind::WouldBlock, "would block"));
            }

            // 4. P0-B: Lock notify_lock, recheck, then wait.
            let guard = self.handle.notify_lock.lock().unwrap();

            // Recheck under the lock.
            if self.handle.tx_ring.available_write() > 0
                || self.handle.peek_error().is_some()
            {
                drop(guard);
                continue;
            }

            // 5. Park with optional timeout.
            match deadline {
                Some(dl) => {
                    let remaining = dl.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "write timed out",
                        ));
                    }
                    let (_guard, _) = self.handle.condvar.wait_timeout(guard, remaining).unwrap();
                }
                None => {
                    let _guard = self.handle.condvar.wait(guard).unwrap();
                }
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        // Flush = hand-off to engine (don't block waiting for ACK).
        Ok(())
    }
}

impl Drop for DpdkTcpStream {
    fn drop(&mut self) {
        // Decrement app_refcount; send Close on last handle.
        if self.handle.app_refcount.fetch_sub(1, Ordering::AcqRel) == 1 {
            let linger = self.handle.linger.lock().unwrap().clone();
            let _ = self.handle.cmd_tx.send(EngineCommand::Close {
                key: self.key,
                linger,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::sync::mpsc;
    use std::thread;

    use crate::clock::MockClock;
    use crate::engine::{EngineConfig, TcpEngine};

    /// A minimal stub backend for testing (no real NIC).
    struct StubBackend {
        mac: [u8; 6],
        rx_pair: Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
    }

    impl StubBackend {
        fn new() -> Self {
            Self {
                mac: [0x02, 0x00, 0x00, 0x00, 0x00, 0x01],
                rx_pair: Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new())),
            }
        }
    }

    impl PacketBackend for StubBackend {
        fn send_frame(&self, _frame: &[u8]) -> io::Result<usize> {
            Ok(_frame.len())
        }
        fn recv_frames(&self, _max: usize) -> io::Result<Vec<Vec<u8>>> {
            Ok(Vec::new())
        }
        fn mac_address(&self) -> [u8; 6] {
            self.mac
        }
        fn backend_name(&self) -> &'static str {
            "stub"
        }
        fn set_promiscuous(&self, _: bool) -> io::Result<()> {
            Ok(())
        }
        fn is_promiscuous(&self) -> bool {
            false
        }
        fn set_allmulticast(&self, _: bool) -> io::Result<()> {
            Ok(())
        }
        fn is_allmulticast(&self) -> bool {
            false
        }
        fn rx_readiness(&self) -> RxReadiness {
            RxReadiness::Condvar(self.rx_pair.clone())
        }
    }

    #[test]
    fn dpdk_tcp_stream_nonblocking_read_returns_would_block() {
        let (tx, _rx) = mpsc::channel();
        let wakeup = Arc::new(EngineWakeup::new());
        let cmd_tx = CommandSender::new(tx, wakeup.clone());
        let key = FourTuple {
            local: "10.0.0.1:1234".parse().unwrap(),
            remote: "10.0.0.2:80".parse().unwrap(),
        };
        let handle = Arc::new(ConnectionHandle::new(1024, 1024, cmd_tx, key));

        let mut stream = DpdkTcpStream::new(handle.clone(), key);
        stream.set_nonblocking(true);

        let mut buf = [0u8; 64];
        let result = stream.read(&mut buf);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::WouldBlock);
    }

    #[test]
    fn dpdk_tcp_stream_nonblocking_write_returns_would_block_when_full() {
        let (tx, _rx) = mpsc::channel();
        let wakeup = Arc::new(EngineWakeup::new());
        let cmd_tx = CommandSender::new(tx, wakeup.clone());
        let key = FourTuple {
            local: "10.0.0.1:1234".parse().unwrap(),
            remote: "10.0.0.2:80".parse().unwrap(),
        };
        // Tiny ring so we can fill it easily.
        let handle = Arc::new(ConnectionHandle::new(1024, 4, cmd_tx, key));

        let mut stream = DpdkTcpStream::new(handle.clone(), key);
        // Fill the tx_ring.
        handle.tx_ring.write(&[1, 2, 3, 4]);

        stream.set_nonblocking(true);
        let result = stream.write(&[5]);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::WouldBlock);
    }

    #[test]
    fn dpdk_tcp_stream_read_returns_data_from_rx_ring() {
        let (tx, _rx) = mpsc::channel();
        let wakeup = Arc::new(EngineWakeup::new());
        let cmd_tx = CommandSender::new(tx, wakeup.clone());
        let key = FourTuple {
            local: "10.0.0.1:1234".parse().unwrap(),
            remote: "10.0.0.2:80".parse().unwrap(),
        };
        let handle = Arc::new(ConnectionHandle::new(1024, 1024, cmd_tx, key));

        // Simulate engine pushing data to rx_ring.
        handle.rx_ring.write(b"hello");

        let mut stream = DpdkTcpStream::new(handle.clone(), key);
        let mut buf = [0u8; 16];
        let n = stream.read(&mut buf).unwrap();
        assert_eq!(n, 5);
        assert_eq!(&buf[..5], b"hello");
    }

    #[test]
    fn dpdk_tcp_stream_read_returns_eof_on_flag() {
        let (tx, _rx) = mpsc::channel();
        let wakeup = Arc::new(EngineWakeup::new());
        let cmd_tx = CommandSender::new(tx, wakeup.clone());
        let key = FourTuple {
            local: "10.0.0.1:1234".parse().unwrap(),
            remote: "10.0.0.2:80".parse().unwrap(),
        };
        let handle = Arc::new(ConnectionHandle::new(1024, 1024, cmd_tx, key));
        handle.set_eof();

        let mut stream = DpdkTcpStream::new(handle.clone(), key);
        let mut buf = [0u8; 16];
        let n = stream.read(&mut buf).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn dpdk_tcp_stream_read_returns_error_when_latched() {
        let (tx, _rx) = mpsc::channel();
        let wakeup = Arc::new(EngineWakeup::new());
        let cmd_tx = CommandSender::new(tx, wakeup.clone());
        let key = FourTuple {
            local: "10.0.0.1:1234".parse().unwrap(),
            remote: "10.0.0.2:80".parse().unwrap(),
        };
        let handle = Arc::new(ConnectionHandle::new(1024, 1024, cmd_tx, key));
        handle.latch_error(crate::error::TcpError::ConnectionReset);

        let mut stream = DpdkTcpStream::new(handle.clone(), key);
        let mut buf = [0u8; 16];
        let result = stream.read(&mut buf);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::ConnectionReset);
    }

    #[test]
    fn dpdk_tcp_stream_write_pushes_to_tx_ring() {
        let (tx, _rx) = mpsc::channel();
        let wakeup = Arc::new(EngineWakeup::new());
        let cmd_tx = CommandSender::new(tx, wakeup.clone());
        let key = FourTuple {
            local: "10.0.0.1:1234".parse().unwrap(),
            remote: "10.0.0.2:80".parse().unwrap(),
        };
        let handle = Arc::new(ConnectionHandle::new(1024, 1024, cmd_tx, key));

        let mut stream = DpdkTcpStream::new(handle.clone(), key);
        let n = stream.write(b"world").unwrap();
        assert_eq!(n, 5);

        // Verify data is in tx_ring.
        let mut buf = [0u8; 16];
        let r = handle.tx_ring.read(&mut buf);
        assert_eq!(r, 5);
        assert_eq!(&buf[..5], b"world");
    }

    #[test]
    fn dpdk_tcp_stream_read_timeout_expires() {
        let (tx, _rx) = mpsc::channel();
        let wakeup = Arc::new(EngineWakeup::new());
        let cmd_tx = CommandSender::new(tx, wakeup.clone());
        let key = FourTuple {
            local: "10.0.0.1:1234".parse().unwrap(),
            remote: "10.0.0.2:80".parse().unwrap(),
        };
        let handle = Arc::new(ConnectionHandle::new(1024, 1024, cmd_tx, key));

        let mut stream = DpdkTcpStream::new(handle.clone(), key);
        stream.set_read_timeout(Some(Duration::from_millis(10)));

        let mut buf = [0u8; 16];
        let result = stream.read(&mut buf);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::TimedOut);
    }

    #[test]
    fn dpdk_tcp_stream_blocking_read_wakes_on_data() {
        let (tx, _rx) = mpsc::channel();
        let wakeup = Arc::new(EngineWakeup::new());
        let cmd_tx = CommandSender::new(tx, wakeup.clone());
        let key = FourTuple {
            local: "10.0.0.1:1234".parse().unwrap(),
            remote: "10.0.0.2:80".parse().unwrap(),
        };
        let handle = Arc::new(ConnectionHandle::new(1024, 1024, cmd_tx, key));
        let h2 = handle.clone();

        let reader = thread::spawn(move || {
            let mut stream = DpdkTcpStream::new(handle, key);
            let mut buf = [0u8; 16];
            stream.read(&mut buf).unwrap();
            buf[..5].to_vec()
        });

        // Give reader time to park.
        thread::sleep(Duration::from_millis(20));

        // Simulate engine delivering data.
        h2.rx_ring.write(b"async");
        h2.notify_all();

        let result = reader.join().unwrap();
        assert_eq!(result, b"async");
    }

    #[test]
    fn dpdk_tcp_stream_drop_sends_close() {
        let (tx, rx) = mpsc::channel();
        let wakeup = Arc::new(EngineWakeup::new());
        let cmd_tx = CommandSender::new(tx, wakeup.clone());
        let key = FourTuple {
            local: "10.0.0.1:1234".parse().unwrap(),
            remote: "10.0.0.2:80".parse().unwrap(),
        };
        let handle = Arc::new(ConnectionHandle::new(1024, 1024, cmd_tx, key));

        {
            let _stream = DpdkTcpStream::new(handle.clone(), key);
            // Stream dropped here.
        }

        // Verify Close command was sent.
        let cmd = rx.try_recv().unwrap();
        assert!(matches!(cmd, EngineCommand::Close { .. }));
    }

    #[test]
    fn connect_v4_resolves_mac_and_sends_connect_command() {
        let (tx, rx) = mpsc::channel();
        let wakeup = Arc::new(EngineWakeup::new());
        let cmd_tx = CommandSender::new(tx, wakeup.clone());
        let backend: Arc<dyn PacketBackend> = Arc::new(StubBackend::new());
        let resolver: Arc<dyn NeighborResolver> =
            Arc::new(dpdk_stdlib_net::neighbor::ArpResolver::with_gateway_mac([
                0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF,
            ]));

        let remote: SocketAddr = "10.0.0.2:80".parse().unwrap();
        let local: SocketAddr = "10.0.0.1:5000".parse().unwrap();

        // Spawn a thread to respond to the connect (engine side).
        let rx_thread = thread::spawn(move || {
            let cmd = rx.recv().unwrap();
            if let EngineCommand::Connect { response, .. } = cmd {
                let key = FourTuple { local, remote };
                response.send(Ok(key));
            }
        });

        let result = connect_v4(remote, local, &backend, &resolver, &cmd_tx, &wakeup);
        rx_thread.join().unwrap();

        assert!(result.is_ok());
        let (key, handle) = result.unwrap();
        assert_eq!(key.local, local);
        assert_eq!(key.remote, remote);
        assert_eq!(handle.app_refcount.load(Ordering::Acquire), 1);
    }

    #[test]
    fn connect_timeout_returns_timed_out() {
        let (tx, _rx) = mpsc::channel();
        let wakeup = Arc::new(EngineWakeup::new());
        let cmd_tx = CommandSender::new(tx, wakeup.clone());
        let backend: Arc<dyn PacketBackend> = Arc::new(StubBackend::new());
        let resolver: Arc<dyn NeighborResolver> =
            Arc::new(dpdk_stdlib_net::neighbor::ArpResolver::with_gateway_mac([
                0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF,
            ]));

        let remote: SocketAddr = "10.0.0.2:80".parse().unwrap();
        let local: SocketAddr = "10.0.0.1:5000".parse().unwrap();

        // Nobody responds → timeout.
        let result = connect_timeout(
            remote,
            local,
            Duration::from_millis(20),
            &backend,
            &resolver,
            &cmd_tx,
            &wakeup,
        );
        assert!(result.is_err());
        assert_eq!(result.err().unwrap().kind(), io::ErrorKind::TimedOut);
    }

    #[test]
    fn engine_loop_processes_commands() {
        let clock = Arc::new(MockClock::new());
        let config = EngineConfig::default();
        let mut engine = TcpEngine::new(clock, config);
        let (tx, rx) = mpsc::channel();
        let wakeup = Arc::new(EngineWakeup::new());
        let _cmd_tx = CommandSender::new(tx.clone(), wakeup.clone());
        let _backend: Arc<dyn PacketBackend> = Arc::new(StubBackend::new());

        // Send a Listen command before starting the loop.
        let (resp_tx, resp_rx) = oneshot_channel();
        let listen_addr: SocketAddr = "10.0.0.1:8080".parse().unwrap();
        tx.send(EngineCommand::Listen {
            addr: listen_addr,
            backlog: 16,
            response: resp_tx,
        })
        .unwrap();
        wakeup.signal();

        // Run one iteration of the loop manually to process the command.
        // (We can't run engine_loop in a thread easily since it's infinite,
        // so we test the components directly.)
        while let Ok(cmd) = rx.try_recv() {
            engine.on_command(cmd);
        }

        // Verify listen was registered.
        let result = resp_rx.recv();
        assert!(result.is_ok());
        assert!(engine.listeners.contains_key(&listen_addr));
    }
}
