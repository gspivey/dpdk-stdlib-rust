//! Multi-core topology planning for DPDK pipeline stages.
//!
//! Determines how many RSS RX queues and worker cores to allocate based on:
//! 1. Explicit builder configuration (highest priority)
//! 2. Environment variables (`DPDK_RX_QUEUES`, `DPDK_WORKERS_PER_QUEUE`)
//! 3. Auto-detection from available lcores and NIC capabilities
//!
//! Under stubs (`dpdk_sys::is_stub()`), the topology always collapses to
//! single-core run-to-completion — no threads are spawned.

use std::env;
use std::fmt;

// ============================================================================
// TopologyConfig — input from builder / env / auto
// ============================================================================

/// User-provided topology hints (from `UdpSocketBuilder` or defaults).
#[derive(Debug, Clone, Default)]
pub struct TopologyConfig {
    /// Explicit RX queue count (from builder API).
    pub rx_queues: Option<u16>,
    /// Explicit workers-per-queue count (from builder API).
    pub workers_per_queue: Option<u16>,
}

// ============================================================================
// TopologyPlan — output of detect_topology()
// ============================================================================

/// The resolved multi-core topology plan.
///
/// When `rx_queues == 1` and `workers_per_queue == 0`, this is run-to-completion
/// mode (current single-threaded behavior, no pipeline overhead).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopologyPlan {
    /// Number of NIC RSS RX queues to configure.
    pub rx_queues: u16,
    /// Number of worker lcores per RX queue.
    pub workers_per_queue: u16,
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
        self.rx_queues <= 1 && self.workers_per_queue == 0
    }

    /// Total number of lcores needed (RX cores + worker cores).
    /// Does not include the main lcore (which calls recv_from/send_to).
    pub fn total_lcores_needed(&self) -> usize {
        let rx = self.rx_queues as usize;
        let workers = rx * self.workers_per_queue as usize;
        if self.is_run_to_completion() {
            0 // no extra threads
        } else {
            rx + workers
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
                "{} RX queues x {} workers/queue ({} lcores, NUMA {}, {:?})",
                self.rx_queues,
                self.workers_per_queue,
                self.total_lcores_needed(),
                self.numa_node,
                self.source,
            )
        }
    }
}

// ============================================================================
// detect_topology() — the main entry point
// ============================================================================

/// Detect the optimal multi-core topology.
///
/// The `nic_numa_node` parameter should come from `Port::numa_node()` —
/// this ensures lcores and memory are allocated on the same NUMA node as
/// the NIC, avoiding cross-socket memory access penalties.
///
/// Configuration precedence: builder API > environment variables > auto-detection.
///
/// Under stubs, always returns a run-to-completion plan regardless of config.
pub fn detect_topology(
    config: &TopologyConfig,
    available_lcores: u32,
    nic_max_rx_queues: u16,
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
            workers_per_queue: 0,
            numa_node,
            source: TopologySource::Stub,
        };
    }

    // Try builder config first
    if let (Some(rq), Some(wpq)) = (config.rx_queues, config.workers_per_queue) {
        return TopologyPlan {
            rx_queues: clamp_rx_queues(rq, nic_max_rx_queues),
            workers_per_queue: wpq,
            numa_node,
            source: TopologySource::Builder,
        };
    }

    // Try environment variables
    let env_rq = env::var("DPDK_RX_QUEUES").ok().and_then(|v| v.parse::<u16>().ok());
    let env_wpq = env::var("DPDK_WORKERS_PER_QUEUE").ok().and_then(|v| v.parse::<u16>().ok());

    // Builder partial + env partial: builder fields win where set
    let rq = config.rx_queues.or(env_rq);
    let wpq = config.workers_per_queue.or(env_wpq);

    if rq.is_some() || wpq.is_some() {
        let rx_queues = rq.unwrap_or_else(|| auto_detect_queues(available_lcores, nic_max_rx_queues));
        let workers_per_queue = wpq.unwrap_or_else(|| auto_detect_workers(available_lcores, rx_queues));
        let source = if config.rx_queues.is_some() || config.workers_per_queue.is_some() {
            TopologySource::Builder
        } else {
            TopologySource::Environment
        };
        return TopologyPlan {
            rx_queues: clamp_rx_queues(rx_queues, nic_max_rx_queues),
            workers_per_queue,
            numa_node,
            source,
        };
    }

    // Full auto-detection
    let rx_queues = auto_detect_queues(available_lcores, nic_max_rx_queues);
    let workers_per_queue = auto_detect_workers(available_lcores, rx_queues);

    TopologyPlan {
        rx_queues,
        workers_per_queue,
        numa_node,
        source: TopologySource::AutoDetected,
    }
}

// ============================================================================
// Auto-detection helpers
// ============================================================================

/// Auto-detect the number of RX queues based on available lcores and NIC caps.
fn auto_detect_queues(lcores: u32, nic_max: u16) -> u16 {
    let queues = match lcores {
        0..=2 => 1,                            // run-to-completion
        3..=4 => 2.min(nic_max),               // small pipeline
        n => ((n / 2) as u16).min(nic_max),    // half for RX, half for workers
    };
    clamp_rx_queues(queues, nic_max)
}

/// Auto-detect workers per queue from remaining lcores after RX allocation.
fn auto_detect_workers(lcores: u32, rx_queues: u16) -> u16 {
    if rx_queues == 0 {
        return 0;
    }
    let remaining = (lcores as usize).saturating_sub(rx_queues as usize);
    if remaining == 0 {
        return 0; // run-to-completion
    }
    (remaining / rx_queues as usize).max(1) as u16
}

/// Ensure rx_queues doesn't exceed NIC maximum.
fn clamp_rx_queues(requested: u16, nic_max: u16) -> u16 {
    if nic_max == 0 {
        return 1; // fallback for unknown NICs
    }
    requested.min(nic_max).max(1)
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn default_config() -> TopologyConfig {
        TopologyConfig::default()
    }

    #[test]
    fn stub_always_run_to_completion() {
        // Under stubs, regardless of config, we get run-to-completion
        let config = TopologyConfig {
            rx_queues: Some(4),
            workers_per_queue: Some(2),
        };
        let plan = detect_topology(&config, 16, 16, 0);
        assert!(plan.is_run_to_completion());
        assert_eq!(plan.source, TopologySource::Stub);
        assert_eq!(plan.rx_queues, 1);
        assert_eq!(plan.workers_per_queue, 0);
    }

    #[test]
    fn auto_detect_2_vcpu() {
        let queues = auto_detect_queues(2, 16);
        assert_eq!(queues, 1);
        let workers = auto_detect_workers(2, queues);
        assert_eq!(workers, 1);
    }

    #[test]
    fn auto_detect_4_vcpu() {
        let queues = auto_detect_queues(4, 16);
        assert_eq!(queues, 2);
        let workers = auto_detect_workers(4, queues);
        assert_eq!(workers, 1);
    }

    #[test]
    fn auto_detect_16_vcpu() {
        let queues = auto_detect_queues(16, 16);
        assert_eq!(queues, 8);
        let workers = auto_detect_workers(16, queues);
        assert_eq!(workers, 1);
    }

    #[test]
    fn auto_detect_32_vcpu() {
        let queues = auto_detect_queues(32, 16);
        assert_eq!(queues, 16); // clamped to NIC max
        let workers = auto_detect_workers(32, queues);
        assert_eq!(workers, 1);
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
    fn total_lcores_run_to_completion() {
        let plan = TopologyPlan {
            rx_queues: 1,
            workers_per_queue: 0,
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
            workers_per_queue: 2,
            numa_node: 0,
            source: TopologySource::AutoDetected,
        };
        assert!(!plan.is_run_to_completion());
        assert_eq!(plan.total_lcores_needed(), 12); // 4 RX + 8 workers
    }

    #[test]
    fn display_run_to_completion() {
        let plan = TopologyPlan {
            rx_queues: 1,
            workers_per_queue: 0,
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
            workers_per_queue: 2,
            numa_node: 1,
            source: TopologySource::Builder,
        };
        let s = format!("{plan}");
        assert!(s.contains("4 RX queues"));
        assert!(s.contains("2 workers/queue"));
    }

    #[test]
    fn stub_propagates_nic_numa_node() {
        let config = TopologyConfig::default();
        let plan = detect_topology(&config, 8, 16, 1);
        // Even under stubs (run-to-completion), the NIC's NUMA node is recorded
        assert_eq!(plan.numa_node, 1);
        assert!(plan.is_run_to_completion());
    }

    #[test]
    fn negative_numa_defaults_to_zero() {
        let config = TopologyConfig::default();
        // SOCKET_ID_ANY is -1
        let plan = detect_topology(&config, 8, 16, -1);
        assert_eq!(plan.numa_node, 0);
    }
}
