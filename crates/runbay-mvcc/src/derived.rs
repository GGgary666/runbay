use anyhow::{Context, Result};
use regex::Regex;
use rusqlite::{params, Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Once, OnceLock};
use std::time::Instant;
use walkdir::WalkDir;

use crate::cow::{is_indexable_markdown, normalize_rel_path, safe_resolve_under_root};

static SQLITE_INIT: Once = Once::new();

/// Disable SQLite's global `mem0.mutex` (`SQLITE_CONFIG_MEMSTATUS = 0`) once before first DB open
/// so multi-threaded read-only snapshot connections scale cleanly across CPU cores.
pub fn init_sqlite_high_concurrency() {
    SQLITE_INIT.call_once(|| {
        // SAFETY: called once prior to any SQLite connection opening in this process.
        unsafe {
            let _ = rusqlite::ffi::sqlite3_config(rusqlite::ffi::SQLITE_CONFIG_MEMSTATUS, 0);
        }
    });
}

/// Zero-lock ASCII identifier tokenizer equivalent to `[A-Za-z0-9_]{2,}` (eliminates Regex pool contention).
fn extract_tokens(s: &str) -> Vec<&str> {
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        while i < bytes.len() && !(bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
            i += 1;
        }
        let start = i;
        while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
            i += 1;
        }
        if i - start >= 2 {
            out.push(&s[start..i]);
        }
    }
    out
}

fn heading_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?m)^(#{1,3}\s+.+)$").expect("valid regex"))
}

/// Deterministic SplitMix64 PRNG + Box-Muller Gaussian (zero external RNG version churn).
struct DeterministicRng {
    state: u64,
}

impl DeterministicRng {
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn next_f64(&mut self) -> f64 {
        // Uniform in (0, 1]
        let u = (self.next_u64() >> 11) as f64 / ((1u64 << 53) as f64);
        u.max(1e-12)
    }

    fn gauss(&mut self) -> f64 {
        let u1 = self.next_f64();
        let u2 = self.next_f64();
        (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
    }
}

#[derive(Debug, Clone)]
pub struct FrozenEncoderState {
    pub dim: usize,
    pub max_vocab: usize,
    pub vocab: HashMap<String, usize>,
    pub idf: Vec<f64>,
    pub proj: Vec<Vec<f64>>,
}

impl FrozenEncoderState {
    pub fn empty(dim: usize, max_vocab: usize) -> Self {
        Self {
            dim,
            max_vocab,
            vocab: HashMap::new(),
            idf: Vec::new(),
            proj: Vec::new(),
        }
    }

    pub fn fit(texts: &[String], dim: usize, max_vocab: usize) -> Self {
        let mut df: HashMap<String, usize> = HashMap::new();
        let n_docs = texts.len().max(1) as f64;
        let mut doc_tokens: Vec<Vec<String>> = Vec::with_capacity(texts.len());

        for txt in texts {
            let toks: Vec<String> = extract_tokens(txt)
                .into_iter()
                .map(|m| m.to_ascii_lowercase())
                .collect();
            let unique: HashSet<&str> = toks.iter().map(|s| s.as_str()).collect();
            for ut in unique {
                *df.entry(ut.to_string()).or_insert(0) += 1;
            }
            doc_tokens.push(toks);
        }

        let mut ranked: Vec<(String, usize)> = df.into_iter().collect();
        ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        ranked.truncate(max_vocab);

        let v_size = ranked.len();
        let mut vocab = HashMap::with_capacity(v_size);
        let mut idf = Vec::with_capacity(v_size);
        for (idx, (w, cnt)) in ranked.into_iter().enumerate() {
            vocab.insert(w, idx);
            idf.push(((n_docs + 1.0) / (cnt as f64 + 1.0)).ln() + 1.0);
        }

        if v_size == 0 {
            return Self {
                dim,
                max_vocab,
                vocab,
                idf,
                proj: Vec::new(),
            };
        }

        // Sparse TF-IDF doc matrix (normalized rows)
        let mut mat: Vec<Vec<(usize, f64)>> = Vec::with_capacity(doc_tokens.len());
        for toks in &doc_tokens {
            if toks.is_empty() {
                mat.push(Vec::new());
                continue;
            }
            let mut tf: HashMap<usize, usize> = HashMap::new();
            for t in toks {
                if let Some(&idx) = vocab.get(t) {
                    *tf.entry(idx).or_insert(0) += 1;
                }
            }
            if tf.is_empty() {
                mat.push(Vec::new());
                continue;
            }
            let len_f = toks.len() as f64;
            let mut row: Vec<(usize, f64)> = tf
                .into_iter()
                .map(|(i, cnt)| (i, (cnt as f64 / len_f) * idf[i]))
                .collect();
            let norm = row.iter().map(|(_, v)| v * v).sum::<f64>().sqrt();
            let norm = if norm == 0.0 { 1.0 } else { norm };
            for (_, v) in &mut row {
                *v /= norm;
            }
            mat.push(row);
        }

        // 3-step power iteration SVD projection matching the Python baseline workload
        let mut rng = DeterministicRng::new(42);
        let actual_dim = dim.min(v_size);
        let mut basis: Vec<Vec<f64>> = Vec::with_capacity(actual_dim);
        let mut nxt = vec![0.0_f64; v_size];

        for _ in 0..actual_dim {
            let mut vec: Vec<f64> = (0..v_size).map(|_| rng.gauss()).collect();
            for _it in 0..3 {
                nxt.fill(0.0);
                for row in &mat {
                    let mut dot = 0.0_f64;
                    for &(col, val) in row {
                        dot += val * vec[col];
                    }
                    if dot != 0.0 {
                        for &(col, val) in row {
                            nxt[col] += dot * val;
                        }
                    }
                }
                for prev in &basis {
                    let mut proj_s = 0.0_f64;
                    for j in 0..v_size {
                        proj_s += nxt[j] * prev[j];
                    }
                    for j in 0..v_size {
                        nxt[j] -= proj_s * prev[j];
                    }
                }
                let norm = nxt.iter().map(|x| x * x).sum::<f64>().sqrt();
                let norm = if norm == 0.0 { 1.0 } else { norm };
                for j in 0..v_size {
                    vec[j] = nxt[j] / norm;
                }
            }
            basis.push(vec);
        }

        Self {
            dim,
            max_vocab,
            vocab,
            idf,
            proj: basis,
        }
    }

    pub fn encode(&self, text: &str) -> Vec<f64> {
        if self.proj.is_empty() || self.vocab.is_empty() {
            return vec![0.0; self.dim];
        }
        let toks: Vec<String> = extract_tokens(text)
            .into_iter()
            .map(|m| m.to_ascii_lowercase())
            .collect();
        if toks.is_empty() {
            return vec![0.0; self.proj.len()];
        }
        let mut tf: HashMap<usize, usize> = HashMap::new();
        for t in &toks {
            if let Some(&idx) = self.vocab.get(t) {
                *tf.entry(idx).or_insert(0) += 1;
            }
        }
        let len_f = toks.len() as f64;
        let tfidf: Vec<(usize, f64)> = tf
            .into_iter()
            .map(|(i, cnt)| (i, (cnt as f64 / len_f) * self.idf[i]))
            .collect();

        let mut out: Vec<f64> = Vec::with_capacity(self.proj.len());
        for comp in &self.proj {
            let mut s = 0.0_f64;
            for &(col, val) in &tfidf {
                s += val * comp[col];
            }
            out.push(s);
        }
        let norm = out.iter().map(|x| x * x).sum::<f64>().sqrt();
        let norm = if norm == 0.0 { 1.0 } else { norm };
        out.into_iter()
            .map(|x| ((x / norm) * 100_000.0).round() / 100_000.0)
            .collect()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RebuildStats {
    pub files: usize,
    pub blocks: usize,
    pub build_ms: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncrementalRefreshStats {
    pub updated_files: usize,
    pub refresh_ms: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub block_id: String,
    pub rel_path: String,
    pub heading: String,
    pub body: String,
    pub score: f64,
}

pub fn split_markdown_blocks(rel_path: &str, content: &str) -> Vec<(String, String, String)> {
    let re = heading_re();
    let mut blocks = Vec::new();
    let mut cur_h = "Document Root".to_string();
    let mut idx: usize = 0;

    let mut last_end = 0usize;
    let mut segments: Vec<(&str, bool)> = Vec::new();
    for m in re.find_iter(content) {
        if m.start() > last_end {
            segments.push((&content[last_end..m.start()], false));
        }
        segments.push((m.as_str(), true));
        last_end = m.end();
    }
    if last_end < content.len() {
        segments.push((&content[last_end..], false));
    }

    for (seg, is_heading) in segments {
        let s = seg.trim();
        if s.is_empty() {
            continue;
        }
        if is_heading {
            cur_h = s.trim_start_matches('#').trim().to_string();
        } else {
            for para in s.split("\n\n") {
                let body = para.trim();
                if body.is_empty() {
                    continue;
                }
                blocks.push((format!("{rel_path}::{idx}"), cur_h.clone(), body.to_string()));
                idx += 1;
            }
        }
    }

    if blocks.is_empty() && !content.trim().is_empty() {
        blocks.push((
            format!("{rel_path}::0"),
            cur_h,
            content.trim().to_string(),
        ));
    }
    blocks
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    hex::encode(h.finalize())
}

pub fn build_fts_query(query: &str) -> Option<String> {
    let toks: Vec<&str> = extract_tokens(query).into_iter().take(8).collect();
    if toks.is_empty() {
        return None;
    }
    Some(
        toks.into_iter()
            .map(|t| format!("\"{t}\""))
            .collect::<Vec<_>>()
            .join(" OR "),
    )
}

pub fn search_connection(
    conn: &Connection,
    encoder: &FrozenEncoderState,
    query: &str,
    top_k: usize,
) -> Result<Vec<SearchHit>> {
    let Some(fts_q) = build_fts_query(query) else {
        return Ok(Vec::new());
    };
    let mut stmt = conn.prepare_cached(
        "SELECT b.block_id, b.rel_path, b.heading, b.body, b.vec_json
         FROM blocks_fts f
         JOIN blocks b ON b.block_id = f.block_id
         WHERE blocks_fts MATCH ?1
         ORDER BY bm25(blocks_fts), b.block_id
         LIMIT ?2",
    )?;
    let limit = (top_k * 3) as i64;
    let rows = stmt.query_map(params![fts_q, limit], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
        ))
    })?;

    let qvec = encoder.encode(query);
    let mut scored: Vec<SearchHit> = Vec::new();
    for (rank, row_res) in rows.enumerate() {
        let (block_id, rel_path, heading, body, vec_json) = row_res?;
        let dvec: Vec<f64> = serde_json::from_str(&vec_json).unwrap_or_default();
        let dot: f64 = qvec.iter().zip(dvec.iter()).map(|(a, b)| a * b).sum();
        let score = (1.0 / (20.0 + rank as f64)) + 0.35 * dot;
        scored.push(SearchHit {
            block_id,
            rel_path,
            heading,
            body,
            score,
        });
    }
    scored.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.block_id.cmp(&b.block_id))
    });
    scored.truncate(top_k);
    Ok(scored)
}

/// Self-contained SQLite FTS5 + SVD dense projection derived-state layer.
pub struct DerivedWorkspaceIndexLayer {
    db_path: PathBuf,
    conn: Connection,
    encoder: Arc<FrozenEncoderState>,
}

impl DerivedWorkspaceIndexLayer {
    pub fn open(db_path: impl AsRef<Path>, dim: usize, max_vocab: usize) -> Result<Self> {
        init_sqlite_high_concurrency();
        let db_path = db_path.as_ref().to_path_buf();
        if let Some(parent) = db_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(&db_path)
            .with_context(|| format!("failed to open sqlite db {}", db_path.display()))?;
        let layer = Self {
            db_path,
            conn,
            encoder: Arc::new(FrozenEncoderState::empty(dim, max_vocab)),
        };
        layer.init_schema()?;
        Ok(layer)
    }

    pub fn open_readonly(
        db_path: impl AsRef<Path>,
        encoder: Arc<FrozenEncoderState>,
    ) -> Result<Self> {
        init_sqlite_high_concurrency();
        let db_path = db_path.as_ref().to_path_buf();
        let conn = Connection::open_with_flags(
            &db_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("failed to open readonly sqlite db {}", db_path.display()))?;
        Ok(Self {
            db_path,
            conn,
            encoder,
        })
    }

    fn init_schema(&self) -> Result<()> {
        self.conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             CREATE TABLE IF NOT EXISTS meta (
                 key TEXT PRIMARY KEY,
                 val TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS file_hashes (
                 rel_path TEXT PRIMARY KEY,
                 sha256 TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS blocks (
                 block_id TEXT PRIMARY KEY,
                 rel_path TEXT NOT NULL,
                 heading TEXT NOT NULL,
                 body TEXT NOT NULL,
                 vec_json TEXT NOT NULL
             );
             CREATE VIRTUAL TABLE IF NOT EXISTS blocks_fts USING fts5(
                 block_id UNINDEXED,
                 rel_path,
                 heading,
                 body,
                 tokenize = 'unicode61'
             );",
        )?;
        Ok(())
    }

    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    pub fn encoder(&self) -> Arc<FrozenEncoderState> {
        Arc::clone(&self.encoder)
    }

    pub fn dim(&self) -> usize {
        self.encoder.dim
    }

    pub fn max_vocab(&self) -> usize {
        self.encoder.max_vocab
    }

    /// Atomically back up this WAL-mode SQLite DB using SQLite's online backup API.
    /// Writes into a unique `.tmp` sibling and renames atomically so readers never observe partial DBs.
    pub fn backup_to(&self, target_path: &Path) -> Result<()> {
        if let Some(parent) = target_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp_path = target_path.with_extension("sqlite.tmp");
        if tmp_path.exists() {
            let _ = fs::remove_file(&tmp_path);
        }
        {
            let mut dst = Connection::open(&tmp_path)?;
            let backup = rusqlite::backup::Backup::new(&self.conn, &mut dst)?;
            backup.run_to_completion(256, std::time::Duration::from_millis(1), None)?;
        }
        fs::rename(&tmp_path, target_path)?;
        Ok(())
    }

    pub fn rebuild_all(&mut self, workspace_dir: &Path, snapshot_id: &str) -> Result<RebuildStats> {
        self.rebuild_all_internal(workspace_dir, snapshot_id, None)
    }

    /// Rebuild a snapshot DB reusing a caller-supplied frozen encoder (for read-order-independent
    /// cold historical MVCC snapshots).
    pub fn rebuild_all_with_frozen_encoder(
        &mut self,
        workspace_dir: &Path,
        snapshot_id: &str,
        frozen_encoder: Arc<FrozenEncoderState>,
    ) -> Result<RebuildStats> {
        self.rebuild_all_internal(workspace_dir, snapshot_id, Some(frozen_encoder))
    }

    fn rebuild_all_internal(
        &mut self,
        workspace_dir: &Path,
        snapshot_id: &str,
        frozen_encoder: Option<Arc<FrozenEncoderState>>,
    ) -> Result<RebuildStats> {
        let t0 = Instant::now();
        let mut md_files: Vec<(String, PathBuf)> = Vec::new();
        for entry in WalkDir::new(workspace_dir).follow_links(false) {
            let entry = entry?;
            if !entry.file_type().is_file() {
                continue;
            }
            let p = entry.path();
            let rel_raw = p
                .strip_prefix(workspace_dir)?
                .to_string_lossy()
                .replace('\\', "/");
            let Ok(canonical_rel) = normalize_rel_path(&rel_raw) else {
                continue;
            };
            if !is_indexable_markdown(&canonical_rel) {
                continue;
            }
            md_files.push((canonical_rel, p.to_path_buf()));
        }
        md_files.sort_by(|a, b| a.0.cmp(&b.0));

        let mut all_blocks: Vec<(String, String, String, String)> = Vec::new();
        let mut file_hashes: Vec<(String, String)> = Vec::with_capacity(md_files.len());
        let mut corpus_texts: Vec<String> = Vec::new();

        for (rel, fp) in &md_files {
            let raw = fs::read_to_string(fp)?;
            let sha = sha256_hex(raw.as_bytes());
            file_hashes.push((rel.clone(), sha));
            for (bid, h, body) in split_markdown_blocks(rel, &raw) {
                corpus_texts.push(format!("{h}\n{body}"));
                all_blocks.push((bid, rel.clone(), h, body));
            }
        }

        self.encoder = match frozen_encoder {
            Some(enc) => enc,
            None => Arc::new(FrozenEncoderState::fit(
                &corpus_texts,
                self.encoder.dim,
                self.encoder.max_vocab,
            )),
        };

        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM blocks", [])?;
        tx.execute("DELETE FROM blocks_fts", [])?;
        tx.execute("DELETE FROM file_hashes", [])?;
        {
            let mut ins_hash =
                tx.prepare("INSERT INTO file_hashes(rel_path, sha256) VALUES (?1, ?2)")?;
            for (rel, sha) in &file_hashes {
                ins_hash.execute(params![rel, sha])?;
            }
            let mut ins_block = tx.prepare(
                "INSERT INTO blocks(block_id, rel_path, heading, body, vec_json) VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            let mut ins_fts = tx.prepare(
                "INSERT INTO blocks_fts(block_id, rel_path, heading, body) VALUES (?1, ?2, ?3, ?4)",
            )?;
            for (bid, rel, h, body) in &all_blocks {
                let vec_json = serde_json::to_string(&self.encoder.encode(&format!("{h}\n{body}")))?;
                ins_block.execute(params![bid, rel, h, body, vec_json])?;
                ins_fts.execute(params![bid, rel, h, body])?;
            }
        }
        tx.execute(
            "INSERT OR REPLACE INTO meta(key, val) VALUES ('snapshot_id', ?1)",
            params![snapshot_id],
        )?;
        tx.commit()?;

        let build_ms = (t0.elapsed().as_secs_f64() * 1000.0 * 100.0).round() / 100.0;
        Ok(RebuildStats {
            files: md_files.len(),
            blocks: all_blocks.len(),
            build_ms,
        })
    }

    pub fn refresh_incremental(
        &mut self,
        workspace_dir: &Path,
        snapshot_id: &str,
        changed_paths: &[String],
    ) -> Result<IncrementalRefreshStats> {
        let t0 = Instant::now();
        let mut updated = 0usize;
        let tx = self.conn.transaction()?;
        for raw_rel in changed_paths {
            let rel = normalize_rel_path(raw_rel)?;
            let fp = safe_resolve_under_root(workspace_dir, &rel)?;
            // Unified eligibility with rebuild_all: non-markdown or hidden paths are never indexed
            if !is_indexable_markdown(&rel) || !fp.is_file() {
                tx.execute("DELETE FROM blocks WHERE rel_path = ?1", params![rel])?;
                tx.execute("DELETE FROM blocks_fts WHERE rel_path = ?1", params![rel])?;
                tx.execute("DELETE FROM file_hashes WHERE rel_path = ?1", params![rel])?;
                updated += 1;
                continue;
            }
            let raw = fs::read_to_string(&fp)?;
            let sha = sha256_hex(raw.as_bytes());
            let prev: Option<String> = {
                let mut stmt =
                    tx.prepare_cached("SELECT sha256 FROM file_hashes WHERE rel_path = ?1")?;
                let mut rows = stmt.query(params![rel])?;
                if let Some(row) = rows.next()? {
                    Some(row.get(0)?)
                } else {
                    None
                }
            };
            if prev.as_deref() == Some(sha.as_str()) {
                continue;
            }
            tx.execute("DELETE FROM blocks WHERE rel_path = ?1", params![rel])?;
            tx.execute("DELETE FROM blocks_fts WHERE rel_path = ?1", params![rel])?;
            {
                let mut ins_block = tx.prepare_cached(
                    "INSERT INTO blocks(block_id, rel_path, heading, body, vec_json) VALUES (?1, ?2, ?3, ?4, ?5)",
                )?;
                let mut ins_fts = tx.prepare_cached(
                    "INSERT INTO blocks_fts(block_id, rel_path, heading, body) VALUES (?1, ?2, ?3, ?4)",
                )?;
                for (bid, h, body) in split_markdown_blocks(&rel, &raw) {
                    let vec_json =
                        serde_json::to_string(&self.encoder.encode(&format!("{h}\n{body}")))?;
                    ins_block.execute(params![bid, rel, h, body, vec_json])?;
                    ins_fts.execute(params![bid, rel, h, body])?;
                }
            }
            tx.execute(
                "INSERT OR REPLACE INTO file_hashes(rel_path, sha256) VALUES (?1, ?2)",
                params![rel, sha],
            )?;
            updated += 1;
        }
        tx.execute(
            "INSERT OR REPLACE INTO meta(key, val) VALUES ('snapshot_id', ?1)",
            params![snapshot_id],
        )?;
        tx.commit()?;

        let refresh_ms = (t0.elapsed().as_secs_f64() * 1000.0 * 1000.0).round() / 1000.0;
        Ok(IncrementalRefreshStats {
            updated_files: updated,
            refresh_ms,
        })
    }

    pub fn search_workspace(&self, query: &str, top_k: usize) -> Result<Vec<SearchHit>> {
        search_connection(&self.conn, &self.encoder, query, top_k)
    }
}
