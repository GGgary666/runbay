use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Component, Path, PathBuf};
use walkdir::WalkDir;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CowMethod {
    ApfsClonefileSyscall,
    ApfsCpC,
    LinuxFicloneIoctl,
    LinuxCpReflink,
    FullCopyFallback,
}

impl CowMethod {
    pub fn as_str(&self) -> &'static str {
        match self {
            CowMethod::ApfsClonefileSyscall => "apfs_clonefile_syscall",
            CowMethod::ApfsCpC => "apfs_cp_c",
            CowMethod::LinuxFicloneIoctl => "linux_ficlone_ioctl",
            CowMethod::LinuxCpReflink => "linux_cp_reflink",
            CowMethod::FullCopyFallback => "full_copy_fallback",
        }
    }
}

#[cfg(target_os = "macos")]
extern "C" {
    fn clonefile(src: *const libc::c_char, dst: *const libc::c_char, flags: u32) -> libc::c_int;
}

#[cfg(target_os = "macos")]
fn try_apfs_clonefile(src: &Path, dst: &Path) -> bool {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let Ok(c_src) = CString::new(src.as_os_str().as_bytes()) else {
        return false;
    };
    let Ok(c_dst) = CString::new(dst.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: both pointers are valid NUL-terminated C strings passed to macOS clonefile(2).
    let rc = unsafe { clonefile(c_src.as_ptr(), c_dst.as_ptr(), 0) };
    rc == 0
}

#[cfg(not(target_os = "macos"))]
fn try_apfs_clonefile(_src: &Path, _dst: &Path) -> bool {
    false
}

/// Linux FICLONE ioctl (`_IOW(0x94, 9, int)`) for O(1) CoW reflink cloning on Btrfs/XFS/bcachefs.
#[cfg(target_os = "linux")]
fn try_linux_ficlone_tree(src: &Path, dst: &Path) -> bool {
    use std::os::unix::io::AsRawFd;
    const FICLONE: libc::c_ulong = 0x4004_9409;

    if fs::create_dir_all(dst).is_err() {
        return false;
    }
    let mut cloned_any_file = false;
    for entry in WalkDir::new(src).follow_links(false) {
        let Ok(entry) = entry else {
            let _ = fs::remove_dir_all(dst);
            return false;
        };
        let Ok(rel) = entry.path().strip_prefix(src) else {
            let _ = fs::remove_dir_all(dst);
            return false;
        };
        if rel.as_os_str().is_empty() {
            continue;
        }
        let target = dst.join(rel);
        let ft = entry.file_type();
        if ft.is_dir() {
            if fs::create_dir_all(&target).is_err() {
                let _ = fs::remove_dir_all(dst);
                return false;
            }
        } else if ft.is_file() {
            if let Some(parent) = target.parent() {
                if fs::create_dir_all(parent).is_err() {
                    let _ = fs::remove_dir_all(dst);
                    return false;
                }
            }
            let Ok(src_f) = fs::File::open(entry.path()) else {
                let _ = fs::remove_dir_all(dst);
                return false;
            };
            let Ok(dst_f) = fs::File::create(&target) else {
                let _ = fs::remove_dir_all(dst);
                return false;
            };
            // SAFETY: valid open file descriptors passed to Linux FICLONE ioctl.
            let rc = unsafe { libc::ioctl(dst_f.as_raw_fd(), FICLONE, src_f.as_raw_fd()) };
            if rc != 0 {
                // Underlying FS (e.g. ext4/tmpfs) does not support reflinks; fall back cleanly.
                let _ = fs::remove_dir_all(dst);
                return false;
            }
            cloned_any_file = true;
        } else if ft.is_symlink() {
            let Ok(link_target) = fs::read_link(entry.path()) else {
                let _ = fs::remove_dir_all(dst);
                return false;
            };
            if std::os::unix::fs::symlink(link_target, &target).is_err() {
                let _ = fs::remove_dir_all(dst);
                return false;
            }
        }
    }
    cloned_any_file
        || !src
            .read_dir()
            .map(|mut d| d.next().is_some())
            .unwrap_or(false)
}

#[cfg(not(target_os = "linux"))]
fn try_linux_ficlone_tree(_src: &Path, _dst: &Path) -> bool {
    false
}

/// Normalize a relative workspace path into canonical forward-slash form (e.g. `./docs/a.md` -> `docs/a.md`).
/// Rejects empty paths, NUL bytes, absolute paths, and `..` traversal.
pub fn normalize_rel_path(rel: &str) -> Result<String> {
    if rel.is_empty() || rel.contains('\0') {
        bail!("PATH_ESCAPE: empty or NUL-containing path");
    }
    let slash_norm = rel.replace('\\', "/");
    let rel_path = Path::new(&slash_norm);
    if rel_path.is_absolute() {
        bail!("PATH_ESCAPE: absolute path not allowed: {rel}");
    }
    let mut parts: Vec<String> = Vec::new();
    for comp in rel_path.components() {
        match comp {
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                bail!("PATH_ESCAPE: illegal path component in {rel}");
            }
            Component::CurDir => {}
            Component::Normal(s) => {
                let s_str = s.to_string_lossy();
                if !s_str.is_empty() {
                    parts.push(s_str.into_owned());
                }
            }
        }
    }
    if parts.is_empty() {
        bail!("PATH_ESCAPE: path resolves to workspace root: {rel}");
    }
    Ok(parts.join("/"))
}

/// Unified index eligibility rule shared by `rebuild_all` and `refresh_incremental`:
/// must be a non-hidden `.md` file (no path component starting with `.`).
pub fn is_indexable_markdown(canonical_rel: &str) -> bool {
    if !canonical_rel.ends_with(".md") {
        return false;
    }
    !canonical_rel
        .split('/')
        .any(|seg| seg.is_empty() || seg.starts_with('.'))
}

/// Recursively copy a directory tree (used as fallback and for full-copy baseline measurements).
pub fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    if dst.exists() {
        fs::remove_dir_all(dst)
            .with_context(|| format!("failed to remove existing dst {}", dst.display()))?;
    }
    fs::create_dir_all(dst).with_context(|| format!("failed to create dst {}", dst.display()))?;
    for entry in WalkDir::new(src).follow_links(false) {
        let entry = entry?;
        let rel = entry.path().strip_prefix(src)?;
        if rel.as_os_str().is_empty() {
            continue;
        }
        let target = dst.join(rel);
        let ft = entry.file_type();
        if ft.is_dir() {
            fs::create_dir_all(&target)?;
        } else if ft.is_file() {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(entry.path(), &target)?;
        } else if ft.is_symlink() {
            #[cfg(unix)]
            {
                let link_target = fs::read_link(entry.path())?;
                std::os::unix::fs::symlink(link_target, &target)?;
            }
        }
    }
    Ok(())
}

/// Clone a directory tree using APFS `clonefile(2)` on macOS or `FICLONE` ioctl on Linux,
/// falling back gracefully to OS reflink `cp` or recursive copy.
pub fn cow_clone_tree(src: &Path, dst: &Path) -> Result<CowMethod> {
    if dst.exists() {
        fs::remove_dir_all(dst)
            .with_context(|| format!("failed to clear existing clone target {}", dst.display()))?;
    }
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }

    if try_apfs_clonefile(src, dst) {
        return Ok(CowMethod::ApfsClonefileSyscall);
    }

    #[cfg(target_os = "macos")]
    {
        let status = std::process::Command::new("/bin/cp")
            .arg("-cR")
            .arg(src)
            .arg(dst)
            .status();
        if let Ok(st) = status {
            if st.success() {
                return Ok(CowMethod::ApfsCpC);
            }
        }
    }

    if try_linux_ficlone_tree(src, dst) {
        return Ok(CowMethod::LinuxFicloneIoctl);
    }

    #[cfg(target_os = "linux")]
    {
        let status = std::process::Command::new("cp")
            .args(["-a", "--reflink=always"])
            .arg(src)
            .arg(dst)
            .status();
        if let Ok(st) = status {
            if st.success() {
                return Ok(CowMethod::LinuxCpReflink);
            }
        }
    }

    copy_dir_recursive(src, dst)?;
    Ok(CowMethod::FullCopyFallback)
}

/// Canonical path containment check under `root`.
/// Blocks NUL bytes, absolute paths, `..` traversal, sibling prefix spoofing, and external symlinks.
pub fn safe_resolve_under_root(root: &Path, rel: &str) -> Result<PathBuf> {
    let canonical_rel = normalize_rel_path(rel)?;
    let root_real = fs::canonicalize(root)
        .with_context(|| format!("failed to canonicalize root {}", root.display()))?;

    let mut cur = root_real.clone();
    let components: Vec<&str> = canonical_rel.split('/').collect();

    for (idx, part) in components.iter().enumerate() {
        cur.push(part);
        if let Ok(meta) = fs::symlink_metadata(&cur) {
            if meta.file_type().is_symlink() {
                let resolved = fs::canonicalize(&cur).with_context(|| {
                    format!("PATH_ESCAPE: broken or escaping symlink {}", cur.display())
                })?;
                if !resolved.starts_with(&root_real) {
                    bail!(
                        "PATH_ESCAPE: symlink escapes workspace root: {}",
                        cur.display()
                    );
                }
                cur = resolved;
            } else if idx + 1 < components.len() {
                let resolved = fs::canonicalize(&cur)?;
                if !resolved.starts_with(&root_real) {
                    bail!(
                        "PATH_ESCAPE: directory escapes workspace root: {}",
                        cur.display()
                    );
                }
            }
        }
    }

    if !cur.starts_with(&root_real) {
        bail!("PATH_ESCAPE: resolved path outside root: {}", cur.display());
    }
    Ok(cur)
}
