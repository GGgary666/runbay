# Runbay

[![CI](https://github.com/wfnuser/runbay/actions/workflows/ci.yml/badge.svg)](https://github.com/wfnuser/runbay/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

**A local-first Rust runtime daemon and systems substrate for isolating, pooling, and scheduling coding-agent sessions.**

Runbay provides a single loopback execution boundary (`runbayd`) for supervising local coding-agent runtimes over HTTP + SSE. It pairs OS-level subprocess sandboxing, copy-on-write MVCC workspace versioning, a host-side secretless model gateway, and cache-aware scheduling in a lightweight Rust workspace.

---

## Core Architecture

- **Three-Layer MVCC / CoW Workspaces (`crates/runbay-mvcc`)**:
  - **L0**: SHA-256 content-addressed read-only layer (`0o444`) materialized via hardlinks so concurrent workers share a single physical inode and OS page-cache copy.
  - **L1**: Copy-on-Write workspace snapshots and speculative branching (`fork` / atomic First-Valid-Commit-Wins `commit` / `abort`) using macOS `clonefile(2)` and Linux `ioctl(FICLONE)` / `--reflink`, paired with RAII `SnapshotLease` pinning and canonical path containment.
  - **L2**: Out-of-sandbox shared versioned derived index (SQLite FTS5 + dense projection) using SQLite online backups (`WAL`-safe), proxy-generation-scoped immutable read-only `mmap` connections for parallel multi-threaded snapshot reads, and verified incremental-vs-full equivalence.
- **Cross-Platform OS Subprocess Sandbox (`crates/runbay-sandbox`)**:
  - **macOS**: Per-run Seatbelt (`sandbox-exec`) profiles with global default-deny host writes (`(deny file-write*)`), sibling pool & secret denial, L0 read-only allowlisting, and network fencing with optional loopback gateway exceptions.
  - **Linux**: Unprivileged `bubblewrap` (`bwrap`) namespaces with global read-only root (`--ro-bind / /`), `tmpfs` pool/secret masking, workspace bind mounts, and `--unshare-net` isolation.
  - Includes a thread-safe `QuarantineRegistry` to retire poisoned worker slots permanently after cleanup failures.
- **Async Secretless Loopback Model Gateway (`crates/runbay-gateway`)**:
  - `tokio` + `hyper` 1.x reverse proxy binding `127.0.0.1:0` that injects host-side API credentials only on outbound requests so sandboxed processes never see real keys.
  - 256-bit OS CSPRNG opaque bearer tokens (`rb_*`), strict route allowlist + origin invariant, optional per-token model scoping (`403 Forbidden`), mid-stream SSE tool-call prewarm hooks with explicit error surfacing, and immediate `Notify`-driven revocation wakeup for pending and stalled streams.
- **Cancellation Guard & Cache-Aware Scheduler (`crates/runbay-scheduler`)**:
  - Atomic `CancelToken` + `GuardedRunSink` rejecting late events after abort.
  - Multi-policy scheduler (`fifo`, `priority_aging`, `warm_env_affinity`, `dual_cache_ttl_aware`) balancing interactive latency, anti-starvation aging, warm workspace affinity, and upstream prompt-cache TTL rescue.

---

## Workspace Layout

```text
runbay/
├── crates/
│   ├── runbay-mvcc/        # L0 CAS, L1 CoW MVCC/branching, L2 shared versioned derived index
│   ├── runbay-sandbox/     # Cross-platform OS sandbox (macOS Seatbelt / Linux bwrap) + quarantine
│   ├── runbay-gateway/     # Async secretless loopback HTTP/1.1 + SSE model gateway
│   ├── runbay-scheduler/   # CancelToken/GuardedRunSink + priority/affinity/cache-TTL scheduler
│   └── runbayd/            # Daemon CLI (`self-check`, `serve`)
├── beta-bench/             # Standalone Rust beta benchmark suite (`cargo run -p runbay-beta-bench --release`)
└── docs/
    └── DESIGN.md           # Architecture & protocol design notes
```

---

## Platform Support

| Capability | macOS (Apple Silicon / Intel) | Linux (x86_64 / arm64) |
| :--- | :--- | :--- |
| OS Subprocess Sandbox | `sandbox-exec` (Seatbelt) | `bubblewrap` (`bwrap`) |
| L1 CoW Cloning | `clonefile(2)` syscall (APFS) | `ioctl(FICLONE)` (Btrfs/XFS) / fallback copy |
| L0 CAS Hardlink Sharing | Supported | Supported |
| L2 Multi-Threaded MVCC Index | Supported | Supported |
| Secretless Loopback Gateway & Scheduler | Supported | Supported |

---

## Quick Start

Requires Rust stable (`rustc` 1.80+). On Linux, install `bubblewrap` (`sudo apt-get install -y bubblewrap`) to enable OS sandbox execution.

```bash
# 1. Run all workspace unit and real OS kernel integration tests (zero API cost)
cargo test --workspace

# 2. Run the daemon self-check (verifies MVCC, OS sandbox, gateway, and cancel guard)
cargo run -p runbayd --release -- self-check

# 3. Start the local loopback control server
cargo run -p runbayd --release -- serve --port 7310

# 4. Run the reproducible Rust Beta Bench suite
cargo run -p runbay-beta-bench --release -- \
  --traces beta-bench/fixtures/recorded_llm_traces.json \
  --out beta-bench/artifacts/beta_bench_results.json
```

See [docs/DESIGN.md](docs/DESIGN.md) for the full daemon design and [beta-bench/README.md](beta-bench/README.md) for benchmark methodology and metrics.

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
