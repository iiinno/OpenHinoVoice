// Integration crates compile full production modules but use only a subset of their APIs.
#![allow(dead_code)]

use std::time::{Duration, Instant};

#[path = "../src/chatgpt.rs"]
mod chatgpt;
#[path = "../src/config.rs"]
mod config;
#[path = "../src/format.rs"]
mod format;

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
