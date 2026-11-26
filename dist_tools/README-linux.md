# LiteBox Linux release

A release contains one binary and one digest-pinned image reference. There is no rootfs tarball,
no packaged OS, no packager step and no image mutation.

## Contents

- `litebox_runner_linux_userland` — the thin LiteBox runner. Pulls raw OCI layers by digest,
  caches them in `.litebox-cache/`, mounts them with whiteout merging and boots the image's own
  ENTRYPOINT/CMD/ENV and `/init` as pid 1. Guest syscalls are intercepted in memory.
- `webtop-image.ref` — the pinned OCI index digest for `linuxserver/webtop`.
- `run-webtop.sh` — launcher; `--publish` maps the guest's HTTP/HTTPS ports onto
  `127.0.0.1` on the host.

## Usage

```sh
./run-webtop.sh                       # image as-is: its own /init, ports 3000 and 3001
./run-webtop.sh --env PUID=1000      # -e style overrides, image bytes untouched
./run-webtop.sh /bin/sh              # CMD override, same unmodified image
./run-webtop.sh --publish 8080:3000  # different host port
```

Directly:

```sh
./litebox_runner_linux_userland --oci-image "$(cat webtop-image.ref)" \
  --publish 3000:3000 --publish 3001:3001
```

## Networking

`--publish H:G` binds an ordinary `127.0.0.1:H` listener and injects each accepted connection
into the guest's own smoltcp stack at port G. This is the shared userspace gateway
(`litebox_userspace_net`), also used on Windows. It needs no root, no TUN device, no driver and
no host container runtime. Guest-initiated outbound TCP/UDP is proxied through the same gateway.

## Readiness and efficiency

`tools/webtop/readiness.sh` in the source tree measures the real milestones: HTTP/Selkies 200 on
the published port, the first real encoded frame, peak RSS, raw layer bytes and process count.
No fixed sleep is used as readiness. Measured results and the gap to the under-5-second target
are in `docs/BENCHMARK.md`.

## Security boundary

LiteBox is a syscall-compatibility layer, not a security sandbox. Guest "processes" share the
runner host process's address space and privilege level. The guest filesystem is virtual and
does not expose arbitrary host paths. Run only code you trust at the privilege of the account
that launched the runner; pair it with host isolation primitives if you need containment.
