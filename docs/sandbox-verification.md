# Linux sandbox verification

## Kernel and daemon checks

| Environment | Checks exercised |
| --- | --- |
| Ubuntu 24.04.5 ARM64, kernel 6.8.0-147, bubblewrap 0.9.0 | Workspace tests and seven kernel invariants, authenticated model gateway, concurrent jobs, cancellation, timeout, daemon restart and delegated resource limits |
| Same VM, native Landlock and seccomp | Same suite, plus native setup and exec failure reporting. The host-side loopback positive control succeeded. The worker could reach the gateway through the bound Unix socket. Host TCP and direct internet access were denied. |
| Ubuntu 5.13.0-19 x86-64, Landlock ABI 1, offline QEMU boot | Ordinary UID 1000, distinct network namespace, active seccomp, Landlock denial on a writable private tmpfs, successful workspace write, read-only input protected from truncation and a Unix socket request/reply. The initramfs contains only the launcher and a test executable. |
| macOS 27.0.1 ARM64 | Workspace tests, Seatbelt self-check and the beta-bench HTTP daemon and gateway oracle |

On 6.8, a private mount namespace also removed bubblewrap from all three discovery paths. The daemon in `auto` mode selected Landlock, passed self-check and answered the sample CoWiki question over its Unix socket. The offline upstream verified the complete Markdown and the question before replying. The answer identified Macro-F1 and cited `paper-note.md` lines 1–5. The daemon confirmed HTTP 200.

The delegated tests set a 20% CPU quota, 48 MiB memory and eight tasks. Both Linux backends recorded CPU throttling, PID-limit rejections and memory-cgroup OOM kills. The daemon test kept another Q&A running during cancellation, then checked timeout and restart cleanup. The Linux filesystem tests also rotate symlinks during concurrent writes and check hard-linked inputs. A writable file with a name outside its granted scope is rejected at admission.

Run these commands from the repository root on a Linux host with the applicable namespace permissions and cgroup delegation. Each backend needs a fresh delegated service.

```sh
for backend in bwrap landlock; do
    export RUNBAY_SANDBOX_BACKEND="$backend"
    cargo test --workspace --locked
    cargo run --locked -p runbayd -- self-check
    systemd-run --user --quiet --wait --pipe --collect --property=Delegate=yes \
        --working-directory="$PWD" --setenv="PATH=$PATH" --setenv="RUNBAY_SANDBOX_BACKEND=$backend" \
        sh scripts/with-delegated-cgroup.sh cargo test --workspace --locked cgroup -- --ignored --nocapture
done
```

The CI workflow pins the SHA-256 of Ubuntu's archived 5.13 kernel package. To repeat the offline boot using an extracted kernel, set `KERNEL_IMAGE` and run `sh scripts/test-linux-5.13.sh` with QEMU, cpio and a static x86-64 C toolchain. Use `KERNEL_CC=cc` on x86-64. The 5.13 run covers the native launcher. The cgroup and full daemon results above were measured on 6.8. Landlock ABI 1 restricts cross-directory reparenting and relies on read-only mounts for truncation protection. [Kernel Landlock documentation](https://docs.kernel.org/userspace-api/landlock.html)

## Launch and syscall timing

Each row uses three warm-ups and 30 fresh processes. Launch time runs from the parent starting the operation to the worker entering `main`, using the same monotonic clock. The worker then performs 100,000 one-byte reads from an already-open `/dev/zero`. Read time is the per-process average per call, including the loop. The OCI sample starts a fresh crun 1.14.1 container for each process, using a local read-only root, a private network namespace and worker UID 65534. Only the OCI launcher ran as root.

| Platform and mode | Launch p50 (ms) | Launch p95 (ms) | Read p50 (ns) | Read p95 (ns) |
| --- | ---: | ---: | ---: | ---: |
| Linux ARM64 bare | 0.115 | 0.211 | 131.8 | 135.8 |
| Linux ARM64 bubblewrap | 1.100 | 1.510 | 131.9 | 133.0 |
| Linux ARM64 Landlock + seccomp | 0.714 | 1.124 | 155.6 | 166.7 |
| Linux ARM64 crun | 1.116 | 1.917 | 124.3 | 126.0 |
| macOS ARM64 bare | 1.328 | 1.865 | 247.9 | 255.0 |
| macOS ARM64 Seatbelt | 7.537 | 9.569 | 249.6 | 288.0 |

The Linux measurements used a two-vCPU Multipass guest and an empty benchmark workspace. The macOS measurements used its host. These numbers describe this machine and workload. Compare each backend with the bare row on its own platform. The cost of checking hard links in a populated writable workspace grows with its contents.

```sh
cargo build --release --locked -p runbay-sandbox --example isolation_bench
bench=target/release/examples/isolation_bench
probe=$(mktemp)
# Use cc -O2 on macOS. Use a static Linux binary for the minimal OCI image.
cc -O2 -static scripts/bench/read-probe.c -o "$probe"
"$bench" bare "$probe"
RUNBAY_SANDBOX_BACKEND=bwrap "$bench" sandbox "$probe"
RUNBAY_SANDBOX_BACKEND=landlock "$bench" sandbox "$probe"
sudo python3 scripts/bench/oci.py "$bench" "$probe"
rm "$probe"
```

On macOS, run the `bare` and `sandbox` modes with the locally compiled probe. Seatbelt is selected automatically.
