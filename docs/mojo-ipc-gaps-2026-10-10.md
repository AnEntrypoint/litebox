# Mojo / IPC fidelity gaps under litebox (RESEARCH ONLY — no source changed)

Repo `/config/workspace/litebox`, branch `main`. All `file:line` are in this tree.
Evidence logs: `/tmp/lbx/log-z0.txt` (60 s, rc=124, `dump_dom_lines=0`).
Context: chromium multi-process never loads a page; `chromium --single-process` DOES load it.
So the discriminator is **everything that changes when the channel leaves the process**.

---

## 1. Feature → status → file:line

| Feature | Status | file:line | Blunt note |
|---|---|---|---|
| `socketpair(AF_UNIX, SOCK_STREAM)` | SUPPORTED | `net.rs:1431 sys_socketpair`, `:1463 do_socketpair`, `unix.rs:3182 new_connected_pair` | Only AF_UNIX. INET/INET6/NETLINK → `EOPNOTSUPP` (`net.rs:1510`). |
| `sendmsg` | PARTIAL | `net.rs:2279 do_sendmsg`, `unix.rs:3022 UnixSocket::sendmsg` | Flags accepted: `DONTWAIT|NOSIGNAL` only (`unix.rs:3031`). INET rejects ancillary data with EINVAL (`net.rs:2325`). |
| `recvmsg` | PARTIAL | `net.rs:2609 sys_recvmsg`, `:2644 do_recvmsg`, `unix.rs:3118` | **Flag gate at `net.rs:2628-2633` accepts only `DONTWAIT|TRUNC|CMSG_CLOEXEC`** — `MSG_PEEK` and `MSG_WAITALL` → EINVAL (see §2). |
| `sendmsg`/`recvmsg` + `SCM_RIGHTS`, **same process** | SUPPORTED | `net.rs:2156 resolve_scm_rights_fds` (real `dt.duplicate(fd)`) | Real descriptor handover. |
| `sendmsg`/`recvmsg` + `SCM_RIGHTS`, **cross-process** | PARTIAL / broken-by-design | `unix.rs:976 SharedView::send`, `file.rs:10560 scm_carry_spec`, `file.rs:10636 rebuild_carried_fd`, `unix.rs:3920 post_fds`, `:3946 take_fds_in` | The fd is **not** handed over: it is a *text spec* the receiver rebuilds. Uncarryable kinds: pipes, epoll fds, pty master, timerfd, signalfd (`file.rs:10474-10482`) — and **one** such fd fails the whole `sendmsg` with `EOPNOTSUPP`. |
| `memfd_create` | SUPPORTED | `file.rs:8166 sys_memfd_create` | Created as `/.memfd:{id}`, then unlinked. |
| `mmap(MAP_SHARED)` of a memfd, cross-process | PARTIAL | `mm.rs:672/694 MemfdEntry.name`, `mm.rs:1260 try_memfd_mmap`, `file.rs:2995 resize_memfd_shared_backing`, `litebox_platform_linux_userland/src/lib.rs:3064 create_named_shared_memory`, `:3085-3092` (128-slot named pool), `:3095 shared_memory_token_for` | The shared object only comes into existence on `ftruncate`. Named pool is **128 slots**; overflow → anonymous + `lbxpool:{handle:x}` token. No `/dev/shm` fallback (seccomp allows `open` only `O_RDONLY`). |
| `eventfd` | SUPPORTED in-process / **MISSING cross-process** | `file.rs:7931 sys_eventfd2`; carry spec `file.rs:10571-10572` (`E|<count>|<flags>`); rebuild `file.rs:10739-10748` | The receiver calls `sys_eventfd2(count, flags)` — a **brand-new, process-local** eventfd seeded with the sender's counter. A later `write()` on the sender is invisible to the receiver's `poll`/`epoll`/`ppoll`. There is no shared eventfd anywhere (`SharedEventfd`: 0 hits). |
| `pipe`/`pipe2` | SUPPORTED | `file.rs:7824 sys_pipe2`; write arm `file.rs:4730`, read arm | Not carryable across processes — refused by `scm_carry_spec`. |
| `fork`/`clone` with `CLONE_*` | PARTIAL | `process.rs:5296 do_clone`; supported set `:5346-5372`; `CLONE_NEWNS/NEWNET/NEWIPC/NEWUTS/NEWCGROUP` → **EPERM** `:5388-5398`; anything outside the set (`CLONE_PIDFD`, `CLONE_IO`) → **EINVAL** `:5448-5454` | `CLONE_NEWPID` works (zygote fix, `2f6ff32240`). |
| `execve` | SUPPORTED | `process.rs:8719 sys_execve` | argv/envp healed against fork relocations (`:8755-8770`). |
| `prctl` | PARTIAL | `process.rs:1745 sys_prctl`, unknown → EINVAL `:1835` | `PR_SET_SECCOMP`/`SET_NO_NEW_PRIVS`/`SET_DUMPABLE`/names covered. |
| `seccomp` | PARTIAL | `seccomp.rs:689 sys_seccomp`; `SECCOMP_SET_MODE_FILTER` reads `args` **before** the privilege check → EFAULT on `NULL` `:716-723`; `SECCOMP_GET_NOTIF_SIZES` deliberately absent `:729` | The EFAULT is what makes chromium believe seccomp-BPF exists (`a3c3bb17`). |
| `/proc/<pid>/*` | PARTIAL | `litebox/src/fs/procfs.rs:651-660 ProcPidEntry::ALL` = stat,status,cmdline,comm,oom_score_adj,oom_adj,environ,statm; dirs `task`/`fd`/`fdinfo`/`ns` `:752-768`, `:944`, `:1503`, `:1678`, `:1706` | **No per-pid `maps`/`smaps`/`limits`/`io`/`cgroup`.** `log-z0.txt` still shows `/proc/{69,78,79,98}/{statm,status}` and `/proc/98/task/100/status` → `errno=2`, i.e. the entries exist but the pid/task lookup misses. |
| `flock` | SUPPORTED | `file.rs:7512 sys_flock`, `do_flock :7552` | Regular files only; socket/pipe/eventfd/epoll/unix → EINVAL `:7536-7547`. Cross-process via `SharedFlockTable`; degrades to per-process when the fd has no path `:7524-7528`. |
| `fcntl(F_GETLK/F_SETLK/F_SETLKW)` | SUPPORTED | `file.rs:7255-7289` (SETLK/SETLKW), `:7230-7238` (GETLK), comment `:7240-7242` | Regular files only; EINVAL on socket/pipe. Cross-process `SharedRecordLockTable` keyed `(dev,path)` (`67b198f`). |
| `epoll` | SUPPORTED, with a band-aid | `epoll.rs:441-467 has_unready_stdin_or_armed_timerfd_interest`, `:473-579 repoll_...`, `:581 epoll_ctl`, `STDIN_REPOLL_INTERVAL = 15ms :1057` | Doc at `:423-440`: **"there is no real cross-process wake in this codebase at all"**; Unix interests are re-polled every 15 ms instead. |
| `ppoll`/`poll` | SUPPORTED, same band-aid | `file.rs:9675 sys_ppoll`, `epoll.rs:1189-1246 PollSet::wait`, `:1219-1239 has_unwakeable_fd`, `:1094 scan_once` | Unix/Socket/Pty interests force the 15 ms bounded loop. Note `scan_once` registers an observer only for entries scanned *before* anything is ready (`:1107`). |
| `writev`/`readv` on AF_UNIX | SUPPORTED | `file.rs:6035 sys_writev`, `:5861 sys_readv`, `:5985 write_to_iovec`, `:5938 read_from_iovec`, unix arm `:4753-4764` | `IOV_MAX = 1024` (`:5916`), chunked at `PAGE_SIZE`. Short write → `break` and report the partial count (matches Linux on a stream). |

---

## 2. Mojo `ChannelPosix`'s read path: `recvmsg` → 0 bytes

Path: `net.rs:2609 sys_recvmsg` → `:2644 do_recvmsg` (one heap buffer of `total_iov_capacity`, one contiguous read, scatter back) → `unix.rs:3118 UnixSocket::recvmsg` → `stream.recvfrom` → `unix.rs:1545 try_recvfrom` → `SharedView::recv :1063` / `SharedByteRing::try_read_with_fds :4080`.

| Case | What litebox returns | Linux | Verdict |
|---|---|---|---|
| (a) no data, peer alive | `TryAgain` (`unix.rs:1073`) → `EAGAIN` if `MSG_DONTWAIT`/`O_NONBLOCK`, else blocks in `wait_on_events_polling` (`unix.rs:3725`) | same | **correct** |
| (b) peer closed (`shutdown_write`) | read ring shut → `ESHUTDOWN` → mapped to `Ok((0, Vec::new()))` at `unix.rs:3164` | `0` | **correct** |
| (c) "peer gone" (no live holder) | `peer_gone()` `unix.rs:873-899` → `ESHUTDOWN` → `0`, **and `read_ring.shutdown()` at `:895` makes it permanent** | never happens | **WRONG — spurious, irreversible EOF** |

Modifiers:

- `MSG_PEEK` — **MISSING at the syscall layer.** `net.rs:2628-2633` rejects it with EINVAL, even though `unix.rs:3127-3130` *implements* it and `recvfrom`'s own path accepts it (`unix.rs:3131`). Asymmetry, not a capability limit.
- `MSG_WAITALL` — **MISSING**, same gate (`net.rs:2628`), not implemented anywhere.
- `MSG_DONTWAIT` / `O_NONBLOCK` — SUPPORTED (`unix.rs:3135-3136`).
- `SO_RCVLOWAT` — **MISSING**. Not in `setsockopt` (`unix.rs:3224`) nor `getsockopt` (`:3302`); `LOWAT` has zero hits in `litebox_common_linux`.
- `FIONREAD` — **MISSING for unix**: `file.rs:8772` returns `EINVAL` for the unix arm (pipes and INET are answered).
- `sys_recvfrom` caps any read at `MAX_LEN = 4096` (`net.rs:2423`, `:2428`) — not Mojo's path but the same family.

Any of `MSG_PEEK`, `MSG_WAITALL`, `FIONREAD`, `SO_RCVLOWAT` being wrong silently loses Mojo messages: a reader that sizes its buffer by `FIONREAD` gets EINVAL, and one that peeks to find a message boundary gets EINVAL and typically drops the channel.

---

## 3. Data + `SCM_RIGHTS` in one `sendmsg` — and is the fd delivered on the right `recvmsg`?

**Yes on the anchoring, no on the identity.**

- One `sendmsg` can carry both bytes and fds: `net.rs:2156 resolve_scm_rights_fds` attaches per-fd specs to the message; `unix.rs:976 SharedView::send` posts the joined spec against the **first byte of the message** (`RingCursor::post_fds :3920`, anchor = `cursor.write_pos` before the bytes are written, `unix.rs:3994-4006`).
- The receiving side delivers the fds on the `recvmsg` that consumes that first byte (`take_fds_in :3946`, called with the byte range just read, `unix.rs:4090-4091`), and a read stops short of the next fd-bearing message (`readable_before_next_fd_mail :3936`, used by `try_read_with_fds :4083` and `try_peek :4062`). **Correct `recvmsg`.**
- No silent fd loss: if there is no free mail slot the write returns 0/false → `EAGAIN` (`unix.rs:3994-4000`, `:4044-4048`); `writable()` also gates on a free slot (`:4012-4018`).
- Limits: `RING_FD_MAIL_ENTRIES = 2` (`unix.rs:3893`) — only **two** outstanding fd-bearing messages; joined spec ≤ `RING_FD_SPEC_BYTES = 640` (`:3894`), separator `\u{1e}` (`:3897`) — **over that, the whole `sendmsg` returns `EOPNOTSUPP`**; a data-less send drops the fds entirely (`:1035-1040`).
- **Cross-process the fd is rebuilt, not transferred** (`file.rs:10636 rebuild_carried_fd`):
  `F|` reopen by path (dirs: `mkdir_chain` on ENOENT/EACCES, `:10678-10698`), `S|` reopen shm by name/token, `T|` byte snapshot, `U|` adopt the shared unix slot, `N|` re-adopt the INET fd, **`E|` → `sys_eventfd2(count, flags)` (`:10739-10748`)**.
  So the fd arrives on the right `recvmsg` but is **not the same object**. For an eventfd that is fatal (§5 #1); for a pipe it is a hard refusal of the whole message.

---

## 4. `writev` / `readv` on AF_UNIX

SUPPORTED. `file.rs:6035 sys_writev` → `write_to_iovec :5985` → `sys_write :4617` → unix arm `file.rs:4753-4764` (`file.sendto(...)`); `file.rs:5861 sys_readv` → `read_from_iovec :5938` → `sys_read :4292`. `IOV_MAX = 1024` (`:5916`); each iovec is served in ≤ `PAGE_SIZE` chunks. Partial writes: `write_to_iovec` breaks on a short write and reports the partial byte count (what Linux does on a stream); `read_from_iovec` breaks on a short read.

Caveat: a cross-process shared ring holds only `SHARED_UNIX_CONN_BUF = 2048` bytes per direction (`unix.rs:3841`), so a large `writev` completes in ≤2048-byte steps, and a single message larger than the ring takes the partial path `try_write_with_fds` (`unix.rs:1041-1049`).

---

## 5. Ranked top 3 — what stops the browser just BEFORE its first window

(`profile_manager.cc:1400 AddKeepAlive(Default, kBrowserWindow)` never appears in `log-z0.txt`; native reaches it.)

### #1 — Everything Mojo needs changes identity when it crosses a process: 2 KiB ring, spec-rebuilt fds, and an eventfd that is a *different* eventfd

**Why.** In-process the channel is a 64 KiB `Channel` with real descriptor duplication (`unix.rs:1201 UNIX_BUF_SIZE`; `net.rs:2156 dt.duplicate`). Cross-process it collapses to `SHARED_UNIX_CONN_BUF = 2048` bytes per direction (`unix.rs:3841`), all-or-nothing writes ≤2048, ≤2 outstanding fd-bearing messages, and fds that are *rebuilt from text*. An `eventfd` handed to a child is a brand-new eventfd seeded with the old counter (`file.rs:10739-10748`) — **the child's wake-up never reaches the parent and vice versa**. Chromium's Mojo invitation, `Broker` and `NodeController` handshake move tens of KiB plus several handles in the first messages, and Mojo's channel wake-up is exactly an eventfd. If either the handshake message exceeds the spec/ring limits (→ `EOPNOTSUPP`/`EAGAIN`) or the carried eventfd is a dead twin, the browser sits on a channel that will never signal. This is precisely the axis on which `--single-process` (64 KiB, real fds) succeeds and multi-process fails.

**PROBE (falsifiable, plain guest binary — no chromium, no runner).**
Parent: `efd = eventfd(0, EFD_NONBLOCK)`; `socketpair(AF_UNIX, SOCK_STREAM)`; `sendmsg(child_sock, {1 byte payload, SCM_RIGHTS:[efd]})`; child `recvmsg` → get byte + fd; child `poll(fd, POLLIN, 5000)`; parent sleeps 500 ms then `write(efd, 1)`.
- PASS (hypothesis wrong): child `poll` returns 1 at ~500 ms.
- FAIL (confirmed): child `poll` times out (`ret 0`).

Second half, same binary: send **three** messages each carrying one fd back-to-back before the child reads. `RING_FD_MAIL_ENTRIES = 2` predicts the third returns `EOPNOTSUPP`/`EAGAIN` or loses its handle.
Log: `LITEBOX_LOG=litebox_diag::unix_conn_teardown=debug`; also grep for `SCM_RIGHTS: this fd cannot cross a process boundary` (`net.rs:2156`'s warn).

### #2 — `recvmsg` rejects `MSG_PEEK` and `MSG_WAITALL` with EINVAL; `FIONREAD` and `SO_RCVLOWAT` don't exist

**Why.** `net.rs:2628-2633` admits only `DONTWAIT|TRUNC|CMSG_CLOEXEC`. `MSG_PEEK` (0x2) and `MSG_WAITALL` (0x100) → EINVAL, although the socket layer below implements PEEK (`unix.rs:3127-3130`). `FIONREAD` on a unix socket → EINVAL (`file.rs:8772`). No `SO_RCVLOWAT`. This is a *hard* errno on a path Mojo/IPC helpers use to size or probe a read; a reader that cannot size reads either fails the channel or reads blind. It is also one of the few places where litebox is strictly less capable than the layer it wraps.

**PROBE (guest binary).** `socketpair`; write 8 bytes to one end; on the other:
`recvmsg(MSG_PEEK)` → Linux 8, expect EINVAL(22) · `recvmsg(MSG_WAITALL)` → EINVAL · `ioctl(FIONREAD,&n)` → Linux 8, expect EINVAL(22) · `setsockopt(SOL_SOCKET, SO_RCVLOWAT)` → Linux 0, expect ENOPROTOOPT(92) · `recv(fd,buf,1,MSG_PEEK)` → should **succeed** (unix.rs:3131 accepts PEEK). Any of the first four returning the Linux answer falsifies the claim; the last one succeeding while `recvmsg(MSG_PEEK)` fails *is* the finding.

### #3 — A 0-byte read is also produced by a liveness heuristic, and it is permanent

**Why.** `unix.rs:1063-1075`: on an empty ring, `peer_gone()` is consulted; if the peer side has no live holder host (`side_gone`, `:4313-4347`) the code shuts the read ring (`:895`) and every later read returns 0. Mojo's `ChannelPosix` reads 0 and declares the channel closed, once, with no reconnect. On this Linux host the inputs to that test are degenerate: `Platform::is_process_alive` is **not** overridden by `litebox_platform_linux_userland` (only `mock.rs:328` and `litebox_platform_windows_userland/src/lib.rs:15456` override it; the trait default is `true`, `litebox/src/platform/mod.rs:2008`), and `current_host_pid()` reads 0 for every litebox process here, so `side_gone`'s `host == me || is_process_alive(host)` (`:4325`) either never fires or fires for the wrong reason — while `mark_side_gone` (`:4361`) can mark a side "held and already gone" at promotion time, before the peer has ever held it. A spurious EOF here is indistinguishable from a real close and is irreversible.

**PROBE (diag-only — no code, no chromium).** Run any two-process guest that passes one socketpair end across `fork()` and reads it; set `LITEBOX_LOG=litebox_diag::unix_conn_teardown=debug` and look for
`DIAG unix shared conn: peer_gone because the peer side has no live holder` (`unix.rs:886-894`) and
`side_gone() declared a holder host dead on a lazy read-time check` (`unix.rs:4335-4342`).
- FAIL (confirmed): the line appears while both processes are provably alive — cross-check the named `dead_host_pid` against a `/proc/<pid>/stat` the guest itself reads in the same second.

Chromium-side corroboration (already in hand, needs one confirming run): `log-z0.txt` carries exactly one Mojo-level rejection — `interface_endpoint_client.cc:748] Message 2 rejected by interface blink.mojom.Widget` — absent from both the native run and the `--single-process` run that loads. Re-run with `litebox_diag::unix_conn_teardown=debug` and check whether a teardown line lands in the same second as that error.

---

## Also worth a probe (not top-3)

- `/proc/<pid>/{statm,status}` and `/proc/<pid>/task/<tid>/status` answer `errno=2` in `log-z0.txt` although `procfs.rs:651-660` lists them as served — so the miss is in the pid/task lookup, not the entry table. Chromium stats pids 69/78/79/98/100 and gets ENOENT.
- `SharedUnixConnTable` has only `SHARED_UNIX_CONN_CAPACITY = 4096` slots (`unix.rs:3823`) and, with `is_process_alive` defaulting to `true` on this host, nothing ever reclaims a leaked slot.
- `clone(CLONE_PIDFD)` → EINVAL (`process.rs:5448-5454`); `CLONE_NEWNET/NEWNS/NEWUTS/NEWIPC` → EPERM (`:5388-5398`) — chromium's zygote only needs `CLONE_NEWPID`, which works.
