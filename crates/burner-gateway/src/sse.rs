//! Server-sent events for the dashboard (Phase 5): a shared hub that
//! publishes JSON events (an overview snapshot every 2s, plus decision-log
//! entries as they appear: the publish loop lives in `gateway.rs`, next
//! to the state it reads) to every connected client.
//!
//! Bounded per-client, drop-oldest: built directly on
//! `tokio::sync::broadcast`, whose one shared ring buffer with a
//! per-receiver read cursor is exactly that contract from each client's
//! own point of view: a receiver that falls behind the buffer's
//! capacity has its cursor advanced past the entries it missed and gets
//! `RecvError::Lagged(n)`, which this module turns into a `dropped` event
//! carrying the exact count, in the very next event that client sees. The
//! sender never blocks on a slow receiver (`send` is synchronous and
//! infallible-in-practice: an error only means there are currently zero
//! subscribers, which is not a failure).
//!
//! Capped at [`MAX_CLIENTS`] simultaneous connections via a semaphore:
//! beyond that, the caller gets a 503 rather than an unbounded number of
//! held connections.

use std::convert::Infallible;
use std::sync::Arc;

use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use tokio::sync::{Semaphore, broadcast};

/// Simultaneous SSE clients allowed before new connections get 503.
pub const MAX_CLIENTS: usize = 8;
/// Ring buffer depth per the hub's single shared broadcast channel. A
/// receiver that falls more than this many events behind starts lagging;
/// generous relative to the 2s overview cadence.
const BROADCAST_CAPACITY: usize = 64;

#[derive(Clone)]
struct SseMessage {
    event: &'static str,
    json: Arc<str>,
}

pub struct SseHub {
    sender: broadcast::Sender<SseMessage>,
    capacity: Arc<Semaphore>,
}

impl SseHub {
    pub fn new() -> Self {
        let (sender, _receiver) = broadcast::channel(BROADCAST_CAPACITY);
        Self {
            sender,
            capacity: Arc::new(Semaphore::new(MAX_CLIENTS)),
        }
    }

    /// Publishes one event to every currently-subscribed client. Silently
    /// does nothing if `payload` fails to serialize (never expected: every
    /// caller passes a plain `#[derive(Serialize)]` struct) or if nobody
    /// is currently subscribed: neither is a failure worth surfacing to
    /// the publisher loop.
    pub fn publish(&self, event: &'static str, payload: &impl Serialize) {
        let Ok(json) = serde_json::to_string(payload) else {
            return;
        };
        let _ = self.sender.send(SseMessage {
            event,
            json: Arc::from(json),
        });
    }
}

impl Default for SseHub {
    fn default() -> Self {
        Self::new()
    }
}

/// Builds the SSE response for one new client connection: reserves a
/// capacity slot (503 if [`MAX_CLIENTS`] is already reached) and streams
/// every subsequent hub event to it until the client disconnects, at
/// which point the reserved slot is released automatically (it lives
/// inside the stream's own state, dropped when the stream is).
pub fn stream_response(hub: &Arc<SseHub>) -> Response {
    let Some(permit) = hub.capacity.clone().try_acquire_owned().ok() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "too many concurrent dashboard streams",
        )
            .into_response();
    };
    let receiver = hub.sender.subscribe();

    let stream = futures::stream::unfold((receiver, permit), |(mut receiver, permit)| async move {
        match receiver.recv().await {
            Ok(message) => {
                let event = Event::default()
                    .event(message.event)
                    .data(message.json.as_ref());
                Some((Ok::<_, Infallible>(event), (receiver, permit)))
            }
            Err(broadcast::error::RecvError::Lagged(dropped)) => {
                let event = Event::default().event("dropped").data(dropped.to_string());
                Some((Ok(event), (receiver, permit)))
            }
            Err(broadcast::error::RecvError::Closed) => None,
        }
    });

    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Serialize)]
    struct Payload {
        value: u32,
    }

    #[tokio::test]
    async fn publish_with_no_subscribers_does_not_panic_or_block() {
        let hub = SseHub::new();
        hub.publish("overview", &Payload { value: 1 });
    }

    #[tokio::test]
    async fn stream_response_beyond_max_clients_returns_503() {
        let hub = Arc::new(SseHub::new());
        let mut responses = Vec::new();
        for _ in 0..MAX_CLIENTS {
            responses.push(stream_response(&hub));
        }
        for response in &responses {
            assert_ne!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        }
        let over_cap = stream_response(&hub);
        assert_eq!(over_cap.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn releasing_a_client_frees_its_capacity_slot() {
        let hub = Arc::new(SseHub::new());
        let mut responses = Vec::new();
        for _ in 0..MAX_CLIENTS {
            responses.push(stream_response(&hub));
        }
        drop(responses); // disconnects every client, releasing every permit

        // A released slot is immediately reusable.
        let response = stream_response(&hub);
        assert_ne!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}
