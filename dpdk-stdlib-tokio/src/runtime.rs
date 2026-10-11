//! Runtime utilities for DPDK-optimized Tokio execution

use std::future::Future;

/// Configuration for DPDK-aware Tokio runtime
#[derive(Debug, Clone)]
pub struct DpdkRuntimeConfig {
    /// Number of worker threads (default: 1 for DPDK core affinity)
    pub worker_threads: usize,
    /// Thread name prefix
    pub thread_name: String,
    /// Enable IO driver
    pub enable_io: bool,
    /// Enable time driver
    pub enable_time: bool,
}

impl Default for DpdkRuntimeConfig {
    fn default() -> Self {
        Self {
            worker_threads: 1,
            thread_name: "dpdk-tokio-worker".into(),
            enable_io: true,
            enable_time: true,
        }
    }
}

/// Builder for creating a DPDK-optimized Tokio runtime
pub struct DpdkRuntimeBuilder {
    config: DpdkRuntimeConfig,
}

impl DpdkRuntimeBuilder {
    pub fn new() -> Self {
        Self {
            config: DpdkRuntimeConfig::default(),
        }
    }

    /// Set the number of worker threads
    pub fn worker_threads(mut self, threads: usize) -> Self {
        self.config.worker_threads = threads;
        self
    }

    /// Set the thread name prefix
    pub fn thread_name(mut self, name: impl Into<String>) -> Self {
        self.config.thread_name = name.into();
        self
    }

    /// Build the Tokio runtime
    pub fn build(self) -> std::io::Result<tokio::runtime::Runtime> {
        let mut builder = tokio::runtime::Builder::new_multi_thread();

        builder.worker_threads(self.config.worker_threads);
        builder.thread_name(&self.config.thread_name);

        if self.config.enable_io {
            builder.enable_io();
        }
        if self.config.enable_time {
            builder.enable_time();
        }

        builder.build()
    }

    /// Build and run a future on the runtime
    pub fn run<F: Future>(self, future: F) -> F::Output {
        let runtime = self.build().expect("Failed to build runtime");
        runtime.block_on(future)
    }
}

impl Default for DpdkRuntimeBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Run an async function with a DPDK-optimized runtime
///
/// This creates a runtime optimized for DPDK workloads with:
/// - Single worker thread (for CPU affinity with DPDK cores)
/// - IO and time drivers enabled
///
/// # Example
///
/// ```rust,ignore
/// use dpdk_tokio::runtime::run_dpdk_async;
///
/// fn main() {
///     run_dpdk_async(async {
///         let socket = dpdk_tokio::bind_udp("0.0.0.0:9000").await?;
///         // ... use socket
///         Ok::<_, std::io::Error>(())
///     }).unwrap();
/// }
/// ```
pub fn run_dpdk_async<F, T>(future: F) -> T
where
    F: Future<Output = T>,
{
    DpdkRuntimeBuilder::new().run(future)
}

/// Spawn a blocking DPDK operation on the Tokio blocking thread pool
///
/// Use this for operations that may block, like DPDK packet receive
/// with polling.
pub async fn spawn_dpdk_blocking<F, T>(f: F) -> std::io::Result<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
}
