# litebox-windows-alpine bundle

Contents:

- `litebox_runner_linux_on_windows_userland.exe` — the LiteBox runner; runs
  unmodified Linux ELF binaries on Windows.
- `alpine-rootfs.tar` — an Alpine Linux rootfs (BusyBox userland), pulled from
  `docker.io/library/alpine:latest` and pre-rewritten with the LiteBox syscall
  rewriter via `litebox-packager --oci-image`.
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

## Known limitation: no interactive shell yet

`fork()`/`vfork()` are not implemented in LiteBox yet (see
[issue #1](https://github.com/AnEntrypoint/litebox/issues/1)), so an
interactive `/bin/sh` session that spawns subprocesses (e.g. typing `ls` at
the prompt) will fail with `can't fork: Function not implemented`. Only
single-applet invocation (no subshell, no subprocess) is supported today —
e.g. `run-alpine.cmd busybox ls /` works because busybox recognizes the
applet name and never forks.

All files must stay in the same directory.
