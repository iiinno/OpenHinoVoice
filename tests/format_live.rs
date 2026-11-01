// Integration crates compile full production modules but use only a subset of their APIs.
#![allow(dead_code)]

use std::time::{Duration, Instant};

#[path = "../src/chatgpt.rs"]
mod chatgpt;
#[path = "../src/config.rs"]
mod config;
#[path = "../src/format.rs"]
mod format;
#[path = "../src/lexicon.rs"]
mod lexicon;

#[test]
#[ignore = "calls the real formatting API with the key in the user config; run explicitly"]
fn real_api_formats_with_prewarm() {
    let cfg = config::load().expect("config must load").format;
    let raw =
        "えーと、明日の会議は午後3時からに変更になりました。資料は共有フォルダにおいてあります。";

    let t = Instant::now();
    let cold = format::format(&cfg, raw).expect("cold request must succeed");
    println!("cold {:?}: {cold}", t.elapsed());

    let mut warm = Vec::new();
    for _ in 0..5 {
        std::thread::sleep(Duration::from_secs(2));
        format::prewarm(&cfg);
        let t = Instant::now();
        let out = format::format(&cfg, raw).expect("warm request must succeed");
        warm.push(t.elapsed());
        assert!(!out.contains("えーと"), "filler must be removed: {out}");
    }
    warm.sort();
    println!("warm after prewarm: {warm:?}");
}

#[test]
#[ignore = "calls the real formatting API with a long transcript from JV_RAW_FILE; run explicitly"]
fn real_api_formats_long_dictation() {
    let mut cfg = config::load().expect("config must load").format;
    if let Some(ms) = std::env::var("JV_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        cfg.timeout_ms = ms;
    }
    if let Ok(provider) = std::env::var("JV_PROVIDER") {
        cfg.provider = provider;
    }
    let path = std::env::var("JV_RAW_FILE").expect("JV_RAW_FILE must point to a transcript");
    let raw = std::fs::read_to_string(path).expect("transcript must be readable");
    println!(
        "provider {} timeout {}ms, {} chars",
        cfg.provider,
        cfg.timeout_ms,
        raw.chars().count()
    );
    for attempt in 1..=3 {
        let t = Instant::now();
        let result = format::format(&cfg, raw.trim());
        println!(
            "attempt {attempt}: {:?} -> {}",
            t.elapsed(),
            match &result {
                Ok(text) => format!("ok, {} chars", text.chars().count()),
                Err(e) => format!("error: {e}"),
            }
        );
        assert!(result.is_ok(), "a long dictation must still be formatted");
    }
}
