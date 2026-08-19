# defradb.rs: cosmetic WARN on every ignition, "failed to decode replicator info peer_id=__local_p2p_identity__"

Status: observed on every cell ignition, live run 2026-08-19. Affects
sourcenetwork/defradb.rs at the workspace state consumed via the
`embedded` crate. Filed from the defraburner project (cosmetic only, not
a functional defect: no known-effect on correctness or wiring).

## Symptom

Every cell ignition (`burner_cell::cell::ignite`, fresh provision,
restart recovery, or a watchdog re-ignite) logs one WARN:

```
WARN embedded::node_recovery: failed to decode replicator info
  peer_id="__local_p2p_identity__" error=...
```

## Root cause (read, not modified: `defradb.rs/crates/embedded/src/node_recovery.rs:32`)

Upstream's peerstore stores this process's own libp2p identity keypair
under a well-known sentinel key (`__local_p2p_identity__`) in the same
storage namespace it uses for replicator records. `node_recovery`'s
replicator-restore pass iterates every entry in that namespace at
ignition and tries to decode each one as `ReplicatorInfo`; the sentinel
identity entry is not a replicator record, so the decode fails and this
WARN fires. The recovery pass itself is unaffected (it correctly skips
the entry after the failed decode and continues restoring the real
replicators), so this has no observed effect on wiring or replication.

## Why this is not suppressed in defraburner's default `RUST_LOG`

`embedded::node_recovery` is the same logging target as three other,
genuinely useful WARNs (`failed to restore replicator`, `failed to
restore collection topic`, `failed to load replicators from storage`).
A target-level filter cannot distinguish this one benign, deterministic
message from those; quieting the whole target would hide real recovery
failures along with the cosmetic one. Left at `warn` (the default), on
purpose, per the same reasoning documented for the sync-broadcaster/
libp2p-kad noise in the console round's log-hygiene pass.

## Recommended upstream fix (not applied here; this repo never modifies `defradb.rs`)

Skip the `__local_p2p_identity__` sentinel key explicitly before
attempting the `ReplicatorInfo` decode in `node_recovery.rs`, or store
the local identity under a distinct namespace/prefix from replicator
records so a `peerstore` iteration never conflates the two.
