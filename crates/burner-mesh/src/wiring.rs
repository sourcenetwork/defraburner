//! Full-mesh replication wiring for one tenant's cell group: connects
//! every member to every other member, then joins (not sequences) every
//! `add_collections` call with every D13 topic-ready wait so a caller's
//! first write after `wire_group` is not racing topic-mesh formation.

use std::collections::HashSet;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use burner_cell::RunningCell;
use futures::future::{BoxFuture, join_all, try_join_all};
use tokio::time::Instant;

use crate::topic_ready::wait_topic_peer;

/// A `(cell_id, collection, peer_id)` triple: one cell having confirmed
/// another cell joined a collection's gossipsub topic.
type TopicJoinKey = (String, String, String);

/// Deadline for one `wait_topic_peer` call inside `wire_group`.
const TOPIC_READY_TIMEOUT: Duration = Duration::from_secs(15);
/// Deadline for [`ensure_group_connected`]'s `connected_peers` poll: the
/// already-placed / recovery path's positively-observable stand-in for
/// the topic-join wait above (see its own doc comment for why that wait
/// cannot be used there).
const CONNECT_CONFIRM_TIMEOUT: Duration = Duration::from_secs(15);
/// Poll interval for the same deadline loop.
const CONNECT_CONFIRM_POLL_STEP: Duration = Duration::from_millis(100);

/// Wires `cells` into a full mesh replicating `collections`: connect_peer
/// between every ordered pair, then every `add_collections` call and
/// every D13 topic-ready wait together, so every wait's subscribe is
/// guaranteed to be registered before any `add_collections`'s network
/// effects can begin.
///
/// Bounded (no concurrency limiter beyond the group itself): groups are
/// sized by replication factor, not fleet size, so this is small by
/// construction. Per D12, nothing here reaches `cell::ignite`, so this is
/// plain `async fn`, safe to `.await` directly on the caller's task (the
/// `try_join_all` below drives everything on the current task too; it
/// never spawns).
///
/// The `add_collections`/wait steps are **joined**, not sequenced
/// (`add_collections` for every member, awaited to completion, *then*
/// every wait), because sequencing them was measured to flake: with
/// upstream's edge-triggered `TopicPeerEvent` (fires once per state
/// transition, never replayed to a late subscriber) and gossipsub
/// delivery running on each cell's own background swarm task, independent
/// of the caller's, a wait that subscribes only after every
/// `add_collections` had already returned lost the race to that
/// background delivery roughly 10-50% of the time in this same fix
/// applied to the (since-removed) Phase 0 spike. `try_join_all` polls
/// every stored future once, in the order given, on its own first poll
/// (a not-yet-ready future still needs its waker registered, so the
/// combinator cannot skip
/// polling any of them), so listing every `wait_topic_peer` future before
/// every `add_collections` future guarantees every subscribe runs to
/// completion (an entirely synchronous prefix: the event-bus subscribe,
/// the topic lookup, and the immediate drain of any already-buffered
/// event) before any `add_collections` future is polled at all.
///
/// Idempotent by construction, not by special-casing errors here: calling
/// this twice for an already-wired group (reconcile's `Placed` branch,
/// re-verifying a recovered cluster) tolerates the repeat because upstream
/// already makes both underlying calls no-ops on a duplicate, verified in
/// source rather than assumed:
/// - `connect_peer` on an already-connected peer returns `Ok(())` without
///   redialing (`defradb.rs/crates/p2p-adapter/src/libp2p.rs:287-298`).
/// - `add_collections` calls `subscribe_collection`, which returns
///   `Ok(false)` (not an error) when the collection is already subscribed
///   (`defradb.rs/crates/p2p/src/sync/coordinator/subscriptions.rs:15-20`).
///
/// Bug-fix round (D25 addendum) update: the gap this doc comment used to
/// name as residual is now closed. `admin_create_tenant` (console round)
/// made `reconcile::reconcile` a genuinely multi-call-per-process-lifetime
/// operation (once per live tenant create/drop), which the assumption
/// below no longer held for: `subscribe_collection` returning `Ok(false)`
/// on an already-subscribed collection means no new gossipsub SUBSCRIBE
/// message is sent, so no new `TopicPeerEvent` fires, so a wait joined
/// against that particular repeat call finds nothing to wait for and
/// times out waiting for an event that will never re-arrive -- observed
/// live: creating a tenant while an unrelated, already-`Placed` tenant's
/// re-wiring hit exactly this, aborting the new tenant's creation with a
/// 500 that named a completely different tenant.
///
/// Fixed via `confirmed_topic_joins` (owned by `burner_cell::Supervisor`,
/// threaded through by the caller): a triple already confirmed is treated
/// as an idempotent ENSURE and never joins a wait at all, so a repeat
/// `wire_group` call against an already-wired group returns immediately
/// instead of re-waiting on an event that cannot fire. Verified there is
/// still no synchronous "current topic peers" snapshot to check instead
/// (`defra_http::P2POperations`'s full method list has no such call), so
/// this in-process tracking is the correct fallback, not a shortcut.
///
/// Bug-fix round (D25, "the real bug"): that fallback still had a real
/// gap this function alone cannot close, so it is no longer this
/// function's job to try. A wait that times out here is NOT a failure:
/// `add_collections` below already ran to completion regardless (they are
/// joined, not sequenced -- see above), so the subscription is
/// established either way, and the triple is simply left out of
/// `confirmed_topic_joins` (unconfirmed, not broken) instead of failing
/// the whole tenant. Only a genuine error -- a collection missing
/// locally, the event bus itself going away -- still fails this call.
/// Callers only ever reach here for a genuinely fresh placement in this
/// process (`reconcile::reconcile`'s `Pending` branch): an already-
/// `Placed` tenant (any later reconcile, including recovery after a
/// restart) calls [`ensure_group_connected`] instead, which never waits
/// on this event at all, because upstream restores a cell's subscriptions
/// from disk at its own startup, before reconcile ever runs
/// (`p2p::sync::coordinator::subscriptions`'s "Loading persisted P2P
/// collections" log fires on every recovered cell) -- the join already
/// happened, with nobody listening, and the edge-triggered event that
/// would prove it will not fire again. Waiting on it there is not
/// narrowing a race, it is waiting on an event that cannot ever arrive.
pub async fn wire_group(
    cells: &[&RunningCell],
    collections: &[String],
    confirmed_topic_joins: &mut HashSet<TopicJoinKey>,
) -> Result<()> {
    for cell in cells {
        let p2p = cell
            .node
            .p2p()
            .with_context(|| format!("cell '{}' has no p2p system", cell.spec.id))?;
        for other in cells {
            if other.spec.id == cell.spec.id {
                continue;
            }
            let addr = other
                .dialable_addr()
                .with_context(|| format!("cell '{}' has no dialable address yet", other.spec.id))?;
            p2p.ops()
                .connect_peer(&addr)
                .await
                .map_err(|error| anyhow!(error))
                .with_context(|| {
                    format!(
                        "connecting cell '{}' to cell '{}'",
                        cell.spec.id, other.spec.id
                    )
                })?;
        }
    }

    // Each step tags its own outcome (`StepOutcome`) instead of sharing
    // one uniform `Result<()>`, so a wait's timeout and a real
    // `add_collections` failure can be told apart after `join_all` below
    // -- but they stay in ONE `Vec`, wait-steps listed before add-steps
    // exactly as before, so the polling-order guarantee above (every
    // wait's subscribe completes before any `add_collections` is polled
    // at all) is unchanged: it depends only on position within a single
    // combinator call, not on which combinator (`join_all` vs
    // `try_join_all`) drives it.
    enum StepOutcome {
        /// `Ok(true)`: joined, confirmed. `Ok(false)`: timed out,
        /// unconfirmed but not fatal. `Err`: a real problem.
        Wait {
            key: TopicJoinKey,
            result: Result<bool>,
        },
        AddCollections(Result<()>),
    }

    let mut steps: Vec<BoxFuture<'_, StepOutcome>> = Vec::new();
    for cell in cells {
        for other in cells {
            if other.spec.id == cell.spec.id {
                continue;
            }
            for collection in collections {
                let key = (
                    cell.spec.id.clone(),
                    collection.clone(),
                    other.peer_id.clone(),
                );
                if confirmed_topic_joins.contains(&key) {
                    continue;
                }
                steps.push(Box::pin(async move {
                    let result = wait_topic_peer(
                        &cell.node,
                        collection,
                        &other.peer_id,
                        TOPIC_READY_TIMEOUT,
                    )
                    .await
                    .with_context(|| {
                        format!(
                            "waiting for cell '{}' to see cell '{}' join the '{collection}' \
                             topic",
                            cell.spec.id, other.spec.id
                        )
                    });
                    StepOutcome::Wait { key, result }
                }));
            }
        }
    }
    for cell in cells.iter().copied() {
        steps.push(Box::pin(async move {
            StepOutcome::AddCollections(add_collections_on(cell, collections).await)
        }));
    }

    let mut newly_confirmed: Vec<TopicJoinKey> = Vec::new();
    for outcome in join_all(steps).await {
        match outcome {
            // A real `add_collections` failure is always fatal: unlike a
            // topic-join wait, there is nothing unobservable about it.
            StepOutcome::AddCollections(result) => result?,
            // `?` here only ever fires for a genuine non-timeout error
            // (see `StepOutcome::Wait`'s doc comment above); `Ok(false)`
            // (timed out) is deliberately not an error and simply never
            // joins `newly_confirmed`.
            StepOutcome::Wait { key, result } => {
                if result? {
                    newly_confirmed.push(key);
                }
            }
        }
    }

    // Only reached once every step resolved without a real error: a
    // partial failure leaves `confirmed_topic_joins` untouched, so a
    // retried reconcile pass correctly re-waits on whichever triples
    // never actually confirmed, rather than marking a wait "confirmed" on
    // the strength of an unrelated step's success.
    confirmed_topic_joins.extend(newly_confirmed);

    Ok(())
}

/// Re-wires an ALREADY-PLACED tenant's cell group (`reconcile::reconcile`'s
/// `Placed` branch: any reconcile after the first, including recovery
/// after a restart): connects every member to every other (idempotent,
/// upstream no-ops an already-connected dial), positively CONFIRMS that
/// connectivity via a deadline-polled `connected_peers` read rather than
/// trusting `connect_peer`'s own `Ok(())` alone, then issues
/// `add_collections` on every cell (idempotent -- see `wire_group`'s doc
/// comment for the verified upstream source lines). Deliberately never
/// waits on a topic-join event: see `wire_group`'s doc comment for why
/// that wait is unobservable here specifically. A cell that cannot be
/// positively confirmed connected within [`CONNECT_CONFIRM_TIMEOUT`] is a
/// real, observable problem, not an unobservable one, so it still fails
/// this call (and, up the stack, degrades the tenant, naming the reason).
pub async fn ensure_group_connected(cells: &[&RunningCell], collections: &[String]) -> Result<()> {
    for cell in cells.iter().copied() {
        let p2p = cell
            .node
            .p2p()
            .with_context(|| format!("cell '{}' has no p2p system", cell.spec.id))?;
        for other in cells {
            if other.spec.id == cell.spec.id {
                continue;
            }
            let addr = other
                .dialable_addr()
                .with_context(|| format!("cell '{}' has no dialable address yet", other.spec.id))?;
            p2p.ops()
                .connect_peer(&addr)
                .await
                .map_err(|error| anyhow!(error))
                .with_context(|| {
                    format!(
                        "connecting cell '{}' to cell '{}'",
                        cell.spec.id, other.spec.id
                    )
                })?;
            confirm_connected(cell, &other.peer_id, CONNECT_CONFIRM_TIMEOUT)
                .await
                .with_context(|| {
                    format!(
                        "confirming cell '{}' is connected to cell '{}'",
                        cell.spec.id, other.spec.id
                    )
                })?;
        }
    }

    try_join_all(
        cells
            .iter()
            .copied()
            .map(|cell| add_collections_on(cell, collections)),
    )
    .await?;

    Ok(())
}

/// One cell's `add_collections` call: idempotent (`Ok(false)`, not an
/// error, on an already-subscribed collection -- see `wire_group`'s doc
/// comment for the verified upstream source lines), shared by
/// [`wire_group`] and [`ensure_group_connected`] so the two call sites
/// cannot drift on it.
async fn add_collections_on(cell: &RunningCell, collections: &[String]) -> Result<()> {
    let p2p = cell
        .node
        .p2p()
        .with_context(|| format!("cell '{}' has no p2p system", cell.spec.id))?;
    p2p.ops()
        .add_collections(collections.to_vec())
        .await
        .map_err(|error| anyhow!(error))
        .with_context(|| format!("adding collections on cell '{}'", cell.spec.id))
}

/// Deadline-polls `cell`'s live `connected_peers` until `peer_id` appears,
/// the only positively-observable connectivity signal available after the
/// fact (see [`ensure_group_connected`]'s doc comment): unlike the
/// topic-join event, this is a plain snapshot re-queried each poll, not an
/// edge-triggered event a late listener can miss.
async fn confirm_connected(cell: &RunningCell, peer_id: &str, timeout: Duration) -> Result<()> {
    let p2p = cell
        .node
        .p2p()
        .with_context(|| format!("cell '{}' has no p2p system", cell.spec.id))?;
    let deadline = Instant::now() + timeout;
    loop {
        let peers = p2p
            .ops()
            .connected_peers()
            .await
            .map_err(|error| anyhow!(error))
            .context("querying connected_peers")?;
        if peers
            .iter()
            .any(|connected| crate::peer_id_of(connected) == peer_id)
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("timed out after {timeout:?} waiting to see peer '{peer_id}' connected");
        }
        tokio::time::sleep(CONNECT_CONFIRM_POLL_STEP).await;
    }
}
