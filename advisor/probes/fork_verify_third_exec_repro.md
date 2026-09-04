# fork_verify AV: minimal repro (3 execs of one large ELF)

Deterministic host-side access violation in litebox's `fork_verify` single-step
healing path. NOT a guest bug and NOT specific to mate-session.

## Repro (~40s, no compositor, no boot)

    runner --initial-files .wfgy/webtop_seatd.tar --resume-from three.tar -- /bin/sh /p.sh

where `/p.sh` is simply:

    /usr/bin/mate-session --version   # OK
    /usr/bin/mate-session --version   # OK
    /usr/bin/mate-session --version   # 64 AV events, faults here

## Signature

    rip=0x0 (64 of 65 events; one rip=0x2)
    is_in_guest=false   is_verifying=true      (all events)
    Protect=0x4 (PAGE_READWRITE -- page IS mapped and writable)
    "no exception-table entry found"
    stack [rsp+0x0..0x38] decodes as UTF-16: "LITEBOX_DIAG_FAT...LT_VEC LE INE 4"
        i.e. litebox's OWN env-var names -- a Windows-side
        GetEnvironmentVariableW-shaped lookup, never guest musl data.

## What is ruled OUT, by measurement

    2 execs of mate-session (straight-line)   clean, 0 AV
    2 execs of busybox                        clean, 0 AV
    mate-session then busybox                 clean, 0 AV
    60 execs of busybox in a loop             clean, 0 AV
    loop vs straight-line                     identical -- shell shape irrelevant
    loop counts 3/4/5/6                       ALL fault at exactly the 3rd exec

So it is not exec count in general (60 small execs are fine), not switching
between binaries (one binary suffices), and not two execs (needs the third).

## Interpretation -- MECHANISM CLAIM WITHDRAWN

An earlier version of this file attributed the fault to a leak/off-by-one in
`fork_verify`'s per-exec relocation-range bookkeeping. **That mechanism is
withdrawn** -- it was stated without verification, and a prior 30+-pass
investigation (`docs/AGENTS_ARCHIVE_2026-09-03.md`, passes 14-233) points
elsewhere: at `litebox/src/mm/exception_table.rs`'s fallible-memory primitives,
whose raw-asm recovery labels have no registered RUNTIME_FUNCTION/.pdata/.xdata,
so ntdll's unwinder faults when anything later unwinds through such a frame.
Note that archive's own pass-208 theory was RETRACTED in pass 209 (the primitive
it blamed turned out to have complete compiler-generated unwind coverage), so
the root cause is genuinely open -- do not treat any of these as settled.

What survives here is the OBSERVATION, not an explanation. The archive's passes
14-19 also caution that secondary faults surface wherever the stack happens to
be corrupted, so differing `rip` values across captures (`RtlpUnwindPrologue`
in one, `rip=0x0` here) are plausibly one corruption seen at different points.

## The one discriminating control (not in the archive)

Three execs each, identical script shape, same layer -- only the program varies:

    /bin/busybox           --version x3   av=0,  completed
    /usr/bin/seatd         --version x3   av=0,  completed
    /usr/bin/mate-session  --version x3   av=64, faults

`seatd` is dynamically linked and uses the same fallible primitives, yet
survives. So the fault scales with the BINARY, not with the operation: a purely
guest-agnostic unwind-metadata gap should be trippable by any dynamically-linked
binary. Something ELF-shape-dependent (size, relocation count, segment count,
TLS) is required to REACH the corrupting condition.

Useful next measurement, cheap and code-free: bisect the image's binaries by
size/relocation count between `seatd` (clean) and `mate-session` (faults) to
identify which property is actually required. That constrains any eventual fix
and is the question the archive never settled.

Earlier framings, both wrong and corrected here: "mate-session --help crashes"
(it returns rc=0 in isolation) and "a sequence of DIFFERENT large binaries is
required" (one binary three times is enough).


## Trigger-property bisection: THREE hypotheses tested, ALL REFUTED

Three execs each, identical script shape, same layer, only the program varying:

    binary            DT_NEEDED   size        LOAD  TLS  result
    marco                  4       18 KB       4     0   clean
    seatd                  1       43 KB       4     0   clean
    mate-mouse-props       -       43 KB       -     -   clean
    gst-play-1.0           9       51 KB       4     0   clean
    mate-font-viewer      13       59 KB       4     0   clean
    sudoreplay             -       81 KB       -     -   clean
    loadkeys               -      138 KB       -     -   clean
    find                   1      212 KB       4     0   clean
    mate-session          19      215 KB       4     0   **FAULTS**
    mate-panel            18      520 KB       4     0   clean
    caja                  22    1,658 KB       4     0   clean

REFUTED, each by a direct control:

1. **Binary size** -- `find` (212 KB) is clean; `mate-session` (215 KB) faults.
   `caja` at 1.6 MB is clean.
2. **Shared-library count (DT_NEEDED)** -- `caja` has 22 (more than
   mate-session's 19) and is clean; `mate-panel` at 18 is clean.
3. **Relocation count / ELF structure** -- all of these are ET_DYN with 4 LOAD
   segments, INTERP present, GNU_RELRO present, and ZERO TLS segments. Nothing
   static separates the faulting binary from the clean ones.

## What survives

    deterministic          3rd exec, every time
    count-independent      loop counts 3/4/5/6 all fault at exactly the 3rd
    shell-shape-agnostic   straight-line and loop identical
    not exec count         60 busybox execs clean
    host-side              all events is_in_guest=false, is_verifying=true

Because every static file property tested is ruled out, the trigger is more
likely something `mate-session` does AT RUNTIME (which libraries it actually
dlopens, what it touches during startup) than a property of the ELF on disk.
That is a different investigation shape from the archive's static/unwind-metadata
angle, and it is the honest open question -- not a mechanism anyone has yet
established.
