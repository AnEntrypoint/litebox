# macOS JIT-entitlement codesigning probe

Answers one concrete question from PRD row
`macos-aarch64-guest-execution-context-switch-is-not-implemented`: can a
Mach-O binary be ad-hoc signed with the `com.apple.security.cs.allow-jit`
entitlement litebox's eventual macOS guest-execution engine will need (W^X
`MAP_JIT` pages, patched via `pthread_jit_write_protect_np`), from a plain
Windows host with no Mac, no Xcode, and no Apple Developer account?

**Answer: yes, for the tooling half of this problem.** `apple-codesign`
(crates.io, binary name `rcodesign`) is a genuine pure-Rust reimplementation
of Apple's codesigning that runs anywhere, no macOS/Xcode required.

```sh
cargo install apple-codesign
```

Signing (ad-hoc, no certificate -- exactly what a local development binary
needs; `com.apple.security.cs.allow-jit` does not require a paid Developer ID
or notarization for local execution, per established community precedent for
JIT-needing macOS ports of browsers/JVMs/emulators):

```sh
rcodesign sign --entitlements-xml-file entitlements.plist <input-binary> <output-binary>
```

Verified this round-trips correctly, not just a zero exit code -- read the
entitlements back out of the SIGNED BINARY'S OWN embedded signature blob:

```sh
rcodesign print-signature-info <output-binary>
```

which shows a real `Entitlements (5)` blob (`magic: fade7171`) and `DER
Entitlements (7)` blob (`magic: fade7172`) in the signature superblob
(`blob_count` goes from 1, for the linker's own bare ad-hoc `CodeDirectory`,
to 5 once entitlements are added), `flags: CodeSignatureFlags(ADHOC)`
confirming ad-hoc (uncertificated) signing, and an `entitlements_plist:`
field in the output that echoes the exact XML written to
`entitlements.plist` -- including both `com.apple.security.cs.allow-jit` and
`com.apple.security.cs.disable-library-validation`.

## What this DOES and does NOT prove

**Proves**: the tooling to produce a validly-structured, ad-hoc-signed
Mach-O binary with the JIT entitlement genuinely exists and works, entirely
outside macOS. This was previously uncertain ("no tooling" per an earlier
pass's characterization) -- it is now a solved, reproducible step.

**Does NOT prove**: that a real macOS kernel will actually grant JIT rights
(permit `mprotect(PROT_EXEC)` on a `MAP_JIT` page, or otherwise honor the
entitlement at runtime) to a binary signed this way. Ad-hoc signing's
runtime enforcement -- and any kernel-side checks beyond what's locally
inspectable in the signature blob -- can only be confirmed by actually
running the signed binary on real Apple Silicon hardware. This remains a
real hardware-dependent verification gap, same as `run_thread`'s own
TPIDR_EL0-based redesign question.

## Reproducing this probe

1. `cargo install apple-codesign` (or use whatever is already on `PATH` --
   confirmed already installed in this environment as `rcodesign.exe`).
2. Build a real `aarch64-apple-darwin` Mach-O binary via the same
   `cargo-zigbuild`-based cross-compile recipe already established for
   `docs/wayland-drm-backend-probe/` (any binary works -- this probe used a
   trivial standalone `println!` crate to isolate the signing step from
   `litebox_platform_macos_userland`'s own build complexity; that crate's
   own binary/example targets would work equally well once one exists).
3. `entitlements.plist` in this directory is the exact plist used.
4. `rcodesign sign --entitlements-xml-file entitlements.plist <in> <out>`,
   then `rcodesign print-signature-info <out>` to confirm the entitlements
   blob round-trips, matching the "Verified" section above.

The signed test binary itself is not checked in (a throwaway artifact,
reconstructible from the recipe above in under a minute).
