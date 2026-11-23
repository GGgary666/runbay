use super::SandboxRunOptions;
use anyhow::{bail, ensure, Context, Result};
use std::fs;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

pub(super) fn launcher() -> Option<PathBuf> {
    // Installed location can have a narrowly scoped Ubuntu AppArmor userns profile.
    [
        Some(PathBuf::from("/usr/local/libexec/runbay-landlock")),
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("runbay-landlock"))),
        Some(PathBuf::from(concat!(env!("OUT_DIR"), "/runbay-landlock"))),
    ]
    .into_iter()
    .flatten()
    .find(|p| p.is_file())
}

pub(super) fn arguments(
    cmd: &[String],
    workspace: &Path,
    pool: &Path,
    forbidden: &[PathBuf],
    opts: &SandboxRunOptions,
) -> Result<Vec<String>> {
    ensure!(!cmd.is_empty(), "sandbox command cannot be empty");
    ensure!(
        opts.allow_network
            || opts.allowed_loopback_port.is_none()
            || opts.gateway_unix_socket.is_some(),
        "FAIL_CLOSED: use a Unix socket for isolated Linux gateway access"
    );
    let mut exposures: Vec<(&str, PathBuf)> = Vec::new();
    let mut forbid = forbidden.to_vec();
    for path in &opts.extra_forbidden_reads {
        forbid.push(fs::canonicalize(path).unwrap_or_else(|_| path.clone()));
    }
    // These are the runtime, loader and small device files. No host home, /run or /etc tree.
    for name in [
        "/usr",
        "/bin",
        "/sbin",
        "/lib",
        "/lib64",
        "/etc/ld.so.cache",
    ] {
        if Path::new(name).exists() {
            exposures.push(("--ro", name.into()));
        }
    }
    if opts.allow_network {
        for name in [
            "/etc/hosts",
            "/etc/resolv.conf",
            "/etc/nsswitch.conf",
            "/etc/ssl/certs",
        ] {
            if Path::new(name).exists() {
                exposures.push(("--ro", name.into()));
            }
        }
    }
    for name in ["/dev/null", "/dev/zero", "/dev/urandom", "/dev/random"] {
        if Path::new(name).exists() {
            exposures.push(("--device", name.into()));
        }
    }
    let ws = fs::canonicalize(workspace).context("resolve isolated workspace")?;
    exposures.push(("--workspace", ws.clone()));
    for (mode, paths) in [
        ("--ro", &opts.extra_readonly_paths),
        ("--rw", &opts.extra_writable_paths),
    ] {
        for path in paths {
            exposures.push((
                mode,
                fs::canonicalize(path).with_context(|| format!("resolve {}", path.display()))?,
            ));
        }
    }
    let binary = Path::new(&cmd[0]);
    let executable = if binary.is_absolute() {
        binary.to_owned()
    } else if binary.components().count() > 1 {
        ws.join(binary)
    } else {
        ["/usr/local/bin", "/usr/bin", "/bin"]
            .iter()
            .map(|d| Path::new(d).join(binary))
            .find(|p| p.is_file())
            .context("resolve worker executable")?
    };
    let executable = fs::canonicalize(executable).context("resolve worker executable")?;
    if !exposures.iter().any(|(_, p)| executable.starts_with(p)) {
        exposures.push(("--ro", executable.clone()));
    }
    let gateway = opts
        .gateway_unix_socket
        .as_ref()
        .map(fs::canonicalize)
        .transpose()?;
    if let Some(sock) = &gateway {
        ensure!(
            fs::metadata(sock)?.file_type().is_socket(),
            "gateway endpoint must be a Unix socket"
        );
        exposures.push(("--ro", sock.clone()));
    }
    let mut args = vec![];
    if opts.allow_network {
        args.push("--network".to_string());
    }
    'exposure: for (i, (mode, path)) in exposures.iter().enumerate() {
        let canonical = fs::canonicalize(path)?;
        ensure!(
            path != Path::new("/")
                && !canonical.starts_with("/proc")
                && !canonical.starts_with("/sys"),
            "invalid sandbox exposure {}",
            path.display()
        );
        let gateway_exception = gateway.as_ref() == Some(path) && *mode == "--ro";
        ensure!(
            gateway_exception
                || !forbid
                    .iter()
                    .any(|f| canonical.starts_with(f) || f.starts_with(&canonical)),
            "sandbox exposure overlaps a forbidden path: {}",
            path.display()
        );
        ensure!(
            canonical != pool || *mode == "--workspace",
            "cannot expose the entire worker pool"
        );
        ensure!(
            !pool.starts_with(&canonical) || canonical == pool,
            "cannot expose a parent of the worker pool"
        );
        for (other_mode, other) in &exposures[..i] {
            if path.starts_with(other)
                && (*mode == *other_mode || (*mode == "--rw" && *other_mode == "--workspace"))
            {
                continue 'exposure;
            }
            if path.starts_with(other) || other.starts_with(path) {
                bail!(
                    "overlapping sandbox exposures: {other_mode} {} and {mode} {}",
                    other.display(),
                    path.display()
                );
            }
        }
        args.push((*mode).to_string());
        args.push(path.to_str().context("non-UTF8 sandbox path")?.to_string());
    }
    args.push("--".into());
    args.push(executable.to_str().context("non-UTF8 worker path")?.into());
    args.extend_from_slice(&cmd[1..]);
    Ok(args)
}

pub(super) fn available(binary: &Path) -> bool {
    static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        let probe = || -> Result<bool> {
            let tmp = tempfile::tempdir()?;
            let args = arguments(
                &["/bin/true".into()],
                tmp.path(),
                tmp.path(),
                &[],
                &Default::default(),
            )?;
            Ok(Command::new(binary)
                .env_clear()
                .args(args)
                .output()?
                .status
                .success())
        };
        probe().unwrap_or(false)
    })
}

pub(super) struct Handshake {
    read: UnixStream,
    write: UnixStream,
}

impl Handshake {
    pub(super) fn install(command: &mut Command) -> Result<Self> {
        let (read, write) = UnixStream::pair()?;
        read.set_read_timeout(Some(std::time::Duration::from_secs(10)))?;
        let fd = write.as_raw_fd();
        command.args(["--ready", &fd.to_string()]);
        unsafe {
            command.pre_exec(move || {
                if libc::fcntl(fd, libc::F_SETFD, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        Ok(Self { read, write })
    }

    pub(super) fn wait(self) -> Result<()> {
        let Self { mut read, write } = self;
        drop(write);
        let mut reply = Vec::new();
        read.read_to_end(&mut reply)?;
        ensure!(
            reply == b"R",
            "native sandbox setup failed: {}",
            String::from_utf8_lossy(&reply)
        );
        Ok(())
    }
}
