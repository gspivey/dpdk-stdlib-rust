//! App↔engine contract types.
//!
//! Pure data/enum types with no dependency back on TcpEngine.
//! These define the shared interface between app threads and the engine thread.

use std::net::{Shutdown, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::mpsc::{self, SendError};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use crate::error::TcpError;
use crate::ring::SpscByteRing;
use crate::state::{FourTuple, TcpState};

// --- EngineWakeup ---

/// Engine wakeup signal. Uses AtomicBool + Condvar (portable/test).
pub struct EngineWakeup {
    flag: AtomicBool,
    condvar: Condvar,
    mutex: Mutex<()>,
}

impl EngineWakeup {
    pub fn new() -> Self {
        Self {
            flag: AtomicBool::new(false),
            condvar: Condvar::new(),
            mutex: Mutex::new(()),
        }
    }

    /// Signal the engine to wake up.
    pub fn signal(&self) {
        self.flag.store(true, Ordering::Release);
        let _guard = self.mutex.lock().unwrap();
        self.condvar.notify_one();
    }

    /// Wait for a signal (with timeout). Returns true if signaled.
    pub fn wait(&self, timeout: Duration) -> bool {
        let guard = self.mutex.lock().unwrap();
        if self.flag.swap(false, Ordering::AcqRel) {
            return true;
        }
        let (_guard, result) = self.condvar.wait_timeout(guard, timeout).unwrap();
        self.flag.swap(false, Ordering::AcqRel) || !result.timed_out()
    }

    /// Check and clear the signal without blocking.
    pub fn try_recv(&self) -> bool {
        self.flag.swap(false, Ordering::AcqRel)
    }
}

// --- CommandSender ---

/// Wrapper around mpsc::Sender that also signals engine_wakeup on every send.
#[derive(Clone)]
pub struct CommandSender {
    inner: mpsc::Sender<EngineCommand>,
    wakeup: Arc<EngineWakeup>,
}

impl CommandSender {
    pub fn new(sender: mpsc::Sender<EngineCommand>, wakeup: Arc<EngineWakeup>) -> Self {
        Self {
            inner: sender,
            wakeup,
        }
    }

    pub fn send(&self, cmd: EngineCommand) -> Result<(), SendError<EngineCommand>> {
        let result = self.inner.send(cmd);
        self.wakeup.signal();
        result
    }
}

// --- Oneshot channel ---

/// Oneshot sender (no tokio dependency).
pub struct OneshotSender<T> {
    inner: Arc<(Mutex<Option<T>>, Condvar)>,
}

/// Oneshot receiver (no tokio dependency).
pub struct OneshotReceiver<T> {
    inner: Arc<(Mutex<Option<T>>, Condvar)>,
}

/// Create a oneshot channel pair.
pub fn oneshot_channel<T>() -> (OneshotSender<T>, OneshotReceiver<T>) {
    let inner = Arc::new((Mutex::new(None), Condvar::new()));
    (
        OneshotSender {
            inner: inner.clone(),
        },
        OneshotReceiver { inner },
    )
}

impl<T> OneshotSender<T> {
    /// Send a value, waking the receiver.
    pub fn send(self, value: T) {
        let (lock, condvar) = &*self.inner;
        let mut slot = lock.lock().unwrap();
        *slot = Some(value);
        condvar.notify_one();
    }
}

impl<T> OneshotReceiver<T> {
    /// Block until a value is received.
    pub fn recv(self) -> T {
        let (lock, condvar) = &*self.inner;
        let mut slot = lock.lock().unwrap();
        loop {
            if let Some(val) = slot.take() {
                return val;
            }
            slot = condvar.wait(slot).unwrap();
        }
    }

    /// Block until a value is received, with timeout.
    pub fn recv_timeout(self, timeout: Duration) -> Option<T> {
        let (lock, condvar) = &*self.inner;
        let mut slot = lock.lock().unwrap();
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if let Some(val) = slot.take() {
                return Some(val);
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return None;
            }
            let (new_slot, _) = condvar.wait_timeout(slot, remaining).unwrap();
            slot = new_slot;
        }
    }
}

// --- SocketOption ---

/// Keepalive configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeepaliveConfig {
    pub idle: Duration,
    pub interval: Duration,
    pub count: u32,
}

/// Socket options routed through EngineCommand::SetOption.
#[derive(Debug, Clone)]
pub enum SocketOption {
    Nodelay(bool),
    Keepalive(Option<KeepaliveConfig>),
    Linger(Option<Duration>),
    RecvBufSize(usize),
    SendBufSize(usize),
    ReuseAddr(bool),
    Ttl(u8),
    ReadTimeout(Option<Duration>),
    WriteTimeout(Option<Duration>),
    Nonblocking(bool),
}

// --- EngineCommand ---

/// Commands sent from app threads to the engine thread via mpsc channel.
pub enum EngineCommand {
    Connect {
        local: SocketAddr,
        remote: SocketAddr,
        src_mac: [u8; 6],
        dst_mac: [u8; 6],
        handle: Arc<ConnectionHandle>,
        response: OneshotSender<Result<FourTuple, TcpError>>,
    },
    Listen {
        addr: SocketAddr,
        backlog: usize,
        response: OneshotSender<Result<(), TcpError>>,
    },
    Accept {
        listen_addr: SocketAddr,
        response: OneshotSender<Result<(FourTuple, Arc<ConnectionHandle>), TcpError>>,
    },
    Shutdown {
        key: FourTuple,
        how: Shutdown,
    },
    SetOption {
        key: FourTuple,
        option: SocketOption,
    },
    Close {
        key: FourTuple,
        linger: Option<Duration>,
    },
}

// --- AtomicWaker (minimal, no tokio) ---

/// Minimal atomic waker for async integration.
/// Stores a single waker that can be registered and woken.
pub struct AtomicWaker {
    waker: Mutex<Option<std::task::Waker>>,
}

impl AtomicWaker {
    pub fn new() -> Self {
        Self {
            waker: Mutex::new(None),
        }
    }

    /// Register a waker (replaces any previous waker).
    pub fn register(&self, waker: &std::task::Waker) {
        let mut slot = self.waker.lock().unwrap();
        *slot = Some(waker.clone());
    }

    /// Wake the registered waker (if any).
    pub fn wake(&self) {
        if let Some(w) = self.waker.lock().unwrap().take() {
            w.wake();
        }
    }
}

// --- ConnectionHandle ---

/// Shared state between app threads and engine thread (via Arc).
pub struct ConnectionHandle {
    /// Received data: engine writes, app reads.
    pub rx_ring: SpscByteRing,
    /// Send data: app writes, engine reads.
    pub tx_ring: SpscByteRing,

    /// Current TCP state (engine updates with Release).
    pub state: AtomicU8,
    /// Explicit EOF flag — set after final rx bytes enqueued on FIN.
    pub eof: AtomicBool,
    /// Latched connection error — sticky (peek/clone, never take).
    pub error: Mutex<Option<TcpError>>,

    /// Condvar + notify_lock for blocking wake (recheck-under-lock).
    pub condvar: Condvar,
    pub notify_lock: Mutex<()>,

    /// Async wakers for read/write.
    pub read_waker: AtomicWaker,
    pub write_waker: AtomicWaker,

    /// Serialize concurrent (&stream).read() calls.
    pub read_mutex: Mutex<()>,
    /// Serialize concurrent (&stream).write() calls.
    pub write_mutex: Mutex<()>,

    /// Number of live app handles (TcpStream + split halves).
    pub app_refcount: AtomicUsize,
    /// Command sender for Close on last-handle-drop.
    pub cmd_tx: CommandSender,
    /// Connection key.
    pub key: FourTuple,
    /// SO_LINGER setting.
    pub linger: Mutex<Option<Duration>>,
}

impl ConnectionHandle {
    /// Create a new connection handle with default buffer sizes.
    pub fn new(
        rx_capacity: usize,
        tx_capacity: usize,
        cmd_tx: CommandSender,
        key: FourTuple,
    ) -> Self {
        Self {
            rx_ring: SpscByteRing::new(rx_capacity),
            tx_ring: SpscByteRing::new(tx_capacity),
            state: AtomicU8::new(TcpState::Closed as u8),
            eof: AtomicBool::new(false),
            error: Mutex::new(None),
            condvar: Condvar::new(),
            notify_lock: Mutex::new(()),
            read_waker: AtomicWaker::new(),
            write_waker: AtomicWaker::new(),
            read_mutex: Mutex::new(()),
            write_mutex: Mutex::new(()),
            app_refcount: AtomicUsize::new(1),
            cmd_tx,
            key,
            linger: Mutex::new(None),
        }
    }

    /// Get the current TCP state.
    pub fn tcp_state(&self) -> TcpState {
        TcpState::from_u8(self.state.load(Ordering::Acquire)).unwrap_or(TcpState::Closed)
    }

    /// Set the TCP state (called by engine).
    pub fn set_state(&self, state: TcpState) {
        self.state.store(state as u8, Ordering::Release);
    }

    /// Latch a connection error (sticky — never cleared).
    pub fn latch_error(&self, err: TcpError) {
        let mut slot = self.error.lock().unwrap();
        if slot.is_none() {
            *slot = Some(err);
        }
    }

    /// Peek at the latched error (clone, never take).
    pub fn peek_error(&self) -> Option<TcpError> {
        self.error.lock().unwrap().clone()
    }

    /// Notify blocked readers/writers and async wakers.
    pub fn notify_all(&self) {
        let _guard = self.notify_lock.lock().unwrap();
        self.condvar.notify_all();
        self.read_waker.wake();
        self.write_waker.wake();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_handle() -> Arc<ConnectionHandle> {
        let (tx, _rx) = mpsc::channel();
        let wakeup = Arc::new(EngineWakeup::new());
        let cmd_tx = CommandSender::new(tx, wakeup);
        let key = FourTuple {
            local: "10.0.0.1:1234".parse().unwrap(),
            remote: "10.0.0.2:80".parse().unwrap(),
        };
        Arc::new(ConnectionHandle::new(65536, 65536, cmd_tx, key))
    }

    #[test]
    fn handle_initial_state() {
        let h = make_test_handle();
        assert_eq!(h.tcp_state(), TcpState::Closed);
        assert!(!h.eof.load(Ordering::Acquire));
        assert!(h.peek_error().is_none());
        assert_eq!(h.app_refcount.load(Ordering::Acquire), 1);
    }

    #[test]
    fn handle_state_transitions() {
        let h = make_test_handle();
        h.set_state(TcpState::SynSent);
        assert_eq!(h.tcp_state(), TcpState::SynSent);
        h.set_state(TcpState::Established);
        assert_eq!(h.tcp_state(), TcpState::Established);
    }

    #[test]
    fn handle_latch_error_sticky() {
        let h = make_test_handle();
        h.latch_error(TcpError::ConnectionReset);
        assert!(matches!(h.peek_error(), Some(TcpError::ConnectionReset)));
        // Second latch doesn't overwrite
        h.latch_error(TcpError::TimedOut);
        assert!(matches!(h.peek_error(), Some(TcpError::ConnectionReset)));
    }

    #[test]
    fn oneshot_send_recv() {
        let (tx, rx) = oneshot_channel();
        tx.send(42u32);
        assert_eq!(rx.recv(), 42);
    }

    #[test]
    fn oneshot_recv_timeout_success() {
        let (tx, rx) = oneshot_channel();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(10));
            tx.send(99u32);
        });
        let val = rx.recv_timeout(Duration::from_secs(1));
        assert_eq!(val, Some(99));
    }

    #[test]
    fn oneshot_recv_timeout_expires() {
        let (_tx, rx) = oneshot_channel::<u32>();
        let val = rx.recv_timeout(Duration::from_millis(10));
        assert_eq!(val, None);
    }

    #[test]
    fn command_sender_signals_wakeup() {
        let (tx, _rx) = mpsc::channel();
        let wakeup = Arc::new(EngineWakeup::new());
        let cmd_tx = CommandSender::new(tx, wakeup.clone());
        let key = FourTuple {
            local: "10.0.0.1:1234".parse().unwrap(),
            remote: "10.0.0.2:80".parse().unwrap(),
        };
        cmd_tx
            .send(EngineCommand::Close {
                key,
                linger: None,
            })
            .unwrap();
        // Wakeup should have been signaled
        assert!(wakeup.try_recv());
    }

    #[test]
    fn engine_wakeup_signal_and_wait() {
        let wakeup = Arc::new(EngineWakeup::new());
        let w2 = wakeup.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(10));
            w2.signal();
        });
        assert!(wakeup.wait(Duration::from_secs(1)));
    }

    #[test]
    fn engine_wakeup_timeout() {
        let wakeup = EngineWakeup::new();
        assert!(!wakeup.wait(Duration::from_millis(10)));
    }

    #[test]
    fn atomic_waker_register_and_wake() {
        use std::sync::atomic::AtomicBool;
        use std::task::{RawWaker, RawWakerVTable, Waker};

        static WOKEN: AtomicBool = AtomicBool::new(false);

        fn clone_fn(ptr: *const ()) -> RawWaker {
            RawWaker::new(ptr, &VTABLE)
        }
        fn wake_fn(_: *const ()) {
            WOKEN.store(true, Ordering::Release);
        }
        fn wake_by_ref_fn(_: *const ()) {
            WOKEN.store(true, Ordering::Release);
        }
        fn drop_fn(_: *const ()) {}

        static VTABLE: RawWakerVTable =
            RawWakerVTable::new(clone_fn, wake_fn, wake_by_ref_fn, drop_fn);

        let raw = RawWaker::new(std::ptr::null(), &VTABLE);
        let waker = unsafe { Waker::from_raw(raw) };

        let aw = AtomicWaker::new();
        aw.register(&waker);
        WOKEN.store(false, Ordering::Release);
        aw.wake();
        assert!(WOKEN.load(Ordering::Acquire));
    }
}
