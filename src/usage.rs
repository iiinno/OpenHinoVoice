//! 利用者の環境での動きを、この PC の中にだけ記録する。
//! 音声は 1 サンプルも記録しない。書くのは状態の変化と時間の数値だけ。
//! どこにも送らず、ファイルは %APPDATA%\OpenHinoVoice\usage-log.jsonl に
//! 残る。利用者の環境での不具合調査のための仕組みで、設定の usage_log で
//! オンとオフを切り替えられる

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// 設定の usage_log を受け取る。起動時と設定の再読み込みのときに更新する
pub static ENABLED: AtomicBool = AtomicBool::new(true);

/// 記録の追記を直列化する。複数のスレッドから書かれるため
static WRITE_LOCK: Mutex<()> = Mutex::new(());

/// 8 MiB を超えたら 1 世代だけ回す
const MAX_LOG_BYTES: u64 = 8 * 1024 * 1024;

pub fn path() -> std::path::PathBuf {
    crate::config::config_path().with_file_name("usage-log.jsonl")
}

/// 1 件を記録する。失敗しても動作には何も影響しない(記録は脇役のため)
pub fn log(event: &str, fields: serde_json::Value) {
    if cfg!(test) || !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let _ = append_to(&path(), build_line(event, fields).as_bytes());
}

/// 記録の 1 行を作る。先頭に時刻とイベントの種類を置く
fn build_line(event: &str, fields: serde_json::Value) -> String {
    let mut object = serde_json::Map::new();
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    object.insert("ts".into(), ts.into());
    object.insert("event".into(), event.into());
    if let serde_json::Value::Object(extra) = fields {
        for (key, value) in extra {
            object.insert(key, value);
        }
    }
    let mut line = serde_json::Value::Object(object).to_string();
    line.push('\n');
    line
}

fn append_to(path: &std::path::Path, line: &[u8]) -> std::io::Result<()> {
    let _guard = WRITE_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // データの増えすぎを防ぎ、直前の記録を 1 世代だけ残す
    if std::fs::metadata(path)
        .map(|m| m.len().saturating_add(line.len() as u64) > MAX_LOG_BYTES)
        .unwrap_or(false)
    {
        let previous = path.with_extension("previous.jsonl");
        if previous.exists() {
            std::fs::remove_file(&previous)?;
        }
        std::fs::rename(path, previous)?;
    }
    std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)?
        .write_all(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ohv-usage-{tag}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn logged_lines_carry_timestamp_event_and_fields() {
        let dir = temp_dir("shape");
        let path = dir.join("usage-log.jsonl");
        let line = build_line(
            "interval_result",
            serde_json::json!({"id": "s0", "latency_ms": 512}),
        );
        append_to(&path, line.as_bytes()).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let value: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
        assert_eq!(value["event"], "interval_result");
        assert_eq!(value["id"], "s0");
        assert_eq!(value["latency_ms"], 512);
        assert!(value["ts"].as_u64().unwrap() > 1_700_000_000_000);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_log_rotates_when_it_grows_over_the_cap() {
        let dir = temp_dir("rotate");
        let path = dir.join("usage-log.jsonl");
        // 大きな行を上限を超えるまで足す(256 KiB × 33 行で 8 MiB 超)。回した
        // あとは本体的なファイルが小さくなるので、回数で打ち切る
        let line = line_bytes("tick", serde_json::json!({"pad": "x".repeat(256 * 1024)}));
        let mut rotated = false;
        for _ in 0..40 {
            append_to(&path, &line).unwrap();
            if path.with_extension("previous.jsonl").exists() {
                rotated = true;
                break;
            }
        }
        assert!(rotated, "上限を超えたら 1 世代だけ回す");
        assert!(std::fs::metadata(&path).unwrap().len() < MAX_LOG_BYTES);
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn line_bytes(event: &str, fields: serde_json::Value) -> Vec<u8> {
        let mut object = serde_json::Map::new();
        object.insert("event".into(), event.into());
        if let serde_json::Value::Object(extra) = fields {
            for (key, value) in extra {
                object.insert(key, value);
            }
        }
        let mut line = serde_json::Value::Object(object).to_string();
        line.push('\n');
        line.into_bytes()
    }
}
