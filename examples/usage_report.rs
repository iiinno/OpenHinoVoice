//! usage-log.jsonl を集計して、この PC での動きの概要を出す。
//!
//! 実行: cargo run --release --example usage_report [-- パス]
//! パスを省略すると %APPDATA%\OpenHinoVoice\usage-log.jsonl を読む

use std::io::BufRead;

fn percentile(values: &mut [u64], p: f64) -> u64 {
    if values.is_empty() {
        return 0;
    }
    values.sort_unstable();
    let index = ((values.len() as f64 - 1.0) * p).round() as usize;
    values[index]
}

fn default_path() -> std::path::PathBuf {
    std::env::var_os("APPDATA")
        .map(|base| {
            std::path::PathBuf::from(base)
                .join("OpenHinoVoice")
                .join("usage-log.jsonl")
        })
        .unwrap_or_else(|| std::path::PathBuf::from("usage-log.jsonl"))
}

fn main() {
    let path = std::env::args()
        .nth(1)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(default_path);
    let Ok(file) = std::fs::File::open(&path) else {
        println!("{} を開けません。まだ記録がありません", path.display());
        return;
    };
    println!("読むもの: {}", path.display());

    let mut sessions = 0u64;
    let mut reused_workers = 0u64;
    let mut enter_to_result: Vec<u64> = Vec::new();
    let mut intervals = 0u64;
    let mut interval_ms: Vec<u64> = Vec::new();
    let mut interval_results = 0u64;
    let mut interval_latencies: Vec<u64> = Vec::new();
    let mut empty_results = 0u64;
    let mut resends = 0u64;
    let mut partials = 0u64;
    let mut paste_ok = 0u64;
    let mut paste_fail: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    let mut probes: std::collections::BTreeMap<String, u64> = Default::default();
    let mut format_ok = 0u64;
    let mut format_fail: Vec<String> = Vec::new();

    for line in std::io::BufReader::new(file).lines() {
        let Ok(line) = line else { continue };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        match value["event"].as_str().unwrap_or("") {
            "session_start" => {
                sessions += 1;
                if value["reused_worker"] == true {
                    reused_workers += 1;
                }
            }
            "interval_closed" => {
                intervals += 1;
                interval_ms.push(value["ms"].as_u64().unwrap_or(0));
            }
            "interval_result" => {
                interval_results += 1;
                interval_latencies.push(value["latency_ms"].as_u64().unwrap_or(0));
                if value["empty"] == true {
                    empty_results += 1;
                }
            }
            "asr_resend" => resends += 1,
            "asr_done" => enter_to_result.push(value["enter_to_result_ms"].as_u64().unwrap_or(0)),
            "partial_sent" => partials += 1,
            "paste" => {
                if value["ok"] == true {
                    paste_ok += 1;
                } else {
                    paste_fail.push(value["error"].as_str().unwrap_or("?").to_string());
                }
            }
            "error" => errors.push(value["message"].as_str().unwrap_or("?").to_string()),
            "edit_probe" => {
                let result = value["result"].as_str().unwrap_or("?").to_string();
                *probes.entry(result).or_insert(0) += 1;
            }
            "format_result" => {
                if value["ok"] == true {
                    format_ok += 1;
                } else {
                    format_fail.push(value["reason"].as_str().unwrap_or("?").to_string());
                }
            }
            _ => {}
        }
    }

    println!("録音の回数: {sessions}(残した worker の再利用 {reused_workers} 回)");
    println!("区間: 合計 {intervals} 個、結果 {interval_results} 件(空 {empty_results} 件)");
    if !interval_ms.is_empty() {
        println!(
            "  区間の長さ ms: p50 {} p90 {} 最大 {}",
            percentile(&mut interval_ms, 0.5),
            percentile(&mut interval_ms, 0.9),
            *interval_ms.last().unwrap_or(&0)
        );
    }
    if !interval_latencies.is_empty() {
        println!(
            "  区間の認識 ms: p50 {} p90 {} 最大 {}",
            percentile(&mut interval_latencies, 0.5),
            percentile(&mut interval_latencies, 0.9),
            *interval_latencies.last().unwrap_or(&0)
        );
    }
    if !enter_to_result.is_empty() {
        println!(
            "Enter から結果まで ms: p50 {} p90 {} 最大 {}(記録 {} 件)",
            percentile(&mut enter_to_result, 0.5),
            percentile(&mut enter_to_result, 0.9),
            *enter_to_result.last().unwrap_or(&0),
            enter_to_result.len()
        );
    }
    println!("仮の認識を送った回数: {partials}、送り直し: {resends} 回");
    println!("整形: 成功 {format_ok}、失敗 {}", format_fail.len());
    for reason in format_fail.iter().take(5) {
        println!("  失敗: {reason}");
    }
    println!("貼り付け: 成功 {paste_ok}、失敗 {}", paste_fail.len());
    for error in paste_fail.iter().take(5) {
        println!("  失敗: {error}");
    }
    if !probes.is_empty() {
        println!("Edit の選択の確認: {probes:?}");
    }
    if !errors.is_empty() {
        println!("エラーの表示: {} 件", errors.len());
        for message in errors.iter().take(10) {
            println!("  {message}");
        }
    }
}
