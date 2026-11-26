# LiteBox project rules (2026-11-26)

## The one goal

Run the **unmodified `linuxserver/webtop` OCI image as-is** under LiteBox (forked), in ordinary
userspace, on Linux and Windows, with no Docker daemon, Podman, runc, root, TUN device or
hypervisor. Boot the image's own raw layers, ENTRYPOINT/CMD/ENV and `/init` as pid 1; intercept
syscalls in memory only. Reach a usable GUI as fast as possible: Docker's under-5-seconds-to-GUI
is the measured target. Maximum efficiency and portability are load-bearing, not aspirational.

`docker.io` is a registry hostname only. LiteBox pulls manifests/blobs itself. Anything that
mutates, flattens, prunes, rewrites or wraps image content before pid 1 starts is forbidden.

## Architecture that serves the goal

- `litebox_packager::oci` is a library for raw OCI: manifest + config (Env/Entrypoint/Cmd/
  WorkingDir), digest-keyed layer cache, resumable blob pulls, gzip decompression to mmap'd cache
  files. It has no binary, no ELF rewriter and no rootfs extractor.
- `litebox::fs::tar_ro::TarRo::from_layers` merges raw layers bottom-to-top, whiteouts included.
  Both runners mount that through `LinuxShimBuilder::default_fs_multi_layer`.
- `litebox_userspace_net` is the one rootless TCP gateway. It terminates guest packets in a
  private smoltcp interface, injects them through `IPInterfaceProvider::receive_ip_packet`, and
  bridges each flow to an ordinary unprivileged host socket. `--publish H:G` binds
  `127.0.0.1:H` and injects a connection to the guest's port G. Both platforms use it.
- Unpatched guest ELFs are accepted: x86_64/aarch64 mmap-time interception, `ICEBP;HLT` and
  SIGSYS/SIGTRAP guest-syscall traps. A guest syscall reaching seccomp must trap to LiteBox,
  never be answered with a bare EINVAL in release builds.
- The image reference is digest-pinned in `dist_tools/webtop-image.ref`. A moving tag in a
  release, launcher, smoke or benchmark is a defect.
- A release is the thin runner plus that pin. No `alpine-rootfs.tar`, no packager invocation, no
  rewritten tar, no generated entrypoint.
- Readiness is a real HTTP/Selkies 200 plus the first real encoded frame; a fixed sleep is never
  readiness. `tools/webtop/readiness.sh` is the only benchmark.

## Working rules (binding)

- Work on `main` in this checkout. No clones or worktrees. Commit as `lanmower`, with no
  trailers and no tool/model attribution. Never stage a file the owner left uncommitted.
  Never push to `origin` without the owner's per-instance instruction.
- Two phases. Phase A: all changes, docs and tests in one pass, no builds; commit each change
  locally and list it in `docs/PENDING_VALIDATION.md` with its witness and red check. Phase B:
  one window on the final tree runs every listed witness and due red check, fixes failures, then
  pushes once.
- Time: do each thing once on final inputs. Prove each change with the narrowest check that can
  fail for it. Rerun only a failing check. Reuse a cache or an existing build instead of
  rebuilding. While something runs, work everything it does not test; never poll; end the turn
  naming what is pending and where its result lands.
- Decide every reversible choice yourself and give the reason afterwards. Never end a turn to
  ask about scope or size.
- No comments in code. Say it in names and structure; what cannot be said that way goes into this
  file once checked true.
- Simplest code that does the job: delete, deduplicate, share modules, drive behaviour from data.
- Heavy windows (compile, many tests, timing measurement) start detached through the gate:
  `GATE_TREE=$(git rev-parse --short HEAD) setsid nohup node /config/workspace/hyperagent/scripts/gate-ticket.cjs run litebox-<part> -- <command> > <result file> 2>&1 < /dev/null &`;
  the result file's last line is `<PART>_DONE <exit code>`. A timing witness needs
  `GATE_EXCLUSIVE=1`. A label that already passed on its `GATE_TREE` prints CACHED.
- Scratch goes in `/config/workspace/tmp/litebox`; remove it and stop every process you started
  by PID when done. Never use a name pattern to kill processes.
- Browser work goes through playwriter where available; an HTTP status question uses curl first.
- gm `codesearch`: `mode: "literal"` is exhaustive (use it to prove absence); `mode: "dual"` is
  a ranked sample (never conclude absence from it). `cwd` uses forward slashes.
- Guest-reachable refusals are errnos or signals, never panics: the host process is the whole
  session. `TypedFd` indices are valid only against the `Descriptors` that inserted them;
  shared-memory structs hold no process-relative pointers.
- A process's in-memory writes are invisible to siblings until it is reaped. Shared registries
  key on `(dev, path)`, never `(dev, ino)`: a fork child renumbers runtime-created inodes.
- Logs: `LITEBOX_LOG`, not `RUST_LOG`. Timestamps are per-process uptime; never subtract across
  a parent and a fork child.
- Repo hygiene: layer tars, frame dumps and debug logs never go in git. Never push to `origin`
  without per-instance confirmation.

## Commands

```sh
export PATH=/config/.cargo/bin:$PATH
cargo fmt --all -- --check
cargo build --release -p litebox_runner_linux_userland
cargo clippy --all-targets --all-features -- -D warnings
cargo nextest run                     # or cargo test --workspace when nextest is unavailable
scripts/check-goal-docs.sh            # docs consistency witness
tools/webtop/readiness.sh             # end-to-end benchmark
```

Windows parity is compile-checked with
`cargo check -p litebox_runner_linux_on_windows_userland --target x86_64-pc-windows-gnu`.

## Historical material (not instructions)

`docs/AGENTS_ARCHIVE_*`, `docs/HARNESS-LESSONS.md`, `docs/LINUX-CHROMIUM.md`,
`docs/WEBTOP-IN-LITEBOX-2026-10-10.md`, `docs/WEBTOP-REBUILD-2026-10-12.md`, `advisor/`,
`tools/webtop/custom-services.d/` and `x7.tar` describe the retired rewritten-rootfs harness,
injected s6 services, overlays, monkey-patches and pruning probes. They are kept only as
evidence of what was tried; see `docs/HISTORICAL.md`. No active release, launcher, smoke or
benchmark may invoke them.
