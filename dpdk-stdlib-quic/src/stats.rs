//! Provider stats and handle for shutdown/observability.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

/// Atomic counters for provider observability.
pub struct ProviderStats {
    pub rx_burst_calls: AtomicU64,
    pub tx_burst_calls: AtomicU64,
    pub datagrams_received: AtomicU64,
    pub datagrams_transmitted: AtomicU64,
    pub rx_drops: AtomicU64,
    pub tx_drops: AtomicU64,
    pub timer_wakeups: AtomicU64,
}

impl ProviderStats {
    pub fn new() -> Self {
        Self {
            rx_burst_calls: AtomicU64::new(0),
            tx_burst_calls: AtomicU64::new(0),
            datagrams_received: AtomicU64::new(0),
            datagrams_transmitted: AtomicU64::new(0),
            rx_drops: AtomicU64::new(0),
            tx_drops: AtomicU64::new(0),
            timer_wakeups: AtomicU64::new(0),
        }
    }

    pub fn snapshot(&self) -> StatsSnapshot {
        StatsSnapshot {
            rx_burst_calls: self.rx_burst_calls.load(Ordering::Relaxed),
            tx_burst_calls: self.tx_burst_calls.load(Ordering::Relaxed),
            datagrams_received: self.datagrams_received.load(Ordering::Relaxed),
            datagrams_transmitted: self.datagrams_transmitted.load(Ordering::Relaxed),
            rx_drops: self.rx_drops.load(Ordering::Relaxed),
            tx_drops: self.tx_drops.load(Ordering::Relaxed),
            timer_wakeups: self.timer_wakeups.load(Ordering::Relaxed),
        }
    }
}

/// Point-in-time counter snapshot.
#[derive(Debug, Clone)]
pub struct StatsSnapshot {
    pub rx_burst_calls: u64,
    pub tx_burst_calls: u64,
    pub datagrams_received: u64,
    pub datagrams_transmitted: u64,
    pub rx_drops: u64,
    pub tx_drops: u64,
    pub timer_wakeups: u64,
}

/// Shared thread handle that allows the provider's `start()` method
/// to store the spawned thread for the handle's `shutdown()` to join.
pub(crate) type SharedThread = Arc<Mutex<Option<JoinHandle<()>>>>;

/// Handle for controlling and observing a running provider.
pub struct ProviderHandle {
    pub(crate) stats: Arc<ProviderStats>,
    pub(crate) shutdown: Arc<AtomicBool>,
    pub(crate) thread: SharedThread,
}

impl ProviderHandle {
    /// Signal the event loop to stop and wait for the thread to exit.
    pub fn shutdown(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Some(handle) = self.thread.lock().unwrap().take() {
            let _ = handle.join();
        }
    }

    /// Get a point-in-time snapshot of provider counters.
    pub fn stats(&self) -> StatsSnapshot {
        self.stats.snapshot()
    }
}
