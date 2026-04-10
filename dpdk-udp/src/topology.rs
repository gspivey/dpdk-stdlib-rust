//! Multi-core topology planning for DPDK pipeline stages.
//!
//! Determines how many RSS RX queues to allocate based on:
//! 1. Explicit builder configuration (highest priority)
//! 2. Environment variable (`DPDK_RX_QUEUES`)
//! 3. Auto-detection from available lcores and NIC capabilities
//!
//! The single knob is `rx_queues`: each queue gets exactly one processing
//! thread. When `rx_queues <= 1`, the socket runs in single-core
//! run-to-completion mode (no pipeline threads spawned).
//!
//! Under stubs (`dpdk_sys::is_stub()`), the topology always collapses to
//! single-core run-to-completion — no threads are spawned.
//!
//! ## Optimizations
//!
//! - **FramePool slab allocator**: Zero-copy frame passing via pre-allocated
//!   slab pool. Frames are passed by `FrameRef` (index + length) through SPSC
//!   rings, eliminating per-packet heap allocation.
//! - **Per-queue SPSC app rings**: Each queue thread has its own SPSC app ring.
//!   `recv_from()` polls round-robin, eliminating CAS contention.
//! - **Direct TX**: App thread sends directly via its own TX queue,
//!   bypassing the tx_ring → RX lcore hop.

use std::env;
use std::fmt;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::arp::{self, ArpCache, ArpHandler};
use crate::frame_pool::{AppPacket, FramePool, FrameRef};
use crate::icmp::{self, IcmpHandler};
use crate::perf::PerfCounters;
use crate::ring::{MpscRing, SpscRing};
use crate::{parse_udp_packet_ref, perf_inc, ETH_HEADER_LEN, ETH_TYPE_IPV4};

// ============================================================================
// TopologyConfig — input from builder / env / auto
// ============================================================================

/// User-provided topology hints (from `UdpSocketBuilder` or defaults).
#[derive(Debug, Clone, Default)]
pub struct TopologyConfig {
    /// Explicit RX queue count (from builder API).
    pub rx_queues: Option<u16>,
}

// ============================================================================
// TopologyPlan — output of detect_topology()
// ============================================================================

/// The resolved multi-core topology plan.
///
/// When `rx_queues <= 1`, this is run-to-completion mode (single-threaded,
/// no pipeline overhead). When `rx_queues > 1`, a pipeline is spawned with
/// `rx_queues` threads total (1 RX dispatcher + rx_queues-1 queue workers).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopologyPlan {
    /// Number of RSS queues / pipeline threads.
    /// `rx_queues <= 1` → run-to-completion, `rx_queues > 1` → pipeline.
    pub rx_queues: u16,
    /// Number of NIC TX queues provisioned.
    /// Queue 0 = RX lcore (ARP/ICMP), queue 1 = app thread direct TX.
    pub nb_tx_queues: u16,
    /// NUMA node ID (0-based). Used for memory allocation affinity.
    pub numa_node: u32,
    /// How the plan was determined.
    pub source: TopologySource,
}

/// How the topology plan was determined, for diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TopologySource {
    /// Explicit values from the builder API.
    Builder,
    /// Values from environment variables.
    Environment,
    /// Auto-detected from available lcores and NIC capabilities.
    AutoDetected,
    /// Stub mode — forced to single-core run-to-completion.
    Stub,
}

impl TopologyPlan {
    /// Returns true if this is a single-core run-to-completion plan
    /// (no pipeline threads needed).
    pub fn is_run_to_completion(&self) -> bool {
        self.rx_queues <= 1
    }

    /// Total number of pipeline threads (excluding the app thread).
    /// When `rx_queues > 1`: 1 RX dispatcher + (rx_queues - 1) queue workers.
    pub fn total_lcores_needed(&self) -> usize {
        if self.is_run_to_completion() {
            0
        } else {
            self.rx_queues as usize
        }
    }
}

impl fmt::Display for TopologyPlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_run_to_completion() {
            write!(f, "run-to-completion (single core, NUMA {})", self.numa_node)
        } else {
            write!(
                f,
                "{} RSS queues, {} TX queues ({} lcores, NUMA {}, {:?})",
                self.rx_queues,
                self.nb_tx_queues,
                self.total_lcores_needed(),
                self.numa_node,
                self.source,
            )
        }
    }
}

// ============================================================================
// Runtime Topology Types — Phase 3
// ============================================================================

/// A processed packet ready for the application's `recv_from()`.
#[derive(Debug, Clone)]
pub struct ProcessedPacket {
    /// UDP payload data.
    pub payload: Vec<u8>,
    /// Source address of the packet.
    pub src_addr: SocketAddr,
    /// Source MAC address (for ARP cache learning).
    pub src_mac: [u8; 6],
    /// Source IP (for ARP cache learning).
    pub src_ip: Ipv4Addr,
}

/// A raw Ethernet frame for transmission, enqueued by `send_to()`.
#[derive(Debug, Clone)]
pub struct TxFrame {
    /// Complete Ethernet frame bytes.
    pub frame: Vec<u8>,
}

/// The live multi-core topology: RX dispatcher, queue workers, and shared rings.
///
/// - `app_rings`: Per-queue SPSC rings carrying `AppPacket` (zero-copy FrameRef)
/// - `tx_ring`: Fallback TX path (unused when `direct_send_fn` is set)
/// - `direct_send_fn`: App thread sends on its own TX queue, bypassing tx_ring
pub struct MultiCoreTopology {
    /// Per-queue SPSC app rings: each queue worker enqueues `AppPacket` (zero-copy),
    /// `recv_from()` polls round-robin across all queue app rings.
    pub app_rings: Vec<Arc<SpscRing<AppPacket>>>,

    /// Legacy MPSC app_ring — kept for API compat.
    pub app_ring: Arc<MpscRing<ProcessedPacket>>,

    /// TX ring for outbound frames (fallback; unused when `direct_send_fn` is set).
    pub tx_ring: Arc<SpscRing<TxFrame>>,

    /// Direct send function for the application thread.
    /// When set, `send_to()` calls this instead of enqueuing to `tx_ring`,
    /// sending on TX queue 1 to avoid contention with the RX dispatcher's queue 0.
    pub direct_send_fn: Option<Arc<dyn Fn(&[u8]) -> io::Result<usize> + Send + Sync>>,

    /// Shutdown signal — set to `true` to stop all pipeline threads.
    pub shutdown: Arc<AtomicBool>,

    /// Handles for all spawned pipeline threads (RX + worker).
    handles: Vec<JoinHandle<()>>,

    /// The topology plan this was built from (for diagnostics).
    pub plan: TopologyPlan,

    /// Number of queue workers (for round-robin polling in recv_from).
    pub num_workers: usize,

    /// Shared frame pool — `recv_from()` reads payload from here, then frees.
    pub frame_pool: Arc<FramePool>,
}

impl MultiCoreTopology {
    /// Signal all pipeline threads to stop and wait for them to finish.
    pub fn shutdown(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        for handle in self.handles.drain(..) {
            let _ = handle.join();
        }
    }

    /// Dequeue an AppPacket from per-queue app rings (round-robin).
    ///
    /// The returned `AppPacket` holds a `FrameRef` into the shared `FramePool`.
    /// The caller MUST free the frame via `frame_pool.free(pkt.frame_ref.pool_idx)`
    /// after copying the payload.
    pub fn dequeue_app(&self, rr_index: &mut usize) -> Option<AppPacket> {
        let n = self.app_rings.len();
        if n == 0 {
            return None;
        }
        for offset in 0..*rr_index + n {
            let idx = offset % n;
            if let Some(pkt) = self.app_rings[idx].dequeue() {
                *rr_index = idx + 1;
                return Some(pkt);
            }
        }
        None
    }
}

impl Drop for MultiCoreTopology {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Configuration for building a live `MultiCoreTopology`.
pub struct PipelineConfig {
    /// The topology plan to realize.
    pub plan: TopologyPlan,
    /// Local port number to filter incoming UDP packets.
    pub local_port: u16,
    /// Local MAC address.
    pub local_mac: [u8; 6],
    /// Local IP address.
    pub local_ip: Ipv4Addr,
    /// Shared ARP cache for MAC learning.
    pub arp_cache: Arc<ArpCache>,
    /// Shared performance counters.
    pub perf_counters: Arc<PerfCounters>,
}

/// Build and start the multi-core pipeline.
///
/// Returns `None` if the plan is run-to-completion (no threads needed).
/// Returns `Some(MultiCoreTopology)` with running pipeline threads otherwise.
///
/// Phase 3 pipeline architecture:
/// ```text
/// NIC → recv_fn → rx_loop → FramePool alloc → SPSC[FrameRef] → worker_loop
///                                                                   ├─ parse UDP
///                                                                   ├─ enqueue to per-worker app_ring (SPSC)
///                                                                   └─ worker-direct TX (send_fn)
///
/// recv_from() ← polls app_rings[0..N] round-robin
/// send_to()   → tx_ring → rx_loop drains → send_fn
/// ```
pub fn start_pipeline<R, S>(
    config: PipelineConfig,
    recv_fn: R,
    send_fn: S,
    direct_send_fn: Option<Arc<dyn Fn(&[u8]) -> io::Result<usize> + Send + Sync>>,
) -> Option<MultiCoreTopology>
where
    R: Fn(usize, &FramePool) -> io::Result<Vec<FrameRef>> + Send + Sync + 'static,
    S: Fn(&[u8]) -> io::Result<usize> + Send + Sync + 'static,
{
    if config.plan.is_run_to_completion() {
        return None;
    }

    let shutdown = Arc::new(AtomicBool::new(false));
    // rx_queues total threads: 1 RX dispatcher + (rx_queues - 1) queue workers
    let num_workers = (config.plan.rx_queues as usize).saturating_sub(1);

    // Ring sizes: 16384 slots to absorb bursts without TX backpressure.
    let ring_capacity = 16384;

    // Frame pool: pre-allocated slab for zero-copy frame passing
    let frame_pool = Arc::new(FramePool::new(ring_capacity, 2048));

    // Per-queue SPSC app rings: queue worker → recv_from()
    let app_rings: Vec<Arc<SpscRing<AppPacket>>> = (0..num_workers)
        .map(|_| Arc::new(SpscRing::new(ring_capacity)))
        .collect();

    // Legacy MPSC app_ring — unused by Phase 3 workers but kept for API compat
    let app_ring = Arc::new(MpscRing::new(2));

    // TX ring: send_to() → RX lcore → NIC. SPSC.
    let tx_ring = Arc::new(SpscRing::new(ring_capacity));

    let mut handles = Vec::new();
    let recv_fn = Arc::new(recv_fn);
    let send_fn = Arc::new(send_fn);

    // Queue SPSC rings (RX dispatcher → queue workers): carry FrameRef
    let worker_rings: Vec<Arc<SpscRing<FrameRef>>> = (0..num_workers)
        .map(|_| Arc::new(SpscRing::new(ring_capacity)))
        .collect();

    // Spawn queue worker threads.
    for (w_idx, w_ring) in worker_rings.iter().enumerate() {
        let w_ring = Arc::clone(w_ring);
        let app_ring_w = Arc::clone(&app_rings[w_idx]);
        let shutdown = Arc::clone(&shutdown);
        let local_port = config.local_port;
        let arp_cache = Arc::clone(&config.arp_cache);
        let perf_counters = Arc::clone(&config.perf_counters);
        let frame_pool = Arc::clone(&frame_pool);

        let handle = thread::Builder::new()
            .name(format!("dpdk-queue-{}", w_idx))
            .spawn(move || {
                worker_loop(
                    w_ring,
                    app_ring_w,
                    shutdown,
                    local_port,
                    arp_cache,
                    perf_counters,
                    frame_pool,
                );
            })
            .expect("failed to spawn worker thread");
        handles.push(handle);
    }

    // Spawn single RX lcore thread.
    let rx_ready = Arc::new(AtomicBool::new(false));
    {
        let worker_rings_clone: Vec<Arc<SpscRing<FrameRef>>> =
            worker_rings.iter().map(Arc::clone).collect();
        let tx_ring = Arc::clone(&tx_ring);
        let shutdown = Arc::clone(&shutdown);
        let recv_fn = Arc::clone(&recv_fn);
        let send_fn = Arc::clone(&send_fn);
        let local_mac = config.local_mac;
        let local_ip = config.local_ip;
        let arp_cache = Arc::clone(&config.arp_cache);
        let perf_counters = Arc::clone(&config.perf_counters);
        let rx_ready_clone = Arc::clone(&rx_ready);
        let frame_pool = Arc::clone(&frame_pool);

        let handle = thread::Builder::new()
            .name("dpdk-rx-0".to_string())
            .spawn(move || {
                rx_loop(
                    recv_fn,
                    send_fn,
                    worker_rings_clone,
                    tx_ring,
                    shutdown,
                    local_mac,
                    local_ip,
                    arp_cache,
                    perf_counters,
                    rx_ready_clone,
                    frame_pool,
                );
            })
            .expect("failed to spawn RX thread");
        handles.push(handle);
    }

    // Wait for the RX thread to signal ready
    while !rx_ready.load(Ordering::Acquire) {
        std::hint::spin_loop();
    }

    Some(MultiCoreTopology {
        app_rings,
        app_ring,
        tx_ring,
        direct_send_fn,
        shutdown,
        handles,
        plan: config.plan,
        num_workers,
        frame_pool,
    })
}

// ============================================================================
// Adaptive Polling — spin → yield → sleep backoff
// ============================================================================

/// Number of iterations to spin (cheapest, ~3 us total on modern CPUs).
const SPIN_ITERS: u32 = 64;
/// Number of iterations to yield after spinning (gives OS scheduler a chance).
const YIELD_ITERS: u32 = 16;
/// Sleep duration after spin + yield phases are exhausted.
const SLEEP_US: u64 = 1;

/// Three-phase adaptive wait to avoid burning CPU when no work is available.
///
/// Phase 1: `spin_loop()` for up to `SPIN_ITERS` — minimal latency for bursty traffic.
/// Phase 2: `yield_now()` for up to `YIELD_ITERS` — allows other threads to run.
/// Phase 3: `sleep(1us)` — prevents CPU waste on idle sockets.
///
/// Caller resets `empty_polls` to 0 when work is found.
#[inline]
fn adaptive_wait(empty_polls: &mut u32) {
    *empty_polls += 1;
    if *empty_polls <= SPIN_ITERS {
        std::hint::spin_loop();
    } else if *empty_polls <= SPIN_ITERS + YIELD_ITERS {
        std::thread::yield_now();
    } else {
        std::thread::sleep(Duration::from_micros(SLEEP_US));
    }
}

/// RX dispatcher main loop.
///
/// Polls the backend for frames, handles ARP/ICMP inline, and distributes
/// data frames to queue worker SPSC rings round-robin.
/// Frames are allocated from the FramePool and passed by FrameRef.
/// Also drains the TX ring and transmits outbound frames.
fn rx_loop<R, S>(
    recv_fn: Arc<R>,
    send_fn: Arc<S>,
    worker_rings: Vec<Arc<SpscRing<FrameRef>>>,
    tx_ring: Arc<SpscRing<TxFrame>>,
    shutdown: Arc<AtomicBool>,
    local_mac: [u8; 6],
    local_ip: Ipv4Addr,
    arp_cache: Arc<ArpCache>,
    perf_counters: Arc<PerfCounters>,
    rx_ready: Arc<AtomicBool>,
    frame_pool: Arc<FramePool>,
) where
    R: Fn(usize, &FramePool) -> io::Result<Vec<FrameRef>>,
    S: Fn(&[u8]) -> io::Result<usize>,
{
    let arp_handler = ArpHandler::with_cache(local_mac, local_ip, Arc::clone(&arp_cache));
    let icmp_handler = IcmpHandler::new(local_mac, local_ip);
    let num_workers = worker_rings.len();
    let mut rr_index: usize = 0;
    let mut empty_polls: u32 = 0;

    // Signal that the RX thread is ready to drain the TX ring.
    // start_pipeline() spins on this before returning.
    rx_ready.store(true, Ordering::Release);

    while !shutdown.load(Ordering::Acquire) {
        // 1. Drain TX ring → send to NIC (up to 256 frames per cycle to keep up with echo workloads)
        let tx_batch = tx_ring.dequeue_batch(256);
        for tx in &tx_batch {
            let _ = send_fn(&tx.frame);
        }

        // 2. Poll NIC for incoming frames — recv_fn writes directly into FramePool,
        //    eliminating the intermediate Vec<u8> allocation and double-copy.
        let frame_refs = match recv_fn(32, &frame_pool) {
            Ok(f) => f,
            Err(_) => {
                adaptive_wait(&mut empty_polls);
                continue;
            }
        };

        if frame_refs.is_empty() && tx_batch.is_empty() {
            adaptive_wait(&mut empty_polls);
            continue;
        }

        // Work found — reset backoff
        empty_polls = 0;

        if !frame_refs.is_empty() {
            perf_inc!(perf_counters.rx_bursts);
            perf_inc!(perf_counters.rx_burst_sum, frame_refs.len() as u64);
        }

        for frame_ref in frame_refs {
            // Read frame data from pool (zero-copy read for header inspection)
            let frame_data = unsafe { frame_pool.frame(frame_ref.pool_idx) };
            let frame_len = frame_ref.len as usize;
            if frame_len < 14 {
                frame_pool.free(frame_ref.pool_idx);
                continue;
            }
            let frame_slice = &frame_data[..frame_len];

            let ethertype = u16::from_be_bytes([frame_slice[12], frame_slice[13]]);

            // Handle ARP inline on RX core — free frame after handling
            if ethertype == arp::ETH_TYPE_ARP {
                if let Some(reply) = arp_handler.process_arp(frame_slice) {
                    let _ = send_fn(&reply);
                }
                frame_pool.free(frame_ref.pool_idx);
                perf_inc!(perf_counters.rx_arp_handled);
                continue;
            }

            // Handle ICMP inline on RX core — free frame after handling
            if ethertype == ETH_TYPE_IPV4 && frame_len > ETH_HEADER_LEN + 9 {
                let protocol = frame_slice[ETH_HEADER_LEN + 9];
                if protocol == icmp::IP_PROTO_ICMP {
                    if let Some(reply) = icmp_handler.process_icmp(frame_slice) {
                        let _ = send_fn(&reply);
                    }
                    frame_pool.free(frame_ref.pool_idx);
                    perf_inc!(perf_counters.rx_icmp_handled);
                    continue;
                }
            }

            // Data frame: enqueue FrameRef to worker (already in pool, no copy needed)
            let target = rr_index % num_workers;
            rr_index = rr_index.wrapping_add(1);

            let mut sent = false;
            for offset in 0..num_workers {
                let idx = (target + offset) % num_workers;
                if worker_rings[idx].enqueue(frame_ref).is_ok() {
                    sent = true;
                    break;
                }
            }
            if !sent {
                frame_pool.free(frame_ref.pool_idx);
                perf_inc!(perf_counters.rx_drops_ring_full);
                perf_inc!(perf_counters.worker_ring_enqueue_fail);
            }
        }

        // 3. Second TX drain pass — workers may have enqueued replies while we were
        //    processing RX frames. Draining here cuts echo latency in half by not
        //    waiting for the next loop iteration.
        let tx_batch2 = tx_ring.dequeue_batch(256);
        for tx in &tx_batch2 {
            let _ = send_fn(&tx.frame);
        }
    }
}

/// Queue worker main loop.
///
/// Dequeues FrameRefs from the SPSC ring, parses UDP headers in-place,
/// and enqueues `AppPacket` (carrying FrameRef + payload offset) to its
/// per-queue SPSC app ring. The frame is NOT freed here — `recv_from()`
/// reads the payload directly from the pool and frees it afterward.
fn worker_loop(
    rx_ring: Arc<SpscRing<FrameRef>>,
    app_ring: Arc<SpscRing<AppPacket>>,
    shutdown: Arc<AtomicBool>,
    local_port: u16,
    arp_cache: Arc<ArpCache>,
    perf_counters: Arc<PerfCounters>,
    frame_pool: Arc<FramePool>,
) {
    let mut empty_polls: u32 = 0;

    while !shutdown.load(Ordering::Acquire) {
        let batch = rx_ring.dequeue_batch(32);
        if batch.is_empty() {
            perf_inc!(perf_counters.worker_idle_polls);
            adaptive_wait(&mut empty_polls);
            continue;
        }

        // Work found — reset backoff
        empty_polls = 0;

        for frame_ref in batch {
            // SAFETY: frame_ref was allocated by rx_loop from the same pool.
            // We hold exclusive access until recv_from() frees it.
            let frame_data = unsafe { frame_pool.frame(frame_ref.pool_idx) };
            let frame_len = frame_ref.len as usize;
            let frame_slice = &frame_data[..frame_len];

            // Parse UDP packet headers in-place (zero-copy — borrows payload)
            if let Some(parsed) = parse_udp_packet_ref(frame_slice) {
                // Validate RX checksums before accepting the packet
                if !crate::verify_ipv4_checksum(frame_slice)
                    || !crate::verify_udp_checksum(frame_slice)
                {
                    perf_inc!(perf_counters.rx_drops_parse_fail);
                    frame_pool.free(frame_ref.pool_idx);
                    continue;
                }

                perf_inc!(perf_counters.worker_packets_processed);
                perf_inc!(perf_counters.rx_packets);
                perf_inc!(perf_counters.rx_bytes, parsed.payload.len() as u64);

                // Learn source MAC from incoming packets (fast-path: skip
                // RwLock write if the same IP→MAC is already cached)
                if frame_slice.len() >= 12 {
                    let src_mac: [u8; 6] = frame_slice[6..12].try_into().unwrap();
                    arp_cache.insert_if_changed(parsed.src_ip, &src_mac);
                }

                // Filter by destination port
                if parsed.dst_port == local_port {
                    let src_mac: [u8; 6] = if frame_slice.len() >= 12 {
                        frame_slice[6..12].try_into().unwrap()
                    } else {
                        [0; 6]
                    };

                    // Compute payload offset within the frame
                    let payload_offset = parsed.payload.as_ptr() as usize
                        - frame_slice.as_ptr() as usize;

                    let app_pkt = AppPacket {
                        frame_ref,
                        payload_offset: payload_offset as u16,
                        payload_len: parsed.payload.len() as u16,
                        src_addr: SocketAddr::V4(SocketAddrV4::new(
                            parsed.src_ip,
                            parsed.src_port,
                        )),
                        src_mac,
                        src_ip: parsed.src_ip,
                    };

                    // P3.4: Enqueue to per-worker SPSC app ring (no CAS contention)
                    // Frame stays alive in pool until recv_from() frees it.
                    if app_ring.enqueue(app_pkt).is_err() {
                        // Ring full — must free the frame since recv_from won't see it
                        frame_pool.free(frame_ref.pool_idx);
                        perf_inc!(perf_counters.app_ring_enqueue_fail);
                    }
                } else {
                    // Not our port — free the frame
                    frame_pool.free(frame_ref.pool_idx);
                }
            } else {
                perf_inc!(perf_counters.rx_drops_parse_fail);
                // Parse failed — free the frame
                frame_pool.free(frame_ref.pool_idx);
            }
        }
    }
}

// ============================================================================
// detect_topology() — the main entry point
// ============================================================================

/// Detect the optimal multi-core topology.
///
/// The single knob is `rx_queues`: each queue gets 1 processing thread.
/// `rx_queues <= 1` → run-to-completion (no pipeline threads).
///
/// Configuration precedence: builder API > environment variable > auto-detection.
///
/// Under stubs, always returns a run-to-completion plan regardless of config.
pub fn detect_topology(
    config: &TopologyConfig,
    available_lcores: u32,
    nic_max_rx_queues: u16,
    nic_max_tx_queues: u16,
    nic_numa_node: i32,
) -> TopologyPlan {
    let numa_node = if nic_numa_node >= 0 {
        nic_numa_node as u32
    } else {
        0 // SOCKET_ID_ANY (-1) → default to node 0
    };

    // Under stubs, always run-to-completion
    if dpdk_sys::is_stub() {
        return TopologyPlan {
            rx_queues: 1,
            nb_tx_queues: 1,
            numa_node,
            source: TopologySource::Stub,
        };
    }

    // Try builder config first
    if let Some(rq) = config.rx_queues {
        let rx_queues = clamp_rx_queues(rq, nic_max_rx_queues);
        return TopologyPlan {
            rx_queues,
            nb_tx_queues: compute_tx_queues(rx_queues, nic_max_tx_queues),
            numa_node,
            source: TopologySource::Builder,
        };
    }

    // Try environment variable
    let env_rq = env::var("DPDK_RX_QUEUES").ok().and_then(|v| v.parse::<u16>().ok());

    if let Some(rq) = env_rq {
        let rx_queues = clamp_rx_queues(rq, nic_max_rx_queues);
        return TopologyPlan {
            rx_queues,
            nb_tx_queues: compute_tx_queues(rx_queues, nic_max_tx_queues),
            numa_node,
            source: TopologySource::Environment,
        };
    }

    // Full auto-detection
    let rx_queues = auto_detect_queues(available_lcores, nic_max_rx_queues);

    TopologyPlan {
        rx_queues,
        nb_tx_queues: compute_tx_queues(rx_queues, nic_max_tx_queues),
        numa_node,
        source: TopologySource::AutoDetected,
    }
}

// ============================================================================
// Auto-detection helpers
// ============================================================================

/// Auto-detect the number of RSS queues based on available lcores and NIC caps.
///
/// Reserves 1 lcore for the application thread. Remaining lcores become
/// pipeline threads (1 per RSS queue). When only 0-1 lcores remain,
/// returns 1 (run-to-completion — no pipeline threads spawned).
fn auto_detect_queues(lcores: u32, nic_max: u16) -> u16 {
    let available = lcores.saturating_sub(1); // reserve 1 for app thread
    if available <= 1 {
        return 1; // run-to-completion
    }
    clamp_rx_queues(available as u16, nic_max)
}

/// Ensure rx_queues doesn't exceed NIC maximum.
fn clamp_rx_queues(requested: u16, nic_max: u16) -> u16 {
    if nic_max == 0 {
        return 1; // fallback for unknown NICs
    }
    requested.min(nic_max).max(1)
}

/// Compute the number of TX queues to provision.
///
/// RTC (rx_queues <= 1): 1 TX queue (app thread does everything inline).
/// Pipeline (rx_queues > 1): 2 TX queues — queue 0 for RX dispatcher
/// (ARP/ICMP replies), queue 1 for app thread direct TX.
fn compute_tx_queues(rx_queues: u16, nic_max_tx_queues: u16) -> u16 {
    if rx_queues <= 1 {
        return 1; // RTC mode: single TX queue
    }
    // Pipeline mode: queue 0 for RX dispatcher + queue 1 for app thread
    let desired = 2u16;
    if nic_max_tx_queues == 0 {
        return desired; // unknown NIC, assume capable
    }
    desired.min(nic_max_tx_queues)
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::perf::PerfCounters;
    use std::sync::Mutex;

    #[test]
    fn stub_always_run_to_completion() {
        // Under stubs, regardless of config, we get run-to-completion
        let config = TopologyConfig {
            rx_queues: Some(4),
        };
        let plan = detect_topology(&config, 16, 16, 16, 0);
        assert!(plan.is_run_to_completion());
        assert_eq!(plan.source, TopologySource::Stub);
        assert_eq!(plan.rx_queues, 1);
        assert_eq!(plan.nb_tx_queues, 1);
    }

    #[test]
    fn auto_detect_2_vcpu() {
        // 2 vCPUs: 1 reserved for app, 1 available → run-to-completion
        let queues = auto_detect_queues(2, 16);
        assert_eq!(queues, 1);
    }

    #[test]
    fn auto_detect_4_vcpu() {
        // 4 vCPUs: 1 reserved for app, 3 available → 3 RSS queues
        let queues = auto_detect_queues(4, 16);
        assert_eq!(queues, 3);
    }

    #[test]
    fn auto_detect_16_vcpu() {
        // 16 vCPUs: 1 reserved for app, 15 available → 15 RSS queues
        let queues = auto_detect_queues(16, 16);
        assert_eq!(queues, 15);
    }

    #[test]
    fn auto_detect_32_vcpu() {
        // 32 vCPUs: 1 reserved for app, 31 available → clamped to NIC max 16
        let queues = auto_detect_queues(32, 16);
        assert_eq!(queues, 16);
    }

    #[test]
    fn auto_detect_respects_nic_max() {
        let queues = auto_detect_queues(16, 4);
        assert_eq!(queues, 4); // clamped to NIC max of 4
    }

    #[test]
    fn clamp_never_zero() {
        assert_eq!(clamp_rx_queues(0, 16), 1);
        assert_eq!(clamp_rx_queues(5, 0), 1); // unknown NIC
    }

    #[test]
    fn compute_tx_queues_rtc() {
        // RTC mode (rx_queues=1) → 1 TX queue
        assert_eq!(compute_tx_queues(1, 16), 1);
    }

    #[test]
    fn compute_tx_queues_pipeline() {
        // Pipeline mode → 2 TX queues (RX dispatcher + app thread)
        assert_eq!(compute_tx_queues(2, 16), 2);
        assert_eq!(compute_tx_queues(4, 16), 2);
    }

    #[test]
    fn compute_tx_queues_clamped() {
        // NIC only supports 1 TX queue → clamped to 1
        assert_eq!(compute_tx_queues(2, 1), 1);
        // Unknown NIC (max=0) → assume capable
        assert_eq!(compute_tx_queues(2, 0), 2);
    }

    #[test]
    fn total_lcores_run_to_completion() {
        let plan = TopologyPlan {
            rx_queues: 1,
            nb_tx_queues: 1,
            numa_node: 0,
            source: TopologySource::AutoDetected,
        };
        assert!(plan.is_run_to_completion());
        assert_eq!(plan.total_lcores_needed(), 0);
    }

    #[test]
    fn total_lcores_pipeline() {
        let plan = TopologyPlan {
            rx_queues: 4,
            nb_tx_queues: 2,
            numa_node: 0,
            source: TopologySource::AutoDetected,
        };
        assert!(!plan.is_run_to_completion());
        assert_eq!(plan.total_lcores_needed(), 4);
    }

    #[test]
    fn display_run_to_completion() {
        let plan = TopologyPlan {
            rx_queues: 1,
            nb_tx_queues: 1,
            numa_node: 0,
            source: TopologySource::Stub,
        };
        let s = format!("{plan}");
        assert!(s.contains("run-to-completion"));
    }

    #[test]
    fn display_pipeline() {
        let plan = TopologyPlan {
            rx_queues: 4,
            nb_tx_queues: 2,
            numa_node: 1,
            source: TopologySource::Builder,
        };
        let s = format!("{plan}");
        assert!(s.contains("4 RSS queues"));
        assert!(s.contains("2 TX queues"));
    }

    #[test]
    fn stub_propagates_nic_numa_node() {
        let config = TopologyConfig::default();
        let plan = detect_topology(&config, 8, 16, 16, 1);
        // Even under stubs (run-to-completion), the NIC's NUMA node is recorded
        assert_eq!(plan.numa_node, 1);
        assert!(plan.is_run_to_completion());
    }

    #[test]
    fn negative_numa_defaults_to_zero() {
        let config = TopologyConfig::default();
        // SOCKET_ID_ANY is -1
        let plan = detect_topology(&config, 8, 16, 16, -1);
        assert_eq!(plan.numa_node, 0);
    }

    // ========================================================================
    // Phase 3: Pipeline tests with FramePool
    // ========================================================================

    #[test]
    fn pipeline_processes_packets() {
        // Simulate a pipeline: feed frames into recv_fn, read from app_rings.
        // Uses mock recv/send functions instead of real DPDK.

        let frames: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
        let sent: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));

        // Create a valid UDP frame for port 9000
        let frame = crate::build_udp_frame(
            &[0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0x01],
            &[0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0x02],
            Ipv4Addr::new(10, 0, 0, 1),
            Ipv4Addr::new(10, 0, 0, 2),
            12345,
            9000,
            b"hello pipeline",
            64,
        ).unwrap();

        // Load frames into the mock
        frames.lock().unwrap().push(frame);

        let frames_clone: Arc<Mutex<Vec<Vec<u8>>>> = Arc::clone(&frames);
        let recv_fn = move |_max: usize, pool: &FramePool| -> io::Result<Vec<FrameRef>> {
            let mut f = frames_clone.lock().unwrap();
            let mut refs = Vec::new();
            for frame in f.drain(..) {
                if let Some(fref) = pool.alloc_copy(&frame) {
                    refs.push(fref);
                }
            }
            Ok(refs)
        };

        let sent_clone: Arc<Mutex<Vec<Vec<u8>>>> = Arc::clone(&sent);
        let send_fn = move |frame: &[u8]| -> io::Result<usize> {
            sent_clone.lock().unwrap().push(frame.to_vec());
            Ok(frame.len())
        };

        // rx_queues=3 → 1 RX dispatcher + 2 queue workers
        let plan = TopologyPlan {
            rx_queues: 3,
            nb_tx_queues: 2,
            numa_node: 0,
            source: TopologySource::Builder,
        };

        let config = PipelineConfig {
            plan,
            local_port: 9000,
            local_mac: [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0x02],
            local_ip: Ipv4Addr::new(10, 0, 0, 2),
            arp_cache: Arc::new(crate::ArpCache::new()),
            perf_counters: Arc::new(PerfCounters::new()),
        };

        let mut topo = start_pipeline(config, recv_fn, send_fn, None)
            .expect("should create pipeline for non-RTC plan");

        // Wait for the packet to flow through the pipeline
        let mut received = None;
        let mut rr = 0;
        for _ in 0..200 {
            if let Some(pkt) = topo.dequeue_app(&mut rr) {
                received = Some(pkt);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        let pkt = received.expect("should have received a packet through the pipeline");

        // Read payload from pool via AppPacket (zero-copy verification)
        let payload = unsafe {
            let frame_data = topo.frame_pool.frame(pkt.frame_ref.pool_idx);
            let start = pkt.payload_offset as usize;
            let end = start + pkt.payload_len as usize;
            &frame_data[start..end]
        };
        assert_eq!(payload, b"hello pipeline");
        assert_eq!(
            pkt.src_addr,
            SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 1), 12345))
        );

        // Free the frame and shut down
        topo.frame_pool.free(pkt.frame_ref.pool_idx);
        topo.shutdown();
    }

    #[test]
    fn pipeline_returns_none_for_rtc() {
        // run-to-completion plan should return None (no pipeline threads)
        let plan = TopologyPlan {
            rx_queues: 1,
            nb_tx_queues: 1,
            numa_node: 0,
            source: TopologySource::Builder,
        };

        let config = PipelineConfig {
            plan,
            local_port: 9000,
            local_mac: [0; 6],
            local_ip: Ipv4Addr::UNSPECIFIED,
            arp_cache: Arc::new(crate::ArpCache::new()),
            perf_counters: Arc::new(PerfCounters::new()),
        };

        let recv_fn = |_: usize, _pool: &FramePool| -> io::Result<Vec<FrameRef>> { Ok(vec![]) };
        let send_fn = |_: &[u8]| -> io::Result<usize> { Ok(0) };

        let topo = start_pipeline(config, recv_fn, send_fn, None);
        assert!(topo.is_none(), "RTC plan should not start a pipeline");
    }

    #[test]
    fn pipeline_tx_ring_drains_to_send() {
        // Verify that frames enqueued to tx_ring get sent via the send_fn
        let sent: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));

        let recv_fn = |_: usize, _pool: &FramePool| -> io::Result<Vec<FrameRef>> { Ok(vec![]) };

        let sent_clone: Arc<Mutex<Vec<Vec<u8>>>> = Arc::clone(&sent);
        let send_fn = move |frame: &[u8]| -> io::Result<usize> {
            sent_clone.lock().unwrap().push(frame.to_vec());
            Ok(frame.len())
        };

        // rx_queues=2 → 1 RX dispatcher + 1 queue worker
        let plan = TopologyPlan {
            rx_queues: 2,
            nb_tx_queues: 2,
            numa_node: 0,
            source: TopologySource::Builder,
        };

        let config = PipelineConfig {
            plan,
            local_port: 9000,
            local_mac: [0; 6],
            local_ip: Ipv4Addr::UNSPECIFIED,
            arp_cache: Arc::new(crate::ArpCache::new()),
            perf_counters: Arc::new(PerfCounters::new()),
        };

        let mut topo = start_pipeline(config, recv_fn, send_fn, None)
            .expect("should create pipeline");

        // Enqueue a TX frame
        let test_frame = vec![0xDE, 0xAD, 0xBE, 0xEF];
        topo.tx_ring.enqueue(TxFrame { frame: test_frame.clone() }).unwrap();

        // Wait for it to be drained
        for _ in 0..100 {
            if !sent.lock().unwrap().is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        topo.shutdown();

        let sent_frames = sent.lock().unwrap();
        assert!(!sent_frames.is_empty(), "TX frame should have been sent");
        assert_eq!(sent_frames[0], test_frame);
    }

    #[test]
    fn configurable_rx_queues() {
        // Test various queue configurations
        for rq in [2, 3, 5, 9] {
            let plan = TopologyPlan {
                rx_queues: rq,
                nb_tx_queues: 2,
                numa_node: 0,
                source: TopologySource::Builder,
            };
            assert!(!plan.is_run_to_completion());
            assert_eq!(plan.total_lcores_needed(), rq as usize);
        }
    }

    #[test]
    fn rx_queues_one_is_rtc() {
        // rx_queues=1 → run-to-completion (no pipeline threads)
        let plan = TopologyPlan {
            rx_queues: 1,
            nb_tx_queues: 1,
            numa_node: 0,
            source: TopologySource::Builder,
        };
        assert!(plan.is_run_to_completion());
        assert_eq!(plan.total_lcores_needed(), 0);
    }

    #[test]
    fn pipeline_graceful_shutdown() {
        // Verify shutdown joins all threads cleanly
        let recv_fn = |_: usize, _pool: &FramePool| -> io::Result<Vec<FrameRef>> {
            std::thread::sleep(std::time::Duration::from_millis(1));
            Ok(vec![])
        };
        let send_fn = |_: &[u8]| -> io::Result<usize> { Ok(0) };

        // rx_queues=4 → 1 RX dispatcher + 3 queue workers
        let plan = TopologyPlan {
            rx_queues: 4,
            nb_tx_queues: 2,
            numa_node: 0,
            source: TopologySource::Builder,
        };

        let config = PipelineConfig {
            plan,
            local_port: 9000,
            local_mac: [0; 6],
            local_ip: Ipv4Addr::UNSPECIFIED,
            arp_cache: Arc::new(crate::ArpCache::new()),
            perf_counters: Arc::new(PerfCounters::new()),
        };

        let mut topo = start_pipeline(config, recv_fn, send_fn, None)
            .expect("should create pipeline");

        // Let threads run briefly
        std::thread::sleep(std::time::Duration::from_millis(20));

        // Shutdown should complete without hanging
        topo.shutdown();
        // If we get here, all threads joined successfully
    }

    #[test]
    fn adaptive_wait_phases() {
        // Verify the three phases of adaptive_wait don't panic
        let mut empty_polls = 0u32;

        // Phase 1: spin (should be fast)
        for _ in 0..SPIN_ITERS {
            adaptive_wait(&mut empty_polls);
        }
        assert_eq!(empty_polls, SPIN_ITERS);

        // Phase 2: yield
        for _ in 0..YIELD_ITERS {
            adaptive_wait(&mut empty_polls);
        }
        assert_eq!(empty_polls, SPIN_ITERS + YIELD_ITERS);

        // Phase 3: sleep (just verify it doesn't panic)
        adaptive_wait(&mut empty_polls);
        assert_eq!(empty_polls, SPIN_ITERS + YIELD_ITERS + 1);

        // Reset
        empty_polls = 0;
        adaptive_wait(&mut empty_polls);
        assert_eq!(empty_polls, 1); // back to spin phase
    }

    #[test]
    fn pipeline_per_queue_app_rings() {
        // Verify that packets arrive on per-queue SPSC app rings
        // and dequeue_app polls them correctly
        let frames: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));

        // Create multiple valid UDP frames
        for i in 0..4u8 {
            let frame = crate::build_udp_frame(
                &[0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0x01],
                &[0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0x02],
                Ipv4Addr::new(10, 0, 0, 1),
                Ipv4Addr::new(10, 0, 0, 2),
                12345,
                9000,
                &[b'a' + i],
                64,
            ).unwrap();
            frames.lock().unwrap().push(frame);
        }

        let frames_clone = Arc::clone(&frames);
        let recv_fn = move |_max: usize, pool: &FramePool| -> io::Result<Vec<FrameRef>> {
            let mut f = frames_clone.lock().unwrap();
            let mut refs = Vec::new();
            for frame in f.drain(..) {
                if let Some(fref) = pool.alloc_copy(&frame) {
                    refs.push(fref);
                }
            }
            Ok(refs)
        };

        let send_fn = |_: &[u8]| -> io::Result<usize> { Ok(0) };

        // rx_queues=3 → 1 RX dispatcher + 2 queue workers = 2 app_rings
        let plan = TopologyPlan {
            rx_queues: 3,
            nb_tx_queues: 2,
            numa_node: 0,
            source: TopologySource::Builder,
        };

        let config = PipelineConfig {
            plan,
            local_port: 9000,
            local_mac: [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0x02],
            local_ip: Ipv4Addr::new(10, 0, 0, 2),
            arp_cache: Arc::new(crate::ArpCache::new()),
            perf_counters: Arc::new(PerfCounters::new()),
        };

        let mut topo = start_pipeline(config, recv_fn, send_fn, None)
            .expect("should create pipeline");

        // Collect all packets
        let mut received = Vec::new();
        let mut rr = 0;
        for _ in 0..400 {
            if let Some(pkt) = topo.dequeue_app(&mut rr) {
                received.push(pkt);
                if received.len() >= 4 {
                    break;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        assert_eq!(received.len(), 4, "should have received all 4 packets");
        // Verify app_rings: rx_queues=3 → 2 queue workers → 2 app_rings
        assert_eq!(topo.app_rings.len(), 2);

        // Free frames back to pool
        for pkt in &received {
            topo.frame_pool.free(pkt.frame_ref.pool_idx);
        }

        topo.shutdown();
    }
}
