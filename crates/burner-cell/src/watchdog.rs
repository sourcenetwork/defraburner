//! Health-probe watchdog: periodically checks each running cell's liveness
//! by querying its `BurnerMarker`; after repeated failures it drains and
//! re-ignites the cell.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;

use crate::supervisor::{Supervisor, verify_marker};

/// Default interval between health-probe rounds.
pub const DEFAULT_PROBE_INTERVAL: Duration = Duration::from_secs(10);
/// Deadline for a single probe query.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
/// Consecutive probe failures before a cell is drained and re-ignited.
const FAILURE_THRESHOLD: u32 = 3;

/// Per-cell probe counters, and the pure failure-counting state machine that
/// decides when a cell has failed enough times to warrant re-ignition. Kept
/// free of tokio so the counting logic is unit-testable without a runtime.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct CellHealth {
    pub probes_ok: u64,
    pub probes_failed: u64,
    pub reignitions: u64,
    /// Internal bookkeeping only; not part of the exposed counter contract.
    #[serde(skip)]
    consecutive_failures: u32,
}

/// What the caller should do after recording one probe result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// The probe succeeded; the failure streak reset.
    Healthy,
    /// The probe failed, but not (yet) enough times in a row to act on.
    StillFailing { consecutive: u32 },
    /// The probe failed for the `FAILURE_THRESHOLD`th consecutive time: the
    /// caller should drain and re-ignite the cell now. The streak has
    /// already been reset by this call.
    ReigniteNow,
}

impl CellHealth {
    /// Records one probe result and returns what the caller should do.
    pub fn record(&mut self, ok: bool) -> ProbeOutcome {
        if ok {
            self.probes_ok += 1;
            self.consecutive_failures = 0;
            return ProbeOutcome::Healthy;
        }

        self.probes_failed += 1;
        self.consecutive_failures += 1;
        if self.consecutive_failures >= FAILURE_THRESHOLD {
            self.reignitions += 1;
            self.consecutive_failures = 0;
            ProbeOutcome::ReigniteNow
        } else {
            ProbeOutcome::StillFailing {
                consecutive: self.consecutive_failures,
            }
        }
    }
}

/// The health-probe watchdog. Holds only its counters; the probe loop
/// itself is [`Watchdog::run`], an `async fn` the caller drives directly
/// (see that method's doc comment for why it is not `tokio::spawn`ed
/// internally).
pub struct Watchdog {
    counters: Arc<Mutex<HashMap<String, CellHealth>>>,
}

impl Default for Watchdog {
    fn default() -> Self {
        Self::new()
    }
}

impl Watchdog {
    pub fn new() -> Self {
        Self {
            counters: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// A snapshot of every probed cell's counters, exposed for status
    /// reporting.
    pub async fn counters(&self) -> HashMap<String, CellHealth> {
        self.counters.lock().await.clone()
    }

    /// Runs the probe loop forever: every `probe_interval`, queries each
    /// running cell's `BurnerMarker` with a bounded deadline, and after
    /// `FAILURE_THRESHOLD` consecutive failures drains and re-ignites it.
    ///
    /// Deliberately **not** spawned onto its own task internally (there is
    /// no `Watchdog::spawn`). `embedded::build_with_store`'s returned
    /// future is not `Send`: with the libp2p transport (every cell here),
    /// the `P2PSetup` it builds holds a `wire_document_acp` field typed
    /// `Option<Box<dyn FnOnce(Arc<dyn acp::DocumentACP>)>>` with no `+
    /// Send` bound, live across an await point, which makes the whole
    /// future non-`Send` regardless of which branch actually populates
    /// that field (confirmed by `cargo check`: wrapping any call chain
    /// that reaches `cell::ignite` in `tokio::spawn` fails with "cannot be
    /// sent between threads safely", `defradb.rs
    /// crates/embedded/src/node_p2p.rs:21` /
    /// `crates/embedded/src/node.rs:495`). Re-ignition on a failed probe
    /// calls straight into `Supervisor::reignite` -> `cell::ignite`, so the
    /// whole loop has to be driven cooperatively on the caller's task
    /// (e.g. a `tokio::select!` branch alongside the shutdown-signal wait)
    /// instead of being spawned.
    pub async fn run(&self, supervisor: Arc<Mutex<Supervisor>>, probe_interval: Duration) -> ! {
        let mut ticker = tokio::time::interval(probe_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // The first tick fires immediately; skip it so the first real probe
        // round happens one interval after cells are up, not the instant
        // this loop starts mid-ignition.
        ticker.tick().await;

        loop {
            ticker.tick().await;
            let ids = supervisor.lock().await.cell_ids();
            for id in ids {
                self.probe_one(&supervisor, &id).await;
            }
        }
    }

    async fn probe_one(&self, supervisor: &Arc<Mutex<Supervisor>>, id: &str) {
        let Some(node) = supervisor.lock().await.node_handle(id) else {
            // Drained concurrently (e.g. by a manual drain) since
            // cell_ids() was read; nothing to probe.
            return;
        };

        let ok = match tokio::time::timeout(PROBE_TIMEOUT, verify_marker(&node, id)).await {
            Ok(Ok(found)) => found,
            Ok(Err(error)) => {
                tracing::warn!(cell_id = %id, error = %error, "watchdog probe query failed");
                false
            }
            Err(_) => {
                tracing::warn!(cell_id = %id, "watchdog probe timed out");
                false
            }
        };

        let outcome = self
            .counters
            .lock()
            .await
            .entry(id.to_string())
            .or_default()
            .record(ok);

        match outcome {
            ProbeOutcome::Healthy => {}
            ProbeOutcome::StillFailing { consecutive } => {
                tracing::error!(cell_id = %id, consecutive, "watchdog: liveness probe failed");
            }
            ProbeOutcome::ReigniteNow => {
                tracing::error!(
                    cell_id = %id,
                    threshold = FAILURE_THRESHOLD,
                    "watchdog: consecutive probe failures reached the threshold, re-igniting"
                );
                let mut supervisor = supervisor.lock().await;
                if let Err(error) = supervisor.reignite(id).await {
                    tracing::error!(cell_id = %id, error = %error, "watchdog: re-ignition failed");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn healthy_probes_keep_counting_and_never_reignite() {
        let mut health = CellHealth::default();
        for _ in 0..10 {
            assert_eq!(health.record(true), ProbeOutcome::Healthy);
        }
        assert_eq!(health.probes_ok, 10);
        assert_eq!(health.probes_failed, 0);
        assert_eq!(health.reignitions, 0);
    }

    #[test]
    fn three_consecutive_failures_trigger_reignition_and_reset() {
        let mut health = CellHealth::default();
        assert_eq!(
            health.record(false),
            ProbeOutcome::StillFailing { consecutive: 1 }
        );
        assert_eq!(
            health.record(false),
            ProbeOutcome::StillFailing { consecutive: 2 }
        );
        assert_eq!(health.record(false), ProbeOutcome::ReigniteNow);
        assert_eq!(health.probes_failed, 3);
        assert_eq!(health.reignitions, 1);

        // The streak reset: two more failures alone should not reignite again.
        assert_eq!(
            health.record(false),
            ProbeOutcome::StillFailing { consecutive: 1 }
        );
        assert_eq!(
            health.record(false),
            ProbeOutcome::StillFailing { consecutive: 2 }
        );
        assert_eq!(health.reignitions, 1);
        assert_eq!(health.record(false), ProbeOutcome::ReigniteNow);
        assert_eq!(health.reignitions, 2);
    }

    #[test]
    fn a_healthy_probe_between_failures_resets_the_streak() {
        let mut health = CellHealth::default();
        assert_eq!(
            health.record(false),
            ProbeOutcome::StillFailing { consecutive: 1 }
        );
        assert_eq!(
            health.record(false),
            ProbeOutcome::StillFailing { consecutive: 2 }
        );
        assert_eq!(health.record(true), ProbeOutcome::Healthy);
        // Two more failures should not reignite: the streak restarted.
        assert_eq!(
            health.record(false),
            ProbeOutcome::StillFailing { consecutive: 1 }
        );
        assert_eq!(
            health.record(false),
            ProbeOutcome::StillFailing { consecutive: 2 }
        );
        assert_eq!(health.reignitions, 0);
        assert_eq!(health.probes_ok, 1);
        assert_eq!(health.probes_failed, 4);
    }
}
