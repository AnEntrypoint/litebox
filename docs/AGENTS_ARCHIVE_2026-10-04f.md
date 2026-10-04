# moved out of AGENTS.md on 2026-10-04f (byte ceiling 29,900)

Mechanism prose trimmed from `AGENTS.md` this pass. `AGENTS.md` keeps the sha, the RULE and the
evidence; what follows is the how, kept so it can be recovered.

## `9997313` -- INET carry, the three silent faults (full text)

`Network::fork_carry_spec`/`fork_adopt`, spec `inet:<cloexec>|<v6>|<nonblock>|<spec>`.

(a) **Field-order mismatch.** The writer `FilesState::raw_fd_inet_carry` emitted
`<spec>|<v6>|<nonblock>` while the reader `install_inet_at_fd` parsed
`<cloexec>|<v6>|<nonblock>|<spec>`. `Network::fork_adopt` therefore matched no kind, returned
`None`, and the fd was simply absent in the child -> `EBADF`.

(b) **Arm order.** The generic `FD_CLOEXEC` arm sat ABOVE the INET arm in
`try_cross_process_fork`, so a close-on-exec INET fd was dropped. Python marks every socket
`SOCK_CLOEXEC` (PEP 446), so in practice no Python-held INET socket ever crossed a fork.

(c) **Half-restored UDP peer.** A carried connected UDP socket restored its network-side peer but
not its shim-side one, so `send()` failed `EDESTADDRREQ`.

Pre-`6c63d41` the `None =>` fall-through arm (`process.rs:4051`/`4084`) made ONE inet fd refuse
the WHOLE fork ("not eligible", `uncarriable=1 kinds=["socket"]`) -> relocating fallback ->
`Exception(14) error_code=0x7` before the child's first instruction.

## `a629714` (`e41cafa9`) -- shm carry, the three traps (full text)

Carry = reserve a placeholder in the child, `MapViewOfFile3` the parent's section into it
(`MEM_REPLACE_PLACEHOLDER`); no `DuplicateHandle` needed.

1. **Compute and validate every segment BEFORE reserving the group placeholder.** `f2d67f8`
   checked after, so a refusal left a live placeholder over the very span the byte-copy fallback
   re-reserved -> 487 -> `spawn/resume failed` -> thread-based fork -> `Exception(14) 0x7`. This is
   why chromium died while the synthetic probes passed: they never exercise a FAILED carry.
2. `VirtualFreeEx(MEM_RELEASE|MEM_PRESERVE_PLACEHOLDER)` over a range that already IS the whole
   placeholder returns **487** -- split every segment but the last.
3. **The alignment check is `PAGE_SIZE`, not the reservation granularity.** A shared range is
   always page-aligned because it is an mmap, so a `GRAN` check refuses a +0x7000 gap segment for
   nothing.

Degradation is measured, not assumed: `LITEBOX_DIAG_FORK_SHARED_FORCE_FAIL=1` fails the carry
after the group is placed, and shmfork1/2 then reproduce the pre-fix baseline exactly
(`parent_reads_child` FAIL both) with zero `falling back`.

## `6183f53` (`bfc33f7`) -- premise correction

chrD72 pids 74/114/154 died on `process_memory_range_by_regions`' `assert!`
(`platform/lib.rs:8366`) over `0x7feffeced000-0x7fefff4e3000`. That range is ~34 GB ABOVE the
arena base `0x7ff800000000` and chrD72 has ZERO carry-failure lines -- never an arena-region bug.
`NoReplace` -> AddressInUse, `Replace` -> OutOfMemory.
