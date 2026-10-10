# Chromium WITHOUT `--no-sandbox` under litebox on the Linux host — 2026-10-10

Binary: Debian Chromium 154.0.8037.92 from `/tmp/lbx/rootfs-chr.tar`.
Runner: `target/release/litebox_runner_linux_userland`. Harness: `/tmp/lbx/probe61.sh`
(60 s, `--headless=new --dump-dom --virtual-time-budget=8000 file:///page.html`,
`LITEBOX_LOG=warn,litebox_diag::process_timeline=debug`, plus a 5 s `ps -eLo` sampler of the
host process tree). Logs: `/tmp/lbx/log-{nf,ng,nv,ns,nz,n1,n0}.txt`, `/tmp/lbx/ps-nf.txt`.

## 1. The hang is localised: the RENDERERS EXIT CLEANLY ~0.5 s AFTER SECCOMP-BPF

Established this session, by `--v=1` on the guest's own stderr (`log-nv.txt`):

- Three renderer processes are forked from the zygote and register as guest pids 72, 78, 82 at
  t≈14.2 s (`DIAG proc-table register new pid=72/78/82`).
- Each logs, as `pid:tid = 72:1` (one thread ⇒ a native host `fork()` child):
  `sandbox/policy/linux/sandbox_linux.cc:78] Activated seccomp-bpf sandbox for process type: renderer.`
- Each then calls `exit_group` with **status 0** at t≈14.67 / 14.93 / 15.23 s. No signal, no
  `Check failed`, no `Exception(...)`, nothing on stderr.
- The browser is left with no renderer, waits forever, and its main thread burns ~97 % CPU
  (`ps`: `1945517 ... RNl 97.4`, tid == pid, `wchan=-`; every other thread sits in
  `futex_do_wait`/`do_wait`). `dump_dom_lines=0`.

Control: the SAME configuration plus `--no-sandbox` (`log-ns.txt`) does **not** exit — its
renderer reaches `extensions/renderer/script_context.cc:150] Created context:` and renders.

So the whole question is: **what makes a renderer return 0 in the window right after
`InitializeSandbox()`?** The zygote's layer-1 namespace sandbox is NOT the problem — the
`CLONE_FS|CLONE_VM|CLONE_VFORK|CLONE_SETTLS` chroot child completes with `exit_group status=0`
(`log-x1.txt`:31-34) and the zygote survives to run layer 2.

## 2. `--no-zygote` cannot be used to isolate it

`log-nz.txt`, rc=1 at t=1.3 s:
`ERROR:content/app/content_main_runner_impl.cc:360] Zygote cannot be disabled if sandbox is
enabled. Use --no-zygote together with --no-sandbox`.
(Useful side effect: this is independent proof that the sandbox IS enabled in these arms.)

## 3. Which fork path is taken

`litebox_platform_linux_userland/src/lib.rs:3447` `has_native_fork() -> true` unconditionally.
`process.rs:5750-5756`: every `clone()` **without** `CLONE_FS`/`CLONE_FILES` goes through
`try_cross_process_fork` → `try_native_cross_process_fork` → a real host `fork()`
(`lib.rs:3474`). `CLONE_FS|CLONE_FILES` clones stay in-process (that is the zygote's chroot
child, and the log says so: "clone: not cross-process -- falling back to same-process
thread-based fork (vforked=shared-pm…)").

The arena is `MAP_SHARED|MAP_ANONYMOUS` (`shared_heap.rs:228-233`, `ARENA_BASE =
0x6000_0000_0000`) and the runner installs `SharedHeap` as `#[global_allocator]`
(`litebox_runner_linux_userland/src/main.rs:8`), so `native_fork_shares_kernel_state()`
(`lib.rs:3517`) is true and the shared tables really are shared across the fork. A private-COW
kernel state is therefore NOT the explanation.

## 4. Native control is unavailable (unchanged, re-confirmed)

This container is `Seccomp: 2`, `CapEff: 0`: every native `CLONE_NEW*`/`unshare` answers EPERM
by seccomp, so steps that need a namespace have no valid native control. Only the bare chromium
binary was extracted (no `icudtl.dat`/`.pak`), so a native chromium run dies `rc=133` on ICU.
The C matrix is `/tmp/lbx/layer1{,b,c}.c` with `native-layer1v2.txt`, `log-layer1{v2,v3,v4}.txt`.

## 5. Divergences the C reproducer measured (litebox vs this host, both unprivileged)

| step | native | litebox |
|---|---|---|
| `clone(FS\|VM\|VFORK\|SETTLS\|SIGCHLD)` verbatim (ChrootToSafeEmptyDir) | ok, waitpid exit 0, `chroot`=EPERM | ok, waitpid exit 0, **no hang**, `chroot`=**EINVAL** |
| `chroot` with CAP_SYS_CHROOT (fresh userns, euid 0) | n/a (0 on real Linux) | **EINVAL** — path resolution collapses |
| `chroot` of a nonexistent path | ENOENT | **EINVAL** |
| `clone(...\|CLONE_NEWNET)` | EPERM (seccomp) | EPERM |
| `clone(NEWUSER)` / `(NEWPID)` | EPERM | ok (child `getpid()==1`) |
| `clone(VM)` without VFORK | ok | **EINVAL** |
| `unshare(CLONE_NEWUSER)` + maps | EPERM | 0, euid→65534, maps write ok, `uid_map` reads `0 1000 1` |

Note the contradiction to reconcile: the C probe sees `chroot`=EINVAL where chromium's child
exits 0, which means chromium's `RAW_CHECK(chroot(...)==0)` passed. Either `/proc/self/fdinfo/`
resolves for chromium and not for the probe, or the probe's path differed. **Unresolved.**

## 6. Fidelity fixes landed 2026-10-10 (each behind a `LITEBOX_*` kill switch, host env)

- **capabilities** — `Credentials::holds_full_capabilities()` (`process.rs:1449`) = owner of a
  user namespace or euid 0; `reported_capability_sets()` (:1460) reports the tracked sets only
  then, else `(0,0,0)`; `capget` gains ESRCH for a foreign pid (`misc.rs:141-146`); `capset`
  gains EPERM for a real change when unprivileged (:202-208); `PR_CAPBSET_READ` follows the same
  rule instead of always answering 1 (:1818). Switch: `LITEBOX_CAPS_FIDELITY_OFF`.
  Measured: uid 1000 `capget` was already `(0,0,0)` before the change; `PR_CAPBSET_READ` went
  `1,1,1` → `0,0,0` for caps 0/7/21. Caveat: native here reads `CAP_SYS_ADMIN=0` but
  `CAP_CHOWN`/`CAP_SETUID=1` **even as root** — the real bounding set is container-specific.
- **`/proc/<pid>/{uid_map,gid_map,setgroups}`** — `ProcPidEntry` variants (`procfs.rs:670`),
  served only for the caller's own pid, `EPERM` (not EBADF) for a refused write
  (`WriteError::PermissionDenied`, `fs/errors.rs:66`), and `set_res_uid`/`set_res_gid` now store
  the KERNEL id, not the inner one (`process.rs:1627-1639`, `:1656-1665`) — before,
  `setresuid(0,0,0)` inside a userns left `getuid()` reporting 65534. Switch:
  `LITEBOX_PROC_USERNS_MAPS_OFF`.
- **Mojo IPC** — `MSG_PEEK`/`MSG_WAITALL` admitted on `AF_UNIX` (`net.rs:2663-2677`,
  `unix.rs:3209-3286`); `FIONREAD` on unix returns queued bytes (`file.rs:8772-8788`);
  `SO_RCVLOWAT` get/set with Linux's default of 1 (`net.rs:355-365`);
  `peer_gone()` gated on a *settled* verdict and never on a non-empty ring (`unix.rs:872-911`).
  Switch: `LITEBOX_UNIX_RECV_FIDELITY_OFF`, plus `LITEBOX_UNIX_PEER_GONE_STRICT=1` for the old
  irreversible-shutdown behaviour.

## 7. Measurement warning

Do not compare wall-clock timings across runs while other work is running: the same
configuration reached guest pid 10 in 1.3 s on an idle host and 20.5 s at load average 7.68.
Check `uptime` and `free -m` before trusting a timing, and keep to ONE runner at a time.

## 8. The exit is `TerminateSelfOnDisconnect` — a 0-BYTE `recvmsg` on the Mojo bootstrap

Chromium source read at tag `154.0.8037.92` (per-file pulls into `/tmp/cr`; no local checkout). This
closes §1's "what returns 0": it is not a crash, not a watchdog, not a CHECK.

**The path** (renderer, single-threaded until it creates `Chrome_ChildIOThread`):

- `content/child/child_thread_impl.cc:727-732` installs `TerminateSelfOnDisconnect` as the
  `child_process_host_` disconnect handler → `:167-214` → `:213`
  `base::Process::TerminateCurrentProcessImmediately(0)` → `_exit(0)`. **Logs nothing** — which is
  why the log is silent after `sandbox_linux.cc:78`.
- It fires from `mojo/core/channel_posix.cc:309-343`: `read_result = SocketRecvmsg(...)`; **`== 0`**
  → `read_error` → `OnError(Error::kDisconnected)`.

**Excluded**, each by a positive observation:

| candidate | how excluded |
|---|---|
| 15 s `EnsureConnected` watchdog (`child_thread_impl.cc:930-934`) | it is `VLOG(0)` and prints; measured Δ is **0.38 / 0.61 / 1.00 s**, never 15 s. (Seen once in `log-cs1.txt`, dated Oct 8, not Oct 10.) |
| `!invitation.is_valid()` (`:633-636`) | prints `LOG(ERROR) "Child process could not find its Mojo invitation"`: **zero occurrences in any log**; and in this ipcz build `core_ipcz.cc:818-829` returns `MOJO_RESULT_OK` unconditionally, so the branch is dead. |
| `OnChannelError`/`OnProcessFinalRelease` → `quit_closure_` | would run the `RunLoop` teardown and emit something at `--v=1`. Nothing is emitted. |

**Order: seccomp FIRST, Mojo read SECOND, and the read is asynchronous.**
`renderer_main.cc:249` `EnableSandbox()` emits `sandbox_linux.cc:78`; `:279` constructs
`RenderThreadImpl` → `child_thread_impl.cc:632` `InitializeMojoIPCChannel()`. But
`ipcz_driver/invitation.cc:348+` `Invitation::Accept` **never touches the socket** (it only calls
`ConnectNode`), so the first real `recvmsg` happens later on `Chrome_ChildIOThread` via
`ChannelPosix::OnFdReadable`. That is precisely the measured 0.4–1.0 s window, and it is why pid 78's
`comm` at exit decodes to **`Chrome_ChildIOThread`** (`DIAG_TIMELINE exit_group pid=78 comm=[…]`).

**Corollary — the post-seccomp sanity check PASSED.** `renderer_main_platform_delegate_linux.cc:64-65`
does `CHECK_EQ(fchmod(-1, 07777), -1); CHECK_EQ(errno, EPERM);`. There is no `Check failed` anywhere,
so litebox's BPF verdict really did return EPERM: **litebox enforces chromium's installed seccomp
filter** (evaluated in `litebox_shim_linux/src/lib.rs:2784`, on the raw syscall number).

**A 0-byte read has three distinct litebox causes**, indistinguishable in the logs so far:
1. `peer_gone()` → 0, made irreversible by `read_ring.shutdown()` (`unix.rs:872-911`).
2. `MSG_CTRUNC` with dropped fds — `SocketRecvmsg` does `return missing_fds ? 0 : result`.
3. A ring that reports empty because the sender's `sendmsg` never landed (cross-process ring is
   `SHARED_UNIX_CONN_BUF = 2048` bytes/direction, `unix.rs:3841`, vs 64 KiB in-process; a joined fd
   spec over `RING_FD_SPEC_BYTES = 640` makes the whole `sendmsg` return `EOPNOTSUPP`).

**Not implicated, despite being in the same area:** the `MSG_PEEK`/`MSG_WAITALL`/`FIONREAD`/
`SO_RCVLOWAT` fidelity work. `SocketRecvmsg` is just `HANDLE_EINTR(recvmsg(fd, &msg, block ? 0 :
MSG_DONTWAIT))`. Those switches are a red herring *for this failure*.

**The browser's 97 % spin is a SEPARATE defect, not a reaction to the death.**
`render_process_host_impl.cc:4346-4349` `OnChannelError` → `GetChildTerminationInfo(true)` →
`ProcessDied` → `:5689-5708` `reset()`/`is_dead_ = true`/`ResetIPC()`/`RenderProcessExited` — purely
event-driven, no retry, no poll, no respawn. The spin is the same family as the process-local
rebuilt eventfd (`file.rs:10739-10748`, `E|` → `sys_eventfd2`) and the 15 ms Unix re-poll
(`epoll.rs:423-440`).

**Also measured, unexplained** (both in the window, t≈14.29-14.32):
`unsupported ioctl cmd: 2147808524` = `_IOR(0xF5, 12, 4)` ×2, and `cmd: 35611` = `_IO(0x8B, 27)`.

**Corrected pairing** for §1: the three exits are pid **78 → 14.668 s (Δ 0.378)**,
**82 → 14.935 s (Δ 0.609)**, **72 → 15.230 s (Δ 0.997)**, not 72/78/82 in order.
