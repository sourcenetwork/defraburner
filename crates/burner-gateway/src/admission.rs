//! Per-tenant GCRA (Generic Cell Rate Algorithm) admission: a lock-free
//! token bucket keyed by tenant name, one atomic per tenant. The shape is
//! borrowed from afterburner's own thrust admission pattern
//! (`afterburner/crates/afterburner-thrust/src/admission.rs`) but
//! reimplemented fresh here, not depended on: thrust is BSL-licensed and
//! crate-private (`pub(crate)`), and the plan scopes this as "thrust's
//! GCRA admission pattern... reimplemented minimally in defraburner (the
//! crates stay in afterburner's workspace; we borrow the shape, not a
//! fork)".

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use kovan_map::HopscotchMap;

/// Default admission rate: 200 requests/sec.
pub const DEFAULT_RATE_PER_SEC: u64 = 200;
/// Default burst capacity: 100 requests.
pub const DEFAULT_BURST: u64 = 100;

/// Outcome of one admission check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    /// Reject; the caller should retry after this many whole seconds
    /// (rounded up), for the `Retry-After` response header.
    Reject {
        retry_after_secs: u64,
    },
}

/// One tenant's GCRA state: nanoseconds-since-`reference` of the
/// theoretical arrival time (TAT), advanced via a lock-free CAS loop.
/// `reference` is fixed at bucket creation and never compared across
/// buckets, so each bucket's own arbitrary starting point is harmless.
struct Bucket {
    period_ns: u64,
    burst_ns: u64,
    reference: Instant,
    tat_ns: AtomicU64,
    /// Per-tenant allowed/rejected counts (Phase 4, D17: feeds
    /// `burner-policy`'s `MetricsSnapshot.tenants[].admission`), alongside
    /// `Admission`'s own cluster-wide totals below.
    allowed: AtomicU64,
    rejected: AtomicU64,
}

impl Bucket {
    fn new(rate_per_sec: u64, burst: u64, now: Instant) -> Self {
        // Never divide by zero, never allow a zero-width burst window: a
        // rate or burst of 0 is normalized to the smallest allowed value
        // (matches afterburner-thrust's own `.max(1)` normalization).
        let rate_per_sec = rate_per_sec.max(1);
        let burst = burst.max(1);
        let period_ns = 1_000_000_000u64 / rate_per_sec;
        let burst_ns = burst.saturating_mul(period_ns);
        Self {
            period_ns,
            burst_ns,
            reference: now,
            tat_ns: AtomicU64::new(0),
            allowed: AtomicU64::new(0),
            rejected: AtomicU64::new(0),
        }
    }

    /// GCRA step:
    /// ```text
    /// tat_new = max(tat, now) + period
    /// if (tat_new - now) > burst: reject, retry after ceil((tat_new - now - burst))
    /// else: CAS tat -> tat_new, allow
    /// ```
    fn check(&self, now: Instant) -> Decision {
        let now_ns = now.saturating_duration_since(self.reference).as_nanos() as u64;
        loop {
            let tat = self.tat_ns.load(Ordering::Relaxed);
            let tat_base = tat.max(now_ns);
            let tat_new = tat_base.saturating_add(self.period_ns);
            let ahead = tat_new.saturating_sub(now_ns);
            if ahead > self.burst_ns {
                let over_ns = ahead - self.burst_ns;
                let retry_after_secs = over_ns.div_ceil(1_000_000_000).max(1);
                return Decision::Reject { retry_after_secs };
            }
            match self
                .tat_ns
                .compare_exchange(tat, tat_new, Ordering::AcqRel, Ordering::Relaxed)
            {
                Ok(_) => return Decision::Allow,
                // Contention: another caller updated tat; retry with the
                // fresher value.
                Err(_) => continue,
            }
        }
    }

    /// Builds a replacement bucket with new rate/burst limits (D23: `PUT
    /// /admin/tenants/{name}/admission`), carrying forward this bucket's
    /// `reference` instant and its current `tat_ns` (the actual GCRA
    /// state), not a fresh reset. Two reasons this matters, not one:
    /// resetting `tat_ns` to 0 would hand the tenant a brand-new full
    /// burst allowance merely because an admin changed a number (a real
    /// loophole a tenant could exploit by asking for a limit change);
    /// and re-anchoring `reference` to `Instant::now()` (as an earlier,
    /// buggy version of this did) breaks the same testability-with-a-
    /// synthetic-clock contract `check`'s own doc comment establishes,
    /// since a caller-supplied `now` from before the swap would then
    /// `saturating_duration_since` to zero against a later real-clock
    /// reference. Cumulative allowed/rejected counters carry over too: a
    /// settings change is not a reason to lose a tenant's history.
    fn with_new_limits(&self, rate_per_sec: u64, burst: u64) -> Self {
        let rate_per_sec = rate_per_sec.max(1);
        let burst = burst.max(1);
        let period_ns = 1_000_000_000u64 / rate_per_sec;
        let burst_ns = burst.saturating_mul(period_ns);
        Self {
            period_ns,
            burst_ns,
            reference: self.reference,
            tat_ns: AtomicU64::new(self.tat_ns.load(Ordering::Relaxed)),
            allowed: AtomicU64::new(self.allowed.load(Ordering::Relaxed)),
            rejected: AtomicU64::new(self.rejected.load(Ordering::Relaxed)),
        }
    }
}

/// Point-in-time admission counters, for `GET /admin/status`.
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct AdmissionCounters {
    pub allowed: u64,
    pub rejected: u64,
    pub tenant_count: usize,
}

/// Per-tenant GCRA admission, keyed by tenant name.
pub struct Admission {
    rate_per_sec: u64,
    burst: u64,
    buckets: HopscotchMap<String, Arc<Bucket>>,
    /// Per-tenant overrides (console round, D23: `PUT
    /// /admin/tenants/{name}/admission`), consulted whenever a tenant's
    /// bucket is (re)built. Absent means "use the process-wide default
    /// `rate_per_sec`/`burst` above".
    overrides: HopscotchMap<String, (u64, u64)>,
    allowed: AtomicU64,
    rejected: AtomicU64,
}

impl Admission {
    pub fn new(rate_per_sec: u64, burst: u64) -> Self {
        Self {
            rate_per_sec,
            burst,
            buckets: HopscotchMap::new(),
            overrides: HopscotchMap::new(),
            allowed: AtomicU64::new(0),
            rejected: AtomicU64::new(0),
        }
    }

    /// Number of currently-tracked tenant buckets (diagnostics).
    pub fn tenant_count(&self) -> usize {
        self.buckets.len()
    }

    /// A snapshot of cumulative allow/reject counts plus the current
    /// tenant-bucket count.
    pub fn counters(&self) -> AdmissionCounters {
        AdmissionCounters {
            allowed: self.allowed.load(Ordering::Relaxed),
            rejected: self.rejected.load(Ordering::Relaxed),
            tenant_count: self.tenant_count(),
        }
    }

    /// Checks (and, on `Allow`, advances) `tenant`'s bucket at `now`.
    /// `now` is caller-supplied (never `Instant::now()` internally) so the
    /// GCRA math is testable without a real sleep.
    pub fn check(&self, tenant: &str, now: Instant) -> Decision {
        let bucket = match self.buckets.get(tenant) {
            Some(bucket) => bucket,
            None => {
                let (rate, burst) = self
                    .overrides
                    .get(tenant)
                    .unwrap_or((self.rate_per_sec, self.burst));
                let fresh = Arc::new(Bucket::new(rate, burst, now));
                self.buckets.get_or_insert(tenant.to_string(), fresh)
            }
        };
        let decision = bucket.check(now);
        match decision {
            Decision::Allow => {
                self.allowed.fetch_add(1, Ordering::Relaxed);
                bucket.allowed.fetch_add(1, Ordering::Relaxed);
            }
            Decision::Reject { .. } => {
                self.rejected.fetch_add(1, Ordering::Relaxed);
                bucket.rejected.fetch_add(1, Ordering::Relaxed);
            }
        };
        decision
    }

    /// Per-tenant allowed/rejected counts for every tenant that has ever
    /// been admission-checked (Phase 4, D17): feeds `burner-policy`'s
    /// `MetricsSnapshot.tenants[].admission` via `defraburner`'s glue
    /// layer (this crate does not depend on `burner-policy`).
    pub fn per_tenant_snapshot(&self) -> Vec<TenantAdmissionSnapshot> {
        self.buckets
            .iter()
            .map(|(tenant, bucket)| TenantAdmissionSnapshot {
                tenant,
                allowed: bucket.allowed.load(Ordering::Relaxed),
                rejected: bucket.rejected.load(Ordering::Relaxed),
            })
            .collect()
    }

    /// Sets (or, when `None`, clears) `tenant`'s admission override
    /// (console round, D23). If `tenant` already has a live bucket, it is
    /// replaced immediately with one built from the new rate/burst so the
    /// change takes effect on the very next request, carrying over the
    /// bucket's cumulative allowed/rejected counters (a settings change is
    /// not a reason to lose the tenant's history).
    pub fn set_override(&self, tenant: &str, admission: Option<(u64, u64)>) {
        match admission {
            Some(pair) => {
                self.overrides.insert(tenant.to_string(), pair);
            }
            None => {
                self.overrides.remove(tenant);
            }
        }
        let (rate, burst) = admission.unwrap_or((self.rate_per_sec, self.burst));
        if let Some(existing) = self.buckets.get(tenant) {
            let fresh = existing.with_new_limits(rate, burst);
            self.buckets.insert(tenant.to_string(), Arc::new(fresh));
        }
    }
}

/// One tenant's point-in-time admission counters, from
/// [`Admission::per_tenant_snapshot`].
#[derive(Debug, Clone, serde::Serialize)]
pub struct TenantAdmissionSnapshot {
    pub tenant: String,
    pub allowed: u64,
    pub rejected: u64,
}

impl Default for Admission {
    fn default() -> Self {
        Self::new(DEFAULT_RATE_PER_SEC, DEFAULT_BURST)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn allows_up_to_burst_then_rejects() {
        // 100/sec, burst 5: period 10ms, burst window 50ms. Five calls at
        // the same instant fit exactly in the burst window; the sixth
        // does not.
        let admission = Admission::new(100, 5);
        let t0 = Instant::now();
        for i in 0..5 {
            assert_eq!(
                admission.check("acme-co", t0),
                Decision::Allow,
                "call {i} should be allowed (within burst)"
            );
        }
        assert!(matches!(
            admission.check("acme-co", t0),
            Decision::Reject { .. }
        ));
    }

    #[test]
    fn reject_then_allow_after_time_passes() {
        // 1/sec, burst 1: period 1s, burst window 1s. First call allowed;
        // an immediate second call rejects with retry_after_secs == 1;
        // a third call one second later (synthetic clock, no real sleep)
        // is allowed again.
        let admission = Admission::new(1, 1);
        let t0 = Instant::now();
        assert_eq!(admission.check("acme-co", t0), Decision::Allow);
        assert_eq!(
            admission.check("acme-co", t0),
            Decision::Reject {
                retry_after_secs: 1
            }
        );
        assert_eq!(
            admission.check("acme-co", t0 + Duration::from_secs(1)),
            Decision::Allow
        );
    }

    #[test]
    fn distinct_tenants_are_isolated() {
        let admission = Admission::new(10, 1);
        let t0 = Instant::now();
        assert_eq!(admission.check("acme-co", t0), Decision::Allow);
        assert!(matches!(
            admission.check("acme-co", t0),
            Decision::Reject { .. }
        ));
        // A different tenant is untouched: still has its own first token.
        assert_eq!(admission.check("other-co", t0), Decision::Allow);
        assert!(matches!(
            admission.check("other-co", t0),
            Decision::Reject { .. }
        ));
    }

    #[test]
    fn zero_rate_and_burst_do_not_panic() {
        let admission = Admission::new(0, 0);
        let t0 = Instant::now();
        assert_eq!(admission.check("acme-co", t0), Decision::Allow);
        assert!(matches!(
            admission.check("acme-co", t0),
            Decision::Reject { .. }
        ));
    }

    #[test]
    fn tenant_count_tracks_first_touch() {
        let admission = Admission::new(10, 1);
        let t0 = Instant::now();
        assert_eq!(admission.tenant_count(), 0);
        admission.check("acme-co", t0);
        assert_eq!(admission.tenant_count(), 1);
        admission.check("acme-co", t0);
        assert_eq!(
            admission.tenant_count(),
            1,
            "repeat touches do not grow the map"
        );
        admission.check("other-co", t0);
        assert_eq!(admission.tenant_count(), 2);
    }

    #[test]
    fn default_uses_the_documented_rate_and_burst() {
        let admission = Admission::default();
        let t0 = Instant::now();
        for i in 0..DEFAULT_BURST {
            assert_eq!(
                admission.check("acme-co", t0),
                Decision::Allow,
                "call {i} should fit inside the default burst"
            );
        }
        assert!(matches!(
            admission.check("acme-co", t0),
            Decision::Reject { .. }
        ));
    }

    #[test]
    fn per_tenant_snapshot_tracks_each_tenant_independently() {
        let admission = Admission::new(10, 2);
        let t0 = Instant::now();
        admission.check("acme-co", t0);
        admission.check("acme-co", t0);
        admission.check("acme-co", t0); // over burst: rejected
        admission.check("other-co", t0);

        let mut snapshot = admission.per_tenant_snapshot();
        snapshot.sort_by(|a, b| a.tenant.cmp(&b.tenant));
        assert_eq!(snapshot.len(), 2);
        assert_eq!(snapshot[0].tenant, "acme-co");
        assert_eq!(snapshot[0].allowed, 2);
        assert_eq!(snapshot[0].rejected, 1);
        assert_eq!(snapshot[1].tenant, "other-co");
        assert_eq!(snapshot[1].allowed, 1);
        assert_eq!(snapshot[1].rejected, 0);
    }

    /// Perf floor (plan Phase 6, "measure or it did not happen"): times 1M
    /// single-threaded admission checks against one tenant bucket, all
    /// landing on the CAS-success (`Allow`) path (a synthetic rate high
    /// enough, and `now` advancing exactly one synthetic nanosecond per
    /// call, that the bucket never exhausts its burst). 10us/check is a
    /// generous order-of-magnitude bound: one atomic load, a `max`/`add`,
    /// and a CAS is expected to cost tens of nanoseconds, not thousands.
    #[test]
    fn gcra_1m_admission_checks_are_cheap() {
        const ITERS: u64 = 1_000_000;
        // Rate high enough (1 check/ns) that `burst` (1M checks worth) is
        // never exhausted by `ITERS` monotonically-advancing calls.
        let admission = Admission::new(1_000_000_000, 1_000_000);
        let t0 = Instant::now();
        for i in 0..ITERS {
            let now = t0 + Duration::from_nanos(i);
            let decision = admission.check("acme-co", now);
            assert_eq!(
                decision,
                Decision::Allow,
                "check {i} should be allowed at this synthetic rate"
            );
        }
        let elapsed = t0.elapsed();
        let per_check_ns = elapsed.as_nanos() / u128::from(ITERS);
        println!("GCRA_NS per_check={per_check_ns}");
        assert!(
            per_check_ns < 10_000,
            "admission check should be well under 10us/check; got {per_check_ns}ns"
        );
    }

    #[test]
    fn counters_tally_allowed_and_rejected() {
        let admission = Admission::new(10, 2);
        let t0 = Instant::now();
        admission.check("acme-co", t0);
        admission.check("acme-co", t0);
        admission.check("acme-co", t0); // over burst: rejected
        let counters = admission.counters();
        assert_eq!(counters.allowed, 2);
        assert_eq!(counters.rejected, 1);
        assert_eq!(counters.tenant_count, 1);
    }

    #[test]
    fn set_override_before_first_touch_governs_the_first_bucket() {
        // Default is 1/sec burst 1; the override raises it to 10/sec
        // burst 5 before "acme-co" is ever checked.
        let admission = Admission::new(1, 1);
        admission.set_override("acme-co", Some((10, 5)));
        let t0 = Instant::now();
        for i in 0..5 {
            assert_eq!(
                admission.check("acme-co", t0),
                Decision::Allow,
                "call {i} should fit the overridden burst of 5, not the default of 1"
            );
        }
        assert!(matches!(
            admission.check("acme-co", t0),
            Decision::Reject { .. }
        ));
    }

    #[test]
    fn set_override_on_an_existing_bucket_takes_effect_immediately_and_keeps_counters() {
        let admission = Admission::new(100, 5);
        let t0 = Instant::now();
        admission.check("acme-co", t0);
        admission.check("acme-co", t0);

        // Tighten the tenant down to burst 1: the next call should reject
        // even though the default (100/5) would still have allowed it.
        admission.set_override("acme-co", Some((1, 1)));
        assert!(matches!(
            admission.check("acme-co", t0),
            Decision::Reject { .. }
        ));

        let snapshot = admission.per_tenant_snapshot();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(
            snapshot[0].allowed, 2,
            "cumulative allowed count survives an override-driven bucket replacement"
        );
        assert_eq!(snapshot[0].rejected, 1);
    }

    #[test]
    fn clearing_an_override_reverts_to_the_process_default() {
        let admission = Admission::new(1, 1);
        admission.set_override("acme-co", Some((10, 5)));
        admission.set_override("acme-co", None);

        let t0 = Instant::now();
        assert_eq!(admission.check("acme-co", t0), Decision::Allow);
        assert!(
            matches!(admission.check("acme-co", t0), Decision::Reject { .. }),
            "burst should be back to the process default of 1"
        );
    }
}
