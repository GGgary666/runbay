pub mod cow;
pub mod derived;
pub mod l0_cas;
pub mod proxy;
pub mod store;

pub use cow::{
    copy_dir_recursive, cow_clone_tree, is_indexable_markdown, normalize_rel_path,
    safe_resolve_under_root, CowMethod,
};
pub use derived::{
    DerivedWorkspaceIndexLayer, FrozenEncoderState, IncrementalRefreshStats, RebuildStats,
    SearchHit,
};
pub use l0_cas::{L0ContentAddressedStore, MaterializeStats};
pub use proxy::{EquivalenceReport, QueryResult, SharedOutOfSandboxDerivedProxy};
pub use store::{
    AtomicBranchCommitOutcome, BranchHandle, BranchStatus, MVCCSnapshot, MVCCWorkspaceStore,
    SnapshotLease,
};

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::fs;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::thread;
    use tempfile::TempDir;

    fn build_sample_workspace(root: &std::path::Path) {
        fs::create_dir_all(root.join("notes")).unwrap();
        for i in 0..8 {
            fs::write(
                root.join(format!("notes/doc_{i}.md")),
                format!("# Service {i}\n\nLatency budget is {}ms with retry token tok_{i}.\n", 100 + i * 10),
            )
            .unwrap();
        }
    }

    #[test]
    fn test_l0_cas_shared_inodes_across_workers() {
        let tmp = TempDir::new().unwrap();
        let cas = L0ContentAddressedStore::new(tmp.path().join("l0")).unwrap();
        let mut manifest = BTreeMap::new();
        for i in 0..10 {
            let (digest, _) = cas
                .put_bytes(format!("immutable payload {i}").as_bytes())
                .unwrap();
            manifest.insert(format!("lib/mod_{i}.txt"), digest);
        }

        let mut worker_inodes = Vec::new();
        for w in 0..4 {
            let target = tmp.path().join(format!("w_{w}"));
            let stats = cas.materialize_hardlink_tree(&manifest, &target).unwrap();
            assert_eq!(stats.files_linked, 10);
            assert_eq!(stats.unique_inodes, 10);
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                worker_inodes.push(fs::metadata(target.join("lib/mod_0.txt")).unwrap().ino());
            }
        }
        #[cfg(unix)]
        {
            assert!(worker_inodes.iter().all(|&ino| ino == worker_inodes[0]));
        }
    }

    #[test]
    fn test_mvcc_interleaved_rev0_rev1_and_wal_snapshot_not_empty() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src");
        build_sample_workspace(&src);

        let store = MVCCWorkspaceStore::new(tmp.path().join("mvcc"), &src).unwrap();
        let proxy = SharedOutOfSandboxDerivedProxy::new(tmp.path().join("proxy"), Arc::clone(&store), 32, 256).unwrap();

        // Pin rev=0 while active_rev == 0
        let lease0 = store.pin_snapshot(Some(0)).unwrap();
        assert_eq!(store.active_leases_for(0), 1);

        // Writer commits rev=1 (mutates doc_0, adds doc_new) and rev=2 (deletes doc_1)
        let mut up1 = BTreeMap::new();
        up1.insert(
            "notes/doc_0.md".to_string(),
            "# Service 0\n\nREV1_UPDATED_CANARY_ALPHA.\n".to_string(),
        );
        up1.insert(
            "notes/doc_new.md".to_string(),
            "# New Module\n\nREV1_ONLY_BRAND_NEW_SECRET.\n".to_string(),
        );
        store.commit_delta(&up1, &[], Some(0)).unwrap();

        let del2 = vec!["notes/doc_1.md".to_string()];
        store.commit_delta(&BTreeMap::new(), &del2, Some(1)).unwrap();

        // Advance active_rev to 2 first (triggers WAL backup of rev=0!)
        let q2_new = proxy.query_at_revision(2, "REV1_ONLY_BRAND_NEW_SECRET", 5).unwrap();
        assert!(!q2_new.hits.is_empty());
        let q2_del = proxy.query_at_revision(2, "tok_1", 5).unwrap();
        assert!(q2_del.hits.is_empty());

        // Now pinned rev=0 reader MUST still see doc_1 (non-empty WAL backup!) and MUST NOT see rev=1/2 mutations
        let q0_old = proxy.query_at_revision(0, "tok_1", 5).unwrap();
        assert!(!q0_old.hits.is_empty(), "rev=0 WAL snapshot must not be empty");
        let q0_future = proxy.query_at_revision(0, "REV1_ONLY_BRAND_NEW_SECRET", 5).unwrap();
        assert!(q0_future.hits.is_empty(), "rev=0 must never read rev=1 future doc");

        drop(lease0);
        assert_eq!(store.active_leases_for(0), 0);
    }

    #[test]
    fn test_multithreaded_concurrent_mvcc_readers_and_writer() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src");
        build_sample_workspace(&src);

        let store = MVCCWorkspaceStore::new(tmp.path().join("mvcc"), &src).unwrap();
        let proxy = Arc::new(
            SharedOutOfSandboxDerivedProxy::new(tmp.path().join("proxy"), Arc::clone(&store), 32, 256)
                .unwrap(),
        );
        let lease0 = store.pin_snapshot(Some(0)).unwrap();

        // Pre-warm rev=0 handle and run 8 concurrent reader threads while writer commits rev 1..=4
        let _ = proxy.query_at_revision_uncached(0, "tok_0", 5).unwrap();

        let mut handles = Vec::new();
        for tid in 0..8 {
            let p = Arc::clone(&proxy);
            handles.push(thread::spawn(move || {
                for iter in 0..25 {
                    let q = format!("tok_{}", (tid + iter) % 8);
                    let res = p.query_at_revision_uncached(0, &q, 3).unwrap();
                    assert!(!res.hits.is_empty());
                    let fut = p.query_at_revision_uncached(0, "CONCURRENT_FUTURE_TOKEN", 3).unwrap();
                    assert!(fut.hits.is_empty());
                }
            }));
        }

        // Concurrent writer advancing rev 1..=4
        for r in 1..=4u64 {
            let mut up = BTreeMap::new();
            up.insert(
                format!("notes/future_{r}.md"),
                format!("# Future {r}\n\nCONCURRENT_FUTURE_TOKEN rev {r}.\n"),
            );
            store.commit_delta(&up, &[], Some(r - 1)).unwrap();
            proxy.sync_to_revision(r).unwrap();
        }

        for h in handles {
            h.join().unwrap();
        }
        drop(lease0);
    }

    #[test]
    fn test_speculative_branch_first_valid_commit_wins_and_aborts_siblings() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src");
        build_sample_workspace(&src);

        let store = MVCCWorkspaceStore::new(tmp.path().join("mvcc"), &src).unwrap();
        let (branches, fork_ms) = store.fork_branch_group("race", 3, None).unwrap();
        assert_eq!(branches.len(), 3);
        assert!(fork_ms >= 0.0);

        let sib1_cancelled = Arc::new(AtomicBool::new(false));
        let sib2_cancelled = Arc::new(AtomicBool::new(false));
        {
            let f1 = Arc::clone(&sib1_cancelled);
            store.register_branch_cancel_hook(&branches[1].branch_id, Arc::new(move || f1.store(true, Ordering::SeqCst)));
            let f2 = Arc::clone(&sib2_cancelled);
            store.register_branch_cancel_hook(&branches[2].branch_id, Arc::new(move || f2.store(true, Ordering::SeqCst)));
        }

        fs::write(
            branches[0].workspace_dir.join("notes/winner.md"),
            "# Winner\n\nBranch 0 won atomically.\n",
        )
        .unwrap();

        let out = store
            .try_atomic_commit_branch(&branches[0].branch_id, &["notes/winner.md".to_string()])
            .unwrap();
        assert!(out.committed);
        assert_eq!(out.new_snapshot.unwrap().rev, 1);
        assert_eq!(out.aborted_siblings.len(), 2);
        assert!(sib1_cancelled.load(Ordering::SeqCst));
        assert!(sib2_cancelled.load(Ordering::SeqCst));
        assert_eq!(store.branch_status(&branches[0].branch_id), Some(BranchStatus::Committed));
        assert_eq!(store.branch_status(&branches[1].branch_id), Some(BranchStatus::Aborted));

        // Second commit on already-aborted sibling fails cleanly
        let out2 = store
            .try_atomic_commit_branch(&branches[1].branch_id, &["notes/winner.md".to_string()])
            .unwrap();
        assert!(!out2.committed);
    }

    #[test]
    fn test_incremental_equals_full_rebuild_strict() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src");
        build_sample_workspace(&src);

        let store = MVCCWorkspaceStore::new(tmp.path().join("mvcc"), &src).unwrap();
        let proxy = SharedOutOfSandboxDerivedProxy::new(tmp.path().join("proxy"), Arc::clone(&store), 32, 256).unwrap();

        let mut up = BTreeMap::new();
        up.insert(
            "notes/doc_0.md".to_string(),
            "# Service 0\n\nUpdated latency budget 42ms retry token tok_0.\n".to_string(),
        );
        up.insert(
            "notes/doc_added.md".to_string(),
            "# Added\n\nBrand new shard router latency budget.\n".to_string(),
        );
        store
            .commit_delta(&up, &["notes/doc_2.md".to_string()], Some(0))
            .unwrap();

        let eq = proxy
            .verify_incremental_equals_full_rebuild(&["latency budget", "tok_0", "shard router", "tok_2"])
            .unwrap();
        assert!(eq.file_hashes_bit_identical);
        assert!(eq.blocks_table_bit_identical);
        assert!(eq.fts5_bm25_exact_match);
        assert!(eq.frozen_encoder_vectors_identical);
        assert!(eq.exact_equivalence);
    }

    #[test]
    fn test_path_containment_blocks_traversal_and_external_symlinks() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().join("space");
        let evil = tmp.path().join("space_evil");
        fs::create_dir_all(&ws).unwrap();
        fs::create_dir_all(&evil).unwrap();
        fs::write(evil.join("secret.txt"), "SECRET").unwrap();

        assert!(safe_resolve_under_root(&ws, "../space_evil/secret.txt").is_err());
        assert!(safe_resolve_under_root(&ws, "/etc/passwd").is_err());
        assert!(safe_resolve_under_root(&ws, "bad\0name.md").is_err());

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(evil.join("secret.txt"), ws.join("escape.md")).unwrap();
            assert!(safe_resolve_under_root(&ws, "escape.md").is_err());
        }
    }

    #[test]
    fn test_reused_proxy_dir_does_not_leak_stale_sqlite_connection() {
        let tmp = TempDir::new().unwrap();
        let shared_proxy_dir = tmp.path().join("reused_proxy");

        // Store A + Proxy A in shared_proxy_dir
        let src_a = tmp.path().join("src_a");
        fs::create_dir_all(&src_a).unwrap();
        fs::write(src_a.join("doc.md"), "# Doc A\n\noldmarker alpha.\n").unwrap();
        let store_a = MVCCWorkspaceStore::new(tmp.path().join("store_a"), &src_a).unwrap();
        let proxy_a =
            SharedOutOfSandboxDerivedProxy::new(&shared_proxy_dir, Arc::clone(&store_a), 32, 256)
                .unwrap();
        let hits_a = proxy_a.query_at_revision_uncached(0, "oldmarker", 5).unwrap();
        assert_eq!(hits_a.hits.len(), 1);
        drop(proxy_a);
        drop(store_a);

        // Store B + Proxy B reusing the EXACT same proxy_dir on the EXACT same thread
        let src_b = tmp.path().join("src_b");
        fs::create_dir_all(&src_b).unwrap();
        fs::write(src_b.join("doc.md"), "# Doc B\n\nnewmarker beta.\n").unwrap();
        let store_b = MVCCWorkspaceStore::new(tmp.path().join("store_b"), &src_b).unwrap();
        let proxy_b =
            SharedOutOfSandboxDerivedProxy::new(&shared_proxy_dir, Arc::clone(&store_b), 32, 256)
                .unwrap();
        let hits_old = proxy_b.query_at_revision_uncached(0, "oldmarker", 5).unwrap();
        let hits_new = proxy_b.query_at_revision_uncached(0, "newmarker", 5).unwrap();
        assert!(hits_old.hits.is_empty(), "reused proxy_dir leaked stale Store A DB");
        assert_eq!(hits_new.hits.len(), 1, "reused proxy_dir failed to read Store B DB");
    }

    #[test]
    fn test_branch_group_id_rejects_traversal_and_duplicates() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src");
        build_sample_workspace(&src);
        let store = MVCCWorkspaceStore::new(tmp.path().join("mvcc"), &src).unwrap();

        // Traversal / absolute / illegal group IDs rejected before touching filesystem
        assert!(store.fork_branch_group("../escape", 1, None).is_err());
        assert!(store.fork_branch_group("/tmp/evil", 1, None).is_err());
        assert!(store.fork_branch_group("a/b", 1, None).is_err());
        assert!(store.fork_branch_group("", 1, None).is_err());

        // Duplicate group ID rejected so active uncommitted edits are never overwritten
        let (b0, _) = store.fork_branch_group("race_1", 2, None).unwrap();
        fs::write(b0[0].workspace_dir.join("notes/uncommitted.md"), "KEEP_ME").unwrap();
        assert!(store.fork_branch_group("race_1", 1, None).is_err());
        assert_eq!(
            fs::read_to_string(b0[0].workspace_dir.join("notes/uncommitted.md")).unwrap(),
            "KEEP_ME"
        );
    }

    #[test]
    fn test_incremental_and_full_share_canonical_path_and_markdown_eligibility() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src");
        build_sample_workspace(&src);
        let store = MVCCWorkspaceStore::new(tmp.path().join("mvcc"), &src).unwrap();
        let proxy =
            SharedOutOfSandboxDerivedProxy::new(tmp.path().join("proxy"), Arc::clone(&store), 32, 256)
                .unwrap();

        let mut up = BTreeMap::new();
        up.insert(".hidden.md".to_string(), "# Hidden\n\nSECRET_HIDDEN_TOK.\n".to_string());
        up.insert("config.txt".to_string(), "PLAIN_CONFIG_TOK=1\n".to_string());
        up.insert(
            "./notes/doc_0.md".to_string(),
            "# Service 0\n\nCANONICAL_REPLACED_TOK.\n".to_string(),
        );
        store.commit_delta(&up, &[], Some(0)).unwrap();

        let h_hidden = proxy.query_at_revision(1, "SECRET_HIDDEN_TOK", 5).unwrap();
        let h_txt = proxy.query_at_revision(1, "PLAIN_CONFIG_TOK", 5).unwrap();
        let h_old = proxy.query_at_revision(1, "tok_0", 5).unwrap();
        let h_new = proxy.query_at_revision(1, "CANONICAL_REPLACED_TOK", 5).unwrap();
        assert!(h_hidden.hits.is_empty(), "hidden .md must not be indexed incrementally");
        assert!(h_txt.hits.is_empty(), "non-markdown .txt must not be indexed incrementally");
        assert!(h_old.hits.is_empty(), "./notes/doc_0.md must replace notes/doc_0.md cleanly");
        assert_eq!(h_new.hits.len(), 1);
        assert_eq!(h_new.hits[0].rel_path, "notes/doc_0.md");

        let eq = proxy
            .verify_incremental_equals_full_rebuild(&["CANONICAL_REPLACED_TOK", "tok_1"])
            .unwrap();
        assert!(eq.file_hashes_bit_identical);
        assert!(eq.blocks_table_bit_identical);
        assert!(eq.fts5_bm25_exact_match);
        assert!(eq.frozen_encoder_vectors_identical);
        assert!(eq.exact_equivalence);
    }

    #[test]
    fn test_cold_historical_snapshot_uses_same_frozen_encoder_regardless_of_read_order() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("a.md"), "# Seed\n\nseedword common baseline alpha.\n").unwrap();

        let store = MVCCWorkspaceStore::new(tmp.path().join("mvcc"), &src).unwrap();
        let mut up1 = BTreeMap::new();
        up1.insert("b.md".to_string(), "# Late\n\nlateword common delta beta.\n".to_string());
        store.commit_delta(&up1, &[], Some(0)).unwrap();
        let mut up2 = BTreeMap::new();
        up2.insert("c.md".to_string(), "# Later\n\nlaterword common gamma.\n".to_string());
        store.commit_delta(&up2, &[], Some(1)).unwrap();

        // Proxy 1: queries rev=1 first (incremental path), then rev=2
        let p1 = SharedOutOfSandboxDerivedProxy::new(
            tmp.path().join("p1"),
            Arc::clone(&store),
            32,
            256,
        )
        .unwrap();
        let r1_warm = p1.query_at_revision(1, "lateword common", 5).unwrap();
        let _ = p1.query_at_revision(2, "laterword", 5).unwrap();

        // Proxy 2: jumps directly to rev=2 first, then queries cold historical rev=1
        let p2 = SharedOutOfSandboxDerivedProxy::new(
            tmp.path().join("p2"),
            Arc::clone(&store),
            32,
            256,
        )
        .unwrap();
        let _ = p2.query_at_revision(2, "laterword", 5).unwrap();
        let r1_cold = p2.query_at_revision(1, "lateword common", 5).unwrap();

        assert_eq!(r1_warm.hits.len(), r1_cold.hits.len());
        for (a, b) in r1_warm.hits.iter().zip(r1_cold.hits.iter()) {
            assert_eq!(a.block_id, b.block_id);
            assert!(
                (a.score - b.score).abs() < 1e-9,
                "score drift across read orders: warm={} vs cold={}",
                a.score,
                b.score
            );
        }
    }
}
