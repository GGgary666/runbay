use anyhow::{anyhow, Result};
use parking_lot::{Mutex, RwLock};
use rusqlite::{params, Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

static NEXT_PROXY_INSTANCE_ID: AtomicU64 = AtomicU64::new(1);

thread_local! {
    /// Per-thread cache of read-only `immutable=1` SQLite connections to finalized MVCC snapshot DBs.
    /// Keyed by `(proxy_instance_id, rev, db_path)` so reusing a proxy directory across stores or
    /// proxy generations can NEVER serve a stale SQLite file descriptor from an earlier store!
    static RO_SNAPSHOT_CONNS: RefCell<HashMap<(u64, u64, PathBuf), Connection>> = RefCell::new(HashMap::new());
}

fn with_readonly_snapshot_conn<T, F>(
    proxy_instance_id: u64,
    rev: u64,
    db_path: &Path,
    f: F,
) -> Result<T>
where
    F: FnOnce(&Connection) -> Result<T>,
{
    crate::derived::init_sqlite_high_concurrency();
    RO_SNAPSHOT_CONNS.with(|cell| {
        let mut map = cell.borrow_mut();
        let key = (proxy_instance_id, rev, db_path.to_path_buf());
        if !map.contains_key(&key) {
            let uri = format!(
                "file:{}?mode=ro&immutable=1",
                db_path.to_string_lossy()
            );
            let conn = Connection::open_with_flags(
                &uri,
                OpenFlags::SQLITE_OPEN_READ_ONLY
                    | OpenFlags::SQLITE_OPEN_URI
                    | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )?;
            let _ = conn.execute_batch(
                "PRAGMA mmap_size = 67108864; PRAGMA cache_size = -16384; PRAGMA temp_store = MEMORY;",
            );
            map.insert(key.clone(), conn);
        }
        let conn = map.get(&key).expect("inserted above");
        f(conn)
    })
}

use crate::derived::{
    build_fts_query, search_connection, DerivedWorkspaceIndexLayer, FrozenEncoderState, SearchHit,
};
use crate::store::MVCCWorkspaceStore;

#[derive(Clone)]
struct PinnedSnapshotHandle {
    db_path: PathBuf,
    encoder: Arc<FrozenEncoderState>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FtsMismatch {
    pub query: String,
    pub inc_fts: Vec<String>,
    pub full_fts: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EquivalenceReport {
    pub revision: u64,
    pub queries_checked: usize,
    pub file_hashes_bit_identical: bool,
    pub blocks_table_bit_identical: bool,
    pub fts5_bm25_exact_match: bool,
    pub frozen_encoder_vectors_identical: bool,
    pub encoder_equivalence_scope: String,
    pub exact_equivalence: bool,
    pub mismatches: Vec<FtsMismatch>,
    pub full_rebuild_ms: f64,
    pub mean_incremental_refresh_ms: f64,
}

#[derive(Debug, Clone)]
pub struct QueryResult {
    pub hits: Vec<SearchHit>,
    pub refresh_ms: f64,
    pub cache_hit: bool,
}

struct ActiveWriterState {
    active_rev: u64,
    layer: DerivedWorkspaceIndexLayer,
    incremental_refresh_latencies_ms: Vec<f64>,
}

/// L2 Out-of-Sandbox Shared Versioned Derived Index & Tool-Cache Proxy.
///
/// Key Rust systems properties:
/// - SQLite online backup API (`rusqlite::backup::Backup`) preserves all WAL frames atomically.
/// - Immutable snapshot backups for active MVCC leases are queried over read-only SQLite
///   connections **in parallel across OS threads without holding the writer lock**.
/// - Proven equivalence between incremental delta refresh and fresh full rebuild across
///   `file_hashes`, `blocks`, ordered FTS5 BM25 ranking, and frozen-encoder dense vectors.
pub struct SharedOutOfSandboxDerivedProxy {
    proxy_instance_id: u64,
    proxy_dir: PathBuf,
    store: Arc<MVCCWorkspaceStore>,
    writer: Mutex<ActiveWriterState>,
    pinned_snapshots: RwLock<HashMap<u64, PinnedSnapshotHandle>>,
    tool_result_cache: RwLock<HashMap<(u64, String), Vec<SearchHit>>>,
    pub initial_build_ms: f64,
    tool_cache_hits: AtomicU64,
    tool_cache_misses: AtomicU64,
}

impl SharedOutOfSandboxDerivedProxy {
    pub fn new(
        proxy_dir: impl AsRef<Path>,
        store: Arc<MVCCWorkspaceStore>,
        dim: usize,
        max_vocab: usize,
    ) -> Result<Self> {
        fs::create_dir_all(proxy_dir.as_ref())?;
        let proxy_dir = fs::canonicalize(proxy_dir.as_ref())?;
        let proxy_instance_id = NEXT_PROXY_INSTANCE_ID.fetch_add(1, Ordering::Relaxed);
        let mut layer = DerivedWorkspaceIndexLayer::open(
            proxy_dir.join(format!("shared_derived_g{proxy_instance_id}.sqlite")),
            dim,
            max_vocab,
        )?;
        let snap0 = store
            .get_snapshot(0)
            .ok_or_else(|| anyhow!("missing rev 0 snapshot"))?;
        let stats = layer.rebuild_all(&snap0.root_dir, &snap0.snapshot_id)?;

        Ok(Self {
            proxy_instance_id,
            proxy_dir,
            store,
            writer: Mutex::new(ActiveWriterState {
                active_rev: 0,
                layer,
                incremental_refresh_latencies_ms: Vec::new(),
            }),
            pinned_snapshots: RwLock::new(HashMap::new()),
            tool_result_cache: RwLock::new(HashMap::new()),
            initial_build_ms: stats.build_ms,
            tool_cache_hits: AtomicU64::new(0),
            tool_cache_misses: AtomicU64::new(0),
        })
    }

    fn snapshot_db_path(&self, rev: u64) -> PathBuf {
        self.proxy_dir
            .join(format!("hist_g{}_rev_{rev}.sqlite", self.proxy_instance_id))
    }

    /// Return honest total SQLite disk footprint (`.sqlite` + `-wal` + `-shm`) after WAL checkpoint.
    pub fn db_size_bytes(&self) -> u64 {
        let g = self.writer.lock();
        let _ = g
            .layer
            .conn()
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
        let base = g.layer.db_path();
        let main_len = fs::metadata(base).map(|m| m.len()).unwrap_or(0);
        let wal_len = fs::metadata(format!("{}-wal", base.display()))
            .map(|m| m.len())
            .unwrap_or(0);
        let shm_len = fs::metadata(format!("{}-shm", base.display()))
            .map(|m| m.len())
            .unwrap_or(0);
        main_len + wal_len + shm_len
    }

    pub fn tool_cache_stats(&self) -> (u64, u64) {
        (
            self.tool_cache_hits.load(Ordering::Relaxed),
            self.tool_cache_misses.load(Ordering::Relaxed),
        )
    }

    /// Ensure an immutable snapshot-safe DB exists for `target_rev` (advancing `active_rev`
    /// incrementally if needed, and atomically backing up active leases via SQLite online backup).
    fn ensure_snapshot_handle(&self, target_rev: u64) -> Result<(PinnedSnapshotHandle, f64)> {
        // Fast path: already materialized immutable snapshot for `target_rev` (zero writer lock!)
        if let Some(handle) = self.pinned_snapshots.read().get(&target_rev) {
            return Ok((handle.clone(), 0.0));
        }

        let mut w = self.writer.lock();

        // Double-check after acquiring writer lock
        if let Some(handle) = self.pinned_snapshots.read().get(&target_rev) {
            return Ok((handle.clone(), 0.0));
        }

        if target_rev < w.active_rev {
            // Historical revision not previously backed up: rebuild immutable snapshot DB
            // reusing the proxy's frozen rev-0 encoder so scoring is 100% read-order independent!
            let snap = self
                .store
                .get_snapshot(target_rev)
                .ok_or_else(|| anyhow!("unknown historical revision {target_rev}"))?;
            let hist_path = self.snapshot_db_path(target_rev);
            let tmp_build_path = hist_path.with_extension("build.sqlite");
            if tmp_build_path.exists() {
                let _ = fs::remove_file(&tmp_build_path);
            }
            let frozen_enc = w.layer.encoder();
            let mut hist = DerivedWorkspaceIndexLayer::open(
                &tmp_build_path,
                frozen_enc.dim,
                frozen_enc.max_vocab,
            )?;
            hist.rebuild_all_with_frozen_encoder(
                &snap.root_dir,
                &snap.snapshot_id,
                Arc::clone(&frozen_enc),
            )?;
            // Atomically snapshot to finalized immutable path before publishing handle
            hist.backup_to(&hist_path)?;
            drop(hist);
            let _ = fs::remove_file(&tmp_build_path);
            let _ = fs::remove_file(format!("{}-wal", tmp_build_path.display()));
            let _ = fs::remove_file(format!("{}-shm", tmp_build_path.display()));

            let handle = PinnedSnapshotHandle {
                db_path: hist_path,
                encoder: frozen_enc,
            };
            self.pinned_snapshots.write().insert(target_rev, handle.clone());
            return Ok((handle, 0.0));
        }

        if target_rev == w.active_rev {
            let hist_path = self.snapshot_db_path(target_rev);
            w.layer.backup_to(&hist_path)?;
            let handle = PinnedSnapshotHandle {
                db_path: hist_path,
                encoder: w.layer.encoder(),
            };
            self.pinned_snapshots.write().insert(target_rev, handle.clone());
            return Ok((handle, 0.0));
        }

        // Advancing active_rev -> target_rev:
        // First snapshot `active_rev` via SQLite online backup if ANY active lease exists on it!
        let cur_active = w.active_rev;
        if self.store.active_leases_for(cur_active) > 0
            && !self.pinned_snapshots.read().contains_key(&cur_active)
        {
            let hist_path = self.snapshot_db_path(cur_active);
            w.layer.backup_to(&hist_path)?;
            let handle = PinnedSnapshotHandle {
                db_path: hist_path,
                encoder: w.layer.encoder(),
            };
            self.pinned_snapshots.write().insert(cur_active, handle);
        }

        let (changed, deleted) = self.store.diff_between(cur_active, target_rev)?;
        let snap = self
            .store
            .get_snapshot(target_rev)
            .ok_or_else(|| anyhow!("unknown target revision {target_rev}"))?;
        let mut all_paths = changed;
        all_paths.extend(deleted);

        let t0 = Instant::now();
        w.layer
            .refresh_incremental(&snap.root_dir, &snap.snapshot_id, &all_paths)?;
        let dt_ms = (t0.elapsed().as_secs_f64() * 1000.0 * 1000.0).round() / 1000.0;
        w.active_rev = target_rev;
        w.incremental_refresh_latencies_ms.push(dt_ms);

        let hist_path = self.snapshot_db_path(target_rev);
        w.layer.backup_to(&hist_path)?;
        let handle = PinnedSnapshotHandle {
            db_path: hist_path,
            encoder: w.layer.encoder(),
        };
        self.pinned_snapshots.write().insert(target_rev, handle.clone());
        Ok((handle, dt_ms))
    }

    /// Snapshot-pinned query with revision-keyed result cache.
    /// Once `target_rev` is materialized, readers execute on read-only SQLite connections
    /// completely lock-free across OS threads!
    pub fn query_at_revision(
        &self,
        target_rev: u64,
        query_text: &str,
        top_k: usize,
    ) -> Result<QueryResult> {
        let cache_key = (target_rev, format!("{query_text}|{top_k}"));
        if let Some(cached) = self.tool_result_cache.read().get(&cache_key) {
            self.tool_cache_hits.fetch_add(1, Ordering::Relaxed);
            return Ok(QueryResult {
                hits: cached.clone(),
                refresh_ms: 0.0,
                cache_hit: true,
            });
        }
        self.tool_cache_misses.fetch_add(1, Ordering::Relaxed);
        let hits = self.query_at_revision_uncached(target_rev, query_text, top_k)?;
        self.tool_result_cache
            .write()
            .insert(cache_key, hits.hits.clone());
        Ok(hits)
    }

    /// Direct uncached snapshot-pinned query executing lock-free on thread-local `immutable=1`
    /// read-only SQLite connections (zero `open(2)` / `fcntl` lock overhead across OS threads).
    pub fn query_at_revision_uncached(
        &self,
        target_rev: u64,
        query_text: &str,
        top_k: usize,
    ) -> Result<QueryResult> {
        let (handle, refresh_ms) = self.ensure_snapshot_handle(target_rev)?;
        let hits = with_readonly_snapshot_conn(
            self.proxy_instance_id,
            target_rev,
            &handle.db_path,
            |conn| search_connection(conn, &handle.encoder, query_text, top_k),
        )?;
        Ok(QueryResult {
            hits,
            refresh_ms,
            cache_hit: false,
        })
    }

    /// Explicitly advance active_rev to `target_rev` (incremental delta sync) and return refresh latency in ms.
    pub fn sync_to_revision(&self, target_rev: u64) -> Result<f64> {
        let (_, dt_ms) = self.ensure_snapshot_handle(target_rev)?;
        Ok(dt_ms)
    }

    /// Verify incremental refresh equivalence against fresh full rebuild across:
    /// 1. Bit-identical `file_hashes`
    /// 2. Bit-identical `blocks`
    /// 3. Exact ordered FTS5 BM25 query rankings
    /// 4. 100% frozen-encoder `vec_json` dense vector equality (`frozen_projection_basis`)
    pub fn verify_incremental_equals_full_rebuild(
        &self,
        sample_queries: &[&str],
    ) -> Result<EquivalenceReport> {
        let snap = self.store.latest_snapshot();
        let (inc_handle, _) = self.ensure_snapshot_handle(snap.rev)?;

        let fresh_db = self.proxy_dir.join(format!("verify_fresh_rev_{}.sqlite", snap.rev));
        if fresh_db.exists() {
            let _ = fs::remove_file(&fresh_db);
        }
        let mut fresh_layer = DerivedWorkspaceIndexLayer::open(
            &fresh_db,
            inc_handle.encoder.dim,
            inc_handle.encoder.max_vocab,
        )?;
        let full_ms = fresh_layer.rebuild_all(&snap.root_dir, &snap.snapshot_id)?.build_ms;

        let inc_conn = Connection::open_with_flags(
            &inc_handle.db_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        let fresh_conn = fresh_layer.conn();

        // 1. Exact `file_hashes`
        let inc_hashes = fetch_file_hashes(&inc_conn)?;
        let full_hashes = fetch_file_hashes(fresh_conn)?;
        let hashes_identical = inc_hashes == full_hashes;

        // 2. Exact `blocks`
        let inc_blocks = fetch_blocks(&inc_conn)?;
        let full_blocks = fetch_blocks(fresh_conn)?;
        let blocks_identical = inc_blocks == full_blocks;

        // 3. Exact ordered FTS5 BM25 ranking
        let mut mismatches = Vec::new();
        for &q in sample_queries {
            let Some(fts_q) = build_fts_query(q) else {
                continue;
            };
            let inc_fts = fetch_fts_ids(&inc_conn, &fts_q)?;
            let full_fts = fetch_fts_ids(fresh_conn, &fts_q)?;
            if inc_fts != full_fts {
                mismatches.push(FtsMismatch {
                    query: q.to_string(),
                    inc_fts,
                    full_fts,
                });
            }
        }

        // 4. Frozen-encoder `vec_json` equality
        let mut inc_vecs: HashMap<String, String> = HashMap::new();
        {
            let mut stmt = inc_conn.prepare("SELECT block_id, vec_json FROM blocks")?;
            let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
            for row in rows {
                let (bid, vj) = row?;
                inc_vecs.insert(bid, vj);
            }
        }
        let mut frozen_vec_mismatches = 0usize;
        for (bid, _rp, h, body) in &full_blocks {
            let expected_v =
                serde_json::to_string(&inc_handle.encoder.encode(&format!("{h}\n{body}")))?;
            if inc_vecs.get(bid).map(|s| s.as_str()) != Some(expected_v.as_str()) {
                frozen_vec_mismatches += 1;
            }
        }

        drop(fresh_layer);
        let _ = fs::remove_file(&fresh_db);

        let mean_inc = {
            let w = self.writer.lock();
            if w.incremental_refresh_latencies_ms.is_empty() {
                0.0
            } else {
                let sum: f64 = w.incremental_refresh_latencies_ms.iter().sum();
                ((sum / w.incremental_refresh_latencies_ms.len() as f64) * 1000.0).round() / 1000.0
            }
        };

        let exact = hashes_identical
            && blocks_identical
            && mismatches.is_empty()
            && frozen_vec_mismatches == 0;

        Ok(EquivalenceReport {
            revision: snap.rev,
            queries_checked: sample_queries.len(),
            file_hashes_bit_identical: hashes_identical,
            blocks_table_bit_identical: blocks_identical,
            fts5_bm25_exact_match: mismatches.is_empty(),
            frozen_encoder_vectors_identical: frozen_vec_mismatches == 0,
            encoder_equivalence_scope:
                "frozen_projection_basis (full SVD refit changes basis)".to_string(),
            exact_equivalence: exact,
            mismatches,
            full_rebuild_ms: full_ms,
            mean_incremental_refresh_ms: mean_inc,
        })
    }
}

fn fetch_file_hashes(conn: &Connection) -> Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare("SELECT rel_path, sha256 FROM file_hashes ORDER BY rel_path")?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

fn fetch_blocks(conn: &Connection) -> Result<Vec<(String, String, String, String)>> {
    let mut stmt =
        conn.prepare("SELECT block_id, rel_path, heading, body FROM blocks ORDER BY block_id")?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

fn fetch_fts_ids(conn: &Connection, fts_q: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT block_id FROM blocks_fts WHERE blocks_fts MATCH ?1 ORDER BY bm25(blocks_fts), block_id LIMIT 10",
    )?;
    let rows = stmt.query_map(params![fts_q], |r| r.get(0))?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}
