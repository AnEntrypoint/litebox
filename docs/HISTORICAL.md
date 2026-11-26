# Historical material

These files describe the retired harness: a rewritten, flattened, pruned rootfs tar with
syscall-rewritten ELFs, generated `litebox/config_and_run.sh`, injected s6 services, overlays,
Selkies/sitecustomize/xkbcomp monkey-patches, a host TUN device and hand-built graphical
"rungs". That harness reached a GUI, but it modified the image and needed root. It is not the
architecture of this tree and none of it is an instruction for the current goal.

| Material | Status |
| --- | --- |
| `docs/AGENTS_ARCHIVE_*` | Historical notes from the Windows chromium/webtop investigation (2026-09 .. 2026-10). |
| `docs/HARNESS-LESSONS.md` | Historical harness lore. |
| `docs/LINUX-CHROMIUM.md` | Historical Linux-host chromium goal statement. |
| `docs/WEBTOP-IN-LITEBOX-2026-10-10.md` | Historical rewritten-rootfs webtop boot. |
| `docs/WEBTOP-REBUILD-2026-10-12.md` | Historical rootfs rebuild from host packages. |
| `docs/WEBTOP-BOOT-2026-10-10.md`, `docs/webtop-debian-xfce-2026-09-08.md` | Historical boot measurements of the mutated image. |
| `docs/CHROMIUM-*`, `docs/MOJO-*`, `docs/mojo-*`, `docs/fork-*`, `docs/track-b-*` | Historical subsystem investigations. |
| `advisor/`, `tools/webtop/custom-services.d/`, `tools/webtop/*.py`, `tools/webtop/*.mjs`, `x7.tar` | Historical probes, overlays, injected services and patches; not invoked by anything active. |
| `.wfgy/` (ignored) | Historical Windows launch scripts and logs. |

The current contract is `README.md` and `AGENTS.md`: raw, digest-pinned, unmodified OCI layers;
LiteBox (forked) as the only engine; userspace published ports; no root, TUN, Docker daemon or
image mutation; readiness measured by HTTP 200 plus the first real frame.

Some facts in the archives remain useful engineering knowledge (a guest-reachable refusal is an
errno, never a panic; shared registries key on `(dev, path)`; a `TypedFd` is process-local).
Those that are still binding have been carried into `AGENTS.md`. Everything else is evidence,
not instruction.
