# `DIAG_TIMELINE` field semantics

Read from source, not inferred from logs. Written because two separate investigations
(2026-09-06) built hypotheses on a misreading of `comm`, and one of them cost a full
night's search direction.

## Where `comm` is written

Exactly two call sites for `set_task_comm`:

- `litebox_shim_linux/src/syscalls/process.rs:1041` -- `prctl(PR_SET_NAME)`, guest renaming itself.
- `litebox_shim_linux/src/syscalls/process.rs:4722` -- inside `sys_execve`, **after**
  `loader.load(argv, envp, ...)` has already returned successfully.

There is no other writer. `comm` therefore lags an `execve` *attempt* and only catches up
once the load has genuinely succeeded.

## What each event carries

| event | emitted at | `comm` at that instant | what it proves |
|---|---|---|---|
| `clone` | `process.rs:3310` | the **parent's** comm | a fork happened; says nothing about the child |
| `execve` | `process.rs:4545` | the **old, pre-exec** comm, plus `argv0` = the new path | an exec was *attempted* with the path resolved |
| `exit_group` | `process.rs:1914` | current comm | post-exec name **iff** the exec completed |
| `exit_signal` | `signal/mod.rs` | current comm | ditto |

## The two traps

**1. An `execve` entry does not mean the exec succeeded.** It is deliberately logged
*before* `load_program`, so that a process dying mid-exec still leaves a record of what it
was trying to become (see that site's own comment). Counting `execve` lines counts
*intents*, not successes.

**2. `comm` on an `execve` line is the OLD name.** A shell forking and exec'ing `Xorg`
logs `comm=bash argv0=/usr/bin/Xorg`. Reading that `comm` as "which program this is"
is wrong; `argv0` is the answer, and only on a *later* event does `comm` reflect it.

## The discriminator these enable

For any pid that died, compare its terminal event's `comm` against its own `execve`
event's `argv0`:

- `comm` == the new program -> it exec'd successfully and died **afterwards**
  (a bug in the running program, or in what litebox provides it).
- `comm` == the old name, while an `execve` entry with a different `argv0` exists
  -> it died **inside** `load_program`, genuinely mid-exec
  (a loader / address-space-relocation bug).

These are different investigations. Conflating them is what happened on 2026-09-06.
