use std::time::{Duration, Instant};
use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows::Win32::UI::WindowsAndMessaging::{DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE};

#[path = "../src/overlay.rs"]
mod overlay;

use overlay::{Mode, Tone};

/// 表示の全状態を順に出す。デザインの確認と画面の撮影に使う
#[test]
#[ignore = "shows the overlay on screen; run explicitly"]
fn cycle_overlay_states() {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    overlay::create().expect("overlay must be created");
    let hold = Duration::from_millis(
        std::env::var("JV_PREVIEW_HOLD_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(2500),
    );
    let modes = [
        Mode::Recording { started: Instant::now() - Duration::from_secs(5) },
        Mode::Busy { label: "認識中".into(), hint: None },
        Mode::Busy { label: "整形中".into(), hint: Some("Enter で生テキスト".into()) },
        Mode::Notice { tone: Tone::Warn, text: "生テキストを貼りました（整形が時間切れ）".into() },
        Mode::Notice { tone: Tone::Error, text: "マイクが見つかりません".into() },
    ];
    for mode in modes {
        let recording = matches!(mode, Mode::Recording { .. });
        overlay::show(mode);
        let start = Instant::now();
        while start.elapsed() < hold {
            if recording {
                // 話し声くらいの大きさで、音節ごとに強弱が付く合成音
                let t = start.elapsed().as_secs_f32();
                let amp = 0.08 * (0.5 + 0.5 * (t * 6.0).sin()).powi(2);
                let samples: Vec<f32> = (0..480).map(|i| amp * ((i as f32) * 0.3).sin()).collect();
                overlay::push_level(&samples);
            }
            pump();
            overlay::tick();
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    overlay::hide();
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(400) {
        pump();
        overlay::tick();
        std::thread::sleep(Duration::from_millis(10));
    }
    overlay::remove_tray();
}

fn pump() {
    unsafe {
        let mut msg = MSG::default();
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}
