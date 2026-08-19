# defraburner: an afterburner-governed DefraDB cluster in one binary.
# `just start` is the front door: build it and land on the dashboard,
# no arguments required.

# Show every recipe (this list).
default:
    @just --list

# Installs the two build-time tools this repo needs and nothing else can
# provide: the `burn` CLI (the afterburner toolchain) and `javy` (pinned
# to the version `burn compile` requires), both into $HOME/.local/bin,
# no root. Idempotent: an already-correct tool is left alone, so running
# it twice is free. Everything it cannot install without a package
# manager (a Rust toolchain, libclang, zstd, tar) is checked and reported
# with the exact command to fix it, never silently assumed. Override the
# install dir with DEFRABURNER_BIN.

# Install/verify the build tooling (burn, javy) and check prerequisites.
setup:
    #!/usr/bin/env bash
    set -euo pipefail
    bin="${DEFRABURNER_BIN:-$HOME/.local/bin}"
    javy_want="8.1.1"
    mkdir -p "$bin"
    missing=0

    have() { command -v "$1" >/dev/null 2>&1; }

    # ----- burn (afterburner CLI) --------------------------------------
    if have burn; then
        echo "burn:  $(burn --version 2>/dev/null || echo present)"
    else
        echo "burn:  installing via https://afterburner.sh"
        BURN_INSTALL="$bin" curl -fsSL https://afterburner.sh | sh
        have burn || echo "burn:  installed to $bin (add it to PATH)"
    fi

    # ----- javy (burn compile's AOT backend, version-pinned) -----------
    javy_have=""
    if have javy; then javy_have="$(javy --version 2>/dev/null | awk '{print $2}')"; fi
    if [ "$javy_have" = "$javy_want" ]; then
        echo "javy:  $javy_have"
    else
        case "$(uname -s)" in
            Linux)  jos="linux" ;;
            Darwin) jos="macos" ;;
            *) echo "javy:  unsupported OS $(uname -s); install javy $javy_want manually" >&2; missing=1; jos="" ;;
        esac
        case "$(uname -m)" in
            x86_64|amd64)  jarch="x86_64" ;;
            aarch64|arm64) jarch="arm" ;;
            *) echo "javy:  unsupported arch $(uname -m); install javy $javy_want manually" >&2; missing=1; jarch="" ;;
        esac
        if [ -n "$jos" ] && [ -n "$jarch" ]; then
            asset="javy-${jarch}-${jos}-v${javy_want}.gz"
            base="https://github.com/bytecodealliance/javy/releases/download/v${javy_want}"
            tmp="$(mktemp -d)"
            trap 'rm -rf "$tmp"' EXIT
            echo "javy:  downloading $asset"
            curl -fsSL -o "$tmp/$asset" "$base/$asset"
            # Pinned by version AND checked against the published digest
            # before anything is executed: a truncated or tampered
            # download fails here rather than at compile time.
            if curl -fsSL -o "$tmp/$asset.sha256" "$base/$asset.sha256"; then
                want_sum="$(awk '{print $1}' "$tmp/$asset.sha256")"
                got_sum="$(sha256sum "$tmp/$asset" | awk '{print $1}')"
                if [ "$want_sum" != "$got_sum" ]; then
                    echo "javy:  SHA-256 mismatch (want $want_sum, got $got_sum); refusing to install" >&2
                    exit 1
                fi
                echo "javy:  sha256 verified"
            else
                echo "javy:  no published .sha256 for $asset; refusing to install unverified" >&2
                exit 1
            fi
            gunzip -c "$tmp/$asset" > "$tmp/javy"
            chmod +x "$tmp/javy"
            mv "$tmp/javy" "$bin/javy"
            echo "javy:  installed $javy_want to $bin/javy"
        fi
    fi

    # ----- prerequisites we cannot install without a package manager ---
    if have cargo; then
        echo "cargo: $(cargo --version 2>/dev/null)"
    else
        echo "cargo: MISSING. install a Rust toolchain: curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh" >&2
        missing=1
    fi
    for tool in zstd tar; do
        if have "$tool"; then echo "$tool:  present"; else echo "$tool:  MISSING (needed by 'just packages')" >&2; missing=1; fi
    done
    # libclang is a genuine build requirement (afterburner's dependency
    # graph compiles rquickjs with bindgen unconditionally, even for the
    # wasm-only feature set we use), and it is the one prerequisite whose
    # absence fails the build late and confusingly, so it is checked here.
    # Deliberately not `ldconfig -p`: ldconfig lives in an sbin dir that is
    # not on a normal user's PATH, so that check reports a false MISSING on
    # a machine that builds fine. Look for the library where it actually
    # lands instead, and ask llvm-config (which bindgen itself consults).
    libclang_found=""
    for candidate in \
        ${LIBCLANG_PATH:+"$LIBCLANG_PATH"/libclang.so*} \
        /usr/lib/libclang.so* /usr/lib64/libclang.so* \
        /usr/local/lib/libclang.so* /usr/lib/llvm*/lib/libclang.so* \
        /usr/lib/*/libclang.so*; do
        [ -e "$candidate" ] && { libclang_found="$candidate"; break; }
    done
    if [ -z "$libclang_found" ] && have llvm-config; then
        libdir="$(llvm-config --libdir 2>/dev/null || true)"
        if [ -n "$libdir" ]; then
            for candidate in "$libdir"/libclang.so* "$libdir"/libclang.dylib; do
                [ -e "$candidate" ] && { libclang_found="$candidate"; break; }
            done
        fi
    fi
    if [ -n "$libclang_found" ]; then
        echo "libclang: $libclang_found"
    else
        echo "libclang: MISSING. install your distro's clang/libclang package (arch: sudo pacman -S clang; debian/ubuntu: sudo apt install libclang-dev; fedora: sudo dnf install clang-devel)" >&2
        missing=1
    fi

    case ":$PATH:" in
        *":$bin:"*) ;;
        *) echo "note: $bin is not on PATH; add it so 'burn' and 'javy' resolve" >&2 ;;
    esac

    if [ "$missing" -ne 0 ]; then
        echo "setup: some prerequisites are missing (listed above); install them, then re-run 'just setup'" >&2
        exit 1
    fi
    echo "setup: ready. next: just start"

# THE front door: builds (release-fast: thin LTO, fast to rebuild, fast
# enough to actually run) and runs `defraburner up`: recovers the
# default data root if a cluster already lives there, else provisions a
# fresh single cell, then prints the dashboard URL (with the admin token
# already in it) and best-effort opens it in a browser. Zero-config
# contract: no flag is ever required for this to work, including on a
# second concurrent run against a different data root (the gateway and
# libp2p ports both scan past an already-occupied default). Depends on
# `packages` so a fresh clone never hits the "policy wasm not built"
# panic on its first run. Extra args pass straight through to `up`, e.g.
# `just start --no-open` or `just start --cells 3`. RUST_LOG defaults to
# keeping our own info logs while quieting dependency noise (libp2p,
# hyper, tower, and libp2p_kad's own DHT routing chatter: generically
# noisy in any small/isolated network, not specific to our tenant
# replication groups); a caller-set RUST_LOG always wins.
#
# Deliberately NOT quieted here (bug-fix round log-hygiene pass): a
# single-cell cluster legitimately logs WARN/ERROR from
# `p2p::sync::broadcaster`/`db_merge::broadcast_mutator`
# ("InsufficientPeers", expected with no replication peers) and one WARN
# from `embedded::node_recovery` per ignition (cosmetic upstream defect,
# see docs/upstream/defradb-rs-replicator-info-decode-warning.md). Every
# one of these three targets also carries genuinely useful WARN/ERROR
# messages for a multi-cell cluster's real replication failures, and a
# tracing target filter cannot distinguish "expected because this cell is
# alone" from "concerning because it should have peers" by message
# content: quieting the target would hide the second case along with
# the first, so all three stay at their default level. The dashboard's
# mesh panel says plainly, in its cluster caption, that a single-cell
# cluster does not replicate, so this noise reads correctly instead.

# Build everything and run a cluster: opens the console in a browser.
start *ARGS: packages
    #!/usr/bin/env bash
    set -euo pipefail
    export RUST_LOG="${RUST_LOG:-info,libp2p=warn,libp2p_swarm=warn,libp2p_gossipsub=warn,libp2p_kad=error,hyper=warn,tower=warn}"
    cargo run --profile release-fast -p defraburner -- up {{ARGS}}

# Alias of `start`, for anyone thinking in subcommand terms.
up *ARGS:
    @just start {{ARGS}}

# Deletes the default data root ($DEFRABURNER_DATA, else
# $HOME/.local/share/defraburner) so the next `just start` provisions a
# fresh single-cell cluster. Destroys every cell's data, every tenant and
# its documents, the signing keys, the admin token, and the decision log:
# it prints what it is about to remove first. Pass a path to wipe a
# different data root, e.g. `just reset-data /tmp/demo`.

# Delete a data root (default: the one `just start` uses).
reset-data *ROOT:
    #!/usr/bin/env bash
    set -euo pipefail
    root="{{ROOT}}"
    if [ -z "$root" ]; then
        root="${DEFRABURNER_DATA:-$HOME/.local/share/defraburner}"
    fi
    if [ ! -d "$root" ]; then
        echo "nothing to remove: $root does not exist"
        exit 0
    fi
    echo "removing $root"
    if [ -f "$root/cluster.json" ]; then
        python3 - "$root/cluster.json" <<'PY' || true
    import json, sys
    with open(sys.argv[1]) as f:
        m = json.load(f)
    cells = [c.get("id") for c in m.get("cells", [])]
    tenants = [t.get("name") for t in m.get("tenants", [])]
    print(f"  cells:   {len(cells)} {cells}")
    print(f"  tenants: {len(tenants)} {tenants}")
    PY
    fi
    du -sh "$root" 2>/dev/null | awk '{print "  size:    " $1}' || true
    rm -rf "$root"
    echo "removed. the next 'just start' will provision a fresh cluster."

# Wipe the data root and immediately start a fresh cluster.
fresh *ARGS:
    @just reset-data
    @just start {{ARGS}}

# Prints the admin token for the default data root
# ($DEFRABURNER_DATA, else $HOME/.local/share/defraburner), if a cluster
# has been provisioned there.

# Print the admin token for the default data root.
token:
    #!/usr/bin/env bash
    set -euo pipefail
    root="${DEFRABURNER_DATA:-$HOME/.local/share/defraburner}"
    path="$root/admin.token"
    if [ -f "$path" ]; then
        cat "$path"
    else
        echo "no admin token found at $path: run 'just start' first" >&2
        exit 1
    fi

# Format, lint, doc-build, and test the whole workspace. Depends on
# `packages` (D9/D17a): burner-policy's build.rs embeds the default
# policies' AOT-compiled wasm at compile time, so it must exist first.

# Format, lint, doc-build, and test the whole workspace.
gate: packages
    cargo fmt --check
    cargo clippy --all-targets -- -D warnings
    RUSTDOCFLAGS="-D warnings" cargo doc --no-deps
    cargo test

# The actual shipped artifact: fat-LTO, stripped, dist profile.
build-release: packages
    cargo build --profile dist

# Same profile `just start` runs on (`release-fast`, thin LTO): a plain
# optimized build without actually launching the binary.
# Optimized build without launching it (same profile as `start`).
build-release-fast: packages
    cargo build --profile release-fast

# AOT-compile every packages/* burn package (D9/D17a): `burn compile`
# ahead-of-time compiles source/ to a self-contained wasm32-wasip1 module
# via javy and bundles it into a `.afb` alongside the source; this recipe
# then extracts that module into packages/<name>/.build/main.wasm, the path
# crates/burner-policy/build.rs embeds by `include_bytes!`. Requires `burn`
# and `javy` 8.1.1 on PATH (build-time only; the shipped binary never
# shells out to either).
# AOT-compile the policy packages into embeddable wasm.
packages:
    #!/usr/bin/env bash
    set -euo pipefail
    # Fail with the fix rather than a bare "command not found" from the
    # first loop iteration.
    for tool in burn javy zstd tar; do
        command -v "$tool" >/dev/null 2>&1 || {
            echo "packages: '$tool' not found on PATH. run 'just setup' first." >&2
            exit 1
        }
    done
    for dir in packages/*/; do
        dir="${dir%/}"
        [ -f "$dir/afb.toml" ] || continue
        echo "packages: compiling $dir"
        (cd "$dir" && burn compile .)
        afb_file=$(ls -t "$dir"/*.afb | head -n1)
        tmp=$(mktemp -d)
        zstd -dc "$afb_file" | tar -x -C "$tmp"
        mkdir -p "$dir/.build"
        mv "$tmp/precompiled/wasm32-wasip1/main.wasm" "$dir/.build/main.wasm"
        rm -rf "$tmp"
    done

# Phase 6 perf harness ("measure or it did not happen"): builds the dist
# binary, then runs loadgen against a 1-cell cluster and a 3-cell cluster,
# each with one tenant (--replicas 1, so the tenant always lands on exactly
# 1 cell). The 3-cell run's point is gateway+process overhead at a higher
# cell count, not more admitted throughput: both runs are gated by the same
# single-tenant default GCRA admission (200 req/s, burst 100), so loadgen's
# 3-thread tight loop intentionally saturates that ceiling on both runs.
# `err=` in each LOADGEN line is therefore mostly real 429 admission
# rejects (expected, not a bug), and `p50_ms`/`p99_ms`: the latency of
# the requests that *were* admitted: are the actual overhead signal this
# recipe measures. Finishes by re-running the golden recovery test and
# printing its RECOVERY timing lines.
# Measure throughput, latency, and recovery timings.
perf: build-release
    #!/usr/bin/env bash
    set -euo pipefail
    bin=target/dist/defraburner
    tmp=$(mktemp -d)
    trap 'pkill -f "$tmp" >/dev/null 2>&1 || true; rm -rf "$tmp"' EXIT

    schema="$tmp/loadtest.graphql"
    echo 'type LoadTest { value: String }' > "$schema"
    query='{"query": "query { LoadTest { value } }"}'

    # Deadline-polls for a ready-file rather than a fixed sleep: fast on a
    # quiet box, still bounded (60s) on a loaded one.
    wait_ready() {
        local ready_file=$1 deadline=$((SECONDS + 60))
        while [ ! -f "$ready_file" ]; do
            if [ "$SECONDS" -ge "$deadline" ]; then
                echo "perf: timed out waiting for $ready_file" >&2
                exit 1
            fi
            sleep 0.25
        done
    }

    run_scenario() {
        local dir_name=$1 display_label=$2 cells=$3
        local root="$tmp/$dir_name"
        mkdir -p "$root"
        local gw_port=$((19000 + RANDOM % 9000))
        local base_port=$((29000 + RANDOM % 9000))
        local ready="$root/ready.json"

        # First start: fresh-provision `$cells` cells, then stop so the
        # pending tenant (declarative provisioning, D14) can be created
        # offline against a quiescent manifest.
        "$bin" start --data-root "$root" --cells "$cells" \
            --base-port "$base_port" --gateway-addr "127.0.0.1:$gw_port" \
            --ready-file "$ready" >"$root/first.log" 2>&1 &
        local first_pid=$!
        wait_ready "$ready"
        kill -TERM "$first_pid"
        wait "$first_pid" 2>/dev/null || true

        local create_out token
        create_out=$("$bin" tenant create --data-root "$root" --name loadtest \
            --schema "$schema" --replicas 1)
        token=$(echo "$create_out" | grep '^tenant loadtest token' | awk '{print $NF}')

        # Second start: recovers the existing cells and reconciles the new
        # pending tenant onto one of them.
        rm -f "$ready"
        "$bin" start --data-root "$root" --base-port "$base_port" \
            --gateway-addr "127.0.0.1:$gw_port" --ready-file "$ready" \
            >"$root/second.log" 2>&1 &
        local second_pid=$!
        wait_ready "$ready"

        echo "=== perf: $display_label ==="
        cargo run --release --package defraburner --example loadgen -- \
            --url "http://127.0.0.1:$gw_port/api/v1/graphql" \
            --token "$token" --threads 3 --secs 10 --body "$query"

        kill -TERM "$second_pid"
        wait "$second_pid" 2>/dev/null || true
    }

    run_scenario one-cell "1 cell" 1
    run_scenario three-cells "3 cells" 3

    echo "=== perf: recovery timing ==="
    cargo test -p defraburner --test recovery -- --nocapture 2>&1 | grep RECOVERY
