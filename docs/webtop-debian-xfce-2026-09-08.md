# 2026-09-08: `linuxserver/webtop:debian-xfce` under litebox

XFCE runs from the stock OCI image with no rebuild: Xvfb, the session bus, `xfce4-session`,
nginx and selkies all start, the real selkies dashboard is served, and the browser reaches the
client over a working websocket (`101 Switching Protocols`). The desktop is **not yet visible**,
and the reason is now located precisely rather than described vaguely: **Xvfb dies with a NULL
dereference once `xfce4-session` starts doing real work.**

## Image choice

`debian-xfce` exists (the companion doc's claim that `ubuntu-xfce` is the only XFCE tag is stale
-- the live tag list now carries `debian-xfce`, `fedora-xfce` and `ubuntu-xfce`). Debian is the
right one: `ubuntu-xfce` ships **Rust coreutils (uutils)** as one 11 MB argv[0]-dispatched
multicall binary, so a single wrong `argv[0]` turns `sleep`, `cat` and `ls` into
`coreutils: unknown program`. Debian's `/usr/bin/sleep` is a 43 KB GNU binary, which cannot fail
that way.

The stock tar is 9.0 GB / 119,722 entries and costs the runner ~7 GB of working set (the index
build touches headers across the whole file), which repeatedly got runs OOM-killed on a 16 GB
host. `advisor/probes/make_debian_xfce_min_rootfs.py` trims it to **3.6 GB** -- locales, git-core,
chromium, firmware, docker, the gcc/perl trees, `usr/bin/X11` (a duplicate of `usr/bin`) and
`usr/share/icons` (370 MB across 34,722 entries; entry count matters as much as bytes). It keeps
everything the earlier alpine trim records as learned from live failures: `usr/libexec`, mesa's
`libgallium`/`libLLVM`/`dri`, all of `usr/share/X11` (xkb), fonts, and `/lsiopy`.

## The remaining blocker: Xvfb NULL-derefs

Reproducible: start Xvfb, then `xfce4-session`. Within ~45 s the session logs
`X connection to :1 broken (explicit kill or server shutdown)`, and any X client then reports
`cannot open display`.

    diag-guest-exception: exception=14 kernel_mode=false rip=0x10501686 cr2=0x20 error_code=0x4
    diag-guest-exception: NO mapping overlaps cr2 (genuinely unmapped) cr2=0x20
    rip bytes: 48 8b 43 20   mov rax, [rbx+0x20]
               4c 89 e7      mov rdi, r12
               ff d0         call rax

`rbx` is 0: Xvfb loads a function pointer from a structure at offset `0x20` and calls it, with the
structure pointer NULL. A second capture caught the next stage -- **`rip=0x0`, `cr2=0x0`**, i.e.
Xvfb jumping through a NULL function pointer outright, then `(EE) Caught signal 11 (Segmentation
fault). Server aborting`. This is memory corruption inside Xvfb, not a bad X request.

### What it is NOT (each tested, not assumed)

| hypothesis | test | result |
|---|---|---|
| framebuffer size | 320x240 vs 1024x768 | crashes either way |
| trim removed a tool | `xrandr`/`xset`/`xdpyinfo` present | all present, all work |
| SIGALRM / smart scheduler | `-dumbSched -s 0` | still crashes |
| Composite extension | `-extension COMPOSITE`, `xfwm4 --compositor=off` | still crashes |
| RandR / extension queries | `xrandr`, `xdpyinfo` standalone | both fine, Xvfb survives |
| any X client | `paint_root.py` drawing client | Xvfb survives |
| plain fork/exec churn | 40 foreground + 60 background forks | Xvfb survives |
| two `ET_EXEC` binaries at one link-time base | ELF headers of Xvfb, xfwm4, xfce4-session, nginx | **all ET_DYN (PIE)** |

### What it IS

`xfwm4` run in the FOREGROUND (`timeout 40 xfwm4 --replace`) runs its full 40 s, takes SIGTERM,
and leaves **Xvfb alive**. The same `xfwm4 --replace &` BACKGROUNDED kills Xvfb within 15 s. So
the trigger is not what xfwm4 asks of the X server -- it is loading xfwm4 as a concurrent process
while Xvfb is live.

Every binary involved is PIE, so litebox chooses their load addresses; this is therefore a
PLACEMENT problem in a single shared host address space, matching the pre-existing note in
`allocate_pages` about `labwc`/`xfwm4`/`xfdesktop` SIGSEGV-ing under concurrent multi-process
`mmap(NULL)`/`munmap()` load. `Vmem` placement only avoids a one-time startup snapshot plus its
OWN mappings, so it has no view of another live guest process's address space.

A first guess -- that `CLAIMED_RANGES` never records ordinary `mmap(NULL)` -- is WRONG and worth
recording as such: the OS-picks-the-address path claims unconditionally, with a comment saying
why. The actual hole is narrower. A `Hint`-mode request that SUCCEEDS at the address
`get_unmmaped_area` chose is never claimed (only `Replace` is claimed on that branch), and
`get_unmmaped_area` chose that address using this process's own `Vmem` alone. The pre-checks that
should still catch a live neighbour (`has_committed_page`, `find_foreign_claim`) run under
`ALLOCATE_PAGES_FIXED_ADDR_LOCK`.

### The collision, captured

Running the repro under `LITEBOX_DIAG_MM=1` catches it directly. The second fault of the run is a
read of `cr2=0x11720cd0`, and THREE separate commits in the same run cover that address:

    0x11100000-0x12144000
    0x1170b000-0x1172c000
    0x11718000-0x11739000     <-- starts BEFORE the previous range ends

The last two overlap each other: two distinct allocations were handed the same pages. Their
neighbours in the log march upward in exact `0x21000` steps (`0x117ae000-0x117cf000`,
`0x117cf000-0x117f0000`, `0x117f0000-0x11811000`, ...), which is the signature of sequential heap
growth -- so this is two guest processes growing heaps into one another in the single shared host
address space, and the corruption is what makes Xvfb jump through a NULL pointer.

That is the bug to fix, and it is squarely the address-space-sharing class this runtime is built
around, not a webtop or XFCE problem. The same run also took the HOST runner down with it
(`EXIT=139`), so it is not contained to the guest.

## Networking: the browser path that does work

The in-guest nginx cannot reach the in-guest selkies at `127.0.0.1:8082` -- it returns a real
**502**, confirming the loopback gap `advisor/probes/hostproxy.py` was written for. The host proxy
is the way around it, and two things are required for its upstream to work at all:

1. **`--publish` only binds the port listed FIRST.** `--publish 3000:3000 --publish 8082:8082`
   binds 3000 and silently drops 8082; reversing the order binds both. The field is
   `publish: Vec<String>`, so this is a bug, not a documented limit.
2. **selkies must bind `0.0.0.0`.** `--publish` forwards to the guest's interface address, so a
   loopback-only bind has nothing listening for the forwarded connection.

With both, `curl` gets `101 Switching Protocols` from the published port and the dashboard (served
by `hostproxy.py` from `advisor/probes/dashboard/`, extracted from the image) loads and connects.

## Fixed this session, with measured before/after

| fix | effect |
|---|---|
| `AT_EXECFN` + `/proc/self/auxv` + `/proc/self/maps` | every uutils binary aborted/segfaulted -> runs |
| claim registry released on unmap, at the `remove_mapping` choke point, and validated against Windows | `libc.so.6: failed to map segment` recurring every run -> **0** |
| `dbus-daemon --nofork` | omitting `--fork` is not enough; debian's daemonizes by default, losing its listening socket across litebox's fork |
| syscall timeline: aimable, and actually prints | it accepted its own env var and emitted nothing, because the log macros are gated on `LITEBOX_LOG` |

## Open, reproduced, not fixed

- **Xvfb NULL deref** (above) -- the blocker.
- **`open("/dev/null")` returns ENOENT transiently**, never reaching the devices backend (zero
  `[diag-dev-open-miss]`), so the miss is in the layered resolver. It matters because dash opens
  `/dev/null` for every background job's stdin, so a single miss means the job never starts.
- **vfork argv crossing**: two concurrent children, one gets the other's `argv[0]`. Deterministic
  repro: `sh -c 'nginx -v > n.log 2>&1 & sleep 4 > s.log 2>&1'` -- `s.log` contains nginx's argv.
- **Eager fork populate**: `Vmem::duplicate` passes `populate_pages_immediately = true`, so Xvfb
  forking `xkbcomp` after loading libGL->gallium->libLLVM materialises its whole address space.
  6743 MB at 1024x768, 5294 MB at 320x240, against a 193 MB trivial-guest baseline -- it scales
  with the process image, not the screen. Making it lazy cut it to 5202 MB but killed Xvfb, so it
  was reverted; the real answer is copy-on-write fork.
- **`/proc/[pid]/maps` and `/proc/[pid]/cmdline`** exist only for `self`, which is why Xvfb's own
  crash backtrace prints `(?+0x0)` with no symbols.
