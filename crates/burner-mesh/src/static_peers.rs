//! Dials operator-configured static peer addresses (cross-host mesh, plan
//! "Scope is host-local autoscaling plus static mesh") from every local
//! cell. Best-effort per peer: one bad address never aborts the rest of
//! `start`.

use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use burner_cell::RunningCell;
use tokio::time::Instant;

/// Deadline for confirming a successfully-dialed peer actually shows up in
/// the dialing cell's `connected_peers()` (D19): `connect_peer` returning
/// `Ok` only means the dial was accepted, not that the swarm task has
/// registered the connection yet.
const CONFIRM_DEADLINE: Duration = Duration::from_secs(10);
const CONFIRM_POLL_STEP: Duration = Duration::from_millis(250);

/// Outcome of dialing one configured peer from one cell.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PeerDialOutcome {
    pub cell_id: String,
    pub peer_addr: String,
    pub ok: bool,
    /// Populated when `ok` is false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// True once the dialed peer id was observed in this cell's own
    /// `connected_peers()` after [`confirm_dialed_peers`] deadline-polled
    /// for it (D19). Always `false` when `ok` is `false`: a dial that never
    /// succeeded has nothing to confirm.
    #[serde(default)]
    pub confirmed: bool,
    /// Populated when `ok` is true but `confirmed` is false: the dial was
    /// accepted but the connection was not observed in `connected_peers()`
    /// before the confirmation deadline elapsed. Never populated when `ok`
    /// is false (the dial's own `error` already explains that case). A
    /// confirmation timeout is recorded honestly here, never treated as a
    /// `start` failure: the dial itself already succeeded, and an unusually
    /// slow (but real) connection settling after the ready-file is written
    /// is still a live peer, just not provably so within the deadline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Dials every configured peer multiaddr (must carry a `/p2p/<peer-id>`
/// suffix; `connect_peer` itself validates and rejects a malformed
/// address, so that grammar is not re-checked here) from every cell.
///
/// Each `(cell, peer)` pair is attempted independently: a failed dial is
/// logged loudly and recorded in the returned outcome, never propagated as
/// an error that would abort the rest of `start`. A stale or unreachable
/// static peer is an expected, recoverable condition (the operator's own
/// peer list can outlive any one peer's uptime), not a startup blocker.
///
/// Returned outcomes carry `confirmed: false` and `note: None` for every
/// entry: dialing and confirming are separate passes (see
/// [`confirm_dialed_peers`]), so the caller (`defraburner start`) can do
/// other setup work between them without holding this crate's opinion on
/// when confirmation should run.
pub async fn dial_static_peers(
    cells: &[&RunningCell],
    peers: &[String],
) -> Result<Vec<PeerDialOutcome>> {
    let mut outcomes = Vec::with_capacity(cells.len() * peers.len());
    for cell in cells {
        let p2p = cell
            .node
            .p2p()
            .with_context(|| format!("cell '{}' has no p2p system", cell.spec.id))?;
        for peer in peers {
            match p2p.ops().connect_peer(peer).await {
                Ok(()) => {
                    tracing::info!(cell_id = %cell.spec.id, peer = %peer, "dialed static peer");
                    outcomes.push(PeerDialOutcome {
                        cell_id: cell.spec.id.clone(),
                        peer_addr: peer.clone(),
                        ok: true,
                        error: None,
                        confirmed: false,
                        note: None,
                    });
                }
                Err(error) => {
                    let error = anyhow!(error);
                    tracing::error!(
                        cell_id = %cell.spec.id,
                        peer = %peer,
                        error = %error,
                        "failed to dial static peer"
                    );
                    outcomes.push(PeerDialOutcome {
                        cell_id: cell.spec.id.clone(),
                        peer_addr: peer.clone(),
                        ok: false,
                        error: Some(error.to_string()),
                        confirmed: false,
                        note: None,
                    });
                }
            }
        }
    }
    Ok(outcomes)
}

/// Deadline-polls every successfully-dialed peer in `outcomes` into its
/// dialing cell's live `connected_peers()`, filling in `confirmed`/`note`
/// in place (D19). The caller (`defraburner start`) runs this after
/// [`dial_static_peers`] and before the ready-file is written, so the
/// ready-file's own `connected_peers` snapshot
/// (`Supervisor::status_with_connected_peers`) reflects a settled
/// connection rather than racing the swarm task that registers it.
///
/// Entries with `ok == false` are left untouched (`confirmed` stays
/// `false`, `note` stays `None`): there is nothing to confirm for a dial
/// that never succeeded.
pub async fn confirm_dialed_peers(cells: &[&RunningCell], outcomes: &mut [PeerDialOutcome]) {
    for outcome in outcomes.iter_mut() {
        if !outcome.ok {
            continue;
        }
        let Some(peer_id) = peer_id_suffix(&outcome.peer_addr) else {
            outcome.note = Some(
                "configured peer address carries no /p2p/<id> suffix; cannot confirm".to_string(),
            );
            continue;
        };
        let Some(cell) = cells.iter().find(|cell| cell.spec.id == outcome.cell_id) else {
            outcome.note = Some(format!(
                "cell '{}' not found for dial confirmation",
                outcome.cell_id
            ));
            continue;
        };

        outcome.confirmed = wait_for_connected_peer(cell, peer_id).await;
        if !outcome.confirmed {
            tracing::warn!(
                cell_id = %outcome.cell_id,
                peer = %outcome.peer_addr,
                deadline = ?CONFIRM_DEADLINE,
                "dialed peer not observed in connected_peers before the confirmation deadline"
            );
            outcome.note = Some(format!(
                "dialed but not observed in connected_peers within {CONFIRM_DEADLINE:?}"
            ));
        }
    }
}

/// Extracts the trailing `/p2p/<peer-id>` suffix from a configured peer
/// multiaddr, e.g. `/ip4/127.0.0.1/tcp/9171/p2p/12D3Koo` -> `12D3Koo`.
/// `None` if the address carries no `/p2p/` component; defensive rather
/// than trusted, since a malformed address here would have already failed
/// `connect_peer` and never reached this function with `ok == true`.
fn peer_id_suffix(multiaddr: &str) -> Option<&str> {
    multiaddr.rsplit_once("/p2p/").map(|(_, id)| id)
}

/// Deadline-polls `cell`'s `connected_peers()` on a fixed step until
/// `peer_id` appears (each listed entry is an address embedding the id,
/// so it is normalized through [`crate::peer_id_of`] before comparing),
/// or `CONFIRM_DEADLINE` elapses. A query error is
/// logged and retried rather than failing the poll outright: a transient
/// error querying `connected_peers` is not proof the connection is absent.
async fn wait_for_connected_peer(cell: &RunningCell, peer_id: &str) -> bool {
    let Some(p2p) = cell.node.p2p() else {
        return false;
    };
    let deadline = Instant::now() + CONFIRM_DEADLINE;
    loop {
        match p2p.ops().connected_peers().await {
            Ok(peers) => {
                if peers
                    .iter()
                    .any(|connected| crate::peer_id_of(connected) == peer_id)
                {
                    return true;
                }
            }
            Err(error) => {
                tracing::warn!(
                    cell_id = %cell.spec.id,
                    peer_id,
                    error = %error,
                    "querying connected_peers during dial confirmation failed"
                );
            }
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(CONFIRM_POLL_STEP).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peer_dial_outcome_omits_error_and_note_field_when_ok_and_confirmed() {
        let outcome = PeerDialOutcome {
            cell_id: "cell-0".to_string(),
            peer_addr: "/ip4/127.0.0.1/tcp/9171/p2p/12D3Koo".to_string(),
            ok: true,
            error: None,
            confirmed: true,
            note: None,
        };
        let json = serde_json::to_string(&outcome).unwrap();
        assert!(!json.contains("\"error\""));
        assert!(!json.contains("\"note\""));
        assert!(json.contains("\"confirmed\":true"));
    }

    #[test]
    fn peer_dial_outcome_round_trips_with_an_error() {
        let outcome = PeerDialOutcome {
            cell_id: "cell-0".to_string(),
            peer_addr: "/ip4/127.0.0.1/tcp/9171/p2p/12D3Koo".to_string(),
            ok: false,
            error: Some("connection refused".to_string()),
            confirmed: false,
            note: None,
        };
        let json = serde_json::to_string(&outcome).unwrap();
        let parsed: PeerDialOutcome = serde_json::from_str(&json).unwrap();
        assert_eq!(outcome.error, parsed.error);
        assert!(!parsed.ok);
        assert!(!parsed.confirmed);
    }

    #[test]
    fn peer_dial_outcome_round_trips_a_dialed_but_unconfirmed_note() {
        let outcome = PeerDialOutcome {
            cell_id: "cell-0".to_string(),
            peer_addr: "/ip4/127.0.0.1/tcp/9171/p2p/12D3Koo".to_string(),
            ok: true,
            error: None,
            confirmed: false,
            note: Some("dialed but not observed in connected_peers within 10s".to_string()),
        };
        let json = serde_json::to_string(&outcome).unwrap();
        let parsed: PeerDialOutcome = serde_json::from_str(&json).unwrap();
        assert_eq!(outcome.note, parsed.note);
        assert!(parsed.ok);
        assert!(!parsed.confirmed);
    }

    #[test]
    fn peer_dial_outcome_deserializes_a_pre_d19_ready_file_without_the_new_fields() {
        // Ready-files predating D19 have no `confirmed`/`note` fields;
        // both must default rather than fail to parse.
        let json =
            r#"{"cell_id":"cell-0","peer_addr":"/ip4/127.0.0.1/tcp/9171/p2p/12D3Koo","ok":true}"#;
        let parsed: PeerDialOutcome = serde_json::from_str(json).unwrap();
        assert!(!parsed.confirmed);
        assert!(parsed.note.is_none());
    }

    #[test]
    fn peer_id_suffix_extracts_the_trailing_p2p_component() {
        assert_eq!(
            peer_id_suffix("/ip4/127.0.0.1/tcp/9171/p2p/12D3Koo"),
            Some("12D3Koo")
        );
        assert_eq!(peer_id_suffix("/ip4/127.0.0.1/tcp/9171"), None);
    }

    #[tokio::test]
    async fn confirm_dialed_peers_leaves_failed_dials_unconfirmed_and_unnoted() {
        let mut outcomes = vec![PeerDialOutcome {
            cell_id: "cell-0".to_string(),
            peer_addr: "/ip4/127.0.0.1/tcp/9171/p2p/12D3Koo".to_string(),
            ok: false,
            error: Some("connection refused".to_string()),
            confirmed: false,
            note: None,
        }];
        // No cells at all: if the `ok == false` skip didn't work, this
        // would panic looking a cell up in an empty slice instead of
        // short-circuiting before that point.
        confirm_dialed_peers(&[], &mut outcomes).await;
        assert!(!outcomes[0].confirmed);
        assert!(outcomes[0].note.is_none());
    }

    #[tokio::test]
    async fn confirm_dialed_peers_notes_a_missing_cell() {
        let mut outcomes = vec![PeerDialOutcome {
            cell_id: "ghost-cell".to_string(),
            peer_addr: "/ip4/127.0.0.1/tcp/9171/p2p/12D3Koo".to_string(),
            ok: true,
            error: None,
            confirmed: false,
            note: None,
        }];
        confirm_dialed_peers(&[], &mut outcomes).await;
        assert!(!outcomes[0].confirmed);
        assert!(
            outcomes[0]
                .note
                .as_ref()
                .is_some_and(|note| note.contains("not found"))
        );
    }

    // dial_static_peers and confirm_dialed_peers's real-connection path both
    // need a real RunningCell (a live embedded node), so that behavior is
    // exercised by the in-process gate test
    // (crates/defraburner/tests/two_process_mesh.rs) rather than duplicated
    // with a fake here.
}
