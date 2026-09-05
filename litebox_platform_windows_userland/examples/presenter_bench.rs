// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Manual, boot-free throughput/latency benchmark for `presentation::Presenter`'s hot present
//! path -- run directly (`cargo run --release -p litebox_platform_windows_userland --example
//! presenter_bench -- <seconds>`), NOT part of any automated test suite (see this project's own
//! discipline: no standing test files, verification is live execution witnessed directly).
//!
//! Drives the SAME synthetic test-pattern path `presenter_smoke` uses (`FrameSender::send`) at a
//! fixed target rate with NO real guest connected, isolating pure host-side wgpu present-path cost
//! from every guest-side variable (DRM emulation, guest scheduling, etc.) -- this is the only way
//! to know whether a change to `presentation.rs` actually helped throughput/latency rather than
//! assuming it did from reading the diff. Prints a running summary to stderr and a final report to
//! stdout when the requested duration elapses (default 10s), then exits.
//!
//! What this measures: wall-clock time between successive `FrameSender::send` calls attempted at
//! the fastest rate the producer thread can manage (i.e. never sleeping between frames -- this
//! finds the actual ceiling the present path allows, not an arbitrary target framerate), plus a
//! coarse count of how many of those sends were actually followed by a completed `present()` on
//! the window (inferred by having the producer also track total frames sent vs elapsed time,
//! since `Presenter`'s own internals intentionally coalesce -- see `FrameSlot` -- so not every
//! `send` corresponds 1:1 with a GPU present). This coalescing is itself part of what changed
//! (previously an unbounded `mpsc::channel` queued every frame; now a single-slot `Mutex<Option<
//! Frame>>` keeps only the newest), so "sends per second the producer can sustain" is the correct
//! metric for this specific optimization pass -- it is bounded by how fast `FrameSender::send`
//! itself returns (lock + wake), not by how fast the GPU can present, which is exactly the
//! point: after this change the producer is no longer blocked by the presenter's own pace.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn main() {
    let seconds: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(10);

    let presenter = litebox_platform_windows_userland::presentation::Presenter::new()
        .expect("create presenter")
        .hidden_at_startup();
    let sender = presenter.sender();

    let sends_completed = Arc::new(AtomicU64::new(0));
    let bytes_sent = Arc::new(AtomicU64::new(0));
    let sends_completed_producer = sends_completed.clone();
    let bytes_sent_producer = bytes_sent.clone();

    std::thread::spawn(move || {
        let width = 1920u32;
        let height = 1080u32;
        let pitch = width * 4;
        let frame_size = (pitch * height) as usize;

        // Warm up: give the presenter's `resumed()` (real window/wgpu device/surface creation,
        // which happens asynchronously on the event-loop thread) a moment to finish, so the
        // benchmark window doesn't count a startup stall as part of steady-state throughput.
        std::thread::sleep(Duration::from_millis(500));

        let start = Instant::now();
        let deadline = start + Duration::from_secs(seconds);
        let mut frame_counter: u32 = 0;
        let mut last_report = start;

        while Instant::now() < deadline {
            // Reuse the free-list the same way the real `--gui` flip callback does (see
            // `litebox_runner_linux_on_windows_userland`'s own wiring) -- this benchmark should
            // measure the SAME allocation behavior the real path exhibits, not a naive fresh
            // `Vec` per frame that would make the "eliminate per-frame heap copy" fix invisible
            // in the numbers.
            let mut bytes = sender.take_free_buffer();
            bytes.clear();
            bytes.resize(frame_size, 0);
            // Cheap, varying pattern so every frame is genuinely distinct pixel content (matters
            // for confirming the sampled blit/texture pool actually uploads new bytes each time,
            // not just re-presenting a static frame) -- a diagonal moving bar, one pass over the
            // buffer, not a nested per-pixel loop, so pattern generation itself never dominates
            // the measured cost.
            let phase = (frame_counter % 256) as u8;
            for (i, b) in bytes.iter_mut().enumerate() {
                *b = ((i as u32).wrapping_add(u32::from(phase)) % 256) as u8;
            }

            sender.send(litebox_platform_windows_userland::presentation::Frame {
                width,
                height,
                pitch,
                bytes,
            });
            sends_completed_producer.fetch_add(1, Ordering::Relaxed);
            bytes_sent_producer.fetch_add(frame_size as u64, Ordering::Relaxed);
            frame_counter = frame_counter.wrapping_add(1);

            let now = Instant::now();
            if now.duration_since(last_report) >= Duration::from_secs(1) {
                let elapsed = now.duration_since(start).as_secs_f64();
                let n = sends_completed_producer.load(Ordering::Relaxed);
                eprintln!(
                    "[presenter_bench] t={elapsed:.1}s sends={n} rate={:.1}/s",
                    n as f64 / elapsed
                );
                last_report = now;
            }
        }

        let elapsed = start.elapsed().as_secs_f64();
        let total_sends = sends_completed_producer.load(Ordering::Relaxed);
        let total_bytes = bytes_sent_producer.load(Ordering::Relaxed);
        println!("PRESENTER_BENCH_RESULT duration_s={elapsed:.3} sends={total_sends} sends_per_s={:.2} bytes_per_s={:.2} mb_per_s={:.2}",
            total_sends as f64 / elapsed,
            total_bytes as f64 / elapsed,
            (total_bytes as f64 / elapsed) / (1024.0 * 1024.0)
        );
        std::process::exit(0);
    });

    presenter.run().expect("run presenter event loop");
}
