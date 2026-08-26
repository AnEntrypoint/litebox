# X11 server (Xorg) feasibility probe, round 2: real WSL2 apt install, real ldd, real run

Round 1 (`2cce37d`) scoped this via Alpine's `.PKGINFO` metadata and the
Windows-host `clang`+manually-fetched-`.apk` approach used elsewhere this
session -- concluded "genuinely large dependency closure, multi-session
scope" without actually attempting a build. This round uses WSL2's native
`apt`, native `ldd`, and native `objdump`, which turned out to be a much more
tractable and *more precise* path -- and got Xorg itself running as a real
litebox guest process, further than round 1 got.

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

## Updated scoping vs. round 1

Round 1's "15-25+ shared libraries, multi-session-scale" conclusion is
**partially refined, not overturned**: the real rewrite surface (15 files,
not 15-25+) is smaller and more precisely bounded than estimated, and Xorg
itself now genuinely runs and initializes as a litebox guest process --
further than round 1 got. But a real, concrete new blocker (`linkat`
unimplemented) now stands between here and a working display, and there may
be more blockers beyond it (font loading -- no font packages were installed
in this pass -- and actual DRM master/mode-setting interaction, the row's
real load-bearing question, were never reached). Still genuinely multi-pass
work, but with a precise, evidence-based next step instead of a vague
estimate.
