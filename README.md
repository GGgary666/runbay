# Runbay

[![CI](https://github.com/wfnuser/runbay/actions/workflows/ci.yml/badge.svg)](https://github.com/wfnuser/runbay/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

**A local-first Rust runtime daemon and systems substrate for isolating, supervising, and scheduling coding-agent sessions.**

Runbay provides an authenticated loopback execution daemon (`runbayd`) over HTTP + SSE backed by OS-level subprocess sandboxing, copy-on-write MVCC workspace snapshots, a host-side secretless model gateway, and bounded live execution supervision.

---

## Implementation Status (Implemented vs Simulated vs Planned)

| Area | Status | Notes |
| :--- | :--- | :--- |
| Loopback Control Daemon (`runbayd serve`) | **Implemented** | Constant-time Bearer auth (`rb_ctl_*`), body/ID/workspace boundary checks, durable SQLite registry, `/health`, `/v1/status`, `/agents`, `/runs`, SSE `/events` with `Last-Event-ID` replay, `POST /runs/:id/cancel` |
| Runtime Execution & Process-Group Cleanup | **Implemented** | Bounded live concurrency & queue (`429` when full), isolated per-run MVCC CoW workspace + RAII `SnapshotLease`, POSIX process-group (`setpgid` + `killpg`) cancel/timeout reaping, explicit `daemon_restarted` startup recovery |
| Runtime Adapters | **Implemented (`one_shot`)** | Honest `one_shot` `codex` adapter (`codex exec --json --ephemeral` through secretless loopback gateway on macOS; fails closed on Linux `--unshare-net`) + opt-in `--enable-fixture-adapter` deterministic protocol worker for tests/CI |
| Cross-Platform OS Sandbox (`runbay-sandbox`) | **Implemented** | macOS Seatbelt (`sandbox-exec`) with loopback TCP gateway allowlist; Linux `bubblewrap` (`bwrap --unshare-net`) with bound Unix-domain-socket gateway channel (fails closed if TCP loopback is requested with `allow_network=false`) |
| Secretless Model Gateway (`runbay-gateway`) | **Implemented** | `tokio` + `hyper` 1.x reverse proxy (TCP loopback + Unix domain socket) injecting real upstream API keys strictly on outbound hops; 256-bit `rb_*` tokens, route/model allowlists, `Notify`-driven immediate revocation |
| Three-Layer MVCC / CoW Store (`runbay-mvcc`) | **Implemented** | L0 CAS hardlinks, L1 APFS `clonefile(2)` / Linux `FICLONE` CoW snapshots & branching, L2 shared SQLite FTS5 + dense index |
| Policy Scheduler (`runbay-scheduler`) | **Live primitives + Simulated policies** | `CancelToken` and `GuardedRunSink` are live in `runbayd`; `fifo` / `priority_aging` / `warm_env_affinity` / `dual_cache_ttl_aware` are currently discrete-event simulations in `beta-bench` |
| Hot / Warm / Cold Process Pool & Native Duplex Resume | **Planned** | Tracked in [Issue #1](https://github.com/wfnuser/runbay/issues/1) and [Issue #5](https://github.com/wfnuser/runbay/issues/5) |
| Speculative Workspace Branch HTTP APIs (`/workspaces/*`) | **Planned** | Library primitives exist in `runbay-mvcc`; HTTP routes tracked in [Issue #2](https://github.com/wfnuser/runbay/issues/2) |

---

## Implemented Control API (`runbayd serve`)

All endpoints except `GET /health` require `Authorization: Bearer <control_token>`.

```text
GET    /health                  Unauthenticated liveness probe
GET    /v1/status               Daemon capabilities, adapters, sandbox & queue metrics
POST   /agents                  Register agent {agent_id, runtime, workspace_dir, config?}
GET    /agents                  List registered agents and MVCC lease state
GET    /agents/:id              Inspect one agent
DELETE /agents/:id              Cancel active runs and delete agent state
POST   /agents/:id/run          Submit a run {prompt, priority?, required_rev?, timeout_secs?, allowed_model?, sandbox_mode?}
GET    /agents/:id/events       SSE stream scoped to agent
GET    /runs                    List recent runs (?agent_id=...)
GET    /runs/:run_id            Query run state, exit code, sandbox/gateway flags, and event counts
POST   /runs/:run_id/cancel     Idempotently cancel queued/running run, kill+reap process group, revoke token
GET    /runs/:run_id/events     SSE event stream with Last-Event-ID / ?after_seq= replay (disconnect-safe)
```

---

## Platform Support & Limits

| Capability | macOS (Apple Silicon / Intel) | Linux (x86_64 / arm64) |
| :--- | :--- | :--- |
| OS Subprocess Sandbox | `sandbox-exec` (Seatbelt) | `bubblewrap` (`bwrap --unshare-net`) |
| Secretless Gateway Under `allow_network=false` | Loopback TCP (`localhost:<port>`) + Unix socket | Bound Unix domain socket (`--bind <sock> <sock>` + `--unshare-net`); **fails closed** on TCP-only loopback requests |
| `codex` One-Shot Adapter (Gateway Mode) | Supported (`codex exec --json --ephemeral` via Seatbelt TCP loopback gateway) | Fails closed when `allow_network=false` (Codex CLI requires TCP loopback, not Unix sockets) |
| L1 CoW Cloning | `clonefile(2)` (APFS) | `ioctl(FICLONE)` (Btrfs/XFS) / fallback copy |
| L0 CAS Hardlink Sharing & L2 MVCC Index | Supported | Supported |

---

## Quick Start

Requires Rust stable (`rustc` 1.80+). On Linux, install `bubblewrap` (`sudo apt-get install -y bubblewrap`).

```bash
# 1. Run all workspace unit, OS sandbox, and black-box daemon E2E tests (zero API cost)
cargo test --workspace --locked

# 2. Run the daemon self-check (verifies MVCC, OS sandbox, gateway, and cancel guard)
cargo run -p runbayd --locked -- self-check

# 3. Start the authenticated loopback daemon (auto-generates control token in state-dir/control/control.token)
RUNBAY_UPSTREAM_API_KEY="your-upstream-key" \
cargo run -p runbayd --release --locked -- serve \
  --port 7310 \
  --state-dir ~/.runbay \
  --allow-workspace-root "$PWD" \
  --upstream-url https://api.openai.com/v1

# 4. Run the reproducible Beta Bench + live HTTP daemon check
cargo build -p runbayd -p runbay-beta-bench --release --locked
target/release/beta-bench \
  --runbayd-bin target/release/runbayd \
  --traces beta-bench/fixtures/recorded_llm_traces.json \
  --out beta-bench/artifacts/beta_bench_results.json
```

See [docs/DESIGN.md](docs/DESIGN.md) for architecture details and [beta-bench/README.md](beta-bench/README.md) for benchmark methodology and measurement categories.

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
