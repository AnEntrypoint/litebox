#!/usr/bin/env python3
"""Turn litebox's own host-crash diagnostics into symbol names.

WHY THIS EXISTS
---------------
When a host-side fault is unrecoverable, `litebox_platform_windows_userland`'s vectored exception
handler prints an allocation-free ring dump before terminating:

    [diag-unrecov-av-ring] [3] code=0xc0000005 rip=0x7ff72bf3539a rva=0xb2539a rsp=... is_in_guest=false
    [diag-unrecov-av-ring-stack] [3][rsp+0x0]=0x7ff72b43fe90 (in-module)
    ...
    [diag-unrecov-av-terminate] rip=0x7ff72bf3539a addr=0x80110c8290

Those `rva=` values and `(in-module)` stack words are the only evidence such a crash leaves, and
until now nothing in the tree converted them to function names -- so every investigation of a
host-side fault started by reading raw hex. `is_in_guest=false` means the fault is in litebox's own
code, which means a symbol name is usually most of the answer.

This script resolves all of them through `llvm-symbolizer` against the matching binary.

MATCHING BINARY -- READ THIS FIRST
----------------------------------
An RVA is only meaningful against the EXACT build that produced it. Symbolizing a log from one build
against a later binary produces confident, plausible, completely wrong answers (observed directly:
a fault in litebox's own fault path resolved to `flt2dec::grisu::possibly_round` and
`__imp_GetSystemMenu`). This script cannot detect the mismatch for you. Snapshot the `.exe` and
`.pdb` alongside the log when you start a run you might need to debug, and pass that copy here.

USAGE
-----
    python advisor/probes/symbolize_litebox_crash.py <crash-log> [--exe PATH] [--symbolizer PATH]

`--exe` defaults to `target/release/litebox_runner_linux_on_windows_userland.exe` relative to the
repository root, which is right only if that binary has not been rebuilt since the crash.
"""

from __future__ import annotations

import argparse
import pathlib
import re
import shutil
import subprocess
import sys

# A ring entry's `rip=`/`rva=` pair. Only entries with `is_in_guest=false` are usable: the handler
# prints `rva` as `rip - image_base` unconditionally, so a GUEST rip -- which is nowhere near the
# module -- yields a wrapped, meaningless value (observed live:
# `rip=0x50d74e9bb rva=0xffff800de233e9bb`). Anchoring on one of those put the computed image base
# 18 exabytes away and resolved every symbol to nonsense.
RING_RE = re.compile(r"rip=0x([0-9a-fA-F]+).*?\brva=0x([0-9a-fA-F]+).*?\bis_in_guest=false")
# A stack word the handler itself already identified as lying inside the module.
IN_MODULE_RE = re.compile(r"=0x([0-9a-fA-F]+)\s+\(in-module\)")
# The final line, which carries a `rip` but no `rva`.
TERMINATE_RE = re.compile(r"\[diag-unrecov-av-terminate\]\s+rip=0x([0-9a-fA-F]+)")


def find_symbolizer(explicit: str | None) -> str:
    if explicit:
        return explicit
    found = shutil.which("llvm-symbolizer") or shutil.which("llvm-symbolizer.exe")
    if found:
        return found
    sys.exit(
        "llvm-symbolizer not found on PATH; pass --symbolizer. "
        "It ships with LLVM (scoop install llvm) and with some Rust toolchains under "
        "lib/rustlib/<target>/bin/."
    )


def symbolize(symbolizer: str, exe: pathlib.Path, rvas: list[int]) -> dict[int, str]:
    """Resolve every RVA in one symbolizer invocation."""
    if not rvas:
        return {}
    stdin = "\n".join(f"0x{rva:x}" for rva in rvas) + "\n"
    proc = subprocess.run(
        [symbolizer, f"--obj={exe}", "--relative-address", "--demangle"],
        input=stdin,
        capture_output=True,
        text=True,
        check=False,
    )
    if proc.returncode != 0:
        sys.exit(f"llvm-symbolizer failed: {proc.stderr.strip()}")
    # Output is one record per input, records separated by a blank line; the first two lines of a
    # record are the function name and the file:line.
    out: dict[int, str] = {}
    records = proc.stdout.split("\n\n")
    for rva, record in zip(rvas, records):
        lines = [line for line in record.splitlines() if line.strip()]
        name = lines[0] if lines else "<unresolved>"
        where = lines[1] if len(lines) > 1 else ""
        out[rva] = f"{name}  {where}".strip()
    return out


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("log", type=pathlib.Path, help="crash log containing the diag-unrecov-av lines")
    ap.add_argument("--exe", type=pathlib.Path, default=None, help="the binary that produced the log")
    ap.add_argument("--symbolizer", default=None, help="path to llvm-symbolizer")
    args = ap.parse_args()

    exe = args.exe
    if exe is None:
        root = pathlib.Path(__file__).resolve().parents[2]
        exe = root / "target" / "release" / "litebox_runner_linux_on_windows_userland.exe"
    if not exe.exists():
        sys.exit(f"binary not found: {exe}")

    text = args.log.read_text(errors="replace")

    # The image base comes from any ring entry that prints both rip and rva. Every entry in one log
    # must agree; a disagreement means the log mixes runs, which is worth refusing rather than
    # guessing at.
    bases = {int(rip, 16) - int(rva, 16) for rip, rva in RING_RE.findall(text)}
    if not bases:
        sys.exit(
            "no host-side ring entry found. Anchoring needs a line carrying both `rva=` and "
            "`is_in_guest=false`; a log with only guest-side faults contains no module address to "
            "resolve."
        )
    if len(bases) > 1:
        sys.exit(f"log contains {len(bases)} different image bases, so it spans more than one run: "
                 + ", ".join(hex(b) for b in sorted(bases)))
    base = bases.pop()

    wanted: list[int] = []
    seen: set[int] = set()

    def want(rva: int) -> None:
        if rva not in seen:
            seen.add(rva)
            wanted.append(rva)

    for _rip, rva in RING_RE.findall(text):
        want(int(rva, 16))
    for addr in IN_MODULE_RE.findall(text):
        want(int(addr, 16) - base)
    for rip in TERMINATE_RE.findall(text):
        want(int(rip, 16) - base)

    symbolizer = find_symbolizer(args.symbolizer)
    resolved = symbolize(symbolizer, exe, wanted)

    print(f"image base: 0x{base:x}")
    print(f"binary:     {exe}")
    print(f"symbolizer: {symbolizer}")
    print()
    print("NOTE: an RVA is only meaningful against the exact build that produced the log. If the")
    print("binary above has been rebuilt since, every name below is wrong but will still look")
    print("plausible. See this script's module docstring.")
    print()

    # Annotate the log in place of the raw hex, so the output reads as the original trace.
    for line in text.splitlines():
        if "diag-unrecov-av" not in line:
            continue
        print(line)
        for rva in sorted({int(r, 16) for _p, r in RING_RE.findall(line)}
                          | {int(a, 16) - base for a in IN_MODULE_RE.findall(line)}
                          | {int(r, 16) - base for r in TERMINATE_RE.findall(line)}):
            print(f"        0x{rva:<10x} {resolved.get(rva, '<unresolved>')}")


if __name__ == "__main__":
    main()
