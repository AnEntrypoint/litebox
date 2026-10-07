# Disk hygiene for litebox test runs

A session ran the machine out of disk: one scratchpad reached **115 GB** and left
1.5 GB free on C:, which crashed another session's tracing writer mid-run. The
cause was not one large file. It was ~40 experiments each copying a 2.4-3.5 GB
layer tar, plus logs of several hundred megabytes each.

## The rule

**Never copy the layer tar per experiment.** One base tar plus a small script is
all a variant needs.

```sh
# WRONG -- 3 GB per experiment, and they accumulate silently
cp base.tar exp1.tar && tar rf exp1.tar exp1.sh
cp base.tar exp2.tar && tar rf exp2.tar exp2.sh

# RIGHT -- one tar, append the scripts you need, invoke a different one per run
cp base.tar work.tar
tar rf work.tar exp1.sh exp2.sh exp3.sh
runner --initial-files work.tar bin/sh ./exp1.sh
runner --initial-files work.tar bin/sh ./exp2.sh
```

Appending a script to an existing tar costs kilobytes. Copying the tar costs
gigabytes. Forty experiments is the difference between 2.4 GB and 120 GB.

## Check before starting anything large

```sh
df -h /c | tail -1        # free space on C:
du -sh "$SCRATCHPAD"      # this session's footprint
```

Run these before a batch of runs, not after something fails. A full disk does
not announce itself clearly: it surfaces as `No space left on device` from a
`cp`, as a tracing writer crash, or as a truncated log that looks like a
different bug entirely.

## Logs

A full-stack run with `LITEBOX_LOG=error` and the DRM diagnostics on produces
tens of megabytes; one reached 67 MB. Delete logs once their findings are
recorded, and keep only those still being cited. `LITEBOX_DIAG_MM` gating exists
because always-on memory diagnostics are heavy enough to distort timing as well
as fill disk.

## Frame dumps

`LITEBOX_DUMP_FRAMES=1` writes one 8.3 MB BMP per page flip into the working
directory. A run producing 30 frames leaves 250 MB behind. Clear them between
runs with `rm -f litebox_frame_dump_*.bmp`, which also makes "newest file" the
frame you actually want rather than one from a previous run.

## What not to delete

- `C:\dev\litebox-main\target` (21 GB) is regenerable but costs everyone a
  multi-minute rebuild, and another session may be building against it. Ask
  first; it is rarely the right thing to reclaim.
- Anything under another session's scratchpad, or any tar under `.wfgy`, without
  checking. Those are someone's active inputs.

Cleaning your own scratchpad is always safe and is usually enough: 115 GB to
3.5 GB in this instance, which took C: from 1.5 GB free to 127 GB.
