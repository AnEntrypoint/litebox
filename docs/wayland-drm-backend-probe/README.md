# Wayland/Smithay DRM backend probe

Deliberately **not** a workspace member (has its own `[workspace]` table in
`Cargo.toml`) -- this is a standalone reference crate, not a shipped feature.
It exists to answer one concrete question from PRD row
`gui-wayland-compositor-on-drm-future`: does `smithay`'s DRM backend, built
with **only** the `backend_drm` feature (`default-features = false`, no
`backend_udev`/`backend_session`/`backend_gbm`), actually compile for a musl
target and drive litebox's virtual DRM device using nothing but a plain
`open("/dev/dri/card0")` fd -- the same guest-syscall model litebox uses for
every other subsystem?

**Answer: yes**, confirmed via `cargo check --target x86_64-unknown-linux-musl`
(see `src/main.rs`'s own doc comment for exactly what was and wasn't
verifiable in this environment -- no musl cross-linker was available here, so
linking and live guest-process verification are the next step for whoever
picks this up on a real musl-linking Linux host).

Run the check yourself:

```sh
cd docs/wayland-drm-backend-probe
cargo check --target x86_64-unknown-linux-musl
```
