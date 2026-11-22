use anyhow::{Context, Result};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::cow::safe_resolve_under_root;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MaterializeStats {
    pub files_linked: usize,
    pub unique_inodes: usize,
    pub elapsed_ms: f64,
}

/// L0 immutable content-addressed read-only layer.
///
/// Blobs are stored once by SHA-256 (`0o444`) and materialized into worker workspaces
/// via hardlinks so N concurrent sandboxes share ONE physical inode and OS page cache copy.
pub struct L0ContentAddressedStore {
    store_root: PathBuf,
    objects_dir: PathBuf,
    lock: Mutex<()>,
}

impl L0ContentAddressedStore {
    pub fn new(store_root: impl AsRef<Path>) -> Result<Self> {
        fs::create_dir_all(store_root.as_ref())?;
        let store_root = fs::canonicalize(store_root.as_ref())?;
        let objects_dir = store_root.join("objects");
        fs::create_dir_all(&objects_dir)?;
        Ok(Self {
            store_root,
            objects_dir,
            lock: Mutex::new(()),
        })
    }

    pub fn store_root(&self) -> &Path {
        &self.store_root
    }

    pub fn put_bytes(&self, content: &[u8]) -> Result<(String, PathBuf)> {
        let mut hasher = Sha256::new();
        hasher.update(content);
        let digest = hex::encode(hasher.finalize());
        let obj_dir = self.objects_dir.join(&digest[..2]);
        let obj_path = obj_dir.join(&digest);

        let _guard = self.lock.lock();
        if !obj_path.exists() {
            fs::create_dir_all(&obj_dir)?;
            let tmp_path = obj_dir.join(format!("{digest}.tmp"));
            fs::write(&tmp_path, content)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&tmp_path, fs::Permissions::from_mode(0o444))?;
            }
            fs::rename(&tmp_path, &obj_path)?;
        }
        Ok((digest, obj_path))
    }

    pub fn materialize_hardlink_tree(
        &self,
        manifest: &BTreeMap<String, String>,
        target_dir: &Path,
    ) -> Result<MaterializeStats> {
        let t0 = Instant::now();
        if target_dir.exists() {
            fs::remove_dir_all(target_dir)?;
        }
        fs::create_dir_all(target_dir)?;

        let mut inodes: HashSet<u64> = HashSet::with_capacity(manifest.len());
        for (rel_path, digest) in manifest {
            let obj_path = self.objects_dir.join(&digest[..2]).join(digest);
            let dst = safe_resolve_under_root(target_dir, rel_path)?;
            if let Some(parent) = dst.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::hard_link(&obj_path, &dst).with_context(|| {
                format!(
                    "failed to hardlink {} -> {}",
                    obj_path.display(),
                    dst.display()
                )
            })?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                inodes.insert(fs::metadata(&dst)?.ino());
            }
        }

        let elapsed_ms = (t0.elapsed().as_secs_f64() * 1000.0 * 1000.0).round() / 1000.0;
        Ok(MaterializeStats {
            files_linked: manifest.len(),
            unique_inodes: inodes.len(),
            elapsed_ms,
        })
    }
}
