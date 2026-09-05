# Premade-library research: where to stop hand-rolling

Research pass only, no code changed. Ordered by what the user asked for: look for
mature, premade solutions before writing/iterating custom code further, in five
specific areas this session accumulated hand-rolled work in.

## 1. OCI/Docker image pulling — partial win, custom merge logic stays

`advisor/probes/pull_oci_image.py` hand-rolls raw Registry V2 HTTP calls (auth
token, manifest-list resolution, layer fetch) because "no `docker` CLI
dependency" was a real constraint.

**Found**: [`oci-client`](https://github.com/oras-project/rust-oci-client)
(formerly `oci-distribution`, under the CNCF-governed `oras-project`), 185
stars, 86 forks, 1,549 commits, actively maintained (updated Jan 2026 per
crates.io). Implements the OCI Distribution spec: auth, manifest-list
resolution, layer pull -- exactly the part currently hand-rolled in Python.

**What it does NOT give you**: whiteout-file (`.wh.*`/`.wh..wh..opq`) handling
or layer-squash/merge semantics -- that's caller-delegated in every OCI client
library checked, this crate included. That logic, plus the Windows-symlink-
avoidance discipline (never extract to the host filesystem, merge at the
tar-stream level -- this project's own hard-won fix for the whole
symlink-flattening bug class), stays custom regardless of which client library
does the pulling.

**Recommendation**: worth a follow-up pass IF this pulling logic moves into
Rust (e.g. into `litebox_packager`, which already owns "convert a pulled
OCI image into a litebox-loadable tar" as its stated role) -- `oci-client`
would replace the raw-HTTP/auth-token/manifest-list part of
`pull_oci_image.py` with a maintained, spec-correct implementation, while the
whiteout-merge and Windows-safe-extraction logic ports over unchanged (it's
tar-stream-level work, not registry-client work). Not worth adopting as a
*Python* dependency swap in the current script -- the value is in a Rust
port, not a like-for-like Python library swap, since a `urllib`-based script
staying in Python has no real interop win from switching one HTTP client for
another.

## 2. ELF parsing for the syscall rewriter — already correct, nothing to change

`litebox_syscall_rewriter` already depends on `object` 0.36.7 (the
`gimli-rs/object` crate, used by `rustc` itself) for ELF parsing and
`iced-x86` 1.21 for x86 instruction decode/encode. This is precisely the
mature, standard choice -- confirmed by reading `litebox_syscall_rewriter/
Cargo.toml` directly, not assumption. **No action needed here; the premise
of this research area doesn't hold.**

## 3. Windows unwind-info construction — no premade crate exists for the write side

This is the one directly relevant to the still-open `RtlpUnwindPrologue`
platform bug (a separate fork this session is actively investigating).

**Found**: [`pe-unwind-info`](https://docs.rs/pe-unwind-info/latest/pe_unwind_info/x86_64/index.html)
exists on crates.io and exposes `FunctionTableEntries`, `RuntimeFunction`,
`UnwindInfo`, `UnwindCode`, `UnwindOperation` -- but verified directly via its
own docs: **it is READ-ONLY**. Its entry point,
`FunctionTableEntries::unwind_frame`, parses and walks EXISTING unwind info
to recover a return address; it has no facility for constructing or writing
`UNWIND_INFO`/`RUNTIME_FUNCTION` byte layouts for `RtlAddFunctionTable`
registration.

Also checked [`nbdd0121/unwinding`](https://github.com/nbdd0121/unwinding)
(a real, actively-maintained cross-arch unwinder) -- confirmed via its own
README to be DWARF/Itanium-ABI-based (ELF `.eh_frame`, FDEs), with **no
Windows SEH involvement at all**. Not applicable to this problem.

**No mature crate exists for the specific write-side operation needed**
(constructing valid `UNWIND_INFO` bytes for a hand-written `asm!` block and
registering it via `RtlAddFunctionTable`). Microsoft's own docs
([x64 exception handling](https://learn.microsoft.com/en-us/cpp/build/exception-handling-x64?view=msvc-170),
[`RtlAddFunctionTable`](https://learn.microsoft.com/en-us/windows/win32/api/winnt/nf-winnt-rtladdfunctiontable))
remain the actual source of truth here, alongside
[V8's own unwinding-info-win64.cc](https://github.com/v8/v8/blob/master/src/diagnostics/unwinding-info-win64.cc)
as a real, battle-tested reference implementation worth reading for the exact
byte-layout conventions a JIT-style dynamically-generated-code unwind table
needs (V8 hits precisely this problem for its own JIT'd functions, which is
structurally the same situation as litebox's hand-written `asm!` recovery
labels).

**Recommendation**: hand-rolling the write side remains the only path --
confirmed, not assumed. If the platform-bug investigation lands on Option (a)
(register real unwind metadata) rather than Option (b) (restructure the
primitives to avoid the hazard), use V8's real source as the structural
reference rather than re-deriving byte layouts from the MSVC docs alone.

## 4. Windows crash-dump tooling — a real, promising find, with one caveat

**Found**: [`minidump-writer`](https://github.com/rust-minidump/minidump-writer)
(Mozilla's `rust-minidump` project, a Rust rewrite of Breakpad's
`minidump_writer`), confirmed via its own docs to support **in-process,
same-process minidump capture on Windows x86_64** via
`MinidumpWriter::dump_local_context()` -- explicitly documented as usable
"from within an exception handler at crash time" using the current thread as
the crashing thread. Support-matrix caveat, quoted directly from the crate's
own docs: Windows x86_64 is marked "usable" but with production caution
advised; other Windows architectures are unimplemented.

**Why this could matter for the stuck investigation**: this session's own
crash diagnostics have hit a real wall -- `cdb`/live-debugger attach was
deemed "architecturally blocked" because it conflicts with `fork_verify`'s own
single-stepping. A minidump captured non-invasively from inside the VEH at
the moment of the SECOND (`RtlpUnwindPrologue`) fault, then analyzed
completely separately (offline, in WinDbg, `cdb -z`, or Visual Studio, with no
live attach needed during the actual crash) would sidestep that conflict
entirely -- the capture happens once, synchronously, inside the process that's
already crashing, not via an external debugger racing `fork_verify`'s
stepping. This reasoning holds up structurally: a minidump write is a
one-shot, non-interactive operation triggered from inside the crash itself,
not a live attach.

**Recommendation**: **worth a real prototype in a follow-up pass.** This is
the single most actionable, safety-relevant finding in this research pass --
if the current hand-rolled stack-dump/fault-ring diagnostics (already fixed
once this session for a `read_volatile`-alignment bug, then found to still
choke on a genuinely bogus `Rsp` value) still can't get sufficient visibility,
swapping to a real minidump write at the VEH's second-fault handler, then
analyzing it OFFLINE with a real debugger, is a genuinely different, more
powerful diagnostic path than continuing to extend a hand-rolled stack walker.
Scope: add `minidump-writer` as a dependency gated behind a diagnostic-only
feature/env-var (matching this file's own `diag_mm_enabled()`-style existing
convention), call `dump_local_context()` from the exact point the current
`[diag-unrecov-av-*]` prints fire, and analyze the resulting `.dmp` file
offline -- no live attach, no conflict with `fork_verify`.

## 5. `windows-sys` vs. `windows` crate — worth a look, not urgent

`litebox_platform_windows_userland` depends only on `windows-sys` 0.60.2 (raw
FFI bindings), not the higher-level `windows` crate (which wraps the same
Win32 surface with safer, more ergonomic types -- `Result`-returning calls,
owned handle types with `Drop`, etc.). This wasn't audited file-by-file (that
would be disproportionate for this pass), but the pattern itself -- manual
`HANDLE`/`MEMORY_BASIC_INFORMATION`/error-code handling seen throughout this
session's own reading of the crash-diagnostic code -- is exactly the shape
`windows` exists to make safer. **Lower-priority, worth revisiting**, not
because anything is broken, but because a future pass touching this file
extensively (e.g. the current `RtlpUnwindPrologue` fix attempt) might find
the higher-level crate reduces the exact class of "did I get the raw FFI call
right" risk this whole investigation has repeatedly hit.

## Summary table

| Area | Premade solution found? | Recommendation |
|---|---|---|
| 1. OCI pull | Yes -- `oci-client` (oras-project, CNCF) | Adopt if/when ported to Rust; whiteout/merge logic stays custom either way |
| 2. ELF parsing | Already using `object`+`iced-x86` | No action -- already correct |
| 3. Unwind-info *write* side | No -- confirmed no crate covers this | Hand-rolling is the only path; use V8's source as a real reference |
| 4. Crash-dump tooling | Yes -- `minidump-writer` (Mozilla) | **Prototype now** -- directly unblocks the stuck diagnostic-visibility problem |
| 5. `windows-sys` vs `windows` | N/A (pattern observation, not a gap) | Low-priority, revisit opportunistically |

## Part 2: whole-codebase audit

Broader pass per the user's own elevation of this into a standing project mantra
("go over everything in the project and make sure this is the case... dont
replace well written libs with code anywhere"). Read every `Cargo.toml` in the
workspace as the baseline; findings below are organized by the same numbered
areas requested.

### 1. Workspace dependency baseline (read in full)

All `[workspace] members` Cargo.toml files were read directly:
`litebox`, `litebox_common_linux`, `litebox_shim_linux`,
`litebox_platform_windows_userland`, `litebox_platform_linux_userland`,
`litebox_platform_macos_userland`, `litebox_packager`,
`litebox_runner_linux_on_windows_userland`, `litebox_runner_linux_userland`,
`litebox_syscall_rewriter`, `litebox_util_log`, `litebox_util_log_macros`,
`litebox_termemu`, `litebox_session_daemon` (plus `litebox_common_optee`,
`litebox_common_lvbs`, `litebox_platform_lvbs`, `litebox_platform_multiplex`,
and the OP-TEE/SNP/LVBS runners exist but are out of this pass's scope --
different deployment targets, not touched by any work this session).

### 2. Tar/archive handling -- MAJOR FINDING: a complete, correct Rust implementation already exists and was duplicated in Python this session

**`litebox_packager` already depends on the `tar` crate (`tar = "0.4"`)** and
**already implements a complete OCI-image-to-litebox-tar pipeline in Rust**,
including real whiteout-file handling. Confirmed by reading `litebox_packager/
src/oci.rs` directly:
- `pub fn pull_and_extract(image_ref: &str, verbose: bool) -> anyhow::Result<ExtractedImage>`
  (line 96) -- pulls a real OCI image via `oci_client::Client`.
- A documented function (line 88) explicitly states: "Layers are applied in
  order (bottom-up), handling whiteout files for..." -- the EXACT logic this
  session's own `advisor/probes/pull_oci_image.py` hand-rolled in Python.
- Real opaque-whiteout (`.wh..wh..opq`) and regular-whiteout (`.wh.<name>`)
  handling, confirmed at lines 349-463.
- `litebox_packager`'s own CLI (`litebox_packager --oci-image
  docker.io/library/alpine:latest --output out.tar`, confirmed via its
  `CliArgs` struct at `lib.rs:34-47`, `#[arg(long = "oci-image", ...
  conflicts_with = "input_files")]`) is **already the complete tool**
  `advisor/probes/fetch_container.py` was built to be this session, in Python,
  with a hand-rolled Registry V2 client -- duplicating existing, tested,
  in-tree Rust functionality.

**This directly changes Part 1's own recommendation** ("worth a follow-up pass
IF this pulling logic moves into Rust... not worth adopting as a Python
dependency swap"): the Rust port isn't hypothetical future work, it already
exists and is a first-class part of this project. `litebox_packager`'s own
doc comment on its role ("offline tool that converts a pulled OCI/Docker image
... into a litebox-loadable tar") was quoted correctly in Part 1's own text,
but its ACTUAL CAPABILITY (pulling directly from a registry ref, not just
converting an already-pulled image) was under-checked before recommending a
Python tool be built alongside it.

**Verified live, not just theorized** (this pass, no other litebox process
running at the time, confirmed via `tasklist` first): `target/release/
litebox_packager.exe --oci-image docker.io/library/busybox:latest --output
packager_test.tar` completed in well under 90 seconds --

```
Pulling OCI image: docker.io/library/busybox:latest
Scanning rootfs...
Found 436 files (416 executables to rewrite)
Rewriting 416 executable ELF files...
Creating .../packager_test.tar...
Created .../packager_test.tar (438 entries, 417.8 MB)
```

-- then that exact output tar was booted directly:
`litebox_runner_linux_on_windows_userland.exe --initial-files
packager_test.tar -- bin/sh -c 'echo PACKAGER_WORKS'` printed
**`PACKAGER_WORKS`**, confirming the pulled-rewritten-packaged tar is
genuinely, immediately litebox-bootable -- pull, rewrite, and package all in
one existing command, no Python, no manual multi-step chain. Test artifact
deleted after verification.

**Recommendation, now confirmed rather than theorized**: retire
`advisor/probes/pull_oci_image.py`, `advisor/probes/batch_rewrite_layer.py`,
and `advisor/probes/fetch_container.py` in favor of `litebox_packager
--oci-image <ref> --output <tar>`, which is the ALREADY-BUILT, ALREADY-WORKING
equivalent -- verified live in this same pass, not assumed. Document
`litebox_packager --oci-image` as the one, real, supported way to fetch a
container image going forward. This is the single highest-value finding in
this whole research pass: real, working, in-tree tooling was sitting unused
while new Python tooling duplicating it was built this session.

Also confirmed: no hand-rolled tar-structure parsing exists anywhere in the
Rust codebase outside `litebox_packager`, `litebox_runner_linux_on_windows_userland`,
and `litebox_runner_linux_userland` -- all three depend on and use the `tar`
crate directly (`grep` confirmed `use tar::` in all three). Nothing to
change here beyond retiring the Python duplication above.

### 3. Compression -- already correct

`litebox_packager` depends on `flate2 = "1.1"` (gated to
`x86_64`/Apple-Silicon targets alongside its OCI support) for gzip layer
decompression. No hand-rolled decompression found anywhere. No action needed.

### 4. HTTP/networking for OCI pulling -- superseded by finding #2 above

Given `litebox_packager` already has a real, in-tree Rust OCI puller using
`oci-client` + `reqwest`(-shaped, via `oci-client`'s own HTTP layer) +
`native-tls`, the original Part 1 question ("should `pull_oci_image.py` move
to Rust eventually") is moot -- it already has moved, in a different file,
and this session simply didn't check for it first. The Python script's own
`urllib`-only constraint was a reasonable call for what it was (a standalone
probe script avoiding new Python deps) but the standing recommendation now is
to use `litebox_packager` instead of maintaining ANY Python OCI-pull path
going forward, per finding #2.

### 5. ELF parsing beyond the syscall rewriter -- one real inconsistency found, worth a closer look

`litebox_syscall_rewriter` and `litebox_packager` both correctly use the
`object` crate (`0.36.7`, `elf`+`read_core` features, `no_std`-compatible --
confirmed `litebox_shim_linux` is itself `#![no_std]` and depends on `object`
directly with `default-features = false`, so `object`'s no_std-compatibility
is independently proven elsewhere in this exact workspace, not a
hypothetical).

**But `litebox_common_linux`'s own ELF LOADING code (`src/loader.rs`, the
guest-side program-header parsing that actually loads a binary into guest
memory, a different concern from the rewriter's syscall-site scanning) uses a
DIFFERENT crate: the `elf` crate (`elf = "0.8.0", default-features = false`),
not `object`.** Confirmed via `litebox_common_linux/src/loader.rs:12-13`
(`use elf::file::FileHeader; use elf::parse::ParseAt as _;`) and its own
Cargo.toml. `litebox_common_linux` is ALSO `#![no_std]` (confirmed via its
`src/lib.rs`), so the no_std constraint applies equally to both crates in
this workspace and cannot explain the inconsistency on its own -- `object`
already proves itself no_std-capable one crate over, in
`litebox_shim_linux`, which itself depends on `litebox_common_linux`.

**Recommendation**: this is a real, worth-investigating inconsistency --
either there's a genuine reason `loader.rs` needs `elf` specifically (e.g. a
particular no_std API shape `object` doesn't offer for program-header
iteration, or historical reasons predating `object`'s adoption elsewhere) or
this is exactly the kind of accumulated duplication the user's mantra is
about: two different, both-mature ELF-parsing crates in the same workspace
for closely-related purposes. Worth a dedicated follow-up pass to read
`loader.rs`'s actual usage of the `elf` crate's API surface and check whether
`object`'s own program-header/segment iteration API covers the same need,
BEFORE assuming a migration is warranted (a real API gap would justify
keeping `elf`; if none exists, consolidating onto `object` alone removes one
whole dependency from the workspace). Not attempted in this pass -- flagged
for a dedicated follow-up given `litebox_common_linux` is core, widely-used,
guest-agnostic code that deserves careful, isolated verification of any
change, not a rushed swap alongside this research pass.

### 6. Windows API usage -- `windows-sys` only, confirmed consistent; no `region`-crate gap found

Confirmed via direct Cargo.toml reads: `litebox` (target `cfg(windows)`) and
`litebox_platform_windows_userland` both depend on `windows-sys` only, never
the higher-level `windows` crate. This is a workspace-wide, consistent
choice, not an isolated oversight -- both files that touch Windows FFI make
the same call. Confirmed no `region`-crate-shaped gap: no repeated
hand-rolled `VirtualQuery`-result post-processing logic was found OUTSIDE the
crash-diagnostic code already flagged in Part 1's own Area 4 finding (the
`pagestate`/stack-dump `VirtualQuery` guard blocks) -- those are few,
already-consistent-with-each-other call sites, not a proliferating pattern a
crate like `region` would meaningfully simplify. No new recommendation beyond
Part 1's own existing "low-priority, revisit opportunistically" note on
`windows-sys` vs `windows`.

### 7. Logging/tracing infrastructure -- already correct, thin wrapper as intended

`litebox_util_log` (read `Cargo.toml` and confirmed via its own
`description = "Logging facade for LiteBox that supports multiple backends"`)
is a thin facade over the two real, standard Rust logging ecosystems:
`log` (`backend_log` feature, `log/kv` for structured key-value logging) and
`tracing` (`backend_tracing` feature) -- selected via Cargo features, not
reimplemented. Dev-dependencies confirm real backends are used for testing:
`env_logger` and `tracing-subscriber`. This is exactly the correct shape (a
thin facade allowing either standard backend, not a custom logging engine)
-- no action needed, confirmed rather than assumed.

### 8. Argument parsing -- already using `clap` everywhere it matters

Confirmed via `grep` across every `Cargo.toml`: `litebox_packager`,
`litebox_runner_linux_on_windows_userland`, `litebox_runner_linux_userland`,
`litebox_syscall_rewriter`, and `dev_bench` all depend on `clap` (`4.5.3x`,
`features = ["derive"]`) for their CLI surfaces. No hand-rolled
`std::env::args()` parsing found in any binary crate checked. **The one real
gap is on the Python side**: `advisor/probes/pull_oci_image.py`,
`batch_rewrite_layer.py`, and the newer `fetch_container.py` (this session's
own work) use positional `sys.argv`/`argparse` rather than being retired in
favor of `litebox_packager`'s own already-`clap`-based CLI -- see finding #2
above, the real actionable item here is retiring the Python tools, not adding
`clap` to them.

### 9. JSON/serialization -- already using `serde`/`serde_json` where structured data crosses a boundary

Confirmed via `grep`: `litebox_session_daemon` and `litebox_util_log`
(dev-only) depend on `serde`+`serde_json` directly. `litebox_packager`'s own
OCI-manifest handling goes through `oci_client`'s and `oci_spec`'s own typed
Rust structs (`oci_spec = { version = "0.9", features = ["image"] }` --
`oci-spec` is itself a mature, dedicated OCI-manifest-schema crate, a further
confirmation of finding #2's "already solved" theme), not hand-rolled JSON
parsing. No gap found.

### 10. Random/UUID/hashing -- minimal surface, nothing hand-rolled found

`litebox_platform_windows_userland` and `litebox_platform_linux_userland`
both depend on `getrandom = "0.3.4"` (the standard, minimal, audited Rust
crate for OS-backed randomness) for whatever real entropy litebox's platform
layer needs. `litebox_runner_linux_userland`'s dev-dependencies include
`sha2` (real hashing, used in test/verification code, per its dev-only
scoping). No hand-rolled RNG, UUID generation, or hashing logic was found
anywhere in the Rust codebase checked. No action needed.

### Part 2 summary table

| Area | Finding | Recommendation |
|---|---|---|
| 2. Tar/OCI pull | **`litebox_packager` ALREADY has a complete, correct Rust OCI-pull-to-tar pipeline** (`oci-client` + `tar` + whiteout handling + syscall rewriting, all wired together) | **Verify and retire the Python `pull_oci_image.py`/`batch_rewrite_layer.py`/`fetch_container.py` chain in favor of `litebox_packager --oci-image`** -- highest-value finding this pass |
| 3. Compression | Already using `flate2` | No action |
| 5. ELF loading (`litebox_common_linux::loader`) | Uses `elf` crate, while the rest of the workspace (including its own no_std sibling `litebox_shim_linux`) uses `object` | Real inconsistency, worth a dedicated follow-up to check for a genuine API gap before consolidating |
| 6. Windows API (`windows-sys` vs `windows`) | Confirmed consistent workspace-wide choice; no `region`-crate-shaped gap | No new action beyond Part 1's existing low-priority note |
| 7. Logging | Already a correct thin facade over `log`/`tracing` | No action |
| 8. CLI arg parsing | Already `clap` everywhere in Rust; gap is Python tooling not yet retired (see #2) | Retire Python tools per #2 |
| 9. JSON/serialization | Already `serde`/`serde_json`/`oci-spec` where needed | No action |
| 10. Random/hashing | Already `getrandom`/`sha2`, nothing hand-rolled | No action |

Sources for Part 2 (in-repo, read directly, no external fetches needed):
- `litebox_packager/Cargo.toml`, `litebox_packager/src/oci.rs`, `litebox_packager/src/lib.rs`
- `litebox_common_linux/Cargo.toml`, `litebox_common_linux/src/loader.rs`, `litebox_common_linux/src/lib.rs`
- `litebox_shim_linux/Cargo.toml`
- `litebox_platform_windows_userland/Cargo.toml`, `litebox_platform_linux_userland/Cargo.toml`
- `litebox_util_log/Cargo.toml`
- `litebox_session_daemon/Cargo.toml`
- Every other workspace-member `Cargo.toml` read directly for the baseline survey

## Part 3: core syscall-emulation logic audit (`litebox`, `litebox_common_linux`, `litebox_shim_linux`)

Neither Part 1 nor Part 2 looked past tooling/infrastructure into the actual guest-facing
emulation logic itself -- the real algorithmic/data-structure/ABI-definition code most likely to
hide genuine reinvention, and the code this whole session's own bug history traces back to most
often. Per the user's further-reinforced mantra ("make extra sure we're not reinventing any
wheels in any of our codebase"), this pass reads the actual source for each area, not just
`grep`, to confirm whether hand-rolling is genuinely justified (usually: `#![no_std]` + guest-ABI
byte-exactness, both real constraints in this workspace) or incidental.

**Baseline constraint, confirmed directly**: `litebox`, `litebox_common_linux`, and
`litebox_shim_linux` are ALL `#![no_std]` (confirmed via each crate's own `src/lib.rs`). This is
the single biggest fact shaping what's genuinely hand-rollable-by-necessity here -- most
std-oriented crates (`nix`, `rustix`'s higher-level API, `region`) are simply not usable in this
code at all, independent of maturity. Findings below account for this throughout.

### 1. Data structures -- mostly ALREADY correct, one real gap found

**`rangemap` is ALREADY a direct dependency of `litebox`** (`Cargo.toml`: `rangemap = { version =
"1.5.1", features = ["const_fn"] }`) and is genuinely used, confirmed via `grep -rl
"rangemap::" litebox/src/` -> `litebox/src/mm/linux.rs` (the VMA-tracking code the "VmArea/
rangemap coalescing bug" commit message refers to). **This refutes the premise of investigating
this as a gap** -- it's not hand-rolled `BTreeMap`-based range logic, it's the real, maintained
crate, already in use, already the subject of at least one bug FIX (not a reinvention) this
session's own history references. No action needed.

**`ringbuf` is ALSO already a direct dependency of BOTH `litebox` and `litebox_shim_linux`**
(confirmed in both `Cargo.toml`s) and genuinely used: `litebox/src/net/socket_channel.rs`,
`litebox/src/pipes.rs`, `litebox_shim_linux/src/channel.rs`. No action needed for these.

**`buddy_system_allocator` and `slabmalloc` are already direct dependencies of `litebox`**,
confirmed used in `litebox/src/mm/allocator.rs` -- this IS the guest heap allocator, genuinely
using two real, purpose-built allocator crates (`buddy_system_allocator` for the buddy allocator,
`slabmalloc` -- pinned to an unreleased git rev with fixes on top of 0.11.0, a deliberate,
documented choice, not accidental -- for slab allocation) rather than hand-rolled free-list logic.
No action needed.

**`bitflags` is already used 30+ times across `litebox`/`litebox_common_linux`/
`litebox_shim_linux`** (confirmed via `grep -c "bitflags::bitflags!"`) for exactly the
`OFlags`/`ProtFlags`/`MapFlags`/`MemoryRegionPermissions`-shaped types this pass set out to check
-- already the standard crate, not hand-rolled bit manipulation. No action needed.

**The one real, concrete, low-risk gap found**: the crash-diagnostic "fault ring buffer"
(`RECENT_FAULTS` in `litebox_platform_windows_userland/src/lib.rs:596`) IS genuinely hand-rolled
-- `static RECENT_FAULTS: RefCell<[(i32, u64, u64, bool); 4]>`, a raw fixed-size array with manual
wraparound logic, not a ring-buffer crate. This is notable specifically because `ringbuf` is
ALREADY a proven, working dependency one crate over in the very same workspace (`litebox`,
`litebox_shim_linux`) -- there's no `no_std` or FFI-boundary reason this one instance couldn't use
it too; `litebox_platform_windows_userland` would just need to add the dependency.
**Recommendation**: low-risk, well-scoped swap candidate for a future pass -- replace the raw
4-element array + manual index wraparound with `ringbuf`'s `HeapRb`/const-generic ring buffer,
using the exact same tested crate this workspace already trusts elsewhere. Not attempted here
(this file is actively owned by a concurrent fork's platform-bug investigation this session --
flagging for whoever picks it up next, not touching it now).

### 2. Linux syscall ABI/struct definitions -- genuinely hand-rolled, and this is where a real historical bug came from

**`litebox_common_linux/src/lib.rs` hand-defines every Linux/DRM/input ABI struct this codebase
needs**: `FileStat`, `Statx`, `StatxTimestamp`, `Statfs`, `Flock`, `Termios`, `Winsize`,
`InputId`, `InputEvent`, `VtStat`, `VtMode`, and ~20 separate `DrmMode*` structs (`DrmModeCreateDumb`,
`DrmModeGetConnector`, `DrmModeObjGetProperties`, etc.) -- confirmed by listing every top-level
`pub struct` in the file. These are `zerocopy`-derived (the crate is already a dependency,
`zerocopy = { version = "0.8", features = ["derive"] }`) for guest-ABI-exact byte layout, which is
a real, defensible reason to hand-define rather than pull in a std-oriented crate like `libc`
(wrong target -- `libc` describes the HOST's libc ABI, not an arbitrary guest's) -- `libc` IS
already a dependency of `litebox_shim_linux`, but only as a **dev-dependency** for test code
running against a real host, confirmed via its Cargo.toml scoping, not for the actual guest-ABI
struct definitions themselves. So this part is genuinely, structurally justified: no existing
crate provides "the Linux syscall ABI struct layouts, no_std, independent of host libc" in the
exact shape this project needs, and `zerocopy`-derived hand-definitions are a reasonable way to
get byte-exact guest-visible structs.

**But the DRM ioctl NUMBER/constant definitions specifically are a different, more concrete
case**, and this is the one place in this whole audit where hand-rolling has a DOCUMENTED, REAL
bug in this exact session's own history (the DPMS/EDID connector-property work, which shipped a
wrong, hand-remembered ioctl number before being caught and fixed via live kernel-header
verification). The file's own comments show real diligence (`"fetched live from the real kernel
drm.h (torvalds/linux master), not guessed"`, `"size independently re-verified"`) -- meaning past
mistakes weren't from carelessness, they're the INHERENT risk of hand-transcribing kernel-header
constants even when done carefully, every single time a new ioctl is added. Confirmed via `grep`:
**no `drm-fourcc`/`drm-rs` dependency exists anywhere in this workspace** -- there's instead a
companion doc, `docs/drm-dumb-buffer-ioctl-reference.md`, that exists specifically to track "gaps"
in this manual transcription process (its own name says so).

**Recommendation**: [`drm-fourcc`](https://crates.io/crates/drm-fourcc) (a small, focused,
maintained crate providing DRM pixel-format constants -- confirmed lightweight enough to plausibly
be `no_std`-portable, though this specific claim needs verifying before adoption, not assumed) is
worth a real look for at least the pixel-format-constant subset of this problem. For the ioctl
NUMBER constants specifically (the actual `DRM_IOCTL_MODE_*` values, the class that caused the
real bug), check whether `linux-raw-sys` (the `no_std`-compatible, actively-maintained crate that
underlies `rustix` itself, providing exactly "raw Linux kernel ABI constants/structs, no host libc
required") has full DRM ioctl coverage -- it's the single best-shaped candidate found in this pass
for eliminating this entire bug CLASS (a maintained crate's constants get fixed by the crate
maintainers when the kernel ABI changes; a hand-transcribed comment doesn't). Not adopted in this
pass -- this is genuinely core, live-tested code (the whole XFCE/labwc DRM investigation this
session depends on it), so swapping constant sources deserves its own dedicated, carefully-
verified follow-up pass, not a rushed change alongside this research.

### 3. Errno handling -- hand-rolled, but for a real, structural reason (not incidental)

`litebox_common_linux::errno::Errno` (`src/errno/mod.rs:28`) is a hand-rolled
`NonZeroU8`-wrapping struct with a large generated constant table (`mod generated`), not
`rustix`'s or `nix`'s `Errno` type. **This is genuinely justified, not incidental**: both `rustix`
and `nix` represent the HOST's own errno (whatever the compiling/running platform's libc defines),
while this `Errno` represents the GUEST Linux ABI's errno values -- litebox's whole architecture
runs guest Linux syscalls under Windows/macOS/other hosts, so a guest-Linux-specific errno type
that's independent of host platform is structurally necessary, not a missed opportunity. No
action recommended -- confirmed, not assumed, via reading the actual type definition and its own
doc comment ("This is a transparent wrapper around Linux error numbers... intended to provide
some type safety").

### 4. ELF loading (`litebox_common_linux::loader`) -- Part 2's flagged inconsistency, now with a
concrete API-shape reason found (not fully resolved, but narrowed)

Part 2 flagged `loader.rs` using the `elf` crate while the rest of the workspace uses `object`,
recommending a dedicated follow-up before assuming a migration is warranted. This pass read
`loader.rs`'s actual `elf`-crate usage (lines 12-13, 55-56, 97-106, 183-203, 336-338) and found a
concrete, plausible reason for the divergence: `loader.rs` parses INCREMENTALLY, against a
fallible `ReadAt`-style trait over a live guest file descriptor (`elf::file::parse_ident`, then
`FileHeader::parse_tail` against a manually-sized stack buffer, then a lazy
`elf::parse::ParsingIterator` over program headers) -- i.e. streaming, no-alloc, I/O-driven
parsing directly against syscall reads, not parsing an already-in-memory byte slice. `object`'s
own typical API shape (used by both `litebox_syscall_rewriter` and `litebox_packager`) parses
against an in-memory `&[u8]` -- a genuinely different access pattern, since those two callers
already have the whole file's bytes on the host side before parsing, while `loader.rs` is loading
a guest program by streaming reads through the GUEST's own file-descriptor emulation, which is a
real, structural difference, not an accident.

**This narrows but does not fully resolve Part 2's own open question**: whether `object` ALSO
exposes an incremental/streaming parsing mode that could cover this exact need is still unchecked
(this pass ran out of scope to verify `object`'s full API surface for a no-alloc streaming
program-header iterator equivalent to `elf`'s `ParsingIterator`). **Recommendation unchanged from
Part 2**: still worth a dedicated follow-up specifically to check `object`'s streaming-parse
capability before concluding either way -- this pass adds real evidence (the streaming-vs-in-memory
distinction) rather than resolving the question, since a rushed swap of core guest-loading logic
without confirming API-shape parity would be exactly the kind of risky, unverified change this
session's own standing discipline (see the CoW-mmap and RtlpUnwindPrologue passes) argues against.

### Part 3 summary table

| Area | Finding | Recommendation |
|---|---|---|
| Range-map/VMA tracking | Already `rangemap`, genuinely used, not hand-rolled | No action |
| Ring buffers (pipes/sockets) | Already `ringbuf`, genuinely used in `litebox`/`litebox_shim_linux` | No action |
| Crash-diagnostic fault ring (`RECENT_FAULTS`) | Hand-rolled fixed array, while `ringbuf` is already proven one crate over | **Low-risk swap candidate** -- adopt `ringbuf` here too |
| Guest heap allocator | Already `buddy_system_allocator` + `slabmalloc`, genuinely used | No action |
| Bit-flag types (`OFlags`/`ProtFlags`/etc.) | Already `bitflags`, 30+ real usages | No action |
| Linux/DRM/input ABI structs | Hand-rolled (`zerocopy`-derived), structurally justified for `no_std` guest-ABI-exact layout | No action on the struct layouts themselves |
| DRM ioctl NUMBER constants specifically | Hand-transcribed from kernel headers, ALREADY caused one real documented bug this session | **Worth a dedicated pass**: check `linux-raw-sys`/`drm-fourcc` coverage and `no_std` fit |
| Guest-Linux `Errno` type | Hand-rolled, but structurally justified (guest ABI ≠ host errno, `rustix`/`nix` represent the wrong platform) | No action |
| ELF loading (`loader.rs` vs. rewriter/packager) | Uses `elf` crate for streaming I/O-driven parsing, a real API-shape difference from `object`'s in-memory usage elsewhere | Still worth a follow-up to check if `object` has an equivalent streaming mode, not yet confirmed either way |

Sources for Part 3 (in-repo, read directly):
- `litebox/Cargo.toml`, `litebox/src/mm/linux.rs`, `litebox/src/mm/allocator.rs`,
  `litebox/src/net/socket_channel.rs`, `litebox/src/pipes.rs`
- `litebox_common_linux/Cargo.toml`, `litebox_common_linux/src/lib.rs`,
  `litebox_common_linux/src/errno/mod.rs`, `litebox_common_linux/src/loader.rs`
- `litebox_shim_linux/Cargo.toml`, `litebox_shim_linux/src/channel.rs`
- `litebox_platform_windows_userland/src/lib.rs` (`RECENT_FAULTS` at line 596)
- `litebox_shim_linux/src/syscalls/drm.rs`, `docs/drm-dumb-buffer-ioctl-reference.md`
- [drm-fourcc on crates.io](https://crates.io/crates/drm-fourcc)
- [linux-raw-sys on crates.io](https://crates.io/crates/linux-raw-sys) (the crate underlying `rustix`)

Sources:
- [oci-client on GitHub](https://github.com/oras-project/rust-oci-client)
- [oci-client on crates.io](https://crates.io/crates/oci-client)
- [pe-unwind-info docs.rs](https://docs.rs/pe-unwind-info/latest/pe_unwind_info/x86_64/index.html)
- [nbdd0121/unwinding on GitHub](https://github.com/nbdd0121/unwinding)
- [minidump-writer on GitHub](https://github.com/rust-minidump/minidump-writer)
- [minidump-writer on crates.io](https://crates.io/crates/minidump-writer)
- [x64 exception handling -- Microsoft Learn](https://learn.microsoft.com/en-us/cpp/build/exception-handling-x64?view=msvc-170)
- [RtlAddFunctionTable -- Microsoft Learn](https://learn.microsoft.com/en-us/windows/win32/api/winnt/nf-winnt-rtladdfunctiontable)
- [V8's unwinding-info-win64.cc](https://github.com/v8/v8/blob/master/src/diagnostics/unwinding-info-win64.cc)
