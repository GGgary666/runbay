use anyhow::{anyhow, bail, Result};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use walkdir::WalkDir;

use crate::cow::{cow_clone_tree, normalize_rel_path, safe_resolve_under_root, CowMethod};

/// Validate a branch `group_id` as a safe single-component identifier (`[A-Za-z0-9_-]{1,64}`).
pub fn validate_group_id(group_id: &str) -> Result<()> {
    if group_id.is_empty() || group_id.len() > 64 {
        bail!("INVALID_GROUP_ID: group_id must be 1..=64 ASCII chars");
    }
    if !group_id
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        bail!("INVALID_GROUP_ID: illegal characters in group_id '{group_id}'");
    }
    Ok(())
}

fn now_unix_f64() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MVCCSnapshot {
    pub rev: u64,
    pub snapshot_id: String,
    pub root_dir: PathBuf,
    pub changed_paths: Vec<String>,
    pub deleted_paths: Vec<String>,
    pub created_at: f64,
    pub active_leases: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BranchStatus {
    Active,
    Committed,
    Aborted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BranchHandle {
    pub branch_id: String,
    pub group_id: String,
    pub base_rev: u64,
    pub workspace_dir: PathBuf,
    pub clone_method: CowMethod,
    pub status: BranchStatus,
    pub created_at: f64,
    pub abort_reason: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AtomicBranchCommitOutcome {
    pub committed: bool,
    pub new_snapshot: Option<MVCCSnapshot>,
    pub aborted_siblings: Vec<String>,
}

/// RAII MVCC read lease. Automatically decrements `active_leases` on Drop so leases never leak.
pub struct SnapshotLease {
    pub rev: u64,
    pub snapshot_id: String,
    pub root_dir: PathBuf,
    store: Arc<MVCCWorkspaceStore>,
}

impl Drop for SnapshotLease {
    fn drop(&mut self) {
        self.store.release_snapshot(self.rev);
    }
}

type CancelHook = Arc<dyn Fn() + Send + Sync>;

struct StoreState {
    current_rev: u64,
    snapshots: HashMap<u64, MVCCSnapshot>,
    branches: HashMap<String, BranchHandle>,
    branch_groups: HashMap<String, Vec<String>>,
    cancel_hooks: HashMap<String, CancelHook>,
}

/// Database-inspired MVCC versioned workspace store with exact snapshot pinning
/// and O(1) CoW speculative branching (fork / commit / abort).
pub struct MVCCWorkspaceStore {
    base_dir: PathBuf,
    snapshots_dir: PathBuf,
    branches_dir: PathBuf,
    state: RwLock<StoreState>,
}

impl MVCCWorkspaceStore {
    pub fn new(base_dir: impl AsRef<Path>, initial_source: impl AsRef<Path>) -> Result<Arc<Self>> {
        let base_dir_ref = base_dir.as_ref();
        if base_dir_ref.exists() {
            fs::remove_dir_all(base_dir_ref)?;
        }
        fs::create_dir_all(base_dir_ref)?;
        let base_dir = fs::canonicalize(base_dir_ref)?;
        let snapshots_dir = base_dir.join("snapshots");
        let branches_dir = base_dir.join("branches");
        fs::create_dir_all(&snapshots_dir)?;
        fs::create_dir_all(&branches_dir)?;

        let rev0_dir = snapshots_dir.join("rev_0");
        cow_clone_tree(initial_source.as_ref(), &rev0_dir)?;

        let mut initial_files: Vec<String> = Vec::new();
        for entry in WalkDir::new(&rev0_dir).follow_links(false) {
            let entry = entry?;
            if entry.file_type().is_file() {
                let rel = entry
                    .path()
                    .strip_prefix(&rev0_dir)?
                    .to_string_lossy()
                    .replace('\\', "/");
                initial_files.push(rel);
            }
        }
        initial_files.sort();

        let mut snapshots = HashMap::new();
        snapshots.insert(
            0,
            MVCCSnapshot {
                rev: 0,
                snapshot_id: "rev-0000".to_string(),
                root_dir: rev0_dir,
                changed_paths: initial_files,
                deleted_paths: Vec::new(),
                created_at: now_unix_f64(),
                active_leases: 0,
            },
        );

        Ok(Arc::new(Self {
            base_dir,
            snapshots_dir,
            branches_dir,
            state: RwLock::new(StoreState {
                current_rev: 0,
                snapshots,
                branches: HashMap::new(),
                branch_groups: HashMap::new(),
                cancel_hooks: HashMap::new(),
            }),
        }))
    }

    pub fn base_dir(&self) -> &Path {
        &self.base_dir
    }

    pub fn current_rev(&self) -> u64 {
        self.state.read().current_rev
    }

    pub fn latest_snapshot(&self) -> MVCCSnapshot {
        let g = self.state.read();
        g.snapshots[&g.current_rev].clone()
    }

    pub fn get_snapshot(&self, rev: u64) -> Option<MVCCSnapshot> {
        self.state.read().snapshots.get(&rev).cloned()
    }

    pub fn active_leases_for(&self, rev: u64) -> usize {
        self.state
            .read()
            .snapshots
            .get(&rev)
            .map(|s| s.active_leases)
            .unwrap_or(0)
    }

    /// Acquire an RAII MVCC read lease pinned to `rev` (or latest if `None`).
    pub fn pin_snapshot(self: &Arc<Self>, rev: Option<u64>) -> Result<SnapshotLease> {
        let mut g = self.state.write();
        let target = rev.unwrap_or(g.current_rev);
        let snap = g
            .snapshots
            .get_mut(&target)
            .ok_or_else(|| anyhow!("unknown revision {target}"))?;
        snap.active_leases += 1;
        Ok(SnapshotLease {
            rev: snap.rev,
            snapshot_id: snap.snapshot_id.clone(),
            root_dir: snap.root_dir.clone(),
            store: Arc::clone(self),
        })
    }

    pub fn release_snapshot(&self, rev: u64) {
        let mut g = self.state.write();
        if let Some(snap) = g.snapshots.get_mut(&rev) {
            snap.active_leases = snap.active_leases.saturating_sub(1);
        }
    }

    /// Atomically create a new MVCC snapshot via O(1) CoW clone + delta writes.
    /// Validates and normalizes ALL delta paths before cloning so failed commits leave zero artifacts.
    pub fn commit_delta(
        &self,
        upserts: &BTreeMap<String, String>,
        deletes: &[String],
        expected_base_rev: Option<u64>,
    ) -> Result<MVCCSnapshot> {
        // Pre-validate & normalize all paths before taking write lock or cloning
        let mut norm_upserts: BTreeMap<String, &String> = BTreeMap::new();
        for (raw_rel, content) in upserts {
            let norm = normalize_rel_path(raw_rel)?;
            norm_upserts.insert(norm, content);
        }
        let mut norm_deletes: Vec<String> = Vec::with_capacity(deletes.len());
        for raw_rel in deletes {
            norm_deletes.push(normalize_rel_path(raw_rel)?);
        }

        let mut g = self.state.write();
        if let Some(exp) = expected_base_rev {
            if exp != g.current_rev {
                bail!(
                    "OCC_CONFLICT: expected rev {exp}, current rev is {}",
                    g.current_rev
                );
            }
        }

        let prev_root = g.snapshots[&g.current_rev].root_dir.clone();
        let new_rev = g.current_rev + 1;
        let new_dir = self.snapshots_dir.join(format!("rev_{new_rev}"));
        cow_clone_tree(&prev_root, &new_dir)?;

        let apply_result: Result<(Vec<String>, Vec<String>)> = (|| {
            let mut changed = Vec::new();
            let mut deleted = Vec::new();
            for (rel, content) in &norm_upserts {
                let dst = safe_resolve_under_root(&new_dir, rel)?;
                if let Some(parent) = dst.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(&dst, content.as_bytes())?;
                changed.push(rel.clone());
            }
            for rel in &norm_deletes {
                let dst = safe_resolve_under_root(&new_dir, rel)?;
                if dst.exists() {
                    fs::remove_file(&dst)?;
                    deleted.push(rel.clone());
                }
            }
            changed.sort();
            deleted.sort();
            Ok((changed, deleted))
        })();

        let (changed, deleted) = match apply_result {
            Ok(v) => v,
            Err(e) => {
                let _ = fs::remove_dir_all(&new_dir);
                return Err(e);
            }
        };

        let snap = MVCCSnapshot {
            rev: new_rev,
            snapshot_id: format!("rev-{new_rev:04}"),
            root_dir: new_dir,
            changed_paths: changed,
            deleted_paths: deleted,
            created_at: now_unix_f64(),
            active_leases: 0,
        };
        g.snapshots.insert(new_rev, snap.clone());
        g.current_rev = new_rev;
        Ok(snap)
    }

    /// Exact union of upserted and deleted paths across `(from_rev, to_rev]`.
    pub fn diff_between(&self, from_rev: u64, to_rev: u64) -> Result<(Vec<String>, Vec<String>)> {
        let g = self.state.read();
        let mut changed = BTreeSet::new();
        let mut deleted = BTreeSet::new();
        for r in (from_rev + 1)..=to_rev {
            let snap = g
                .snapshots
                .get(&r)
                .ok_or_else(|| anyhow!("missing revision {r} during diff"))?;
            for p in &snap.changed_paths {
                deleted.remove(p);
                changed.insert(p.clone());
            }
            for p in &snap.deleted_paths {
                changed.remove(p);
                deleted.insert(p.clone());
            }
        }
        Ok((changed.into_iter().collect(), deleted.into_iter().collect()))
    }

    /// Fork `n_branches` isolated CoW branch workspaces off `base_rev`.
    /// Validates `group_id` as a safe identifier and rejects duplicate/reused group IDs before touching disk.
    pub fn fork_branch_group(
        &self,
        group_id: &str,
        n_branches: usize,
        base_rev: Option<u64>,
    ) -> Result<(Vec<BranchHandle>, f64)> {
        validate_group_id(group_id)?;
        let t0 = Instant::now();
        let mut g = self.state.write();
        if g.branch_groups.contains_key(group_id) {
            bail!("BRANCH_GROUP_EXISTS: group_id '{group_id}' is already reserved");
        }
        let rev = base_rev.unwrap_or(g.current_rev);
        let base_root = g
            .snapshots
            .get(&rev)
            .ok_or_else(|| anyhow!("unknown base_rev {rev}"))?
            .root_dir
            .clone();

        // Pre-validate all branch destination paths under `branches_dir` before cloning
        let mut planned: Vec<(String, PathBuf)> = Vec::with_capacity(n_branches);
        for i in 0..n_branches {
            let bid = format!("{group_id}-b{i}");
            if g.branches.contains_key(&bid) {
                bail!("BRANCH_ID_EXISTS: branch_id '{bid}' is already reserved");
            }
            let bdir = safe_resolve_under_root(&self.branches_dir, &bid)?;
            planned.push((bid, bdir));
        }

        let mut handles = Vec::with_capacity(n_branches);
        let mut ids = Vec::with_capacity(n_branches);
        for (bid, bdir) in planned {
            let method = cow_clone_tree(&base_root, &bdir)?;
            let handle = BranchHandle {
                branch_id: bid.clone(),
                group_id: group_id.to_string(),
                base_rev: rev,
                workspace_dir: bdir,
                clone_method: method,
                status: BranchStatus::Active,
                created_at: now_unix_f64(),
                abort_reason: None,
            };
            g.branches.insert(bid.clone(), handle.clone());
            handles.push(handle);
            ids.push(bid);
        }
        g.branch_groups.insert(group_id.to_string(), ids);
        let elapsed_ms = (t0.elapsed().as_secs_f64() * 1000.0 * 1000.0).round() / 1000.0;
        Ok((handles, elapsed_ms))
    }

    pub fn register_branch_cancel_hook(&self, branch_id: &str, hook: CancelHook) {
        self.state
            .write()
            .cancel_hooks
            .insert(branch_id.to_string(), hook);
    }

    /// First-Valid-Commit-Wins: atomically commits winning branch and aborts all siblings.
    /// Registered cancel hooks are invoked outside the store lock to prevent deadlocks.
    pub fn try_atomic_commit_branch(
        &self,
        branch_id: &str,
        modified_rel_paths: &[String],
    ) -> Result<AtomicBranchCommitOutcome> {
        let mut aborted_siblings = Vec::new();
        let mut hooks_to_fire: Vec<CancelHook> = Vec::new();
        let new_snap: MVCCSnapshot;

        {
            let mut g = self.state.write();
            let (status, base_rev, ws_dir, group_id) = {
                let h = g
                    .branches
                    .get(branch_id)
                    .ok_or_else(|| anyhow!("unknown branch {branch_id}"))?;
                (
                    h.status.clone(),
                    h.base_rev,
                    h.workspace_dir.clone(),
                    h.group_id.clone(),
                )
            };

            if status != BranchStatus::Active {
                return Ok(AtomicBranchCommitOutcome {
                    committed: false,
                    new_snapshot: None,
                    aborted_siblings: Vec::new(),
                });
            }

            if base_rev != g.current_rev {
                let cur = g.current_rev;
                if let Some(h) = g.branches.get_mut(branch_id) {
                    h.status = BranchStatus::Aborted;
                    h.abort_reason = Some(format!("stale_base_rev:{base_rev}!=current:{cur}"));
                }
                return Ok(AtomicBranchCommitOutcome {
                    committed: false,
                    new_snapshot: None,
                    aborted_siblings: Vec::new(),
                });
            }

            let mut upserts = BTreeMap::new();
            let mut deletes = Vec::new();
            for raw_rel in modified_rel_paths {
                let rel = normalize_rel_path(raw_rel)?;
                let p = safe_resolve_under_root(&ws_dir, &rel)?;
                if p.exists() {
                    upserts.insert(rel, fs::read_to_string(&p)?);
                } else {
                    deletes.push(rel);
                }
            }

            // Create new snapshot directly under the same exclusive lock (atomic CAS!)
            let prev_root = g.snapshots[&g.current_rev].root_dir.clone();
            let new_rev = g.current_rev + 1;
            let new_dir = self.snapshots_dir.join(format!("rev_{new_rev}"));
            cow_clone_tree(&prev_root, &new_dir)?;

            let mut changed = Vec::new();
            let mut deleted = Vec::new();
            for (rel, content) in &upserts {
                let dst = safe_resolve_under_root(&new_dir, rel)?;
                if let Some(parent) = dst.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(&dst, content.as_bytes())?;
                changed.push(rel.clone());
            }
            for rel in &deletes {
                let dst = safe_resolve_under_root(&new_dir, rel)?;
                if dst.exists() {
                    fs::remove_file(&dst)?;
                    deleted.push(rel.clone());
                }
            }
            changed.sort();
            deleted.sort();

            new_snap = MVCCSnapshot {
                rev: new_rev,
                snapshot_id: format!("rev-{new_rev:04}"),
                root_dir: new_dir,
                changed_paths: changed,
                deleted_paths: deleted,
                created_at: now_unix_f64(),
                active_leases: 0,
            };
            g.snapshots.insert(new_rev, new_snap.clone());
            g.current_rev = new_rev;

            if let Some(h) = g.branches.get_mut(branch_id) {
                h.status = BranchStatus::Committed;
            }

            let siblings = g.branch_groups.get(&group_id).cloned().unwrap_or_default();
            for sib_id in siblings {
                if sib_id == branch_id {
                    continue;
                }
                let mut ws_to_remove = None;
                if let Some(sib) = g.branches.get_mut(&sib_id) {
                    if sib.status == BranchStatus::Active {
                        sib.status = BranchStatus::Aborted;
                        sib.abort_reason = Some(format!("lost_race_to:{branch_id}"));
                        aborted_siblings.push(sib_id.clone());
                        ws_to_remove = Some(sib.workspace_dir.clone());
                    }
                }
                if let Some(ws) = ws_to_remove {
                    if ws.exists() {
                        let _ = fs::remove_dir_all(ws);
                    }
                    if let Some(hook) = g.cancel_hooks.get(&sib_id) {
                        hooks_to_fire.push(Arc::clone(hook));
                    }
                }
            }
        }

        // Fire cancel hooks outside lock to avoid deadlocks with streaming/worker threads
        for hook in hooks_to_fire {
            hook();
        }

        Ok(AtomicBranchCommitOutcome {
            committed: true,
            new_snapshot: Some(new_snap),
            aborted_siblings,
        })
    }

    pub fn abort_branch(&self, branch_id: &str, reason: &str) {
        let mut g = self.state.write();
        if let Some(b) = g.branches.get_mut(branch_id) {
            b.status = BranchStatus::Aborted;
            b.abort_reason = Some(reason.to_string());
            if b.workspace_dir.exists() {
                let _ = fs::remove_dir_all(&b.workspace_dir);
            }
        }
    }

    pub fn branch_status(&self, branch_id: &str) -> Option<BranchStatus> {
        self.state.read().branches.get(branch_id).map(|b| b.status.clone())
    }
}
