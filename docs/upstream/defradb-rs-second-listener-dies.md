# defradb.rs: with N>1 embedded nodes in one process, every libp2p TCP listener after the first dies shortly after startup

Status: reproduced deterministically on 2026-08-19 (17/17 attempts across
two independent investigations). Affects sourcenetwork/defradb.rs at the
workspace state of ~2026-08-18 (v0.5.0), consumed via the `embedded` crate
(`build_with_store` + `TransportConfig::Libp2p`). Filed from the defraburner
project, which runs N `embedded::EmbeddedNode`s in one process.

## Symptom

Ignite two (or more) embedded nodes in ONE process, each with its own
`Libp2pConfig { listen_addr: "/ip4/127.0.0.1/tcp/<distinct port>" }`.
Every node reports a healthy listener through the API:

- `P2POperations::listen_addresses()` returns the multiaddr for every node.
- `libp2p_tcp` logs `listening on 127.0.0.1:<port>` for every node.

But at the OS level only the FIRST node's port is actually listening:

```
$ ss -tln | grep -E "395(00|01)"
LISTEN 0 1024 127.0.0.1:39500 0.0.0.0:*        # node 1: present
                                               # node 2 (39501): ABSENT
```

A TCP connect to the second port returns ECONNREFUSED (no listener), not a
timeout. With 3 nodes, the LAST node's listener dies; earlier ones survive.
A debug trace shows the second listener's teardown happening unprompted
about 500 ms after creation:

```
[libp2p_tcp] listening on 127.0.0.1:39002
[libp2p_tcp] Registering for port reuse ip=127.0.0.1 port=39002
[libp2p_swarm] New listener address /ip4/127.0.0.1/tcp/39002
... ~500ms later, no API call in between ...
[libp2p_tcp] Unregistering for port reuse ip=127.0.0.1 port=39002
```

"Unregistering for port reuse" fires when a libp2p-tcp listener stream is
dropped, so something drops the newest listener while the node's own API
continues to advertise it.

## Why in-process replication tests never catch this

Node-to-node wiring performed immediately after ignition (the pattern in
defradb.rs's own multi-node tests and in defraburner's in-process tenant
wiring) completes inside the ~500 ms window, and established connections
survive the listener teardown. Only a LATE inbound dial (from another
process or host, seconds after startup) hits the dead listener: that is
exactly the cross-process mesh case.

## Repro (from defraburner, but any two-embedded-node harness works)

```
# defraburner checkout sibling to defradb.rs
cargo build -p defraburner
RUST_LOG=libp2p_tcp=debug ./target/debug/defraburner start \
  --data-root /tmp/repro --cells 2 --base-port 39500 \
  --ready-file /tmp/repro-ready.json
# wait for the ready file, then:
ss -tln | grep -E "395(00|01)"     # only 39500 present
```

Minimal upstream repro sketch: build two `embedded::EmbeddedNode`s with
libp2p on fixed distinct loopback ports in one tokio process, sleep 2 s,
then check both ports with ss or a raw connect.

## Ruled out

- Resource exhaustion: conntrack 94/262144, 1 TIME_WAIT socket, ulimit
  1048576 at repro time.
- Port collision: distinct fixed ports, freshly picked.
- Host load: reproduces identically on a quiet run; the load only affected
  how often the old flaky-looking cross-process test lost the race window.

## Where to look

The teardown is inside the p2p host / swarm lifetime, not defraburner
(defraburner holds every node behind Arcs in a supervisor map and never
drops the p2p system). Suspects, in order: listener or transport state in
`crates/p2p`'s host construction that is inadvertently keyed or shared
process-wide (the newest instance surviving-or-dying pattern suggests a
last-writer-wins registration), and libp2p-tcp port-reuse bookkeeping
interacting badly with multiple `Transport` instances in one process.

## Impact on embedders until fixed

Inbound dials to any node after the first fail once the window closes;
outbound dialing from every node keeps working, and connections formed
early survive. Cross-process/cross-host meshes should dial OUT from
later-ignited nodes toward first-ignited nodes as a workaround topology.
