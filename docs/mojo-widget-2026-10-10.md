# "Message 2 rejected by interface blink.mojom.Widget" — what it is, and whether it causes "no page ever loads"

Date: 2026-10-10. Binary: `/usr/lib/chromium/chromium`, **`Chromium 154.0.8037.92`** (stripped).
Log under investigation: `/tmp/lbx/log-z0.txt` (65 lines, 8984 B, `TAG=z0 rc=124 DOM=0`).

---

## 1. What the message means

### The log site
`mojo/public/cpp/bindings/lib/interface_endpoint_client.cc` (fetched at tag `154.0.8037.92`; the
`LOG` is at line 652 on `main`, 748 on the 154 branch — matching the `:748` in our log):

```cpp
bool InterfaceEndpointClient::HandleIncomingMessage(Message* message) {
  const char* interface_name = interface_name_;
  uint32_t name = message->name();
  if (!dispatcher_.Accept(message)) {
    LOG(ERROR) << "Message " << name << " rejected by interface " << interface_name;
    return false;
  }
  return true;
}
```

* `name` = `message->name()` = **the mojom method ordinal**, not a sequence number.
* The message is **inbound at this endpoint** and the endpoint's dispatcher refused it.
* This is *not* the "endpoint already closed" path (that is `NotifyError`, a different log).

### Method 2 on `blink.mojom.Widget`
`interface Widget` is **not** in `page/widget.mojom` — it is in
`third_party/blink/public/mojom/widget/platform_widget.mojom` (line 100 at tag `154.0.8037.92`),
comment `// Implemented in Blink`. Declaration order:

| ordinal | method |
|---|---|
| 0 | `ForceRedraw() => ()` |
| 1 | `UpdateVisualProperties(VisualProperties)` |
| **2** | **`UpdateScreenRects(gfx.mojom.Rect widget_screen_rect, gfx.mojom.Rect window_screen_rect) => ()`** |
| 3 | `WasHidden()` |
| 4 | `WasShown(bool, RecordContentToVisibleTimeRequest?)` |
| 5 | `RequestSuccessfulPresentationTimeForNextFrame(...)` |
| 6 | `CancelSuccessfulPresentationTimeRequest()` |
| 7 | `SetupBrowserRenderInputRouterConnections(pending_receiver<RenderInputRouterClient>)` |

Ordinals here are sequential, not hashed — proven inside our own corpus: another rejection in these
logs is `Message 1473036071 rejected by interface blink.mojom.FrameWidgetHost`, i.e. Chromium does emit
hash-ordinals for some interfaces, so "2" is a genuine sequential ordinal, not a truncated hash.

### Which side, and what was rejected
`content/browser/renderer_host/render_widget_host_impl.h` (tag 154):

```
1652:  mojo::AssociatedReceiver<blink::mojom::WidgetHost> ...
1654:  mojo::AssociatedRemote<blink::mojom::Widget> blink_widget_;
```

So the **browser is the client (Remote) side** of `Widget`. The log came from pid 1714187 (browser —
it is the same pid that logs `chrome/browser/signin/...`, `chrome/browser/net/...`, disk cache) on its
**main thread** (`1714187:1714187`). Therefore the rejected message is an **inbound response to
`Widget::UpdateScreenRects`**, refused by the generated `WidgetResponseValidator`
(`ValidateResponseGeneric` over `kWidgetValidationInfo`), i.e. the response failed
`ValidateMessageIsResponse` and/or `Widget_UpdateScreenRects_ResponseParams_Data::Validate`.

### The consequence — this is not a benign "drop the message"
`mojo/public/cpp/bindings/lib/multiplex_router.cc` (tag 154), `ProcessIncomingMessage`:

```cpp
    result = !tmp_message.IsNull() && client->HandleIncomingMessage(&tmp_message);
  }
  if (!result) {
    RaiseErrorInNonTestingMode();      // -> RaiseError() -> connector_.RaiseError()
  }
```

`Connector::RaiseError()` → `HandleError(/*force_pipe_reset=*/true, /*force_async_handler=*/true)` →

```cpp
  if (force_pipe_reset) { CancelWait(); message_pipe_.reset(); MessagePipe dummy_pipe; ... }
```

**The entire Mojo message pipe is reset.** Because `blink.mojom.Widget` is an *associated* interface
multiplexed over the browser↔renderer legacy IPC channel, one rejected message tears down **the whole
IPC channel to that renderer**, not just one interface. By construction this is fatal for that
renderer's page. (Note: `HandleError(true,true)` takes the `force_async_handler` branch, which calls
`WaitToReadMore()` on the dummy pipe and does *not* run the connection-error handler — so the teardown
can be silent, which fits "the browser goes silent".)

---

## 2. Context in log-z0 (±10 lines) and what happened afterwards

```
53  Session Storage /proc/self/fdinfo ... errno=2
54  /proc/self/fdinfo/proc/79/statm  errno=2
55  /proc/self/fdinfo/proc/79/status errno=2
56  [1714187:1714187:1010/143322.441616] Message 2 rejected by interface blink.mojom.Widget   <-- HERE
57  /sys/bus/cpu/devices/cpu0/microcode/version errno=2
58  /proc/self/fdinfo/proc/69/statm  errno=2
59  /proc/self/fdinfo/proc/69/status errno=2
60  [1714187:68:1010/143324.106505] net/disk_cache/blockfile/backend_impl.cc:1988 Invalid file magic
61  /proc/self/fdinfo/proc/self/maps errno=2   (x2)
64  /proc/98/task/100/status errno=2           <-- last line of the whole log
```

* Browser main thread, t≈26 s into the run (log's first chromium line is 14:32:56).
* Nothing crashes; the browser keeps running ≥1.7 s afterwards (disk-cache line at 14:33:24.106).
* **No second rejection in the run** — it is a single event, not a repeating failure.
* Whether the same thread later sends more Widget messages cannot be read from this log
  (default verbosity emits nothing for successful Mojo traffic). At this verbosity the *only*
  observable is the rejection itself.

Neighbouring litebox-side lines in other runs at the same instant are suggestive of transport
trouble rather than of Chromium logic:
* `log-vton.txt`: right after the same `Widget` 2 rejection →
  `unsupported feature=sys_kill with a pid that is neither registered nor a direct child` plus
  repeated `/proc/83/stat errno=2`.
* `log-ab-off.txt`: **two lines before** its `FrameWidgetHost` rejection →
  `syscalls::net: recvmsg: rebuilding a carried SCM_RIGHTS fd failed errno=Errno(2 = ENOENT)
  spec=F|0|0|/proc/79/status`, and `DIAG_TIMELINE exit_group pid=79 comm=Chrome_ChildIOT status=0`
  (the peer's IO child had just exited).

---

## 3. Does it appear when the page loads? (the A/B)

Corpus survey of every log in `/tmp/lbx` (`grep -a "rejected by interface"`), with each run classified
single- vs multi-process by the presence of Chromium's own `Cannot use V8 Proxy resolver in single
process mode`:

| run | mode | rejection | outcome |
|---|---|---|---|
| `log-sp1` | single-process | **none** | **page LOADED** (`<!DOCTYPE html>`+`<html>`) |
| `log-sp2` | single-process | **none** | rc=124, hung (screenshot arm) |
| `log-b3` | single-process | **none** | rc=124, DOM=0, hung |
| `log-m3` | single-process | **YES** — `WidgetHost` 6 and 7 | DOM=0 |
| `log-b1` | **multi-process** | **none** | **rc=0, DOM=2 — page LOADED** |
| `log-z0` | multi-process | YES — `Widget` 2 | rc=124, DOM=0 |
| `log-cg1` | multi-process | YES — `Widget` 2 | rc=124, DOM=0 |
| `log-mt1` | multi-process | YES — `Widget` 2 | rc=124, DOM=0 |
| `log-vton` | multi-process | YES — `Widget` 2 | rc=124, DOM=0 |
| `log-ab-off`, `log-abon`, `log-nsoff`, `log-stk1`, `log-dbgstall` | multi-process | YES — `FrameWidgetHost` 1473036071 | rc=124, DOM=0 |

Full distribution across the corpus:
`Widget` 2 ×4 (cg1, mt1, vton, z0) · `WidgetHost` 6 & 7 ×1 (m3) · `FrameWidgetHost` 1473036071 ×7
(ab-off, abon, dbgstall, nsoff, stk1).

Reads:
* **For causality:** every multi-process run that hangs (9/9) has a rejection, and the one
  multi-process run that loads (`log-b1`) has none. The mechanism (whole-pipe reset) is exactly the
  kind of thing that would stop a page forever, and it can be silent (see §1), which matches "the
  browser goes silent".
* **Against causality:**
  1. `log-m3` is **single-process** and still gets rejections (`WidgetHost` 6 and 7 — precisely the
     two of WidgetHost's eight methods that carry Mojo handles: `CreateFrameSink` (3 handles) and
     `RegisterRenderFrameMetadataObserver` (2 handles)). So the rejection is not exclusive to
     cross-process transport.
  2. `log-b3` and `log-sp2` hang with **no** rejection at all.
  3. `log-dbgstall` (per-syscall log, timestamps): the browser main-thread `ppoll(timeout=0ns)` spin
     **starts at t=16.5049 s**, ~0.96 s **before** its rejection at t=17.4656 s. The spin is therefore
     not caused by the rejection in that run.
  4. It fires exactly once per run, on a different interface each time — the signature of an
     occasional upstream fault, not of a deterministic blocker.

---

## 4. Verdict

**Not proven causal; best available correlate, and damaging by construction.**

It is *not* a benign bystander log: the code path behind it resets the whole Mojo pipe the interface
rides on, and for an associated interface that is the whole browser↔renderer IPC channel. But the
counter-evidence (single-process `m3` also rejects; `b3`/`sp2` hang without rejecting; the dbgstall
spin precedes the rejection) means it cannot be the sole cause of "no page ever loads". The most
consistent reading is that **the rejection and the stall are two symptoms of one upstream fault in
litebox's cross-process plumbing** — most likely Mojo's UNIX-stream transport under litebox
(SCM_RIGHTS / carried-fd rebuild failures are directly visible: `log-ab-off` line 493; and the only
`WidgetHost` methods ever rejected are the handle-carrying ones).

**Biggest lead found in passing, not asked for:** `log-b1.txt` is a **multi-process** litebox run
(child pids 29/68/77/79/98/110, no "single process mode" line) that finished **rc=0 with the full DOM
dumped at 14:53:29** and has **no** Mojo rejection. Whatever flags arm `b1` used (the spec list passed
to `/tmp/lbx/bisect.sh` was not saved anywhere I can find — `bisect.sh` takes `tag|secs|flags`
triples) is the one known configuration where multi-process chromium loads a page under litebox.
Recovering `b1`'s flags is worth more than any further Mojo archaeology.

---

## 5. Next experiment (a probe that could FAIL)

**Probe W1 — "does the pipe actually die?"** (cheap, no litebox change, one 90 s run).
Re-run the multi-process `--dump-dom` arm with Mojo's own logging on:
`--enable-logging=stderr --v=1 --vmodule=interface_endpoint_client=3,multiplex_router=3,connector=3,*mojo*=2`.
Then:
* **Prediction if causal:** the rejection is followed within <1 s by channel-death evidence —
  `Connector`/`ChannelMojo` pipe-error logging, `RenderProcessHostImpl` renderer-gone / sad-tab /
  reload logging, and **zero** further traffic on `blink.mojom.Widget`/`WidgetHost`.
* **Falsified if:** the channel keeps carrying traffic normally afterwards, or a run hangs with no
  rejection while still showing a live `Widget` channel.

**Probe W2 — the real A/B.** Re-run arm `b1` (flags unknown, must be recovered first) and the plain
multi-process arm, 3× each, and tabulate `rejection ∈ {0,1}` against `DOM`.
* **Prediction if causal:** rejection ⟺ DOM=0, perfectly, over 6 runs.
* **Falsified if:** any run rejects and still dumps the DOM, or any run dumps the DOM while rejecting.
(This is the only probe that can settle it; every other arm pair we have is confounded.)

**Probe W3 — find the corrupt frame** (in-shim diag, needed because `ptrace`/strace is blocked here).
Add a `LITEBOX_DIAG_MOJO` that logs, for every `read`/`recvmsg` on AF_UNIX SOCK_STREAM fds,
`bytes_requested` vs `bytes_returned`, `MSG_CTRUNC`/`MSG_TRUNC`, and the SCM_RIGHTS fd count, and
print 64 bytes around the frame header when a short read or a dropped-rights read occurs.
* **Prediction:** exactly one malformed frame immediately precedes each rejection.
* **Falsified if:** every frame crossing the channel is intact — which would force the conclusion that
  the rejection is Chromium-side (and, with `m3` being single-process, probably a bystander).

I did not change any litebox source, did not launch chromium under litebox, and did not commit
anything.
