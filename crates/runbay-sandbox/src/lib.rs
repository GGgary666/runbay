use anyhow::{bail, Context, Result};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxExecResult {
    pub command: Vec<String>,
    pub returncode: i32,
    pub stdout: String,
    pub stderr: String,
    pub elapsed_ms: f64,
    pub sandbox_enforced: bool,
    pub kernel_denied: bool,
}

#[derive(Debug, Clone, Default)]
pub struct SandboxRunOptions {
    pub extra_readonly_paths: Vec<PathBuf>,
    pub extra_forbidden_reads: Vec<PathBuf>,
    pub allow_network: bool,
    pub allowed_loopback_port: Option<u16>,
    pub env_overrides: BTreeMap<String, String>,
}

fn sb_escape(path: &Path) -> String {
    let resolved = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    resolved
        .to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
}

fn detect_kernel_denial(returncode: i32, stderr: &str) -> bool {
    returncode != 0
        && (stderr.contains("Operation not permitted")
            || stderr.contains("Read-only file system")
            || stderr.contains("Permission denied")
            || stderr.contains("No such file or directory")
            || stderr.contains("PermissionError")
            || stderr.contains("EPERM")
            || stderr.contains("EROFS")
            || stderr.contains("Errno 1")
            || stderr.contains("Errno 30"))
}

/// Generates per-run macOS Seatbelt (`.sb`) profiles and executes real OS child processes.
///
/// Write policy: **default-deny globally** (`(deny file-write*)`), allowing writes ONLY inside
/// `allowed_workspace` and `/dev/null`.
/// Read policy: explicitly denies `pool_root` (sibling workspaces) and `forbidden_read_paths`,
/// while allowing system libraries/binaries, `extra_readonly_paths` (L0), and `allowed_workspace`.
pub struct MacOSSeatbeltSandbox {
    pool_root: PathBuf,
    forbidden_read_paths: Vec<PathBuf>,
    sandbox_exec_bin: Option<PathBuf>,
}

impl MacOSSeatbeltSandbox {
    pub fn new(pool_root: impl AsRef<Path>, forbidden_read_paths: &[PathBuf]) -> Self {
        let pool_root = fs::canonicalize(pool_root.as_ref())
            .unwrap_or_else(|_| pool_root.as_ref().to_path_buf());
        let forbidden = forbidden_read_paths
            .iter()
            .map(|p| fs::canonicalize(p).unwrap_or_else(|_| p.clone()))
            .collect();
        let bin = if cfg!(target_os = "macos") && Path::new("/usr/bin/sandbox-exec").exists() {
            Some(PathBuf::from("/usr/bin/sandbox-exec"))
        } else {
            None
        };
        Self {
            pool_root,
            forbidden_read_paths: forbidden,
            sandbox_exec_bin: bin,
        }
    }

    pub fn is_available(&self) -> bool {
        self.sandbox_exec_bin.is_some()
    }

    pub fn build_profile(&self, allowed_workspace: &Path, opts: &SandboxRunOptions) -> String {
        let ws = sb_escape(allowed_workspace);
        let pool = sb_escape(&self.pool_root);

        let mut lines = vec![
            "(version 1)".to_string(),
            "(allow default)".to_string(),
            // Default-deny ALL host filesystem writes globally!
            "(deny file-write*)".to_string(),
            // Deny reads across pool root (isolates sibling worker runs)
            format!("(deny file-read* (subpath \"{pool}\"))"),
        ];

        for fpath in self
            .forbidden_read_paths
            .iter()
            .chain(opts.extra_forbidden_reads.iter())
        {
            let esc = sb_escape(fpath);
            lines.push(format!("(deny file-read* (subpath \"{esc}\"))"));
        }

        // Allow shared read-only L0 layers
        for ro in &opts.extra_readonly_paths {
            let esc = sb_escape(ro);
            lines.push(format!("(allow file-read* (subpath \"{esc}\"))"));
        }

        // Allow read + write strictly inside this run's isolated workspace (+ /dev/null)
        lines.push(format!("(allow file-read* (subpath \"{ws}\"))"));
        lines.push(format!("(allow file-write* (subpath \"{ws}\"))"));
        lines.push("(allow file-write* (literal \"/dev/null\"))".to_string());

        if !opts.allow_network {
            lines.push("(deny network*)".to_string());
            if let Some(port) = opts.allowed_loopback_port {
                lines.push(format!(
                    "(allow network-outbound (remote ip \"localhost:{port}\"))"
                ));
            }
        }

        lines.join("\n") + "\n"
    }

    pub fn run(
        &self,
        cmd: &[String],
        allowed_workspace: &Path,
        opts: &SandboxRunOptions,
    ) -> Result<SandboxExecResult> {
        let Some(ref sb_bin) = self.sandbox_exec_bin else {
            bail!("macOS sandbox-exec is unavailable on this host");
        };
        if cmd.is_empty() {
            bail!("sandbox command cannot be empty");
        }

        let profile_text = self.build_profile(allowed_workspace, opts);
        let ws_real = fs::canonicalize(allowed_workspace)
            .unwrap_or_else(|_| allowed_workspace.to_path_buf());

        let mut tf = tempfile::Builder::new().suffix(".sb").tempfile()?;
        std::io::Write::write_all(&mut tf, profile_text.as_bytes())?;
        let profile_path = tf.path().to_path_buf();

        let mut command = Command::new(sb_bin);
        command.arg("-f").arg(&profile_path);
        for arg in cmd {
            command.arg(arg);
        }
        command.current_dir(&ws_real);
        command.env_clear();
        command.env("PATH", "/usr/bin:/bin:/opt/homebrew/bin");
        command.env("HOME", &ws_real);
        command.env("PYTHONDONTWRITEBYTECODE", "1");
        for (k, v) in &opts.env_overrides {
            command.env(k, v);
        }

        let t0 = Instant::now();
        let output = command
            .output()
            .with_context(|| format!("failed to spawn sandboxed process {:?}", cmd))?;
        let elapsed_ms = (t0.elapsed().as_secs_f64() * 1000.0 * 1000.0).round() / 1000.0;

        let returncode = output.status.code().unwrap_or(-1);
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let kernel_denied = detect_kernel_denial(returncode, &stderr);

        Ok(SandboxExecResult {
            command: cmd.to_vec(),
            returncode,
            stdout,
            stderr,
            elapsed_ms,
            sandbox_enforced: true,
            kernel_denied,
        })
    }
}

/// Linux unprivileged namespace sandbox backed by `bubblewrap` (`bwrap`).
///
/// Enforces:
/// - Global read-only root (`--ro-bind / /`): zero writes anywhere on host outside `allowed_workspace`;
/// - Sibling pool & forbidden directory masking (`--tmpfs <dir>`);
/// - Shared L0 read-only bind (`--ro-bind <l0> <l0>`);
/// - Isolated workspace read-write bind (`--bind <ws> <ws>`);
/// - Network namespace isolation (`--unshare-net`) when network is disabled.
pub struct LinuxBubblewrapSandbox {
    pool_root: PathBuf,
    forbidden_read_paths: Vec<PathBuf>,
    bwrap_bin: Option<PathBuf>,
}

impl LinuxBubblewrapSandbox {
    pub fn new(pool_root: impl AsRef<Path>, forbidden_read_paths: &[PathBuf]) -> Self {
        let pool_root = fs::canonicalize(pool_root.as_ref())
            .unwrap_or_else(|_| pool_root.as_ref().to_path_buf());
        let forbidden = forbidden_read_paths
            .iter()
            .map(|p| fs::canonicalize(p).unwrap_or_else(|_| p.clone()))
            .collect();
        let bin = if cfg!(target_os = "linux") {
            ["/usr/bin/bwrap", "/bin/bwrap", "/usr/local/bin/bwrap"]
                .iter()
                .map(PathBuf::from)
                .find(|p| p.exists())
        } else {
            None
        };
        Self {
            pool_root,
            forbidden_read_paths: forbidden,
            bwrap_bin: bin,
        }
    }

    pub fn is_available(&self) -> bool {
        self.bwrap_bin.is_some()
    }

    /// Build the exact `bwrap` argument list (testable on all platforms!).
    pub fn build_bwrap_args(
        &self,
        cmd: &[String],
        allowed_workspace: &Path,
        opts: &SandboxRunOptions,
    ) -> Vec<String> {
        let ws_real = fs::canonicalize(allowed_workspace)
            .unwrap_or_else(|_| allowed_workspace.to_path_buf());
        let ws_s = ws_real.to_string_lossy().to_string();
        let pool_s = self.pool_root.to_string_lossy().to_string();

        let mut args: Vec<String> = vec![
            "--die-with-parent".into(),
            "--unshare-ipc".into(),
            "--unshare-pid".into(),
            "--unshare-uts".into(),
            // Mount entire host filesystem read-only by default (zero arbitrary host writes!)
            "--ro-bind".into(),
            "/".into(),
            "/".into(),
            "--dev".into(),
            "/dev".into(),
            "--proc".into(),
            "/proc".into(),
            // Mask pool_root with an empty tmpfs so sibling worker slots are invisible
            "--tmpfs".into(),
            pool_s,
        ];

        // Mask explicit forbidden secret paths with empty tmpfs
        for fpath in self
            .forbidden_read_paths
            .iter()
            .chain(opts.extra_forbidden_reads.iter())
        {
            let real = fs::canonicalize(fpath).unwrap_or_else(|_| fpath.clone());
            args.push("--tmpfs".into());
            args.push(real.to_string_lossy().to_string());
        }

        // Re-expose shared L0 layers read-only
        for ro in &opts.extra_readonly_paths {
            let real = fs::canonicalize(ro).unwrap_or_else(|_| ro.clone());
            let s = real.to_string_lossy().to_string();
            args.push("--ro-bind".into());
            args.push(s.clone());
            args.push(s);
        }

        // Re-expose ONLY this run's isolated workspace read-write
        args.push("--bind".into());
        args.push(ws_s.clone());
        args.push(ws_s.clone());
        args.push("--chdir".into());
        args.push(ws_s);

        if !opts.allow_network && opts.allowed_loopback_port.is_none() {
            args.push("--unshare-net".into());
        }

        args.push("--".into());
        args.extend(cmd.iter().cloned());
        args
    }

    pub fn run(
        &self,
        cmd: &[String],
        allowed_workspace: &Path,
        opts: &SandboxRunOptions,
    ) -> Result<SandboxExecResult> {
        let Some(ref bwrap) = self.bwrap_bin else {
            bail!("Linux bubblewrap (bwrap) is unavailable on this host");
        };
        if cmd.is_empty() {
            bail!("sandbox command cannot be empty");
        }

        let ws_real = fs::canonicalize(allowed_workspace)
            .unwrap_or_else(|_| allowed_workspace.to_path_buf());
        let args = self.build_bwrap_args(cmd, &ws_real, opts);

        let mut command = Command::new(bwrap);
        command.args(&args);
        command.env_clear();
        command.env("PATH", "/usr/local/bin:/usr/bin:/bin");
        command.env("HOME", &ws_real);
        command.env("PYTHONDONTWRITEBYTECODE", "1");
        for (k, v) in &opts.env_overrides {
            command.env(k, v);
        }

        let t0 = Instant::now();
        let output = command
            .output()
            .with_context(|| format!("failed to spawn bwrap sandboxed process {:?}", cmd))?;
        let elapsed_ms = (t0.elapsed().as_secs_f64() * 1000.0 * 1000.0).round() / 1000.0;

        let returncode = output.status.code().unwrap_or(-1);
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let kernel_denied = detect_kernel_denial(returncode, &stderr);

        Ok(SandboxExecResult {
            command: cmd.to_vec(),
            returncode,
            stdout,
            stderr,
            elapsed_ms,
            sandbox_enforced: true,
            kernel_denied,
        })
    }
}

/// Cross-platform OS sandbox facade auto-selecting macOS Seatbelt or Linux bubblewrap.
pub enum LocalOsSandbox {
    MacOS(MacOSSeatbeltSandbox),
    Linux(LinuxBubblewrapSandbox),
}

impl LocalOsSandbox {
    pub fn new(pool_root: impl AsRef<Path>, forbidden_read_paths: &[PathBuf]) -> Self {
        if cfg!(target_os = "macos") {
            Self::MacOS(MacOSSeatbeltSandbox::new(pool_root, forbidden_read_paths))
        } else {
            Self::Linux(LinuxBubblewrapSandbox::new(pool_root, forbidden_read_paths))
        }
    }

    pub fn is_available(&self) -> bool {
        match self {
            Self::MacOS(s) => s.is_available(),
            Self::Linux(s) => s.is_available(),
        }
    }

    pub fn mechanism_name(&self) -> &'static str {
        match self {
            Self::MacOS(_) => "macOS Seatbelt (sandbox-exec) global deny file-write* + pool/secret read-deny + workspace allowlist",
            Self::Linux(_) => "Linux bubblewrap (bwrap) global --ro-bind / + tmpfs pool/secret masking + workspace --bind + --unshare-net",
        }
    }

    pub fn run(
        &self,
        cmd: &[String],
        allowed_workspace: &Path,
        opts: &SandboxRunOptions,
    ) -> Result<SandboxExecResult> {
        match self {
            Self::MacOS(s) => s.run(cmd, allowed_workspace, opts),
            Self::Linux(s) => s.run(cmd, allowed_workspace, opts),
        }
    }
}

/// Tracks worker slots whose cleanup failed so poisoned environments are never reused.
#[derive(Debug, Default)]
pub struct QuarantineRegistry {
    quarantined: RwLock<BTreeMap<String, String>>,
}

impl QuarantineRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn mark_poisoned(&self, worker_id: &str, reason: &str) {
        self.quarantined
            .write()
            .insert(worker_id.to_string(), reason.to_string());
    }

    pub fn is_reusable(&self, worker_id: &str) -> bool {
        !self.quarantined.read().contains_key(worker_id)
    }

    pub fn snapshot(&self) -> BTreeMap<String, String> {
        self.quarantined.read().clone()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxCheckOutcome {
    pub passed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stderr: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxOverheadReport {
    pub bare_exec_mean_ms: f64,
    pub sandbox_exec_mean_ms: f64,
    pub added_sandbox_overhead_ms: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxVerificationReport {
    pub measurement_type: String,
    pub platform: String,
    pub sandbox_mechanism: String,
    pub all_invariants_passed: bool,
    pub checks: BTreeMap<String, SandboxCheckOutcome>,
    pub overhead_benchmark: SandboxOverheadReport,
}

/// Execute real OS child processes under the host sandbox (macOS Seatbelt or Linux bwrap)
/// to prove all 7 kernel isolation invariants, including arbitrary outside-workspace host writes.
pub fn run_os_sandbox_verification_suite(base_dir: &Path) -> Result<SandboxVerificationReport> {
    if base_dir.exists() {
        fs::remove_dir_all(base_dir)?;
    }
    fs::create_dir_all(base_dir)?;
    let base_real = fs::canonicalize(base_dir)?;

    let pool_root = base_real.join("pool");
    let secret_dir = base_real.join("host_secrets");
    let unrelated_host_dir = base_real.join("unrelated_host_writable");
    let l0_ro = pool_root.join("l0_shared_ro");
    let run1_ws = pool_root.join("worker_0_run1");
    let run2_ws = pool_root.join("worker_0_run2");
    for d in [&secret_dir, &unrelated_host_dir, &l0_ro, &run1_ws, &run2_ws] {
        fs::create_dir_all(d)?;
    }

    fs::write(secret_dir.join("host_canary.txt"), "HOST_SECRET_CANARY_999")?;
    fs::write(l0_ro.join("shared_lib.txt"), "SHARED_READONLY_L0_DATA")?;
    fs::write(run1_ws.join("private_canary.txt"), "RUN1_PRIVATE_CANARY_ABC")?;
    fs::write(run2_ws.join("task.txt"), "RUN2_ALLOWED_TASK_INPUT")?;

    let escape_link = run2_ws.join("symlink_to_secret.txt");
    #[cfg(unix)]
    std::os::unix::fs::symlink(secret_dir.join("host_canary.txt"), &escape_link)?;

    let sb = LocalOsSandbox::new(&pool_root, &[secret_dir.clone()]);
    if !sb.is_available() {
        bail!("No supported OS sandbox binary available on this platform");
    }
    let ro_opts = SandboxRunOptions {
        extra_readonly_paths: vec![l0_ro.clone()],
        ..Default::default()
    };

    // 1. Allowed workspace read/write + shared L0 RO read
    let r_allow = sb.run(
        &[
            "/bin/sh".into(),
            "-c".into(),
            format!(
                "cat \"{}/task.txt\" && cat \"{}/shared_lib.txt\" && echo ok > \"{}/out.txt\"",
                run2_ws.display(),
                l0_ro.display(),
                run2_ws.display()
            ),
        ],
        &run2_ws,
        &ro_opts,
    )?;

    // 2. Host secret read blocked
    let r_host_secret = sb.run(
        &[
            "/bin/cat".into(),
            secret_dir.join("host_canary.txt").to_string_lossy().into(),
        ],
        &run2_ws,
        &ro_opts,
    )?;

    // 3. Sibling run private canary blocked
    let r_prev_canary = sb.run(
        &[
            "/bin/cat".into(),
            run1_ws.join("private_canary.txt").to_string_lossy().into(),
        ],
        &run2_ws,
        &ro_opts,
    )?;

    // 4. Symlink escape blocked
    let r_symlink = sb.run(
        &["/bin/cat".into(), escape_link.to_string_lossy().into()],
        &run2_ws,
        &ro_opts,
    )?;

    // 5. L0 RO tamper write blocked
    let r_l0_write = sb.run(
        &[
            "/bin/sh".into(),
            "-c".into(),
            format!("echo pwned > \"{}/tamper.txt\"", l0_ro.display()),
        ],
        &run2_ws,
        &ro_opts,
    )?;

    // 6. Arbitrary outside-workspace host write blocked (not under pool_root or secret_dir!)
    let outside_target = unrelated_host_dir.join("arbitrary_escape_write.txt");
    let r_arbitrary_write = sb.run(
        &[
            "/bin/sh".into(),
            "-c".into(),
            format!("echo escaped > \"{}\"", outside_target.display()),
        ],
        &run2_ws,
        &ro_opts,
    )?;

    // 7. Outbound internet blocked
    let r_net = sb.run(
        &[
            "/bin/sh".into(),
            "-c".into(),
            "curl -sS --max-time 2 https://1.1.1.1".into(),
        ],
        &run2_ws,
        &ro_opts,
    )?;

    // Bare vs sandboxed subprocess overhead (10 trials)
    let mut bare_ms = Vec::with_capacity(10);
    let mut sb_ms = Vec::with_capacity(10);
    let task_path = run2_ws.join("task.txt").to_string_lossy().to_string();
    for _ in 0..10 {
        let t0 = Instant::now();
        let _ = Command::new("/bin/cat").arg(&task_path).output()?;
        bare_ms.push(t0.elapsed().as_secs_f64() * 1000.0);

        let res = sb.run(&["/bin/cat".into(), task_path.clone()], &run2_ws, &ro_opts)?;
        sb_ms.push(res.elapsed_ms);
    }

    let bare_mean = ((bare_ms.iter().sum::<f64>() / bare_ms.len() as f64) * 100.0).round() / 100.0;
    let sb_mean = ((sb_ms.iter().sum::<f64>() / sb_ms.len() as f64) * 100.0).round() / 100.0;
    let added = ((sb_mean - bare_mean) * 100.0).round() / 100.0;

    let all_passed = r_allow.returncode == 0
        && r_allow.stdout.contains("RUN2_ALLOWED_TASK_INPUT")
        && r_allow.stdout.contains("SHARED_READONLY_L0_DATA")
        && r_host_secret.kernel_denied
        && !r_host_secret.stdout.contains("HOST_SECRET_CANARY_999")
        && r_prev_canary.kernel_denied
        && !r_prev_canary.stdout.contains("RUN1_PRIVATE_CANARY_ABC")
        && r_symlink.kernel_denied
        && !r_symlink.stdout.contains("HOST_SECRET_CANARY_999")
        && r_l0_write.kernel_denied
        && !l0_ro.join("tamper.txt").exists()
        && r_arbitrary_write.kernel_denied
        && !outside_target.exists()
        && r_net.returncode != 0;

    let base_prefix = base_real.to_string_lossy().to_string();
    let redact = |s: &str| s.trim().replace(&base_prefix, "<SANDBOX_TMP>");

    let mut checks = BTreeMap::new();
    checks.insert(
        "allowed_workspace_and_l0_ro".into(),
        SandboxCheckOutcome {
            passed: r_allow.returncode == 0,
            elapsed_ms: Some(r_allow.elapsed_ms),
            stderr: None,
        },
    );
    checks.insert(
        "block_host_secret_read".into(),
        SandboxCheckOutcome {
            passed: r_host_secret.kernel_denied && !r_host_secret.stdout.contains("HOST_SECRET_CANARY_999"),
            elapsed_ms: None,
            stderr: Some(redact(&r_host_secret.stderr)),
        },
    );
    checks.insert(
        "block_prev_run_private_canary".into(),
        SandboxCheckOutcome {
            passed: r_prev_canary.kernel_denied && !r_prev_canary.stdout.contains("RUN1_PRIVATE_CANARY_ABC"),
            elapsed_ms: None,
            stderr: Some(redact(&r_prev_canary.stderr)),
        },
    );
    checks.insert(
        "block_symlink_escape".into(),
        SandboxCheckOutcome {
            passed: r_symlink.kernel_denied && !r_symlink.stdout.contains("HOST_SECRET_CANARY_999"),
            elapsed_ms: None,
            stderr: Some(redact(&r_symlink.stderr)),
        },
    );
    checks.insert(
        "block_l0_tamper_write".into(),
        SandboxCheckOutcome {
            passed: r_l0_write.kernel_denied && !l0_ro.join("tamper.txt").exists(),
            elapsed_ms: None,
            stderr: Some(redact(&r_l0_write.stderr)),
        },
    );
    checks.insert(
        "block_arbitrary_outside_host_write".into(),
        SandboxCheckOutcome {
            passed: r_arbitrary_write.kernel_denied && !outside_target.exists(),
            elapsed_ms: None,
            stderr: Some(redact(&r_arbitrary_write.stderr)),
        },
    );
    checks.insert(
        "block_outbound_network".into(),
        SandboxCheckOutcome {
            passed: r_net.returncode != 0,
            elapsed_ms: None,
            stderr: Some(redact(&r_net.stderr).chars().take(160).collect()),
        },
    );

    Ok(SandboxVerificationReport {
        measurement_type: "real_os_kernel_exec".into(),
        platform: format!("{} ({})", std::env::consts::OS, std::env::consts::ARCH),
        sandbox_mechanism: sb.mechanism_name().into(),
        all_invariants_passed: all_passed,
        checks,
        overhead_benchmark: SandboxOverheadReport {
            bare_exec_mean_ms: bare_mean,
            sandbox_exec_mean_ms: sb_mean,
            added_sandbox_overhead_ms: added,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_quarantine_registry() {
        let reg = QuarantineRegistry::new();
        assert!(reg.is_reusable("worker-0"));
        reg.mark_poisoned("worker-0", "cleanup_eperm");
        assert!(!reg.is_reusable("worker-0"));
        assert_eq!(
            reg.snapshot().get("worker-0").map(|s| s.as_str()),
            Some("cleanup_eperm")
        );
    }

    #[test]
    fn test_macos_profile_uses_global_write_deny() {
        let tmp = TempDir::new().unwrap();
        let sb = MacOSSeatbeltSandbox::new(tmp.path().join("pool"), &[tmp.path().join("sec")]);
        let profile = sb.build_profile(&tmp.path().join("pool/ws"), &SandboxRunOptions::default());
        assert!(profile.contains("(deny file-write*)"));
        assert!(profile.contains("(allow file-write* (literal \"/dev/null\"))"));
    }

    #[test]
    fn test_linux_bwrap_args_enforce_ro_root_and_net_isolation() {
        let tmp = TempDir::new().unwrap();
        let pool = tmp.path().join("pool");
        let ws = pool.join("ws");
        let sec = tmp.path().join("sec");
        fs::create_dir_all(&ws).unwrap();
        fs::create_dir_all(&sec).unwrap();

        let sb = LinuxBubblewrapSandbox::new(&pool, &[sec]);
        let args = sb.build_bwrap_args(
            &["/bin/true".into()],
            &ws,
            &SandboxRunOptions {
                allow_network: false,
                allowed_loopback_port: None,
                ..Default::default()
            },
        );
        assert!(args.windows(3).any(|w| w == ["--ro-bind", "/", "/"]));
        assert!(args.contains(&"--unshare-net".to_string()));
        assert!(args.contains(&"--tmpfs".to_string()));
    }

    #[test]
    fn test_real_os_sandbox_all_7_invariants() {
        let tmp = TempDir::new().unwrap();
        let sb = LocalOsSandbox::new(tmp.path(), &[]);
        if !sb.is_available() {
            return;
        }
        let report = run_os_sandbox_verification_suite(&tmp.path().join("sb")).unwrap();
        assert!(
            report.all_invariants_passed,
            "OS sandbox suite failed: {:?}",
            report.checks
        );
        assert!(report.checks["block_arbitrary_outside_host_write"].passed);
    }
}
