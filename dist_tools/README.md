# LiteBox Windows release

A release contains one binary and one digest-pinned image reference. The image stays unmodified:
there is no rootfs tarball, no packaged OS, no packager step and no image mutation.

## Contents

- `litebox_runner_linux_on_windows_userland.exe` — the thin LiteBox runner. Pulls raw OCI layers
  by digest, caches them in `.litebox-cache/`, mounts them with whiteout merging and boots the
  image's own ENTRYPOINT/CMD/ENV and `/init` as pid 1. Guest syscalls are intercepted in memory.
- `webtop-image.ref` — the pinned OCI index digest for `linuxserver/webtop`.
- `run-webtop.ps1` / `run-webtop.cmd` — launchers; `--publish` maps the guest's HTTP/HTTPS
  ports onto `127.0.0.1` on the host.

## Usage

```powershell
.\run-webtop.ps1
.\run-webtop.ps1 -Publish 8080:3000
```

Directly:

```powershell
$ref = Get-Content webtop-image.ref
.\litebox_runner_linux_on_windows_userland.exe --oci-image $ref --publish 3000:3000 --publish 3001:3001
```

## Networking

`--publish H:G` binds an ordinary `127.0.0.1:H` listener and injects each accepted connection
into the guest's own smoltcp stack at port G. This is the shared rootless userspace gateway
(`litebox_userspace_net`), the same one the Linux runner uses. No Administrator privileges,
driver, TUN device or host container runtime are needed. Guest-initiated outbound TCP/UDP is
proxied through the same gateway.

## Readiness and efficiency

Readiness is a real HTTP/Selkies 200 plus the first real encoded frame, never a fixed sleep.
The source tree's `tools/webtop/readiness.sh` is the Linux-host benchmark; measured results and
the gap to the under-5-second target are in `docs/BENCHMARK.md`.

## Security boundary

LiteBox is a syscall-compatibility layer, not a security sandbox. Treat the runner process as
having exactly the privileges of the account that launched it, and run only mutually trusted
guest workloads. The guest filesystem is virtual and does not expose arbitrary host paths.
