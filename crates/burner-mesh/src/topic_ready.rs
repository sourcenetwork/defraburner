//! D13: a deterministic wait for gossipsub topic-mesh formation, built on
//! the per-cell event bus. Used both by tenant group wiring
//! (`wiring::wire_group`) before declaring a group ready, and by the
//! Phase 0 spike before its post-subscription write: the flake D13 fixes
//! was a write published immediately after `add_collections`, racing
//! topic-mesh formation and occasionally failing with a libp2p gossipsub
//! `InsufficientPeers` publish error.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use tokio::time::Instant;

/// Waits until `peer_id` is observed joining `collection`'s gossipsub topic
/// on `node`, returning `Ok(true)`. A deadline elapsing without seeing it
/// returns `Ok(false)`, not an error (D25 "the real bug" fix): the event
/// is edge-triggered (see this comment's own race discussion below) and
/// can be missed by nobody having listened yet -- most notably, upstream
/// silently restores a cell's subscriptions from disk at its own startup
/// (`p2p::sync::coordinator::subscriptions`'s "Loading persisted P2P
/// collections" log), before any caller here ever runs, so the join
/// already happened and will not fire again. That is a genuinely
/// unobservable outcome, not evidence of a real failure, so it is not
/// this function's place to call it an error; `wiring::wire_group`'s doc
/// comment covers what its caller does with a `false`. `Err` is reserved
/// for an actual problem: the collection not existing locally yet, or the
/// event bus itself going away mid-wait.
///
/// Subscribes to the event bus for `TopicPeerEvent` *before* checking
/// anything else: the subscription is live from that point on, so a JOINED
/// event published from here onward can never be missed. It then drains
/// whatever is already buffered on the fresh subscription before falling
/// into the deadline loop, so a peer that joined moments earlier (e.g.
/// during this same wiring pass's own `add_collections` call) resolves
/// instantly instead of waiting out the deadline.
///
/// This narrows, but cannot fully eliminate, the race: the upstream P2P
/// surface exposes no synchronous "current topic peers" snapshot, only
/// this edge-triggered event stream (verified: `defra_http::P2POperations`
/// has no such method), so an event that already fired, with nobody
/// subscribed, before this call's own `subscribe` registers is genuinely
/// unrecoverable, since gossipsub delivery runs on the cell's own
/// background swarm task, independent of the caller's. That is a real
/// improvement over zero wait (D13's actual bug), not a provable fix; see
/// `wiring::wire_group`'s doc comment for why every call site today still
/// gets the intended behavior in practice.
///
/// Generic over the store backend (`S`), not fixed to
/// `embedded::EmbeddedStore`: this primitive also has to serve the Phase 0
/// spike, whose cells are built directly over `storage::LarkStore` rather
/// than through the `EmbeddedStore` enum burner-cell's cells use.
pub async fn wait_topic_peer<S: storage::corekv::Store + 'static>(
    node: &embedded::EmbeddedNode<S>,
    collection: &str,
    peer_id: &str,
    timeout: Duration,
) -> Result<bool> {
    // Subscribed first, deliberately before resolving the topic id below:
    // every statement after this line risks costing time the background
    // swarm task could use to fire (and, unsubscribed, drop) the event
    // we're here to catch.
    let mut subscription = node
        .event_bus
        .subscribe(&[events::EventName::TopicPeerEvent]);

    let topic = resolve_collection_topic(node, collection).with_context(|| {
        format!("resolving topic id for collection '{collection}' before waiting on it")
    })?;

    if drain_for_match(&mut subscription, &topic, peer_id) {
        return Ok(true);
    }

    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(false);
        }
        match tokio::time::timeout(remaining, subscription.recv()).await {
            Ok(Some(message)) => {
                if topic_peer_matches(&message, &topic, peer_id) {
                    return Ok(true);
                }
            }
            Ok(None) => bail!(
                "event bus subscription closed while waiting for peer '{peer_id}' on \
                 collection '{collection}'"
            ),
            Err(_) => return Ok(false),
        }
    }
}

/// Resolves a human-readable collection name to the gossipsub topic string
/// `add_collections` actually subscribes.
///
/// Verified (D13): upstream's `P2POperations::add_collections` subscribes
/// `DefraTopic::Collection(collection_id)`, the collection's
/// content-addressed id, never the bare name
/// (`defradb.rs/crates/p2p-adapter/src/libp2p.rs` resolves
/// `get_collection_id` before calling `subscribe_collection`;
/// `defradb.rs/crates/p2p/src/topics.rs`'s `Collection(id).topic_string()`
/// returns that id unchanged), and `TopicPeerEventData::topic` carries that
/// same id verbatim (`defradb.rs/crates/embedded/src/node_tasks.rs`
/// publishes it straight from the libp2p `gossipsub::Event::Subscribed`
/// topic hash). `node.database.get_collection` is a cheap, synchronous,
/// in-memory lookup (no `spawn_blocking` needed), and needs no extra crate
/// dependency here: its return type is never named, only inferred through
/// the closure below.
fn resolve_collection_topic<S: storage::corekv::Store + 'static>(
    node: &embedded::EmbeddedNode<S>,
    collection: &str,
) -> Result<String> {
    node.database
        .get_collection(collection)
        .map_err(|error| anyhow!("looking up collection '{collection}': {error}"))?
        .map(|found| found.collection_id().to_string())
        .ok_or_else(|| {
            anyhow!("collection '{collection}' not found on this node; add_schema must run first")
        })
}

/// Drains whatever is already buffered on `subscription` without blocking,
/// returning `true` the moment a JOINED event for `topic`/`peer_id` is
/// found.
fn drain_for_match(subscription: &mut events::Subscription, topic: &str, peer_id: &str) -> bool {
    loop {
        match subscription.try_recv() {
            Ok(message) => {
                if topic_peer_matches(&message, topic, peer_id) {
                    return true;
                }
            }
            Err(_) => return false,
        }
    }
}

/// True if `message` is a `TopicPeerEvent` reporting `peer_id` having
/// JOINED `topic`. Split out from the recv loops so it is unit-testable
/// without a running node.
fn topic_peer_matches(message: &events::Message, topic: &str, peer_id: &str) -> bool {
    message.as_topic_peer_event().is_some_and(|data| {
        data.topic == topic && data.peer_id == peer_id && data.event_type == "JOINED"
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use events::{Bus, ChannelBus, Message, TopicPeerEventData};

    fn joined(topic: &str, peer_id: &str) -> Message {
        Message::topic_peer_event(TopicPeerEventData {
            peer_id: peer_id.to_string(),
            topic: topic.to_string(),
            event_type: "JOINED".to_string(),
        })
    }

    fn left(topic: &str, peer_id: &str) -> Message {
        Message::topic_peer_event(TopicPeerEventData {
            peer_id: peer_id.to_string(),
            topic: topic.to_string(),
            event_type: "LEFT".to_string(),
        })
    }

    #[test]
    fn matches_a_joined_event_for_the_exact_topic_and_peer() {
        assert!(topic_peer_matches(
            &joined("topic-a", "peer-1"),
            "topic-a",
            "peer-1"
        ));
    }

    #[test]
    fn rejects_a_left_event() {
        assert!(!topic_peer_matches(
            &left("topic-a", "peer-1"),
            "topic-a",
            "peer-1"
        ));
    }

    #[test]
    fn rejects_a_different_topic_or_peer() {
        assert!(!topic_peer_matches(
            &joined("topic-a", "peer-1"),
            "topic-b",
            "peer-1"
        ));
        assert!(!topic_peer_matches(
            &joined("topic-a", "peer-1"),
            "topic-a",
            "peer-2"
        ));
    }

    #[test]
    fn rejects_a_non_topic_peer_event() {
        assert!(!topic_peer_matches(&Message::merge(), "topic-a", "peer-1"));
    }

    #[test]
    fn drain_for_match_finds_a_buffered_match_without_blocking() {
        let bus = ChannelBus::new();
        let mut subscription = bus.subscribe(&[events::EventName::TopicPeerEvent]);
        bus.publish(joined("topic-a", "peer-1"));
        assert!(drain_for_match(&mut subscription, "topic-a", "peer-1"));
    }

    #[test]
    fn drain_for_match_skips_non_matching_and_finds_the_match_behind_it() {
        let bus = ChannelBus::new();
        let mut subscription = bus.subscribe(&[events::EventName::TopicPeerEvent]);
        bus.publish(joined("topic-a", "peer-other"));
        bus.publish(left("topic-a", "peer-1"));
        bus.publish(joined("topic-a", "peer-1"));
        assert!(drain_for_match(&mut subscription, "topic-a", "peer-1"));
    }

    #[test]
    fn drain_for_match_returns_false_when_empty() {
        let bus = ChannelBus::new();
        let mut subscription = bus.subscribe(&[events::EventName::TopicPeerEvent]);
        assert!(!drain_for_match(&mut subscription, "topic-a", "peer-1"));
    }
}
