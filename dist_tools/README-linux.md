# litebox-linux-alpine bundle

Contents:

- `litebox_runner_linux_userland` — the LiteBox runner; runs Linux ELF
  binaries through LiteBox's own syscall-rewriting shim on a Linux host.
- `alpine-rootfs.tar` — an Alpine Linux rootfs (BusyBox userland) with
  `nodejs`, `npm`, `python3`, `py3-pip`, `build-base`, `git`, `bash`, `curl`,
  `openssh-client`, and `tzdata` preinstalled, built from
  [`dist_tools/base-image/Dockerfile`](base-image/Dockerfile), published to
  `ghcr.io/<org>/litebox-alpine-base`, and pre-rewritten with the LiteBox
  syscall rewriter via `litebox-packager --oci-image`. The same rootfs tar
  used by the Windows bundle — guest content is host-architecture-agnostic.
- `run-alpine.sh` — launcher script.

## Usage

```sh
./run-alpine.sh busybox ls /       # run a single busybox applet
./run-alpine.sh /bin/busybox ls /  # an already-absolute path is used as-is
```

Or directly:

```sh
./litebox_runner_linux_userland --unstable --initial-files alpine-rootfs.tar --program-from-tar /bin/busybox ls /
```

A bare command name (e.g. `busybox`) passed to the launcher script is
resolved to `/bin/busybox` automatically; an already-absolute path is left
unchanged.

Interactive shells and subprocesses (`/bin/sh`, `sh -c "cmd1; cmd2"`, etc.)
work normally.

## Stock Alpine XFCE / Graphical Desktop Environment inside LiteBox

LiteBox supports running Wayland/DRM compositors (such as `labwc` with XFCE components `xfsettingsd`, `xfce4-panel`, and `xfdesktop`) directly on top of LiteBox's Linux syscall shim.

### GUI & Display Mapping (DRM to wgpu)
- **Virtual DRM Device (`/dev/dri/card0`)**: LiteBox implements a software DRM/KMS dumb-buffer device in `litebox_shim_linux/src/syscalls/drm.rs`.
- **Dumb Buffer Allocation & Page Flipping**:
  - `DRM_IOCTL_MODE_CREATE_DUMB`: Allocates shared memory backing buffers.
  - `DRM_IOCTL_MODE_MAP_DUMB`: Exposes page-aligned mmap offsets into the guest address space.
  - `DRM_IOCTL_MODE_ADDFB2` & `DRM_IOCTL_MODE_PAGE_FLIP`: Attach scanout framebuffers and trigger vblank/page-flip events (`DrmEventVblank`).
- **wgpu Host Presentation**: Flipped pixel buffers are transferred via the registered flip callback directly to a host `wgpu` rendering surface (e.g., `litebox_platform_windows_userland::presentation::Presenter`).

### Running XFCE / Wayland Compositor in Stock Alpine
To launch the XFCE desktop session inside stock Alpine rootfs under LiteBox:
```sh
# Ensure udev daemon is started and launch labwc with XFCE session components:
udevd --daemon && labwc -s "xfsettingsd & xfce4-panel & xfdesktop &"
```

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

Real TCP/UDP networking (DNS, HTTP, HTTPS, `apk add <package>`, ...) is
supported via a real TUN device (`litebox_platform_linux_userland`), which
**does require root** to create (`scripts/tun-setup.sh` in the source tree)
-- unlike the Windows bundle's userspace-NAT-gateway approach, which needs no
elevation. Networking is opt-in: guest programs that don't touch the network
run fine as an unprivileged user with no TUN device present.

## Persistent, resumable state

By default, everything the guest writes (installed packages, generated
files, `.npm`/`.pip` caches, ...) lives only in memory and is lost when the
runner exits. Two flags make a session's on-disk state durable and
resumable:

```sh
./litebox_runner_linux_userland --unstable --initial-files alpine-rootfs.tar --program-from-tar \
  --export-writable-layer session.tar /bin/sh -c "npm install -g some-tool"

./litebox_runner_linux_userland --unstable --initial-files alpine-rootfs.tar --program-from-tar \
  --resume-from session.tar /bin/sh -c "some-tool --version"
```

`--export-writable-layer <path>` walks every file the guest created or
modified this run and writes it to a tar archive after the program exits.
`--resume-from <path>` seeds the writable layer from a previously exported
archive before the guest program starts. The exported archive is a delta
against the base rootfs, not a full snapshot, so it stays small regardless
of how large `alpine-rootfs.tar` is.

## Security boundary — read this before using LiteBox as an agent sandbox

**This is a syscall-rewriting compatibility/porting layer, not a security
sandbox.** If you're running untrusted or semi-trusted agent-generated code
(e.g. an LLM agent's Node.js/Python workload) and relying on this to contain
it, understand exactly what isolation does and does not exist:

- **No process isolation between guests.** Every guest "process" started by
  a single runner invocation is, under the hood, an ordinary host thread
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
  implement a seccomp-style allow/deny policy of its own. Every syscall the
  shim implements is serviced; there is no mechanism to deny a guest syscall
  on security grounds beyond what the host's own real seccomp/capabilities
  configuration (which this runner does not set up for you) would provide.
- **No privilege drop.** The guest runs at exactly the host process's own
  privilege level. There is no user namespace, seccomp filter, or capability
  drop applied on the guest's behalf.
- **No resource limits enforced by the host.** Only `RLIMIT_NOFILE` and
  `RLIMIT_SIGPENDING` are tracked (as in-process bookkeeping, not
  OS-enforced caps). There is no CPU or memory limit — a guest process can
  consume the full, unbounded resource budget of the host process.
- **The guest filesystem is virtual and does not expose the host
  filesystem** — the rootfs is entirely in-memory/tar-backed, with no
  passthrough to arbitrary host paths. This part *is* a real boundary.

**Practical implication:** treat every guest program run by the same runner
invocation as fully mutually trusted, and treat the runner process itself as
having exactly the privileges of whatever account launched it. Do not run
code you don't trust at that privilege level under the expectation that
LiteBox will contain a deliberately malicious payload — it is built to run
Linux programs correctly under its own syscall shim, not to defend against
one that is actively trying to escape. For real sandboxing on Linux, pair
this runner with the host's own isolation primitives (containers, a
dedicated user, seccomp, namespaces) the same way you would any other
unprivileged process you don't fully trust.

## Known limitations

**PTY-based CPython workloads may crash intermittently (upstream bug, not a
LiteBox defect).** Programs that combine `fork()` with CPython's `pty`
module (e.g. `pty.spawn(...)`, `pty.fork()`) can hit a use-after-free inside
CPython 3.14.7's fork-child cleanup path (`threading._after_fork()`
resizing a dict while musl 1.2.6's `mallocng` allocator is in a
post-`fork()` state), causing a segfault in the guest. This was
root-caused across an extensive investigation (see the project's git
history and `FINDINGS.txt` for the full characterization) to be a genuine
bug in upstream CPython/musl, reproducible independent of LiteBox, with no
newer CPython or musl version available in Alpine to fix it and no known
mitigation (including `PYTHONMALLOC=malloc`). It is tracked as a known,
currently-unfixable-from-LiteBox's-side limitation. All other functionality
in this bundle, including `apk add`, general `fork`+`exec`, and the rest of
this smoke-tested surface, is unaffected.

All files must stay in the same directory.
