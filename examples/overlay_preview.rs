//! Hidden renderer fixtures: no HWND, microphone, audio, or application state changes.
//! cargo run --example overlay_preview -- <output-directory> [--all-dpi | --benchmark]
use openhinovoice::feedback::{FeedbackMotion, HudStyle};
use openhinovoice::overlay::{motion_enabled, preview_frame, Mode, PreviewFrame, Tone};
use std::path::Path;
use std::time::{Duration, Instant};

fn write_bmp(path: &Path, frame: &PreviewFrame, background: [u8; 3]) -> std::io::Result<()> {
    let stride = (frame.width * 3 + 3) & !3;
    let size = 54 + stride * frame.height;
    let mut bytes = vec![0u8; size];
    bytes[..2].copy_from_slice(b"BM");
    bytes[2..6].copy_from_slice(&(size as u32).to_le_bytes());
    bytes[10..14].copy_from_slice(&54u32.to_le_bytes());
    bytes[14..18].copy_from_slice(&40u32.to_le_bytes());
    bytes[18..22].copy_from_slice(&(frame.width as i32).to_le_bytes());
    bytes[22..26].copy_from_slice(&(frame.height as i32).to_le_bytes());
    bytes[26..28].copy_from_slice(&1u16.to_le_bytes());
    bytes[28..30].copy_from_slice(&24u16.to_le_bytes());
    for y in 0..frame.height {
        for x in 0..frame.width {
            let src = (y * frame.width + x) * 4;
            let dest = 54 + (frame.height - 1 - y) * stride + x * 3;
            let alpha = u32::from(frame.rgba[src + 3]);
            for channel in 0..3 {
                bytes[dest + 2 - channel] = ((u32::from(frame.rgba[src + channel]) * alpha
                    + u32::from(background[channel]) * (255 - alpha)
                    + 127)
                    / 255) as u8;
            }
        }
    }
    std::fs::write(path, bytes)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let directory = args.next().ok_or("output directory is required")?;
    let options: Vec<String> = args.collect();
    let all_dpi = options.iter().any(|argument| argument == "--all-dpi");
    let measure = options.iter().any(|argument| argument == "--benchmark");
    let output = Path::new(&directory);
    std::fs::create_dir_all(output)?;
    let started = Instant::now();
    let states = [
        (
            "recording",
            Mode::Recording {
                started,
                preparing: false,
            },
        ),
        (
            "preparing",
            Mode::Recording {
                started,
                preparing: true,
            },
        ),
        ("recognizing", Mode::Recognizing { preparing: false }),
        (
            "formatting",
            Mode::Busy {
                label: "整形中".into(),
                hint: Some("Enter で原文を入力".into()),
            },
        ),
        (
            "success",
            Mode::Notice {
                tone: Tone::Success,
                text: "入力しました".into(),
            },
        ),
        (
            "warning",
            Mode::Notice {
                tone: Tone::Warn,
                text: "原文で入力しました".into(),
            },
        ),
        (
            "error",
            Mode::Notice {
                tone: Tone::Error,
                text: "入力できません。トレイからテキストを回収できます".into(),
            },
        ),
        (
            "cancel",
            Mode::Notice {
                tone: Tone::Info,
                text: "取り消しました".into(),
            },
        ),
        (
            "startup",
            Mode::Notice {
                tone: Tone::Startup,
                text: "OpenHinoVoiceを起動しました".into(),
            },
        ),
    ];
    if measure {
        return benchmark(output, &states);
    }
    let scales: &[f32] = if all_dpi {
        &[1.0, 1.25, 1.5, 1.75, 2.0]
    } else {
        &[1.0]
    };
    let mut results = Vec::new();
    for (theme, background) in [("hinomaru", [242, 243, 245]), ("dark", [27, 30, 36])] {
        for (style_name, style) in [
            ("standard", HudStyle::Standard),
            ("soft", HudStyle::Soft),
            ("minimal", HudStyle::Minimal),
            ("rich", HudStyle::Rich),
        ] {
            for (state_name, mode) in &states {
                for (motion_name, motion) in [
                    ("system", FeedbackMotion::System),
                    ("reduced", FeedbackMotion::Reduced),
                ] {
                    for &scale in scales {
                        let elapsed = if *state_name == "success" {
                            Duration::from_millis(200)
                        } else {
                            Duration::from_millis(1250)
                        };
                        let before = Instant::now();
                        let frame = preview_frame(mode, theme, style, motion, elapsed, 0.62, scale);
                        let render_us = before.elapsed().as_micros();
                        let name = format!(
                            "{theme}-{style_name}-{state_name}-{motion_name}-{}.bmp",
                            (scale * 100.0) as u32
                        );
                        write_bmp(&output.join(&name), &frame, background)?;
                        results.push(serde_json::json!({
                            "file": name, "theme": theme, "style": style_name, "state": state_name,
                            "motion": motion_name, "scale": scale, "elapsed_ms": elapsed.as_millis(),
                            "synthetic_level": 0.62, "width": frame.width, "height": frame.height,
                            "render_us": render_us,
                        }));
                    }
                }
            }
        }
    }
    std::fs::write(
        output.join("index.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "renderer": "production overlay::preview_frame", "windows_created": 0, "audio_played": false,
            "microphone_accessed": false, "system_motion_enabled": motion_enabled(FeedbackMotion::System),
            "timing_note": "single cold/warm fixture calls, not a performance benchmark",
            "frames": results,
        }))?,
    )?;
    println!(
        "Rendered {} offscreen fixtures into {}",
        results.len(),
        output.display()
    );
    Ok(())
}

#[repr(C)]
#[derive(Default)]
struct ProcessMemoryCounters {
    cb: u32,
    page_fault_count: u32,
    peak_working_set: usize,
    working_set: usize,
    quota_peak_paged_pool: usize,
    quota_paged_pool: usize,
    quota_peak_nonpaged_pool: usize,
    quota_nonpaged_pool: usize,
    pagefile: usize,
    peak_pagefile: usize,
    private_usage: usize,
}

#[link(name = "kernel32")]
extern "system" {
    fn K32GetProcessMemoryInfo(
        process: *mut std::ffi::c_void,
        counters: *mut ProcessMemoryCounters,
        size: u32,
    ) -> i32;
}

#[derive(serde::Serialize)]
struct Resources {
    private_bytes: usize,
    working_set_bytes: usize,
    cpu_ms: f64,
    gdi: u32,
    user: u32,
}

fn resources() -> Result<Resources, Box<dyn std::error::Error>> {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::Threading::{
        GetCurrentProcess, GetGuiResources, GetProcessTimes, GR_GDIOBJECTS, GR_USEROBJECTS,
    };
    let mut memory = ProcessMemoryCounters {
        cb: std::mem::size_of::<ProcessMemoryCounters>() as u32,
        ..Default::default()
    };
    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    unsafe {
        let process = GetCurrentProcess();
        if K32GetProcessMemoryInfo(
            process.0,
            &mut memory,
            std::mem::size_of::<ProcessMemoryCounters>() as u32,
        ) == 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        GetProcessTimes(process, &mut creation, &mut exit, &mut kernel, &mut user)?;
        let ticks =
            |time: FILETIME| (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime);
        Ok(Resources {
            private_bytes: memory.private_usage,
            working_set_bytes: memory.working_set,
            cpu_ms: (ticks(kernel) + ticks(user)) as f64 / 10_000.0,
            gdi: GetGuiResources(process, GR_GDIOBJECTS),
            user: GetGuiResources(process, GR_USEROBJECTS),
        })
    }
}

fn percentile(sorted: &[u128], percent: usize) -> u128 {
    sorted[((sorted.len() * percent).div_ceil(100)).saturating_sub(1)]
}

fn benchmark(output: &Path, states: &[(&str, Mode)]) -> Result<(), Box<dyn std::error::Error>> {
    const FRAMES: usize = 120;
    let mut measurements = Vec::new();
    for (style_name, style) in [("standard", HudStyle::Standard), ("rich", HudStyle::Rich)] {
        for (name, mode) in states.iter().filter(|(name, _)| {
            matches!(
                *name,
                "recording" | "recognizing" | "formatting" | "success"
            )
        }) {
            for _ in 0..12 {
                std::hint::black_box(preview_frame(
                    mode,
                    "hinomaru",
                    style,
                    FeedbackMotion::System,
                    Duration::from_millis(200),
                    0.62,
                    1.0,
                ));
            }
            for repetition in 1..=3 {
                let before = resources()?;
                let wall = Instant::now();
                let mut durations = Vec::with_capacity(FRAMES);
                for frame in 0..FRAMES {
                    let elapsed = Duration::from_millis(if *name == "success" {
                        (frame % 36) as u64 * 17
                    } else {
                        frame as u64 * 17
                    });
                    let start = Instant::now();
                    std::hint::black_box(preview_frame(
                        mode,
                        "hinomaru",
                        style,
                        FeedbackMotion::System,
                        elapsed,
                        0.3 + (frame as f32 * 0.2).sin().abs() * 0.5,
                        1.0,
                    ));
                    durations.push(start.elapsed().as_micros());
                }
                let wall_us = wall.elapsed().as_micros();
                let after = resources()?;
                durations.sort_unstable();
                measurements.push(serde_json::json!({
                    "style": style_name, "state": name, "repetition": repetition, "frames": FRAMES,
                    "p50_us": percentile(&durations, 50), "p95_us": percentile(&durations, 95),
                    "p99_us": percentile(&durations, 99), "max_us": durations.last(),
                    "total_wall_us": wall_us, "process_cpu_ms": after.cpu_ms - before.cpu_ms,
                    "resources_before": before, "resources_after": after,
                }));
            }
        }
    }
    let cycle = || {
        for index in 0..100 {
            let style = [
                HudStyle::Standard,
                HudStyle::Soft,
                HudStyle::Minimal,
                HudStyle::Rich,
            ][index % 4];
            let mode = &states[(index / 4) % states.len()].1;
            let theme = if (index / 32) % 2 == 0 {
                "hinomaru"
            } else {
                "dark"
            };
            let scale = if index < 64 { 1.0 } else { 1.5 };
            // Dropping each frame is the offscreen adapter's stop boundary.
            std::hint::black_box(preview_frame(
                mode,
                theme,
                style,
                FeedbackMotion::Reduced,
                Duration::from_millis(200),
                0.62,
                scale,
            ));
        }
    };
    let before_cycles = resources()?;
    cycle();
    let after_first_100 = resources()?;
    cycle();
    let after_second_100 = resources()?;
    let gdi_stable = after_second_100.gdi <= after_first_100.gdi;
    let user_stable = after_second_100.user <= after_first_100.user;
    let private_growth =
        after_second_100.private_bytes as i64 - after_first_100.private_bytes as i64;
    let bounded_after_warmup = gdi_stable && user_stable && private_growth <= 1024 * 1024;
    let report = serde_json::json!({
        "status": if bounded_after_warmup { "measured" } else { "resource_growth_detected" },
        "path": "shared preview_frame: text cache + CPU raster + unmultiplied RGBA conversion",
        "exclusions": ["native UpdateLayeredWindow", "settings texture upload", "application event scheduling",
            "input latency", "ASR", "actual preview button stop or timer cleanup"],
        "conditions": { "theme": "hinomaru", "scale": 1.0, "frames_per_repetition": FRAMES,
            "repetitions": 3, "warmup_frames_per_state": 12, "execution": "unpaced offscreen throughput",
            "system_motion_enabled": motion_enabled(FeedbackMotion::System), "audio": false, "windows": false },
        "measurements": measurements,
        "resource_cycles": { "switches_per_cycle": 100, "cycles": 2, "motion": "reduced",
            "before": before_cycles, "after_first_100": after_first_100, "after_second_100": after_second_100,
            "gdi_stable": gdi_stable, "user_stable": user_stable,
            "second_cycle_private_growth_bytes": private_growth, "private_growth_budget_bytes": 1024 * 1024,
            "note": "Resource plateau after warming the same 100 transitions; this does not observe settings repaint scheduling." }
    });
    std::fs::write(
        output.join("benchmark.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    if !bounded_after_warmup {
        return Err(
            "Offscreen preview resources grew beyond the warm-cycle bounds; inspect benchmark.json"
                .into(),
        );
    }
    println!(
        "Measured shared preview path: {} state/repetition pairs",
        measurements.len()
    );
    Ok(())
}
