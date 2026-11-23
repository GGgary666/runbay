#![cfg(target_os = "linux")]

use runbay_sandbox::{LocalOsSandbox, SandboxRunOptions};
use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier};
use tempfile::tempdir;

#[test]
fn readonly_names_remain_immutable_during_concurrent_workspace_updates() {
    let root = tempdir().unwrap();
    let pool = root.path().join("pool");
    let shared = pool.join("shared");
    let ws = pool.join("work");
    fs::create_dir_all(&shared).unwrap();
    fs::create_dir(&ws).unwrap();
    fs::write(shared.join("note"), "original").unwrap();
    fs::hard_link(shared.join("note"), shared.join("second-name")).unwrap();
    symlink(shared.join("second-name"), ws.join("reference")).unwrap();
    let sb = LocalOsSandbox::new(&pool, &[]);
    if !sb.is_available() {
        assert!(
            std::env::var_os("RUNBAY_SANDBOX_BACKEND").is_none(),
            "selected backend unavailable"
        );
        return;
    }
    let opts = SandboxRunOptions {
        extra_readonly_paths: vec![shared.clone()],
        ..Default::default()
    };
    let barrier = Arc::new(Barrier::new(3));
    let finished = AtomicBool::new(false);
    std::thread::scope(|scope| {
        // Atomically rotate a public reference between two names of the same read-only
        // note while both workers use it. Every observation must retain its original policy.
        let changing = scope.spawn(|| {
            let mut updates = 0;
            while !finished.load(Ordering::Acquire) {
                let target = if updates % 2 == 0 {
                    "note"
                } else {
                    "second-name"
                };
                symlink(shared.join(target), ws.join("replacement")).unwrap();
                fs::rename(ws.join("replacement"), ws.join("reference")).unwrap();
                updates += 1;
                std::thread::yield_now();
            }
            updates
        });
        let mut workers = Vec::new();
        for name in ["a", "b"] {
            let gate = Arc::clone(&barrier);
            let (sb, ws, opts) = (&sb, &ws, &opts);
            workers.push(scope.spawn(move || {
                gate.wait();
                let command = [
                    "/bin/sh".into(),
                    "-c".into(),
                    r#"set -eu
                       i=0
                       while [ "$i" -lt 12 ]; do
                           echo local > "$1.tmp"
                           mv -f "$1.tmp" "$1"
                           test "$(cat reference)" = original
                           if truncate -s 0 reference 2>/dev/null; then exit 1; fi
                           i=$((i+1))
                       done
                       test "$(cat reference)" = original
                       if truncate -s 0 reference 2>/dev/null; then exit 1; fi
                       if ln -- "$(readlink reference)" "$1.link" 2>/dev/null; then exit 1; fi
                    "#
                    .into(),
                    "worker".into(),
                    name.into(),
                ];
                let result = sb.run(&command, ws, opts).unwrap();
                assert_eq!(result.returncode, 0, "{}", result.stderr);
            }));
        }
        barrier.wait();
        let completed: Vec<_> = workers.into_iter().map(|worker| worker.join()).collect();
        finished.store(true, Ordering::Release);
        assert!(changing.join().unwrap() > 0);
        for result in completed {
            result.unwrap();
        }
    });
    for path in ["note", "second-name"] {
        assert_eq!(fs::read(shared.join(path)).unwrap(), b"original");
    }
    for name in ["a", "b"] {
        assert_eq!(fs::read(ws.join(name)).unwrap(), b"local\n");
    }
}

#[tokio::test]
async fn native_launch_error_precedes_started_child() {
    let tmp = tempdir().unwrap();
    let sb = LocalOsSandbox::new(tmp.path(), &[]);
    if sb.gateway_transport() != "linux_landlock_seccomp_unix_socket" {
        return;
    }
    let file = tmp.path().join("ordinary-text");
    fs::write(&file, "sample document").unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
    let cmd = [file.to_str().unwrap().to_string()];
    let error = sb
        .spawn_async(&cmd, tmp.path(), &Default::default())
        .err()
        .expect("exec must fail");
    assert!(error.to_string().contains("Permission denied"), "{error:#}");
}

#[test]
fn writable_files_must_own_all_their_hard_links() {
    let tmp = tempdir().unwrap();
    let ws = tmp.path().join("work");
    fs::create_dir(&ws).unwrap();
    fs::write(ws.join("one"), "fixture").unwrap();
    fs::hard_link(ws.join("one"), ws.join("two")).unwrap();
    let sb = LocalOsSandbox::new(tmp.path(), &[]);
    if !sb.is_available() {
        return;
    }
    let command = ["/bin/true".into()];
    assert_eq!(
        sb.run(&command, &ws, &Default::default())
            .unwrap()
            .returncode,
        0
    );
    fs::hard_link(ws.join("one"), tmp.path().join("retained")).unwrap();
    let error = sb.run(&command, &ws, &Default::default()).unwrap_err();
    assert!(
        error.to_string().contains("hard link outside its scope"),
        "{error:#}"
    );
}
