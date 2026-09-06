# Xorg SIGSEGV as a forked child (write to non-present page)

## The bug
Xorg runs perfectly as **pid 1** but SIGSEGVs ~0.17s after exec whenever it is a
**forked child**, regardless of launch pattern. Distinct from ADVISORY 3N's
safe-linking bug (that is `error_code=0x4`, a READ, at `__libc_malloc+0x76`).

## Signature (two runs, different ASLR bases)
    run A: rip=0x6b10b98 cr2=0x6cd0008  mapping 0x6c93000..0x6d0e000
    run B: rip=0x7570b98 cr2=0x7730008  mapping 0x76f3000..0x776e000
    both:  Exception(14) error_code=0x6 kernel_mode=false
           flags = VM_READ | VM_EXEC | VM_MAYREAD | VM_MAYWRITE | VM_MAYEXEC

Byte-exact invariants across both runs:
    mapping size       0x7b000
    cr2 - range_start  0x3d008
    cr2 - rip          0x1bf470
    rip - range_start  -0x182468   (rip is in a DIFFERENT, EARLIER mapping)
    base delta         0xa60000    (applied uniformly to rip, cr2, mapping)

Deterministic: a fixed code site writing a fixed data offset. Not corruption,
not a race.

`error_code=0x6` is **write to a NOT-PRESENT page**, not `0x7` (write to
read-only). The VMA exists and is `VM_MAYWRITE`, but has no `VM_WRITE` and the
page is not committed. The parent writes here successfully (pid-1 Xorg runs
fine), so the child lost a permission/commit state the parent had.

`rip` living in an earlier object than `cr2` fits one object writing into
another's GOT/`.data.rel.ro` -- the classic RELRO `mprotect(W)` -> write ->
`mprotect(R)` pattern, where a child inheriting the wrong state faults.

## Repro (~2.5 min, one boot)
    LITEBOX_LOG=error litebox_runner --unstable \
      --oci-image linuxserver/webtop:debian-xfce --gui-hidden \
      --resume-from x5.tar -- /bin/sh /r5.sh
where `x5.tar` contains `r5.sh`. Xorg dies ~71.7s in; grep `diag-guest-exception`.
Captured output: `X5-fork-segv.log`.

## Control that MUST keep passing
    litebox_runner --unstable --oci-image linuxserver/webtop:debian-xfce \
      --gui-hidden --resume-from x3.tar -- /usr/bin/bash /srv.sh
`srv.sh` ends in `exec Xorg`, so Xorg is pid 1. Runs clean for minutes: full
wgpu + DRM framebuffer + frame dumps, zero crashes. Captured output:
`x1-pid1-control.log`.

## Variables already eliminated
- **`&` vs `exec`**: NOT the discriminator. `sh -c "exec Xorg ..."` (the pattern
  that keeps xsetroot/xkbcomp alive) still faults identically.
- **General fork breakage**: NOT the cause. In the same failing run every other
  fork+exec child survived (`sleep`, `xsetroot` x2, `/bin/sh`), script reached
  `R5_DONE`, zero safe-linking aborts.

## Suggested next measurement
At fork time, dump the parent's VMA flags AND host page protection for the
mapping containing this offset, then the child's immediately after
`duplicate()`. If the parent has `VM_WRITE` and the child does not, that is the
bug, located in one step.

## UNTESTED IDEA: get a client under a pid-1 Xorg (`srv7`/`wrap`)

The fork bug above blocks Xorg *as a child*. But a **pid-1 Xorg** demonstrably
forks children that SURVIVE: it runs `/bin/sh -c "xkbcomp ..."` and both the
shell (pid 2) and xkbcomp (pid 3) exit `status=0` in the control log.

So a pid-1 Xorg can host a surviving client. `srv7-pid1-with-client.sh` +
`xkbcomp-paint-wrap.sh` exploit that: move the real binary to `xkbcomp.real`,
drop in a wrapper that runs it and then also runs
`xsetroot -solid navy` on `:0`. Xorg stays pid 1 (never forked, never faults),
and the painting client rides in on the one fork path already proven to work.

If it paints, the server's frame goes from `non_black_pixels=0` (see
`baseline_xorg_pid1_black.bmp`) to a solid navy fill.

NOT YET RUN -- the host was handed to the fork investigating the SIGSEGV before
this could execute. Worth trying; it needs no fix to any litebox code.

### RESULT of the pid-1-with-client route: FAILED (and the timing theory RETRACTED)

Ran twice. The spawn mechanism WORKS -- a real client (`xsetroot`) executes and
runs as a descendant of a pid-1 Xorg via Xorg's own `xkbcomp` spawn path, and
reaches an X connection attempt. That part is proven and is independent of the
layout bug below.

But it never painted. Both runs died identically:

    exception=Exception(14) rip=0x669ff36 cr2=0x8 error_code=0x4
    NO mapping overlaps cr2 (genuinely unmapped)

`cr2=0x8` is a NULL dereference -- `XOpenDisplay()` returned NULL and `xsetroot`
dereferenced it unchecked.

**A retry loop does not help, and the first attempt to add one was a bad probe.**
`xsetroot` does not return non-zero on a failed connection, it SEGFAULTS, which
killed the wrapper shell (`pid 7`, `exit_signal`) before the loop could iterate.
Only 1 of 40 attempts ever ran.

**RETRACTED: the "client fired too early, before Xorg's socket was up" timing
hypothesis.** The `rip` is byte-identical (`0x669ff36`) across two runs with
completely different timing -- one firing during keyboard init, one after the
real `xkbcomp` had finished. A race would not reproduce to the byte. This is a
deterministic layout problem, and it is explained by the separate finding that a
forked child's address space is packed into a ~120MB low window (a high-address
`VM_OWN_FORK_PADDING` placeholder defeats the high-region placement fast path),
so libraries land single-digit-KB apart and glibc's `sysmalloc` heap growth
writes a chunk header into an adjacent library's text. In such a process,
libX11's setup returning NULL is an ordinary downstream symptom, not evidence
about socket readiness.

Do not build a workaround on the timing theory.
