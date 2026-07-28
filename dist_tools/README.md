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
run-alpine.cmd                  # interactive /bin/sh (BusyBox ash)
run-alpine.cmd busybox ls /     # run a specific command
```

Or directly:

```
litebox_runner_linux_on_windows_userland.exe --initial-files alpine-rootfs.tar /bin/sh
```

All three files must stay in the same directory.
