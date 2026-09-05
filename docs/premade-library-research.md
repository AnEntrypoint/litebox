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
