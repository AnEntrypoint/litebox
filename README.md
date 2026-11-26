# LiteBox

LiteBox (forked) runs an **unmodified OCI image as-is, in userspace, on any host**, with no
Docker daemon, Podman, runc, container runtime, root, TUN device or hypervisor. One goal drives
this repository:

> Boot the unmodified `linuxserver/webtop` image under LiteBox, using the image's own layers,
> ENTRYPOINT/CMD/ENV and `/init` as pid 1, and reach a usable GUI as fast and as cheaply as
> possible. Docker reaches a GUI in under 5 seconds; that is the measured target, not an
> assumption.

Everything else in the tree is subordinate to that goal. `docker.io` is only a registry
hostname. LiteBox pulls manifests and blobs itself with anonymous registry auth.

## Why this is small

- **No image mutation.** Raw layer tars are kept byte-for-byte and mounted bottom-to-top with
  whiteout merging (`litebox::fs::tar_ro::TarRo::from_layers`). No ELF rewriting, rootfs
  flattening, symlink materialization, pruning, generated `config_and_run.sh`, injected s6
  services, Selkies/sitecustomize/xkbcomp patches or nginx overrides.
- **Syscall interception is in memory.** Unpatched guest binaries are handled at mmap time or
  through the platform's SIGSYS/SIGTRAP guest-syscall traps. Nothing on the image is patched.
- **One engine, two hosts.** The Linux runner (`litebox_runner_linux_userland`) and the Windows
  runner (`litebox_runner_linux_on_windows_userland`) share the same raw-OCI pull/cache library
  (`litebox_packager::oci`) and the same userspace TCP gateway (`litebox_userspace_net`).
- **No privileges.** Published ports are ordinary `127.0.0.1` listeners bridged into the guest's
  own smoltcp stack by direct packet injection. No root, no TUN, no driver.
- **Thin releases.** A release is the runner binary plus a digest-pinned image reference. There
  is no rootfs tarball, no packaged OS and no packager step in a release.
- **Readiness, not sleepiness.** The benchmark waits for a real HTTP/Selkies 200 and the first
  real encoded frame, then reports wall time, peak memory, image bytes and process count.

## The image reference

`dist_tools/webtop-image.ref` holds the sole pinned reference:

```
docker.io/linuxserver/webtop@sha256:8bb9f1ac0bcf64d6e0be56bcd65dbfa985ee8c2b09c16820ddae29d3e44aeaa3
```

It is an OCI **index** digest (`linux/amd64` and `linux/arm64`), so one pin is portable across
hosts. A moving tag is not accepted anywhere in a release, launcher, smoke test or benchmark.

## Run it

```sh
cargo build --release -p litebox_runner_linux_userland
./target/release/litebox_runner_linux_userland \
  --oci-image "$(cat dist_tools/webtop-image.ref)" \
  --publish 3000:3000 --publish 3001:3001
```

With no command after the flags, the runner boots the image's own ENTRYPOINT/CMD (`/init`) as
pid 1 with the image's own ENV. Guest arguments, if given, replace CMD exactly as they do for a
container runtime. `--env K=V` overrides an image variable. `--publish H:G` exposes the guest's
port `G` on the host's `127.0.0.1:H`.

Windows parity (PowerShell):

```powershell
.\litebox_runner_linux_on_windows_userland.exe `
  --oci-image (Get-Content dist_tools\webtop-image.ref) `
  --publish 3000:3000 --publish 3001:3001
```

Layers are cached by digest under `.litebox-cache/`; a warm boot does no network work. The
runner also accepts the older `--initial-files <tar>` / `--program-from-tar` path for local
experiments, but the release and benchmark paths never use it.

## Measure it

```sh
tools/webtop/readiness.sh
```

The benchmark starts the pinned image on the final binary, polls the published HTTP port until
it answers 200, watches the image's own encoder for the first real frame, samples peak RSS and
process/thread count, sums the raw layer bytes, and prints one result line per milestone. It
never treats a fixed sleep as readiness. The latest measured result and the gap to the
under-5-second target are in `docs/BENCHMARK.md`.

## Repository shape

| Path | Role |
| --- | --- |
| `litebox/` | core kernel/shim state: fs (including `tar_ro::from_layers`), net, pipes, sync |
| `litebox_shim_linux/` | Linux syscall shim, ELF loader, mmap-time interception |
| `litebox_platform_linux_userland/` | Linux host platform, seccomp/SIGSYS guest traps, DRM presentation |
| `litebox_platform_windows_userland/` | Windows host platform, VEH guest traps, cross-process fork |
| `litebox_userspace_net/` | shared rootless TCP gateway and published-port injection |
| `litebox_packager/` | raw OCI manifest/config/layer pull and digest cache (library only) |
| `litebox_runner_linux_userland/` | thin Linux runner: `--oci-image`, `--publish`, readiness |
| `litebox_runner_linux_on_windows_userland/` | Windows parity runner |
| `dist_tools/`, `tools/webtop/` | launchers, digest pin, readiness benchmark |
| `docs/HISTORICAL.md` | pre-goal harness notes, archives and retired experiments |

`AGENTS.md` holds the binding engineering rules. Everything under `docs/AGENTS_ARCHIVE_*`,
`docs/LINUX-CHROMIUM.md`, `docs/WEBTOP-IN-LITEBOX-*` and `docs/WEBTOP-REBUILD-*` is historical:
it describes the old rewritten-rootfs harness and is not an instruction for this tree.

## Efficiency contract

Startup work is strictly one of: resolve a digest (cached), mmap cached layers (no copies),
merge the layer index (cached), install the image's own config, and start pid 1. Any new step
that pulls, rewrites, extracts, prunes or wraps image content before pid 1 starts is out of
contract. Measurements, including failures, are recorded instead of hidden.

## License

MIT; see `LICENSE` and `NOTICE.txt`.
