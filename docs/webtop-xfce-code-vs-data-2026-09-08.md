# 2026-09-08: the XFCE desktop was black because litebox rewrote data as if it were code

The desktop now assembles. `xfwm4` owns `WM_S0`, 21 windows exist, and `xfce4-session` starts
`xfwm4`, `xfsettingsd`, `xfdesktop`, `xfce4-panel` and `xfconfd`. None of that happened before this
pass, and the reason it did not is a single defect with a long tail.

Companion docs: `fork-fs-veh-2026-09-08.md` (cross-process `fork()`, the VEH work),
`webtop-debian-xfce-2026-09-08.md` (the image and the earlier investigation).

## The defect: a `PROT_EXEC` mapping is not a code segment

The runtime rewriter patched **the whole mapping** whenever `PROT_EXEC` was set. An ELF's `PF_X`
`PT_LOAD` is not all code. `libLLVM.so.19.1`'s first `PT_LOAD` is `RX` and 118 MB long:

| section | file offset | size |
|---|---|---|
| `.dynsym` | `0x260` | 1.2 MB |
| `.dynstr` | `0x1340f0` | 3.8 MB |
| `.gnu.hash` | `0x4d2598` | 393 KB |
| `.gnu.version` | `0x535204` | 105 KB |
| `.gnu.version_d` / `.gnu.version_r` | `0x54ec90` | 968 B |
| `.text` | `0xcf6740` | 56 MB |
| `.rodata` | `0x4574080` | 42 MB |

Linear disassembly does not know where code stops. It decoded the symbol tables as instructions,
found byte pairs that read as `syscall`, and wrote a 5-byte `JMP` over them.

Measured from inside a guest, mapping that segment `PROT_READ|PROT_EXEC` and comparing against the
file: **1984 bytes differed**, and the sampled differences were all outside `.text` -- at file
offsets `0x5dc4` and `0xa0c1c`, both inside `.dynsym`.

## What that did, step by step

1. `ld.so` could not resolve `_ZTSN4llvm11logicalview22LVScopeFunctionInlinedE, version LLVM_19.1`.
   The file defines it perfectly well: `.dynsym[1296]`, `shndx=14`, `GLOBAL`.
2. So `libLLVM` failed to load, and with it `libgallium` and `libGLX_mesa` -- confirmed directly:
   all three `dlopen`s returned that same error, and all three now succeed.
3. glvnd retried the vendor load on every GLX call. `xfwm4` re-loaded
   `libgallium` -> `libLLVM` -> `libz3` about three times a second, for ever.
4. It therefore never created a single X window. The desktop was black, and every earlier
   explanation of that blackness was a description of this.

`xfwm4`, same image, same workload:

| | large mappings | time inside `mmap` | patching | windows |
|---|---|---|---|---|
| before | 3387 | 233.7 s | 225.9 s | 0, ever |
| after | 5 | -- | -- | `WM_S0 owner=0x600014` |

The ahead-of-time `patch_binary` path never had this bug: it patches `text_sections` only. The fix
is to give the runtime path the same notion of code, from the same source of truth -- the ELF's own
section headers, read once per file and cached per `(device, inode)`.

## Two rewriter fixes found on the way

**The decode ran twice and was fully materialized.** `patch_code_segment` decoded the segment to
collect control-transfer targets, then `hook_syscalls_in_section` decoded the same bytes again,
with the first `Vec` still alive. An `iced_x86::Instruction` is 40 bytes against roughly 4 bytes of
x86 per instruction, so each `Vec` is about 10x the size of the code it describes: ~2.6 GB of
transient allocation for one library, inside a guest `mmap`, in a host process shared by every
guest. The repeated host OOM kills were that.

Nothing needed it. The hooker asks the decode two questions -- "is this address a control-transfer
target" and "what are the few instructions either side of this `syscall`" -- and both scans
provably terminate within 3 instructions, because they stop once the replacement range reaches the
5 bytes a `JMP rel32` needs. One streaming pass with an 8-instruction ring buffer answers both.

**The scan was redone per mapping.** It is a pure function of the bytes, so it is now kept in
segment-relative offsets (`SegmentScanTemplate`) and cached per `(device, inode, offset, length)`.
Rebasing is sound because an `iced_x86::Instruction` derives every address it reports from its own
`ip` plus decoded displacement bytes, which are identical wherever the segment loads.

Whole-workload effect, 2316 -> 1663 patch calls: **180.9 s -> 6.0 s**, worst single call
26.4 s -> 2.7 s. Equivalence was not assumed: patching 17 real binaries from the image (libLLVM,
libc, Xvfb, xfwm4, xfce4-session, dbus-daemon, every mesa DRI driver) gives byte-identical output
and identical trapped-site lists in all 17 cases.

## `/dev/null` was being destroyed, and that is what broke D-Bus

Three defects in the layered filesystem, compounding:

1. **`O_TRUNC` was applied to non-regular files.** On Linux `do_open()` gates `handle_truncate()`
   on `S_ISREG`, so `O_TRUNC` on a character device does nothing. Here a shell's `> /dev/null`
   reached `truncate`, which could not truncate a device and fell through to `migrate_file_up` --
   which copies a file's BYTES into a newly created upper-layer file. `/dev/null` reads as instant
   EOF, so what appeared on the writable layer was an empty **regular file** shadowing the
   character device.
2. **`migrate_file_up` was type-blind**, so nothing stopped it from changing what an object is.
3. **Unlinking a lower-layer character device was `unimplemented!()`**, which panics the HOST
   process and kills every guest in the address space. Reproduced directly with `rm -f /dev/null`.

The chain: once `/dev/null` looked like a regular file, GNU `ld` -- run by selkies at startup, with
`/dev/null` as its output -- applied libiberty's `unlink_if_ordinary()`, which unlinks only what
`lstat` calls a regular file or symlink, precisely so it can never delete a device node. Told it was
ordinary, it deleted it. The tombstone then made every `open("/dev/null")` **without `O_CREAT`**
return `ENOENT`.

That is why it looked intermittent and was not: shell redirections carry `O_CREAT` and kept
working, while `dash` opening `/dev/null` for a background job's stdin does not. So
`dbus-daemon ... &` never started at all, `DBUS_SESSION_BUS_ADDRESS` was empty, and `xfce4-session`
came up with no session bus and started nothing. The stack reports `DBUS_UP` where it reported
`DBUS_FAILED`.

## Also corrected

**`--publish` binds every port.** An earlier note in this series claimed only the first was bound.
It is wrong: `publish.join(",")` and the parser both handle the whole list, and a live run logs
`published 127.0.0.1:3000` and `published 127.0.0.1:8082`. No fix was needed and none was made.

**A `| sed` pipe kills the desktop.** The launcher ran the session through `sed` for log prefixing.
`xfwm4` died of `SIGPIPE` (signal 13, confirmed from the syscall timeline), and because `$!` after a
pipeline is the pid of `sed`, the script's own liveness check reported `DE_ALIVE` for the wrong
process entirely. Output now goes to a file.

## Still open, located precisely

**The OCI `/init` path cannot boot s6-overlay: static `ET_EXEC` binaries collide.**
`--oci-image docker.io/linuxserver/webtop:debian-xfce` pulls, rewrites and runs (Debian 13 trixie,
17 layers, all binaries present), and `/init` reaches `s6-overlay-suexec` -> `preinit` ->
`s6-mkdir`, where `execve` fails with `LoadError(Map(EEXIST))`. `s6-mkdir` is a static non-PIE
linked at `0x400000`, and `s6-overlay-suexec` -- still alive -- already occupies that address in
litebox's single host address space.

Cross-process fork would give the child its own address space and clear this, but it is declined:
`clone: cross-process fork() skipped for a vfork child`. Both forks here are `vfork`
(`CloneFlags(16640)`, `CloneFlags(16384)`), and a vfork child shares the parent's address space by
design, so there is nothing to transfer.

The way through is that **`vfork` shares an address space only until `execve`**. On Linux the child
gets a fresh one at exec; that is the contract vfork is written against. So the moment to hand a
guest child a real Windows process is `execve`, not `clone` -- and exec is the easy case, because
exec discards the image anyway, so nothing has to be duplicated. This is a spawn, not a fork.

**A host access violation at ~100 s** during full desktop startup, amid constant
`fork_verify: stale CODE pointer` healing. Two shapes seen: `rip=0x0, addr=0x0,
is_in_guest=false`, and a read of `rcx` pointing into `MEM_RESERVE`/`PAGE_NOACCESS` guest memory
(`State=0x2000, Protect=0x0`) -- an unchecked host read of guest memory. Same family as the
corrupted-context resume recorded in `fork-fs-veh-2026-09-08.md`.

**`nginx` returns `HTTP_LOCAL_FAIL`**, so the dashboard is not served yet and the desktop has not
been seen in a browser. Not yet diagnosed.

## Update: the desktop runs; the last mile is an orphaned accepted socket

Since the above, the desktop assembles reliably and stays up: `xfwm4` owns `WM_S0`, 14 windows,
`DE_ALIVE` through T=90s, with `xfsettingsd`, `xfdesktop`, `xfce4-panel`, `xfconfd`,
`dconf-service` and `at-spi2-registryd` all running. `selkies` starts, reports
`Data WebSocket Server listening on port 8082`, and the host's `--publish` listener binds. The
dashboard is served and loads in a browser. It never paints, and the reason is now exact.

### `--publish` delivers inbound bytes and loses every reply

A one-shot HTTP server in the guest, with nothing else running, on a `--publish`ed port:

```
GUEST_ACCEPT ('10.0.0.1', 49152)
GUEST_RECV   82  b'GET /hello HTTP/1.1'
GUEST_SENT   76
```

and the host's `curl` gets `HTTP 000`, zero bytes. The request arrives; the reply never does.

Packet-level, everything the guest ever transmits on that connection is:

```
src=10.0.0.2 dst=10.0.0.1 sport=8082 dport=49152 flags=SA payload=0
src=10.0.0.2 dst=10.0.0.1 sport=8082 dport=49152 flags=AF payload=0
src=10.0.0.2 dst=10.0.0.1 sport=8082 dport=49152 flags=A  payload=0
```

Handshake, FIN, ACK. **The 76-byte payload is never put on the wire at all**, which is why the
gateway's own socket sits in `CloseWait` with `recv_q=0` for thousands of poll cycles and then
closes.

### Why: the accepted socket is orphaned one cycle after `accept()`

The guest's socket writes go into a per-socket TX ring (`proxy.try_write`), and
`Net::drain_all_socket_channel_buffers` is what moves that ring into the smoltcp socket. Logging
every socket it visits, per cycle:

```
h=SocketHandle(0) listening=true  state=Closed      ...   x3807
h=SocketHandle(1) listening=false state=Established recv_q=82   x1
```

The accepted socket is visited **exactly once** -- the cycle that delivers the request into its RX
ring -- and never again. Counting the entries the drain iterates confirms it: `entries=2` for one
cycle, `entries=1` before and after. The accepted socket's `Network` descriptor-table entry is
removed almost immediately, while the guest's fd stays perfectly usable at the shim layer.

That is why every observable at the ends looks healthy and the middle is empty: `accept()` returns
a working fd, `recv()` returns 82 bytes (already in the ring), `sendall()` returns 76 (accepted
into the ring) -- and nothing ever drains that ring into smoltcp, because the socket the drain
iterates over is gone. `close()` still reaches the smoltcp socket directly, so the FIN goes out
while the data does not.

The fd handoff itself looks correct on inspection -- `Net::accept` inserts the handle
(`descriptor_table_mut().insert`), `initialize_socket` attaches the proxy, and
`insert_raw_fd` stores the owning `TypedFd` in the raw descriptor store rather than dropping it --
so what removes the entry is not yet identified. It is not the smoltcp socket being destroyed:
that socket is still alive later, since it transmits the FIN.

**This is the whole remaining distance to a visible desktop.** Everything upstream of it works.

## Update 2: `--publish` fixed, and two of my own measurements corrected

**`close(2)` was discarding queued data.** `CloseBehavior::Graceful` -- which is what an ordinary
close with `SO_LINGER` unset maps to -- closed the socket immediately without checking
`has_pending_tx()`, so `write(fd, response); close(fd);` lost the response whenever the periodic
drain had not run in between. Only `GracefulIfNoPendingData` checked. Linux does the opposite: the
kernel flushes queued data and sends FIN afterwards.

That is what "the guest never transmits the payload" above actually was. Host round-trips through a
`--publish`ed port now return HTTP 200 with the body, 5/5 and 3/3 on clean builds with logging off,
where every attempt before returned HTTP 000.

Two things recorded above were wrong, and the corrections matter more than the claims:

* **The accepted socket is not orphaned.** The "visited exactly once / `entries=2` for one cycle"
  reading came from a run whose own instrumentation logged thousands of lines a second and changed
  the timing it was measuring. With the close fix in and the probes out, accepted sockets stay in
  the table and long-lived connections work.
* **`nginx` was never failing.** `HTTP_LOCAL_FAIL` came from the launcher's own check running
  `wget`, which this image does not ship (`/bin/sh: wget: not found`). Asked with `python3`
  instead, in-guest nginx answers `200` with the 762-byte dashboard, and one guest process fetching
  another over loopback works.

Also confirmed wrong: an earlier reading here of "101 Switching Protocols on every path" from
selkies was a leftover test server of mine still holding port 8082, not selkies.

## Still open

**selkies does not answer the WebSocket handshake.** Its log says
`Data WebSocket Server listening on port 8082`, the host reaches it through `--publish`, and the
connection is accepted -- but no handshake response is ever sent, so the dashboard loads and sits
on `WebSocket disconnected. Attempting to reconnect...`. A plain HTTP server put on the same port
in the same image answers fine through the same path, so this is selkies-side, not transport.

**An intermittent host access violation** still ends runs at varying points, now in a different
shape from the one fixed in `8aa05af`: `rip == fault address == 0x7ff003444000`, an instruction
fetch in the host-allocator region rather than a context caught on the way into the guest. The
`switch_to_guest` fix converts one corruption path into a guest SIGSEGV; this is another.

The desktop itself is in good shape -- `WM_S0` owned, 14-21 windows, `DE_ALIVE` past T=255s where
it used to die at ~100s -- and has still never been seen rendered in a browser.

## Update 3: the image's own web UI now reaches the browser

`--publish` carrying a real HTTP response (the `close(2)` flush fix above) plus `AF_INET6` existing
means the guest's own nginx serves the webtop dashboard to the host:

```
UI HTTP 200 bytes=762      # host -> --publish -> in-guest nginx
```

**`AF_INET6` was the reason nginx would not start.** Stock configs `listen [::]:80`, and
`socket(AF_INET6)` returning `EAFNOSUPPORT` made nginx log `[emerg]` and exit. v6 sockets now use
the v4 machinery (Linux's `bindv6only=0` behaviour), v4-meaningful v6 addresses map (`::`, `::1`,
`::ffff:a.b.c.d`), and a v6 `bind()` succeeds while listening on nothing -- because no IPv6 packet
can arrive here, and mapping it onto the v4 wildcard instead made `0.0.0.0:80` and `[::]:80`
collide with `EADDRINUSE`, which killed nginx just as dead.

Where the image's own service scripts are needed, they are read and followed rather than guessed:
`svc-selkies/run` is `selkies --addr="localhost" --mode="websockets"`, `svc-nginx/run` is
`nginx -g 'daemon off;'`. The 3000-port site config is generated at runtime by `init-nginx`, which
is part of `/init` -- still blocked by the `ET_EXEC` collision -- so that one config file is
supplied directly, the way a container bind-mount would.

## Still open

**selkies never opens its data WebSocket.** With `--addr=localhost` it stops after its gamepad
interposers (`EVDEV interposer server listening on /tmp/selkies_event1003.sock`) and prints no
error; nginx's proxy to it therefore answers `502 Bad Gateway`. The desktop itself is fine
underneath -- `WM_S0` owned, 17-21 windows -- so the last missing piece is the pixel stream.

**A script run from a `--resume-from` path hangs the shell.** `sh /stack.sh` produces no output at
all, not even under `sh -x`; `cp /stack.sh /tmp/s.sh && sh /tmp/s.sh` runs the identical 2601 bytes
correctly (`XVFB_UP`, `SELKIES_LAUNCHED`, `DBUS_UP`). `sh -n` accepts the file and `wc -c` agrees on
both paths, so this is the layered filesystem stalling a read of an imported file that is being
executed as a script, not a damaged file.

**The intermittent host access violation** remains, and now ends some runs during Xvfb startup
rather than at ~100s.

## Update 4: selkies found and reachable-in-principle; the stream is still not rendered

**selkies was never listening where anything was looking.** Run with `--debug`, it reports
`SelkiesStreamingApp initialized: encoder=x264enc`, `All main components initialized. Running
server...`, and then:

```
INFO:data_websocket:Data WebSocket Server listening on port 8081
```

**Port 8081** -- not the 8082 that the launcher's `CUSTOM_WS_PORT`, the nginx `proxy_pass` and the
host proxy were all aimed at. Every `502 Bad Gateway` and every `WebSocket disconnected` in this
series was pointed at a port nothing was on. That single wrong number is why earlier readings here
described selkies as "not answering the handshake": it was answering nowhere near where it was
asked.

The transport underneath is fine, and each layer was checked separately rather than assumed:

* `--publish` round-trips to a server in **pid 1**: HTTP 200.
* `--publish` round-trips to a server in a **forked child**: HTTP 200 (this refutes a per-process
  network-namespace theory recorded during the investigation).
* An **asyncio** server -- the shape selkies actually uses -- through `--publish`: HTTP 200,
  `AIO_ACCEPTED` / `AIO_RECV 78` / `AIO_SENT`.
* The image's own nginx serving its own dashboard to the host: `UI HTTP 200`, 762 bytes.

With 8081 published and the host proxy corrected to it, selkies still returns nothing to a
handshake, which points at its data WebSocket binding `127.0.0.1` rather than the wildcard --
i.e. the image's intended path, where in-guest nginx reaches it over loopback.

**A correction.** An earlier conclusion in this session that "a guest process connecting to another
over 127.0.0.1 segfaults" is not established. The run it came from was degraded across the board --
in the same run `cat` died with `libc.so.6: failed to map segment from shared object` -- so those
segfaults are address-space/mapping exhaustion under several concurrent processes, not evidence
about loopback. Whether cross-process loopback works is currently unknown, and needs testing in a
run that is not already failing to map libc.

**"Failed to map segment" under load is itself a finding**: with the desktop, selkies and a couple
of python processes live, the shared address space stops being able to map an ordinary library.
That is the same single-address-space pressure that the `ET_EXEC` collision comes from, and it is
the most likely explanation for the intermittent host AV that ends runs at varying points.
