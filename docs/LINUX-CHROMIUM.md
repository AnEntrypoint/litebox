# Chromium under litebox on THIS Linux container (2026-10-09)

Active goal, verbatim: **"make it work perfectly in our current linux environment, the goal is
achieved when chromium starts without --no-sandbox and is visible in a webtop container without
modifying the software our runner is running"**. Every fix lands in litebox (shim/platform/runner).
**Never modify chromium or its rootfs.** Chromium flags ARE ours to choose, but `--no-sandbox` is
NEVER passed in the delivered configuration.

## Harness (all in /tmp/lbx, git-ignored, NOT in the repo)

| script | what it does |
|---|---|
| `cmpnat.sh <tag> <secs> dumpdom` | runs **native** chromium (`--no-sandbox`, mandatory here) AND chromium **under litebox** (no `--no-sandbox`) with identical flags, then diffs |
| `long1.sh <tag> <secs> [flags]` | litebox run, `--dump-dom --v=1`, writes `log-<tag>.txt` |
| `mx.sh <tag> <secs> [flags]` | generic one-config litebox run; prints `rc`, `DOM=`, `last_chromium:`, `png_in_writable:` |
| `dbg1.sh <tag> <secs>` | litebox + `LITEBOX_LOG=warn,litebox_shim_linux::syscalls::file=debug` + `LITEBOX_DIAG_MAINTHREAD=1` (log ~62 MB — analyse with python, never `cat`) |
| `norm.py` | normalises both logs (digits→N, hex→0xH, strips `[pid:tid:time:LEVEL:`) and prints a difflib diff |
| `dis.sh <file_off> [before] [after]` | `dd` a window out of `/tmp/lbx/usr/lib/chromium/chromium` + `objdump -D -b binary -m i386:x86-64 --adjust-vma=<off>` (seconds, vs minutes for objdump on the 327 MB binary) |

Rootfs `/tmp/lbx/rootfs-chr.tar`, page `/tmp/lbx/page.html`, runner
`target/release/litebox_runner_linux_userland`.

## The native reference (this host)

`/usr/lib/chromium/chromium`, 327,123,310 B = 0x137f816e.
`--no-sandbox --headless=old --disable-gpu --user-data-dir=/tmp/udd-x --dump-dom file:///tmp/lbx/page.html`
→ **rc=0 in ~0.7 s**, ~601 chromium log lines, DOM printed.
`--screenshot=/tmp/lbx/nat2.png` → rc=0 in 0.81 s, 800x600, (255,212,0) 99.77%.
**Native WITHOUT `--no-sandbox` fails here** — unprivileged user namespaces are disabled in this
container — so **every native control must carry `--no-sandbox`** (fine for a control, never for
the deliverable).

Address math: exec PT_LOAD `p_offset=0x2cda000` / `p_vaddr=0x2cdb000`, so within that segment
`file_off = vaddr - 0x1000`. **The load bias is per-run — measure it, never reuse it** (the rp1
run was 0x2a810000000 → `file_off = vaddr - 0x2A810001000`).

## ESTABLISHED: what actually happens under litebox

- **Multi-process chromium HANGS PERMANENTLY** (proven: identical state at 70 s and 150 s,
  rc=124). It never navigates. Marker counts, litebox vs native: `page.html` 0 vs 2,
  `Navigation` 1 vs 192, `RenderProcessHost` 0 vs 19, `DidFinishLoad` 0 vs 1, `Mojo` 0 vs 12.
- **`--single-process` DOES load the page** (`--dump-dom` prints it). So Blink, the file loader,
  profile creation and DOM storage all work when there is no IPC. **The stall is a cross-process
  handoff** (commit `10891b8f`).
- **The browser MAIN THREAD IS NOT SPINNING.** With `LITEBOX_DIAG_MAINTHREAD=1` it sits in `ppoll`
  on exactly two fds — fd 11 = eventfd, fd 13 = pipe — always `ready_count=0`, timeouts 0 ns…
  3.168 s, forever. It waits for a task/message that never arrives.
  `syscall=clock_gettime num=228` in the histogram is a SAMPLING ARTIFACT, not a spin.
- **The visible loop** is a browser thread (guest tid 17), period ~470–600 ms, forever:
  `base/files/file_util_posix.cc:314] Cannot stat "/tmp/udd-lg/Default/Session Storage/exp-v1": No such file or directory (2)`
  alternating with `".../Default/Local Storage/leveldb/exp-v1"`, interleaved with
  `[diag-proc-sys-open-miss] unregistered path opened: /proc/self/fdinfo/tmp/udd-lg/Default/Session Storage errno=2`,
  `.../fdinfo/proc/72/statm`, `.../fdinfo/proc/72/status`.
  **Native logs ZERO occurrences of `leveldb` or `exp-v1`** (its DBs exist, so nothing is logged).
  `exp-v1` occurs once in the binary, at file offset **0x1e42473**, inside a merged string blob.
- A renderer DOES run under litebox (guest pid 83 logged `script_context.cc:150 Created context`
  WEBUI contexts) and the GPU process initialises
  (`InitializeSandbox() called with multiple threads in process gpu-process`).
- Process timeline: only ONE utility `execve` (`network.mojom.NetworkService`, pid 45) — **no
  `storage.mojom.StorageService`** — plus two zygotes, then exit_groups at 15.86/16.06/17.99/18.27 s.

## Litebox-only chromium errors (not seen natively)

`token_service_table.cc:216] Failed to load tokens (invalid SQL statement)`;
several `dbus/property.cc:94]` / `dbus/bus.cc:405]` shapes (usually `NameHasOwner`);
`/sys/devices/system/cpu/cpuN/cpufreq/cpuinfo_max_freq`;
`/sys/devices/virtual/dmi/id/{sys_vendor,product_name}`;
`/sys/bus/cpu/devices/cpu0/microcode/version`;
`/proc/self/fdinfo/proc/self/maps`.

## Rootfs gap found

`/tmp/lbx/rootfs-chr.tar` has **no** `etc/hosts`, `etc/nsswitch.conf`, `etc/resolv.conf`,
`etc/localtime`, `etc/passwd`, `etc/group` — all present on this host. Guest tid 17 retries
`/etc/nsswitch.conf` + `/etc/hosts` (both ENOENT) about every 1.4 s. Not yet shown to be causal.

## Flag matrix — ALL hang (70 s each, rc=124, DOM=0, no PNG)

`--in-process-gpu`; `--headless=new`; `--single-process --screenshot=/tmp/out.png`;
`--disable-features=Vulkan,StorageServiceOutOfProcess`;
`--user-data-dir=/tmp/org.chromium.udd` (i.e. a dir ON a spilled prefix).

## NEW BUG: the runner SEGFAULTS with `LITEBOX_SHARED_WRITE_PREFIXES` set

`LITEBOX_SHARED_WRITE_PREFIXES=/tmp/udd-s2/ <mx.sh s2 70 --dump-dom>` → **`rc=139`,
`Segmentation fault (core dumped)`, 0 bytes of output.** A host process dying is the most serious
bug class in this repo ("Guest-reachable code returns an errno, never a panic — the host process IS
the whole session"). Suspects: a shared arena `Region` that is absent for a path that is
shared-write but NOT in `SPILLED_PREFIXES`, or an extent computed from an unregistered prefix.

## 2026-10-10 — THE HANG IS CHROMIUM'S LAYER-1 NAMESPACE SANDBOX (proved); layer-2 is NOT causal

- **`--no-sandbox` is the only thing that makes multi-process work** (`bisect.sh b1`: rc=0, DOM=2).
- **`--disable-namespace-sandbox` with NO `--no-sandbox` reproduces the NATIVE failure verbatim**:
  `zygote_host_impl_linux.cc:129] No usable sandbox!` rc=1. So under litebox layer-1 IS the
  namespace sandbox and it is REQUIRED. `--disable-seccomp-filter-sandbox` alone = rc=124, the
  identical hang, so **layer-2 is not the blocker**. Both together = the same rc=1.
- `zygote_host_impl_linux.cc:115-129`: the namespace branch wins on
  `!kDisableNamespaceSandbox && Credentials::CanCreateProcessInNewUserNS()`; otherwise the SUID
  branch (`chrome-sandbox`, not shipped by Debian's chromium package here); otherwise `LOG(FATAL)`.
  **There is no `capget` probe on this path.**
- **`zygote_main_linux.cc:202` — "ZygoteMain: initializing 0 fork delegates", which we DO see under
  `--v=1` — is logged BEFORE `EnterLayerOneSandbox` at :209.** It proves the zygote started, not
  that the sandbox finished. Everything after it is unobserved.
- Layer-1 for 154.0.8037.92 (verified against `refs/tags/154.0.8037.92`; **no mount, no pivot_root,
  no setresuid**): `NamespaceSandbox::LaunchProcess` clones `CLONE_NEWUSER|CLONE_NEWPID|CLONE_NEWNET`
  and the child, async-signal-safe, writes `setgroups`="deny", then `uid_map`, then `gid_map`. The
  zygote then does `unshare(CLONE_NEWUSER)` + `SetGidAndUidMaps` (gid then uid),
  `DropFileSystemAccess`, and **`ChrootToSafeEmptyDir()` = `clone(CLONE_FS|CLONE_VM|CLONE_VFORK|
  CLONE_SETTLS|SIGCHLD)` whose child runs `chroot("/proc/self/fdinfo/"); chdir("/"); _exit(0)` while
  the PARENT is suspended in `waitpid` (`credentials.cc:52-62`)** — the ONE step that can hang with
  no log; every other failure is a `RAW_CHECK`/`CHECK` and crashes.
- Measured litebox vs native (probe51/52/53/54): `unshare(CLONE_NEWUSER)`=0 and euid becomes 65534
  (that is correct Linux); `uid_map`/`gid_map` **WRITE = EBADF(9)** (native outside a userns: EPERM);
  `setgroups` write = OK; `mount` = ENOSYS (chromium does not mount — ignore it); `capget` =
  `(0xffffffff, 0xffffffff, 0)` for an unprivileged guest vs native `(0,0,0)`; `chroot` is allowed at
  euid 65534 and refused at 1000.
- **Python cannot arbitrate `CLONE_VM`** — native segfaults too — so a static C repro of
  `ChrootToSafeEmptyDir` is the arbiter; treat any Python `clone` result as invalid.

## REFUTED — do not re-open without a NEW reproduction

- **chroot leaks across processes** (would have explained the browser's `/proc/self/fdinfo/`
  prefixes). probe54: a child that `unshare(CLONE_NEWUSER)`s AND successfully chroots leaves its
  parent and later siblings able to open real paths. `chroot` is correctly per-process; those
  prefixes are chromium's own chrooted processes.

- **`/proc/self/fdinfo/<fd>` is a litebox gap.** `probe36.py` / `probe37.py`: `/proc/self/fdinfo`
  lists the fd AND returns `pos:` content for a regular file, a `/proc` file, a pipe and a socket,
  under **both** native and litebox (T0–T3 all YES). The `/proc/self/fdinfo/<target-path>` strings
  in the miss-diag therefore come from a path substitution, not a missing feature.
- **Cross-process `MAP_SHARED` memfd visibility, incl. over SCM_RIGHTS.** `probe35.py`: all YES on
  both native and litebox.
- **The browser main thread spins in `base::TimeTicks::Now()`** (commits `678f096e`, `18ce120b`).
  It idle-polls; see above.
- **A monotonic-clock or clock-origin bug** (`6c83bdf9`).
- **The profile dir needs to be on a spilled prefix for cross-process writes.** s1 with
  `--user-data-dir=/tmp/org.chromium.udd` still hangs, and gets LESS far (32 chromium lines).
