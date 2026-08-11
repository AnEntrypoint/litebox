# litebox-windows-alpine bundle

Contents:

- `litebox_runner_linux_on_windows_userland.exe` — the LiteBox runner; runs
  unmodified Linux ELF binaries on Windows.
- `alpine-rootfs.tar` — an Alpine Linux rootfs (BusyBox userland) with
  `nodejs`, `npm`, `python3`, `py3-pip`, `build-base`, `git`, `bash`, `curl`,
  `openssh-client`, and `tzdata` preinstalled, built from
  [`dist_tools/base-image/Dockerfile`](base-image/Dockerfile), published to
  `ghcr.io/<org>/litebox-alpine-base`, and pre-rewritten with the LiteBox
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
setup, and `python3-dev` is included alongside it -- Alpine splits Python's
own C headers (`Python.h`) into a separate package from the interpreter, so
without it `pip install <anything with a C extension>` fails immediately
with `Python.h: No such file or directory` even with `build-base` present.
This bites harder on Alpine than on glibc-based images, since most PyPI
wheels are `manylinux`-tagged rather than `musllinux`, so `pip` falls back
to a source build (and therefore needs `Python.h`) far more often here.
`git`, `bash`, `curl`, and `openssh-client` are also preinstalled,
covering the common day-one needs of an agent workload (cloning a repo,
running `#!/bin/bash` scripts shipped by npm packages, fetching a file) so
that no extra `apk add` round-trip is needed just to get started. `tzdata`
is included too -- Alpine/musl doesn't ship timezone data by default, which
otherwise silently breaks Python's `zoneinfo` module (and any date/time
library built on it) for any timezone other than UTC. Nothing about the
base image blocks installing further tooling at runtime — see
Networking below for `apk add`/`npm install`/`pip install`.

## Networking

Real TCP/UDP networking (DNS, HTTP, HTTPS, `apk add <package>`, ...) works
out of the box, with **no Administrator privileges and no driver
installation required**. This is implemented as an in-process userspace NAT
gateway: guest TCP/UDP flows are proxied to real, unprivileged Windows
sockets rather than requiring a virtual network adapter (unlike a TUN driver
or WinDivert, both of which need elevation on Windows). See
`litebox_platform_windows_userland::net` in the source tree for details.

## Persistent, resumable state

By default, everything the guest writes (installed packages, generated
files, `.npm`/`.pip` caches, ...) lives only in memory and is lost when the
runner exits. Two flags make a session's on-disk state durable and
resumable:

```
litebox_runner_linux_on_windows_userland.exe --initial-files alpine-rootfs.tar ^
  --export-writable-layer session.tar /bin/sh -c "npm install -g some-tool"

litebox_runner_linux_on_windows_userland.exe --initial-files alpine-rootfs.tar ^
  --resume-from session.tar /bin/sh -c "some-tool --version"
```

`--export-writable-layer <path>` walks every file the guest created or
modified this run and writes it to a tar archive after the program exits.
`--resume-from <path>` seeds the writable layer from a previously exported
archive before the guest program starts. The exported archive is a delta
against the base rootfs, not a full snapshot, so it stays small regardless
of how large `alpine-rootfs.tar` is.

## Security boundary — read this before using LiteBox as an agent sandbox

**This is a Linux-on-Windows compatibility/porting layer, not a security
sandbox.** If you're running untrusted or semi-trusted agent-generated code
(e.g. an LLM agent's Node.js/Python workload) and relying on this to contain
it, understand exactly what isolation does and does not exist:

- **No process isolation between guests.** Every guest "process" started by
  a single runner invocation is, under the hood, an ordinary Windows thread
  inside that one host process — there is no OS-level process boundary
  between them. Two guest programs run by the same `--initial-files`
  invocation can observe and interfere with each other's host-process-global
  state.
- **No memory isolation.** All guest threads share the host process's
  address space. There is no per-guest memory protection beyond what the
  syscall rewriter and shim happen to enforce in software.
- **The syscall rewriter is a compatibility shim, not a policy-enforcement
  layer.** It patches `syscall` instructions in the guest ELF with a
  trampoline so LiteBox can intercept and emulate them — it does not
  implement a seccomp-style allow/deny policy. Every syscall the shim
  implements is serviced; there is no mechanism to deny a guest syscall on
  security grounds.
- **No privilege drop.** "No Administrator privileges required" describes
  what LiteBox avoids needing (a TUN driver, raw sockets) — it is not an
  added sandboxing token. The guest runs at exactly the host process's own
  Windows privilege level. There is no AppContainer, restricted token, or
  Job Object confinement.
- **No resource limits enforced by the host.** Only `RLIMIT_NOFILE` and
  `RLIMIT_SIGPENDING` are tracked (as in-process bookkeeping, not
  OS-enforced caps). There is no CPU or memory limit — a guest process can
  consume the full, unbounded resource budget of the host Windows process.
- **The guest filesystem is virtual and does not expose the host
  filesystem** — the rootfs is entirely in-memory/tar-backed, with no
  passthrough to arbitrary host paths. This part *is* a real boundary.

**Practical implication:** treat every guest program run by the same runner
invocation as fully mutually trusted, and treat the runner process itself as
having exactly the privileges of whatever account launched it. Do not run
code you don't trust at that privilege level under the expectation that
LiteBox will contain a deliberately malicious payload — it is built to run
*unmodified* Linux programs correctly, not to defend against one that is
actively trying to escape.

All files must stay in the same directory.
