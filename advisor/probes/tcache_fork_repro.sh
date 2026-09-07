#!/bin/bash
# Step-0 decisive experiment for ADVISORY-002 (Track B, D==0 fork).
#
# Minimal, beyond_stdio==0 (stdio-only fd table -- no extra open files, sockets, or pipes)
# glibc fork-WITHOUT-exec repro that exercises tcache's safe-linked freelist, matching the
# exact fault signature symbolized in ADVISORY-001 section 3N: __libc_malloc+0x76, the
# REVEAL_PTR of glibc's inlined tcache_get, error_code=0x4 (read of a not-present/garbage
# page) hit only in the child, never the parent.
#
# Mechanism: `bash -c '( ... )'` runs a POSIX subshell. Bash's subshell implementation calls
# fork() and runs the grouped commands directly in the child WITHOUT ever calling execve() --
# the child _exit()s at the end of the group. This is a completely stock, unmodified glibc
# bash binary already present in any debian-based image; no guest compiler is needed (this
# repo's guest gcc/cc1 are broken -- see advisor/probes/README.md), and the fd table stays
# stdio-only throughout (no pipes/sockets opened), so it hits the beyond_stdio==0 gate exactly.
#
# Inside the subshell, heavy shell-variable/array activity (assignment, expansion, `seq`,
# arithmetic) drives glibc malloc/free through many same-size-class allocations, populating
# and then draining the tcache freelist for that size class -- exactly the pattern that pops a
# safe-linked `next` pointer computed at the PARENT's address, in the CHILD's relocated memory.
#
# Usage:
#   default (broken) fork path:
#     litebox_runner --unstable --oci-image linuxserver/webtop:debian-xfce \
#       --initial-files tcache_fork_repro.tar -- /bin/bash /tcache_fork_repro.sh
#   D==0 cross-process fork path:
#     LITEBOX_PROCESS_FORK=1 litebox_runner ...  (same args)
#
# On success (no corruption) this prints TCACHE_REPRO_CHILD_OK and TCACHE_REPRO_DONE and exits
# 0. A tcache-corruption crash kills the child with SIGSEGV/SIGABRT before either line prints;
# the parent's `wait`/exit status reflects that, and litebox's own diag-guest-exception logging
# (if enabled via LITEBOX_LOG=error or similar) should show the fault at __libc_malloc+0x76.

set -u
echo "TCACHE_REPRO_START pid=$$"

(
    # Child: no exec anywhere in this subshell. Populate several tcache size classes.
    declare -a bufs
    for round in 1 2 3 4 5 6 7 8 9 10; do
        # Allocate a burst of same-shape strings (same glibc chunk size class across a burst),
        # forcing multiple chunks into that class's tcache freelist.
        for i in $(seq 1 40); do
            bufs[i]="padpadpadpadpadpad_${round}_${i}"
        done
        # Now drop them: freeing populates the tcache freelist (up to 7 per class by default).
        unset bufs
        declare -a bufs
        # Immediately allocate again: this is the tcache POP that dereferences the safe-linked
        # `next` pointer at __libc_malloc+0x76 -- the exact fault site from ADVISORY-001 3N.
        for i in $(seq 1 40); do
            bufs[i]="repadrepadrepadrepad_${round}_${i}"
        done
    done
    echo "TCACHE_REPRO_CHILD_OK pid=$$"
)
child_status=$?
echo "TCACHE_REPRO_SUBSHELL_STATUS=${child_status}"

echo "TCACHE_REPRO_DONE"
exit 0
