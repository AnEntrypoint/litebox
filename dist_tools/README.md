# litebox-windows-alpine bundle

Contents:

- `litebox_runner_linux_on_windows_userland.exe` — the LiteBox runner; runs
  unmodified Linux ELF binaries on Windows.
- `alpine-rootfs.tar` — an Alpine Linux rootfs (BusyBox userland) with
  `nodejs`, `npm`, `python3`, `py3-pip`, and `build-base` preinstalled, built
  from [`dist_tools/base-image/Dockerfile`](base-image/Dockerfile), published
  to `ghcr.io/<org>/litebox-alpine-base`, and pre-rewritten with the LiteBox
  syscall rewriter via `litebox-packager --oci-image`.
- `run-alpine.cmd` / `run-alpine.ps1` — launcher scripts.

## Usage

```
run-alpine.cmd busybox ls /       # run a single busybox applet
run-alpine.cmd /bin/busybox ls /  # an already-absolute path is used as-is
```

Or directly:

```
litebox_runner_linux_on_windows_userland.exe --initial-files alpine-rootfs.tar /bin/busybox ls /
```

A bare command name (e.g. `busybox`) passed to the launcher scripts is
resolved to `/bin/busybox` automatically; an already-absolute path is left
unchanged.

Interactive shells and subprocesses (`/bin/sh`, `sh -c "cmd1; cmd2"`, etc.)
work normally.

## Language runtimes

`node`, `npm`, `python3`, and `pip3` are preinstalled and ready to run agent
workloads out of the box. `build-base` (gcc, make, and friends) is included
so native npm/pip packages that need a compiler can build without any extra
setup. Nothing about the base image blocks installing further tooling at
runtime — see Networking below for `apk add`/`npm install`/`pip install`.

## Networking

Real TCP/UDP networking (DNS, HTTP, HTTPS, `apk add <package>`, ...) works
out of the box, with **no Administrator privileges and no driver
installation required**. This is implemented as an in-process userspace NAT
gateway: guest TCP/UDP flows are proxied to real, unprivileged Windows
sockets rather than requiring a virtual network adapter (unlike a TUN driver
or WinDivert, both of which need elevation on Windows). See
`litebox_platform_windows_userland::net` in the source tree for details.

All files must stay in the same directory.
