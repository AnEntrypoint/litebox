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

## Interpretation

Whatever accumulates scales with ELF size/relocation count, not exec count:
60 small execs survive, 3 large ones do not. Consistent with a leak or
off-by-one in `fork_verify`'s per-exec relocation-range bookkeeping --
`mate-session` carries far more relocation ranges than busybox.

Earlier framings, both wrong and corrected here: "mate-session --help crashes"
(it returns rc=0 in isolation) and "a sequence of DIFFERENT large binaries is
required" (one binary three times is enough).
