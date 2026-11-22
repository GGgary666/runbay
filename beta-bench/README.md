# Runbay Beta Bench

Reproducible Rust component benchmark, correctness harness, and live HTTP daemon execution check for Runbay.

Measurement categories are explicitly labeled in the JSON output so real OS/daemon execution is never conflated with trace replays or policy simulations:

- **Correctness Oracle (`real_os_kernel_exec`)**:
  - 7-check cross-platform OS subprocess isolation (macOS Seatbelt `sandbox-exec` / Linux `bubblewrap`) including global outside-workspace write denial + bare-vs-sandboxed spawn overhead;
  - Poisoned worker slot quarantine (`QuarantineRegistry`);
  - MVCC exact snapshot pinning + SQLite online-backup WAL preservation;
  - Incremental-vs-full derived index equivalence (`file_hashes`, `blocks`, ordered FTS5 BM25, frozen-encoder vectors);
  - `CancelToken` / `GuardedRunSink` strict late-event rejection;
  - Real OS-sandboxed subprocess -> Rust `SecretlessLoopbackGateway` end-to-end verification (macOS loopback TCP + Seatbelt; Linux `--unshare-net` + bound Unix domain socket), enforcing direct outbound internet denial (`1.1.1.1`) on **both** platforms.
- **Live Daemon HTTP Execution Check (`real_daemon_http_e2e`)**:
  - Spawns the compiled `runbayd` binary on an ephemeral loopback port (`--port 0`) with `--enable-fixture-adapter`, then executes `POST /agents` -> `POST /agents/:id/run` -> `GET /runs/:id/events` (SSE) -> `GET /runs/:id` -> `DELETE /agents/:id`, verifying real OS sandboxing, direct-network denial, secretless gateway injection, and clean teardown.
  - Note: Comprehensive black-box daemon tests (auth, boundaries, failure, cancellation, grandchild process-group reaping, SSE disconnect/reconnect, live queue `429`, timeout, restart recovery, and real `codex` binary gateway execution) live in `crates/runbayd/tests/daemon_e2e.rs`.
- **L0 / L1 / L2 Scaling (`real_os_kernel_exec`)**:
  - `100 / 500 / 1200`-file workspace provisioning (Full Copy vs OS CoW `clonefile(2)` / `FICLONE` vs L0 CAS hardlink inode sharing);
  - 8-worker duplicated index build vs shared L2 versioned proxy.
- **Concurrent MVCC Snapshot Throughput (`real_os_kernel_exec`)**:
  - Multi-threaded (`1 / 4 / 8 / 16` OS threads) parallel uncached FTS5 + dense vector reads against pinned snapshots.
- **Trace-Driven Replay & Scheduling (`real_trace_replay`, `controlled_stress_simulation`)**:
  - Mid-stream tool-call prewarm overlap paired with real Rust CoW + L2 delta sync;
  - Speculative 4-branch CoW fork + early-abort replay (simulated from 6 recorded LLM traces);
  - Concurrent burst churn saturation (`10% / 30% / 50%` churn, simulated arrival model);
  - 4-policy scheduler comparison (`fifo`, `priority_aging`, `warm_env_affinity`, `dual_cache_ttl_aware`) — discrete-event simulation only, **not** live daemon policy execution.

## Quick Run

```bash
# Run all workspace unit, OS sandbox, and black-box daemon E2E tests (zero API cost)
cargo test --workspace --locked

# Build release binaries and run Beta Bench + live HTTP daemon check
cargo build -p runbayd -p runbay-beta-bench --release --locked
target/release/beta-bench \
  --runbayd-bin target/release/runbayd \
  --traces beta-bench/fixtures/recorded_llm_traces.json \
  --out beta-bench/artifacts/beta_bench_results.json
```
