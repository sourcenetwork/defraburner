//! `loadgen`: a minimal load generator for defraburner's gateway GraphQL
//! endpoint (plan Phase 6, "measure or it did not happen"; driven by
//! `just perf`). Runs `--threads` tight POST loops against `--url` for
//! `--secs` seconds; each thread collects its own per-request latencies
//! locally (a `Vec`, naturally bounded by the run's own duration rather
//! than a separately-capped buffer). At the end every thread's samples are
//! merged and one `LOADGEN` summary line is printed. Percentile computation
//! mirrors afterburner's own `burn bench` idiom
//! (`afterburner/crates/afterburner/src/cli/bench.rs`'s `report_bench`):
//! sort, then index by rank.
//!
//! Examples compile against the crate's `[dev-dependencies]` too, so
//! `ureq` (already pulled in for `tests/gateway.rs` and friends) is
//! available here without adding a new dependency.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use clap::Parser;

#[derive(Parser)]
#[command(
    name = "loadgen",
    about = "Minimal load generator for defraburner's gateway GraphQL endpoint"
)]
struct Args {
    /// Full URL of the GraphQL endpoint to hammer, e.g.
    /// http://127.0.0.1:9181/api/v1/graphql.
    #[arg(long)]
    url: String,
    /// Tenant bearer token, sent as `Authorization: Bearer <token>`.
    #[arg(long)]
    token: String,
    /// Number of concurrent request-issuing threads.
    #[arg(long, default_value_t = 1)]
    threads: usize,
    /// Run duration in seconds.
    #[arg(long, default_value_t = 10)]
    secs: u64,
    /// Raw JSON request body, e.g. `{"query": "query { Foo { id } }"}`.
    #[arg(long)]
    body: String,
}

fn main() -> Result<()> {
    let args = Args::parse();
    anyhow::ensure!(args.threads >= 1, "--threads must be at least 1");

    let deadline = Instant::now() + Duration::from_secs(args.secs);
    let err_count = AtomicU64::new(0);

    let run_started = Instant::now();
    let latencies_us: Vec<u128> = std::thread::scope(|scope| -> Result<Vec<u128>> {
        let handles: Vec<_> = (0..args.threads)
            .map(|_| {
                // Borrows `args`/`err_count`/`deadline` directly: `scope`
                // guarantees every spawned thread joins before this block
                // returns, so nothing here needs `Arc` to outlive the loop.
                scope.spawn(|| {
                    let mut latencies = Vec::new();
                    while Instant::now() < deadline {
                        let started = Instant::now();
                        let outcome = ureq::post(&args.url)
                            .set("Authorization", &format!("Bearer {}", args.token))
                            .set("Content-Type", "application/json")
                            .send_string(&args.body);
                        match outcome {
                            Ok(_) => latencies.push(started.elapsed().as_micros()),
                            Err(_) => {
                                err_count.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                    latencies
                })
            })
            .collect();

        let mut all = Vec::new();
        for handle in handles {
            let part = handle
                .join()
                .map_err(|_| anyhow!("a loadgen thread panicked"))?;
            all.extend(part);
        }
        Ok(all)
    })?;
    let elapsed = run_started.elapsed();

    report(&latencies_us, elapsed, err_count.load(Ordering::Relaxed));
    Ok(())
}

/// Prints the `LOADGEN` summary line. Percentiles: sort, then index by
/// rank (the same 10-line idiom `afterburner`'s `burn bench` uses).
fn report(latencies_us: &[u128], elapsed: Duration, err: u64) {
    let req_total = latencies_us.len() as u64 + err;
    let req_per_sec = req_total as f64 / elapsed.as_secs_f64();

    let mut sorted = latencies_us.to_vec();
    sorted.sort_unstable();
    let (p50_ms, p99_ms, max_ms) = if sorted.is_empty() {
        (0.0, 0.0, 0.0)
    } else {
        let p50 = sorted[sorted.len() / 2];
        let p99_idx = ((sorted.len() as f64) * 0.99) as usize;
        let p99 = sorted[p99_idx.min(sorted.len() - 1)];
        let max = *sorted.last().expect("checked non-empty above");
        (
            p50 as f64 / 1000.0,
            p99 as f64 / 1000.0,
            max as f64 / 1000.0,
        )
    };

    println!(
        "LOADGEN req_total={req_total} req_per_sec={req_per_sec:.1} \
         p50_ms={p50_ms:.3} p99_ms={p99_ms:.3} max_ms={max_ms:.3} err={err}"
    );
}
