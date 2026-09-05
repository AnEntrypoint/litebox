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
//! What this measures, and the distinction that matters:
//!
//! - **`presents_per_s`** -- the real one. Counted inside `present()` itself (see
//!   `presentation::present_stats`), so it only advances when GPU work was genuinely submitted and
//!   `SurfaceTexture::present` was called. This is the framerate the host can actually sustain,
//!   and the only number that moves if the texture pool, the sampled blit, or the present-mode
//!   selection regresses. `mean_present_ms` is the per-present cost behind it.
//! - **`sends_per_s`** -- what the producer could hand over. Bounded by how fast
//!   `FrameSender::send` returns (a lock plus a wake), NOT by how fast anything is drawn.
//!
//! Frames are deliberately coalesced (`FrameSlot` keeps only the newest), so a producer that
//! outruns the GPU just overwrites the pending frame; `coalesced_dropped` reports how many. That
//! coalescing is correct and intended -- but it means a producer-side rate alone is NOT a
//! throughput measurement: it would read identically whether `present()` ran ten thousand times or
//! never once. An earlier version of this benchmark reported only the producer rate (and paid for
//! an 8.3MiB per-byte pattern fill inside the timed loop, which dominated it); both are fixed.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn main() {
    let seconds: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(10);

    // NOT `hidden_at_startup()`: a hidden window never receives a real `RedrawRequested` from
    // the OS on this platform, so `present()` (which only ever runs from that callback -- see
    // its own doc comment on why `get_current_texture()` must not be called from an arbitrary
    // event-loop callback) would never run at all, no matter how many frames are sent. Confirmed
    // live: with `hidden_at_startup()` this benchmark reported `presents=0` for its entire
    // duration despite hundreds of sends/s -- exactly the blind spot real present-side counting
    // was added to catch. Visibility is unrelated to this benchmark's boot-free/synthetic
    // property (no guest, no DRM, no OCI pull) -- it only controls whether a real window shows.
    let presenter = litebox_platform_windows_userland::presentation::Presenter::new()
        .expect("create presenter");
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

        // Pre-generate a small ring of DISTINCT frames ONCE, outside the timed loop.
        //
        // The previous version filled an 8.3MiB buffer with a scalar per-byte loop on every
        // iteration. At the rates this benchmark reports that is several GiB/s of single-byte
        // writes -- i.e. the "throughput" number was dominated by the benchmark's own pattern
        // generator rather than by anything in `presentation.rs`. Distinct content still matters
        // (identical bytes would let a future upload path skip work and flatter the result), so
        // keep several genuinely different frames and cycle them; just stop paying to synthesize
        // them inside the measurement window.
        const PATTERN_COUNT: usize = 8;
        let patterns: Vec<Vec<u8>> = (0..PATTERN_COUNT)
            .map(|p| {
                let phase = (p * 32) as u8;
                (0..frame_size)
                    .map(|i| ((i as u32).wrapping_add(u32::from(phase)) % 256) as u8)
                    .collect()
            })
            .collect();

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
            // Copy one of the pre-generated distinct patterns (a bulk `memcpy`, which is what the
            // real flip path does too -- see the runner's own `extend_from_slice` from the guest's
            // DRM mapping) rather than synthesizing pixels byte-by-byte inside the timed loop.
            bytes.extend_from_slice(&patterns[(frame_counter as usize) % PATTERN_COUNT]);

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
                let (presents, _) =
                    litebox_platform_windows_userland::presentation::present_stats();
                eprintln!(
                    "[presenter_bench] t={elapsed:.1}s sends={n} ({:.1}/s) presents={presents} ({:.1}/s)",
                    n as f64 / elapsed,
                    presents as f64 / elapsed
                );
                last_report = now;
            }
        }

        let elapsed = start.elapsed().as_secs_f64();
        let total_sends = sends_completed_producer.load(Ordering::Relaxed);
        let total_bytes = bytes_sent_producer.load(Ordering::Relaxed);
        // The number that actually describes the present path. `presents_per_s` is the real
        // framerate this host can sustain through wgpu; `sends_per_s` is only what the producer
        // could hand over. `coalesced_ratio` is how many frames the single-slot `FrameSlot` threw
        // away because the producer outran the GPU -- a healthy figure under a deliberately
        // unthrottled producer, and the whole point of coalescing, but meaningless to report as
        // "throughput".
        let (presents, present_nanos) =
            litebox_platform_windows_userland::presentation::present_stats();
        let mean_present_ms = if presents > 0 {
            (present_nanos as f64 / presents as f64) / 1.0e6
        } else {
            f64::NAN
        };
        let presented_bytes_per_s = (presents as f64 * frame_size as f64) / elapsed;
        // p99 present latency: sort the recent-sample snapshot from `presentation.rs`'s bounded
        // ring and index the 99th percentile directly -- exactly the kind of occasional-stall
        // signal a running mean alone cannot surface.
        let mut samples =
            litebox_platform_windows_userland::presentation::present_latency_samples_nanos();
        samples.sort_unstable();
        let p99_present_ms = if samples.is_empty() {
            f64::NAN
        } else {
            let idx = ((samples.len() as f64) * 0.99).floor() as usize;
            let idx = idx.min(samples.len() - 1);
            samples[idx] as f64 / 1.0e6
        };
        println!("PRESENTER_BENCH_RESULT duration_s={elapsed:.3} sends={total_sends} sends_per_s={:.2} presents={presents} presents_per_s={:.2} mean_present_ms={mean_present_ms:.3} p99_present_ms={p99_present_ms:.3} presented_mb_per_s={:.2} coalesced_dropped={} coalesced_drop_rate={:.4} sent_mb_per_s={:.2}",
            total_sends as f64 / elapsed,
            presents as f64 / elapsed,
            presented_bytes_per_s / (1024.0 * 1024.0),
            total_sends.saturating_sub(presents),
            if total_sends > 0 { total_sends.saturating_sub(presents) as f64 / total_sends as f64 } else { 0.0 },
            (total_bytes as f64 / elapsed) / (1024.0 * 1024.0)
        );
        if presents == 0 {
            eprintln!(
                "[presenter_bench] WARNING: zero completed presents -- the presenter never drew. \
                 A producer-side rate alone would have looked identical to a healthy run."
            );
        }
        std::process::exit(0);
    });

    presenter.run().expect("run presenter event loop");
}
