# Runbay Beta Bench

Reproducible Rust benchmark suite and correctness harness for Runbay's runtime engine:

- **Correctness Oracle (`real_os_kernel_exec`)**:
  - 7-check cross-platform OS subprocess isolation (macOS Seatbelt `sandbox-exec` / Linux `bubblewrap`) including global outside-workspace write denial + bare-vs-sandboxed spawn overhead;
  - Poisoned worker slot quarantine (`QuarantineRegistry`);
  - MVCC exact snapshot pinning + SQLite online-backup WAL preservation;
  - Incremental-vs-full derived index equivalence (`file_hashes`, `blocks`, ordered FTS5 BM25, frozen-encoder vectors);
  - `CancelToken` / `GuardedRunSink` strict late-event rejection;
  - Real OS-sandboxed subprocess -> Rust `SecretlessLoopbackGateway` end-to-end verification.
- **L0 / L1 / L2 Scaling (`real_os_kernel_exec`)**:
  - `100 / 500 / 1200`-file workspace provisioning (Full Copy vs OS CoW `clonefile(2)` / `FICLONE` vs L0 CAS hardlink inode sharing);
  - 8-worker duplicated index build vs shared L2 versioned proxy.
- **Concurrent MVCC Snapshot Throughput (`real_os_kernel_exec`)**:
  - Multi-threaded (`1 / 4 / 8 / 16` OS threads) parallel uncached FTS5 + dense vector reads against pinned snapshots.
- **Trace-Driven Replay & Scheduling (`real_trace_replay`, `controlled_stress_simulation`)**:
  - Mid-stream tool-call prewarm overlap with real Rust CoW + L2 delta sync;
  - Speculative 4-branch CoW fork + early-abort replay;
  - Concurrent burst churn saturation (`10% / 30% / 50%` churn @ 12 RPS);
  - 4-policy scheduler comparison (`fifo`, `priority_aging`, `warm_env_affinity`, `dual_cache_ttl_aware`) under normal and overload regimes.

## Quick Run

```bash
# Run all workspace unit & integration tests (zero API cost)
cargo test --workspace

# Run the full Rust Beta Bench suite in release mode
cargo run -p runbay-beta-bench --release -- \
  --traces beta-bench/fixtures/recorded_llm_traces.json \
  --out beta-bench/artifacts/beta_bench_results.json
```
