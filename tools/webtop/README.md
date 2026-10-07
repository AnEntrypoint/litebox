# Webtop harness (host side)

Scripts used to boot the `linuxserver/webtop:debian-xfce` rootfs under `litebox_runner_linux_userland` and observe it. They
assume a scratch directory `/home/user/wt` holding the extracted rootfs (`rootfs/`) and a tar of it (`rootfs.tar`); adjust the
paths at the top of each script.

- `runwt.sh WAIT` -- full s6 boot (`--pid1`, TUN networking `10.0.0.1` host / `10.0.0.2` guest), diagnostics on
  (`LITEBOX_DIAG_FAULT`, `LITEBOX_DIAG_BIGALLOC`, `LITEBOX_PRINT_EXE_BASE`); `TL=comm,...` adds the syscall timeline.
  `netcfg.py` configures the host side of `tun0`.
- `t.sh 'cmds'` / `t2.sh` -- quick guest command without / with the TUN device.
- `custom-services.d/` -- appended to `rootfs.tar` (`tar rf rootfs.tar -C <dir> custom-services.d/<name>`): `zz-ps` serves `/tmp/pub`
  over HTTP (:8000) with a raw framebuffer (`x.xwd`, convert with `xwd2png.py`), `zz-apps` launches a list of desktop apps one
  at a time and records alive/exited plus a screenshot each, `zz-shot` stops the boot after `$WAIT` seconds.
- `shot.mjs` / `act.mjs` -- drive a headless Chromium (`--remote-debugging-port=9333`) that views the Selkies UI at
  `http://10.0.0.2:3000` (screenshot, click, type). Never add a browser as a dependency of the repo; these only talk CDP.
- `sym.py`, `stall.py` -- symbolize a panic backtrace; inspect a stalled runner (futex word a thread sleeps on).

The dev container's memory cgroup is ~14 GB: `cat /sys/fs/cgroup/memory/process_api/*/claude-code-bash/memory.usage_in_bytes`.
