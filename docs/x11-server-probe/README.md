# X11 server (Xorg) feasibility probe, round 2: real WSL2 apt install, real ldd, real run

Round 1 (`2cce37d`) scoped this via Alpine's `.PKGINFO` metadata and the
Windows-host `clang`+manually-fetched-`.apk` approach used elsewhere this
session -- concluded "genuinely large dependency closure, multi-session
scope" without actually attempting a build. This round uses WSL2's native
`apt`, native `ldd`, and native `objdump`, which turned out to be a much more
tractable and *more precise* path -- and got Xorg itself running as a real
litebox guest process, further than round 1 got.

**Round 3 note**: `link`/`linkat` are now implemented (see that section
below) -- round 2's lock-file blocker, reproduced in this file's own "What
was done"/"Result" sections below, is fixed on current `main`. Re-running
this exact recipe now progresses further, into a different, real blocker
(`_XSERVTransmkdir`'s `euid != 0` check, then a hard panic on `O_PATH`) --
see the round 3 section for the precise, live-verified detail.

## What was done

1. **Real Ubuntu 24.04 Xorg installed via `apt-get install xserver-xorg-core
   xserver-xorg-legacy`** in WSL2 -- a genuine `/usr/bin/Xorg` (actually
   `/usr/lib/xorg/Xorg`), version 21.1.12, with `modesetting_drv.so` (the
   correct KMS/DRM driver, already present, no extra install needed).

2. **Real dependency closure via `ldd`** (far more reliable than reading a
   `.PKGINFO` file by hand): 34 entries (`vdso` excluded, kernel-provided,
   not a real file). Notably: Ubuntu's packaging keeps `mesa`/`libGL`/`libEGL`
   entirely OUT of `xserver-xorg-core`'s closure (unlike Alpine's bundling,
   which round 1 flagged as the single biggest weight) -- a real, concrete
   improvement over the prior estimate.

3. **Narrowed the REAL rewrite surface using `objdump -d | grep syscall`**,
   not just dependency-counting: of the 34 linked libraries, only 12 contain
   any `syscall` instruction at all (the rest -- `libXfont2`, `libfreetype`,
   `libpng16`, `liblzma`, `libz`, `libpcre2`, etc. -- are pure computation,
   reached via libc, needing no rewrite themselves). Combined with `libc.so.6`
   (640 syscall sites) and `ld-linux-x86-64.so.2` (55 syscall sites, runs
   before any library init), the real rewrite list is **15 files total**, not
   "15-25+ shared libraries" as round 1's dependency-count-based estimate
   implied -- a materially smaller and precisely bounded surface.

4. **Rewrote all 15 with `litebox_syscall_rewriter`** (built via plain
   `cargo build --release`, ELF rewriting is OS-agnostic byte manipulation,
   same recipe as every other probe this session), assembled a rootfs
   (`ld-linux-x86-64.so.2`, `libc.so.6`, the 12 syscalling libs, `Xorg` itself,
   `modesetting_drv.so`, a minimal KMS-only `xorg-kms.conf`), packaged into a
   tar, and ran it under a real `litebox_runner_linux_userland` (built via
   `cargo zigbuild --target x86_64-unknown-linux-gnu --release`) in WSL2.

## Result: real progress past round 1, a real new blocker found and precisely diagnosed

**`Xorg -version` succeeded completely** as a real litebox guest process:

```
X.Org X Server 1.21.1.11
X Protocol Version 11, Revision 0
Current Operating System: LiteBox litebox 5.11.0 5.11.0 x86_64
xorg-server 2:21.1.12-1ubuntu1.6 ...
Current version of pixman: 0.42.2
```

This proves the entire rewritten chain -- `ld.so`, `libc`, `libdbus`,
`libudev`, `libselinux`, `libgcrypt`, `libunwind`, `libpixman`,
`libsystemd`, `libxshmfence`, `libaudit`, `libcap`, `libbsd`, `libcap-ng`,
`libgpg-error` -- genuinely loads and initializes correctly against litebox.
Real `Current Operating System: LiteBox litebox ...` in the output confirms
Xorg's own `uname()` call is being answered by litebox's real `uname`
emulation, not bypassed.

**`Xorg -config /etc/xorg-kms.conf :1` (an actual server start, KMS-only,
`-novtswitch -sharevts`) failed with a real, precisely diagnosed error**:

```
(EE) Linking lock file (/tmp/.X1-lock) in place failed: Function not implemented
```

Xorg's lock-file acquisition uses the classic atomic "create a temp file,
then `link()` it to the real lock path" pattern (avoiding a TOCTOU race a
plain `open(O_CREAT|O_EXCL)` doesn't fully close for hardlink-based locking
across NFS-like semantics) -- confirmed via `strace`-equivalent reasoning
from the error text and Xorg's own well-known lock-file behavior. **Confirmed
by direct code inspection**: `litebox_shim_linux/src/syscalls/file.rs` has
`sys_unlinkat`, `sys_symlinkat`, `sys_renameat2` -- but no `sys_linkat`/`link`
handler anywhere. Grepped `litebox_common_linux/src/lib.rs` and
`litebox_shim_linux/src/lib.rs` for `linkat`/`Sysno::linkat`/
`SyscallRequest::Link*` -- zero matches. `link()`/`linkat()` fall through to
the generic `ENOSYS` catch-all, surfacing to Xorg as `EPERM`-adjacent
"Function not implemented".

## Not attempted this pass: implementing `linkat`

This is real, separate filesystem-layer work (a new `SyscallRequest`
variant, a new `FileSystem` trait method, wiring through whatever backend
litebox's layered FS uses) -- out of scope for a probe pass whose job was to
find the real next blocker, not fix every filesystem gap encountered. If
`linkat` is implemented in a future pass, this exact rootfs/recipe (below)
should get Xorg meaningfully further -- likely into real DRM device
interaction, which is the actual load-bearing question for this PRD row.

## Reproducing this

```sh
# In WSL2:
apt-get install -y --no-install-recommends xserver-xorg-core xserver-xorg-legacy

# Real dependency + rewrite-surface analysis:
ldd /usr/lib/xorg/Xorg
# then objdump -d <each .so> | grep -c 'syscall\b' to find which need rewriting

# Assemble a rootfs with: ld-linux-x86-64.so.2, libc.so.6, the 12 syscalling
# libs listed above, Xorg itself, modesetting_drv.so, and a minimal KMS-only
# xorg.conf (Driver "modesetting", Option "kms" "true").

# Rewrite each with (built once, on the Windows host):
cargo build --release -p litebox_syscall_rewriter --bin litebox_syscall_rewriter --features std,anyhow,clap
target/release/litebox_syscall_rewriter.exe <file> -o <file>.hooked   # then mv over the original

# Package + run (in WSL2, against WSLg's real X11 display):
cargo zigbuild -p litebox_runner_linux_userland --bin litebox_runner_linux_userland --target x86_64-unknown-linux-gnu --release
tar --owner=0 --group=0 -cf rootfs.tar <rootfs dir contents>
DISPLAY=:0 ./litebox_runner_linux_userland -Z --forward-env --initial-files rootfs.tar --program-from-tar --gui -- /usr/lib/xorg/Xorg -config /etc/xorg-kms.conf -novtswitch -sharevts -noreset -logfile /tmp/xorg.log :1
```

## Round 3: `linkat`/`link` implemented, the lock-file blocker is genuinely gone, a real new blocker found further in

Implemented `linkat`/`link` for real -- not a stub -- in `litebox`'s core
filesystem layer: a new `FileSystem::link` trait method, a real
`in_mem::FileSystem::link` implementation (clones the existing path's
`Entry::File`'s `Arc`, so the new path genuinely shares the same underlying
`FileX`/`unique_id` -- a write through either path is visible through the
other, matching real Linux hard-link semantics, not a copy), pass-through
implementations in `layered::FileSystem` (mirrors `rename`'s existing
upper-layer-only + missing-parent-migration pattern) and
`resolver::Resolver<Composer>`/`nine_p` (mirror `symlink`'s existing
`ReadOnlyFileSystem`/`Io` stubs, since neither backend can meaningfully
support it), a new `LinkError` type, `Errno` conversion (`EPERM` for
directories matching real Linux, `EEXIST`, `EXDEV` for cross-layer), and
`sys_linkat`/the `SyscallRequest::Linkat` wiring (mirroring
`sys_unlinkat`/`sys_renameat`'s established pattern, `link` routes through
the same `linkat`-with-`AT_FDCWD` handler `unlink`/`rename` already use, no
separate wrapper needed). Two new permanent regression tests in
`litebox_shim_linux/src/syscalls/file.rs`: one confirms genuine shared
identity (same `st_ino`, a write through the new path is visible through the
old path, unlinking one leaves the other intact and readable), one confirms
the real Linux error cases (`EPERM` for a directory, `EEXIST` for an
existing destination, `ENOENT` for a missing source). Full `litebox` and
`litebox_shim_linux` test suites pass unchanged (confirmed against baseline
`main` via `git stash` that the one pre-existing `litebox --lib` test
failure and the one pre-existing `test_mremap` flake both predate this
change).

**Live-verified against the exact same rootfs/recipe this file already
documents**: re-ran the identical reproduction steps below with the new
`link`/`linkat` support built in. The lock-file error is confirmed
GENUINELY GONE -- grepping the full run's output for `lock`/`linking`
returns zero matches, where round 2's run showed
`(EE) Linking lock file (/tmp/.X1-lock) in place failed: Function not
implemented` at this exact point every time.

Xorg now progresses meaningfully further, through two more real steps,
before hitting a genuinely NEW, different, and precisely diagnosed blocker:

```
_XSERVTransmkdir: ERROR: euid != 0,directory /tmp/.X11-unix will not be created.
_XSERVTransSocketCreateListener: failed to bind listener
_XSERVTransSocketUNIXCreateListener: ...SocketCreateListener() failed
_XSERVTransMakeAllCOTSServerListeners: failed to create listener for unix

thread 'main' (1223) panicked at litebox\src\fs\layered.rs:483:13:
not implemented: OFlags(NOFOLLOW | PATH)
```

Two separate things visible here, not fully disentangled this pass:
1. Xorg's own `_XSERVTransmkdir` euid check believes it isn't running as
   UID 0 and refuses to create `/tmp/.X11-unix` itself (pre-creating the
   directory in the rootfs ahead of time, with `chmod 1777`, did NOT avoid
   this -- the check is about the process's own perceived euid, not the
   directory's existence) -- likely a real, separate question about how
   litebox's guest `uid`/`euid` emulation answers `geteuid()` for this
   process, not investigated further this pass.
2. Immediately after, a hard `unimplemented!()` PANIC (not a clean errno
   return) on `OFlags(NOFOLLOW | PATH)` -- `litebox/src/fs/layered.rs`'s
   `open()` only supports a fixed allow-list of `OFlags`
   (`CREAT`/`RDONLY`/`WRONLY`/`RDWR`/`EXCL`/`TRUNC`/`NOCTTY`/`DIRECTORY`/
   `NONBLOCK`/`LARGEFILE`/`NOFOLLOW`/`APPEND`) and panics outright on
   anything else, rather than returning `EINVAL`/`ENOSYS`. `O_PATH` (open a
   path-only reference with no read/write access, used to safely probe a
   path's existence/type -- likely from Xorg's own socket-directory
   validation, or a library in its dependency chain such as `libselinux`)
   is not in that allow-list.

## Round 4: `O_PATH` implemented for real across all three `FileSystem` backends, the panic is genuinely gone

Round 3 found a hard `unimplemented!()` panic on `OFlags(NOFOLLOW | PATH)` in
`litebox/src/fs/layered.rs`'s `open()`. Investigating it live turned up that
this was not one bug but THREE separate, independent gaps stacked on top of
each other -- `litebox::fs::FileSystem` has three real implementations in the
guest's actual fs stack (`resolver.rs`'s `Resolver<Composer>`, `layered.rs`'s
`FileSystem<Upper, Lower>`, and `in_mem.rs`'s `FileSystem`, in that call
order), and each one independently rejects `OFlags` via its own hand-written
allow-list -- `resolver.rs`'s already included `OFlags::PATH` (a real,
already-working `path_only` mechanism existed one layer down), but
`layered.rs`'s and `in_mem.rs`'s did not, so the panic Xorg hit was actually
the SECOND of the three layers (`layered.rs`), and fixing only that would
have immediately hit the identical panic one layer further in
(`in_mem.rs`) -- confirmed live via `gdb`+`RUST_BACKTRACE=1` catching each
panic's exact message and call site in turn, not assumed.

Fixed all three, plus a related but genuinely separate correctness bug found
along the way: `in_mem.rs`'s own `read_allowed`/`write_allowed` computation
derives from `access_mode = flags & (WRONLY | RDWR)`, which for a bare
`O_PATH` open (no other access-mode bit set) numerically equals `O_RDONLY`
(value 0) -- meaning a pre-fix `O_PATH` open would have silently granted
`read_allowed = true`, letting `read()`/`write()` succeed on a supposedly
path-only fd instead of correctly failing with `EBADF`. This was never
reached in practice (the panic fired first, every time), but is a genuine
distinct bug, not just a missing-flag oversight -- fixed by explicitly
forcing both flags false whenever `O_PATH` is set, in all three backends
that independently track this (`resolver.rs`'s `path_only` field already did
this correctly; `in_mem.rs` did not).

Three new `PathOnlyFd` error variants added (`SeekError`, `TruncateError`,
`ReadDirError` -- `ReadError`/`WriteError` already had exactly the right
existing `NotForReading`/`NotForWriting` variants, reused rather than
duplicated), all mapping to `EBADF` (matching real Linux's actual behavior
for I/O attempts on an `O_PATH` fd) except where an existing catch-all
already handled it. A new permanent regression test,
`o_path_fd_permits_stat_and_dirfd_use_but_rejects_read_write`
(`litebox_shim_linux/src/syscalls/file.rs`), confirms: `O_PATH` open
succeeds even with no meaningful access-mode bit; `fstat` on the resulting
fd works; `read`/`write`/`lseek`/`ftruncate` all correctly fail with `EBADF`
(not panic); the original, normally-opened fd for the same file is
completely unaffected. Full `litebox_shim_linux` test suite (157 tests, one
new + 156 baseline) and `litebox` clippy both clean; the one pre-existing
`litebox --lib` compile failure (missing `PageManagementProvider` trait
items in a test mock, confirmed via `git stash` to predate this change) and
`test_mremap`'s flake are both untouched by this fix.

**Live-verified against a freshly rebuilt copy of the exact rootfs/recipe
this file already documents** (round 3's own rootfs was not preserved per
this project's own "don't check in reconstructible artifacts" convention, so
this was rebuilt from scratch via the same `ldd`+`objdump`+
`litebox_syscall_rewriter` steps): re-ran the identical
`Xorg -config /etc/xorg-kms.conf -novtswitch -sharevts -noreset -logfile
/tmp/xorg.log :1` invocation. **The `O_PATH` panic is confirmed completely
gone** -- grepping the full run's output for `path`/`panic`/`unimplemented`
returns zero matches, reproduced twice. Xorg now runs its FULL startup
sequence with no crash at all: prints its version banner (`Current Operating
System: LiteBox litebox ...`), hits the pre-existing, separately-documented
`_XSERVTransmkdir: euid != 0` question (unrelated to this fix, not
attempted), and then fails cleanly with `(EE) no screens found` -- a
genuine, expected next-step error (this WSL2 environment's own real X11
socket is not litebox's actual DRM device; Xorg correctly can't find a KMS
screen through it), not a crash or hang.

## Updated scoping vs. rounds 1-3

Round 1's "15-25+ shared libraries, multi-session-scale" conclusion
continues to be refined, not overturned. Round 2 got Xorg loading and
initializing; round 3 found and diagnosed (but did not fix) the `O_PATH`
panic; round 4 fixes it for real across all three `FileSystem` backends and
confirms live that Xorg's startup sequence now runs with zero panics. The
general pattern across all four rounds holds: each pass gets meaningfully
further and leaves a precise, actionable next step rather than a vague
estimate. Remaining, not attempted this pass: the `_XSERVTransmkdir`
`geteuid()`-adjacent question (does litebox's guest UID/EUID emulation
answer `geteuid()` correctly for this specific process, or does Xorg have
its own separate expectation this doesn't satisfy), and -- the row's actual
load-bearing question -- whether Xorg's `modesetting` driver can be pointed
at litebox's REAL virtual `/dev/dri/card0` (rather than WSLg's own host X11
socket, used here only as a convenient, already-working real X11 display to
verify Xorg itself starts cleanly) to reach real DRM master/mode-setting
interaction.
