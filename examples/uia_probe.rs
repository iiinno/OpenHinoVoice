//! Edit モードが、どのアプリで選択を読めるかを確かめるための試験用のプログラム。
//! 前面のアプリの選択範囲を 1 秒ごとに読んで表示するだけのもの。
//!
//! 実行: cargo run --release --example uia_probe
//! 使い方: このプログラムを動かしながら、確かめたいアプリで文を選ぶ。
//! 結果は次の 3 種類。
//!
//! - 選択あり: TextPattern で読めた(Edit が使える)
//! - 対応していない: TextPattern がない(Edit を使えないと表示する)
//! - 選択なし: TextPattern はあるが、いまは選んでいない
//!
//! Ctrl+C で終わる

#[path = "../src/context.rs"]
mod context;
#[path = "../src/uia.rs"]
mod uia;

use std::time::Duration;

fn focused_app() -> String {
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};
    unsafe {
        let hwnd = GetForegroundWindow();
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == 0 {
            return "不明".into();
        }
        context::process_name(pid).unwrap_or_else(|| format!("pid {pid}"))
    }
}

fn main() {
    println!("前面のアプリの選択範囲を 1 秒ごとに読みます。確かめたいアプリで文を選んでください(Ctrl+C で終了)");
    let mut last = String::new();
    loop {
        std::thread::sleep(Duration::from_secs(1));
        let app_name = focused_app();
        let probe = uia::probe_selected_text(Duration::from_millis(500));
        let line = match probe {
            uia::SelectionProbe::Selected(text) => {
                let shortened: String = text.chars().take(60).collect();
                format!("[{app_name}] 選択あり: {shortened}")
            }
            uia::SelectionProbe::Unavailable => {
                format!("[{app_name}] 対応していない(Edit を使えないと表示する)")
            }
            uia::SelectionProbe::None => format!("[{app_name}] 選択なし"),
        };
        if line != last {
            println!("{line}");
            last = line;
        }
    }
}
