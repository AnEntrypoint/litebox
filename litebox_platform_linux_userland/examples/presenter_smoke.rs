// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Manual, visually-witnessed smoke check for `presentation::Presenter` -- run directly
//! (`cargo run -p litebox_platform_linux_userland --example presenter_smoke`), mirroring
//! `litebox_platform_windows_userland`'s identically-named example. NOT run in this port's own
//! development environment (no `DISPLAY`/Wayland socket/Xvfb available -- see `presentation.rs`'s
//! module doc comment); build-verified only via `cargo check --target x86_64-unknown-linux-gnu`.
//! A real Linux desktop session should confirm a window titled "litebox virtual display" appears
//! showing a red/green gradient, then close on its own or when the window is closed.

fn main() {
    let presenter =
        litebox_platform_linux_userland::presentation::Presenter::new().expect("create presenter");
    let sender = presenter.sender();

    std::thread::spawn(move || {
        let width = 1920u32;
        let height = 1080u32;
        let pitch = width * 4;
        let mut bytes = vec![0u8; (pitch * height) as usize];
        for y in 0..height {
            for x in 0..width {
                let idx = (y * pitch + x * 4) as usize;
                // BGRA8/XRGB8888 byte order: a horizontal red ramp, vertical green ramp, fixed
                // blue. `* 255 / height`/`* 255 / width` are always in 0..=255, exact by
                // construction, so the narrowing below is not a real precision loss.
                bytes[idx] = 128; // B
                #[allow(clippy::cast_possible_truncation)]
                {
                    bytes[idx + 1] = (y * 255 / height) as u8; // G
                    bytes[idx + 2] = (x * 255 / width) as u8; // R
                }
                bytes[idx + 3] = 255; // X/A
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
        sender.send(litebox_platform_linux_userland::presentation::Frame {
            width,
            height,
            pitch,
            bytes,
        });
        println!("sent synthetic gradient frame");
    });

    presenter.run().expect("run presenter event loop");
}
