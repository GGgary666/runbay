use anyhow::{bail, Result};
use std::fs;
use std::path::{Path, PathBuf};

pub const MAX_BODY_BYTES: usize = 256 * 1024;
pub const MAX_PROMPT_BYTES: usize = 64 * 1024;
pub const MAX_TIMEOUT_SECS: u64 = 3600;

pub fn generate_csprng_token(prefix: &str) -> String {
    let mut buf = [0u8; 32];
    getrandom::getrandom(&mut buf).expect("OS CSPRNG unavailable");
    format!("{prefix}{}", hex::encode(buf))
}

/// Constant-time comparison to avoid timing leaks on control bearer tokens.
pub fn constant_time_eq(left: &str, right: &str) -> bool {
    let a = left.as_bytes();
    let b = right.as_bytes();
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (&x, &y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

pub fn extract_bearer_token(header: Option<&str>) -> Option<&str> {
    let raw = header?.trim();
    let token = raw.strip_prefix("Bearer ")?.trim();
    if token.is_empty() {
        None
    } else {
        Some(token)
    }
}

/// Validate agent IDs and run IDs against a strict safe identifier pattern.
pub fn validate_identifier(value: &str, field: &str) -> Result<()> {
    if value.is_empty() || value.len() > 64 {
        bail!("{field} must be 1..=64 characters");
    }
    if matches!(value, "." | "..") {
        bail!("{field} cannot be '.' or '..'");
    }
    let bytes = value.as_bytes();
    if !bytes[0].is_ascii_alphanumeric() {
        bail!("{field} must start with an ASCII alphanumeric character");
    }
    if !bytes
        .iter()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    {
        bail!("{field} may only contain ASCII alphanumerics, '-', '_', or '.'");
    }
    Ok(())
}

/// Canonicalize and validate a caller-supplied workspace directory against daemon boundaries.
pub fn validate_workspace_dir(
    raw: &str,
    state_dir: &Path,
    allowed_roots: &[PathBuf],
) -> Result<PathBuf> {
    if raw.trim().is_empty() || raw.contains('\0') {
        bail!("workspace_dir must be a non-empty valid path");
    }
    let candidate = Path::new(raw);
    if !candidate.is_absolute() {
        bail!("workspace_dir must be an absolute path");
    }
    let canonical = fs::canonicalize(candidate)
        .map_err(|e| anyhow::anyhow!("workspace_dir does not exist or cannot be resolved: {e}"))?;
    if !canonical.is_dir() {
        bail!("workspace_dir must be an existing directory");
    }
    // Never allow mounting filesystem root or daemon's own state/control directory.
    if canonical.parent().is_none() {
        bail!("workspace_dir cannot be filesystem root");
    }
    let state_real = fs::canonicalize(state_dir).unwrap_or_else(|_| state_dir.to_path_buf());
    if canonical.starts_with(&state_real) || state_real.starts_with(&canonical) {
        bail!("workspace_dir cannot overlap runbayd state_dir");
    }
    if !allowed_roots.is_empty() {
        let within_allowed = allowed_roots.iter().any(|root| {
            let root_real = fs::canonicalize(root).unwrap_or_else(|_| root.clone());
            canonical.starts_with(&root_real)
        });
        if !within_allowed {
            bail!(
                "WORKSPACE_FORBIDDEN: workspace_dir '{}' is outside allowed workspace roots",
                canonical.display()
            );
        }
    }
    Ok(canonical)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_identifier_validation_and_constant_time_eq() {
        assert!(validate_identifier("agent-1_ok.v2", "agent_id").is_ok());
        assert!(validate_identifier("../escape", "agent_id").is_err());
        assert!(validate_identifier("-bad", "agent_id").is_err());
        assert!(validate_identifier("", "agent_id").is_err());
        assert!(constant_time_eq("rb_ctl_abc", "rb_ctl_abc"));
        assert!(!constant_time_eq("rb_ctl_abc", "rb_ctl_abd"));
        assert!(!constant_time_eq("short", "longer"));
    }

    #[test]
    fn test_workspace_boundary_enforces_allowed_roots_and_blocks_state_dir() {
        let tmp = TempDir::new().unwrap();
        let state = tmp.path().join("state");
        let allowed = tmp.path().join("allowed");
        let ok_ws = allowed.join("repo");
        let outside = tmp.path().join("outside");
        for d in [&state, &ok_ws, &outside] {
            fs::create_dir_all(d).unwrap();
        }

        assert!(validate_workspace_dir(
            ok_ws.to_str().unwrap(),
            &state,
            std::slice::from_ref(&allowed)
        )
        .is_ok());
        assert!(validate_workspace_dir(
            outside.to_str().unwrap(),
            &state,
            std::slice::from_ref(&allowed)
        )
        .is_err());
        assert!(validate_workspace_dir(state.to_str().unwrap(), &state, &[]).is_err());
        assert!(validate_workspace_dir("/", &state, &[]).is_err());
        assert!(validate_workspace_dir("relative/path", &state, &[]).is_err());
    }
}
