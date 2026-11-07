// 本物の speech.exe と評価用 corpus を使った性能測定。名前を指定したときだけ動く。
// 実行: cargo test --release --test perf_live -- --ignored --nocapture
// 前提: cargo run --release --example gen_corpus が tests/corpus/ に WAV と正解を作っていること。
// 結果は reports/perf/perf-live-<日時>.md に書かれる。
#![allow(dead_code)]

#[path = "../src/asr.rs"]
mod asr;
#[path = "../src/chatgpt.rs"]
mod chatgpt;
#[path = "../src/config.rs"]
mod config;
#[path = "../src/format.rs"]
mod format;
#[path = "../src/lexicon.rs"]
mod lexicon;

use std::time::{Duration, Instant};

const READY_TIMEOUT: Duration = Duration::from_secs(300);
/// 30 分の音声を 1 リクエストで認識させたとき、guardrail が止めるまで待つ上限
const END_TIMEOUT: Duration = Duration::from_secs(900);

/// 句読点などの区切り記号。Haas がこれらを出すかを P1 の判断のために数える
const PUNCTUATION: &[char] = &['、', '。', '！', '？', '，', '．', ',', '.', '!', '?', '・'];

fn corpus_path(name: &str, ext: &str) -> Option<std::path::PathBuf> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/corpus")
        .join(format!("{name}.{ext}"));
    path.exists().then_some(path)
}

fn feed_wav(worker: &mut asr::Worker, sample_rate: u32, pcm: &[u8]) {
    let chunk_bytes = (sample_rate as usize / 10) * 2;
    for chunk in pcm.chunks(chunk_bytes.max(2)) {
        while !worker.send_chunk(chunk).expect("chunk を送れませんでした") {
            worker.poll_writer().expect("worker への送信が進みません");
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

/// end か error まで読む。時間切れは結果の stop に入れる(測定を続けるため失敗にしない)
fn drain_until_end(worker: &mut asr::Worker) -> (String, String) {
    let started = Instant::now();
    loop {
        worker.poll_writer().expect("worker への送信が進みません");
        match worker.poll_end() {
            Some(Ok(text)) => return (text, "complete".into()),
            Some(Err(asr::AsrError::Worker(message))) => return (String::new(), message),
            Some(Err(other)) => return (String::new(), other.to_string()),
            None => {
                if started.elapsed() > END_TIMEOUT {
                    return (String::new(), "timeout".into());
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

/// 空白だけを取り除いた文字列で比べる厳しめの CER と、さらに句読点も無視する緩めの CER
fn normalize(text: &str, strip_punctuation: bool) -> Vec<char> {
    text.chars()
        .filter(|c| !c.is_whitespace())
        .filter(|c| !(strip_punctuation && PUNCTUATION.contains(c)))
        .collect()
}

fn cer(reference: &[char], hypothesis: &[char]) -> f64 {
    if reference.is_empty() {
        return if hypothesis.is_empty() { 0.0 } else { 1.0 };
    }
    let mut prev: Vec<u32> = (0..=hypothesis.len() as u32).collect();
    let mut current = vec![0u32; hypothesis.len() + 1];
    for (i, ref_char) in reference.iter().enumerate() {
        current[0] = i as u32 + 1;
        for (j, hyp_char) in hypothesis.iter().enumerate() {
            let cost = u32::from(ref_char != hyp_char);
            current[j + 1] = (prev[j + 1] + 1).min(current[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut current);
    }
    prev[hypothesis.len()] as f64 / reference.len() as f64
}

struct Measurement {
    name: String,
    cold_ready_ms: u128,
    warm_ready_ms: u128,
    transcribe_ms: u128,
    audio_seconds: f64,
    cer_strict: f64,
    cer_loose: f64,
    stop: String,
    comma_count: usize,
    period_count: usize,
    ref_chars: usize,
    output_chars: usize,
}

fn measure(name: &str, speech_exe: &str, model: &str, words: &str) -> Option<Measurement> {
    let wav_path = corpus_path(name, "wav")?;
    let ref_path = corpus_path(name, "ref.txt")?;
    let bytes = std::fs::read(&wav_path).expect("WAV を読めません");
    let wav = asr::parse_wav(&bytes).expect("WAV が不正です");
    let reference = std::fs::read_to_string(&ref_path).expect("正解を読めません");

    // 1 回めはモデルをディスクから読む。常駐させない今の方式では、録音のたびにこれがかかる
    let spawn_started = Instant::now();
    let mut worker = asr::Worker::spawn(speech_exe, model).expect("worker を起動できません");
    worker
        .wait_ready(READY_TIMEOUT)
        .expect("worker が準備完了になりません");
    let cold_ready_ms = spawn_started.elapsed().as_millis();

    feed_wav(&mut worker, wav.sample_rate, &wav.pcm_le);
    let transcribe_started = Instant::now();
    while !worker
        .transcribe(wav.sample_rate, words)
        .expect("transcribe を送れませんでした")
    {
        worker.poll_writer().expect("worker への送信が進みません");
        std::thread::sleep(Duration::from_millis(1));
    }
    let (text, stop) = drain_until_end(&mut worker);
    let transcribe_ms = transcribe_started.elapsed().as_millis();
    worker.shutdown();

    // 2 回めはモデルのページキャッシュが残った状態。続けて話すときの待ち時間に近い
    let warm_ready_ms = worker_startup_ms(speech_exe, model);

    let ref_strict = normalize(&reference, false);
    let hyp_strict = normalize(&text, false);
    let ref_loose = normalize(&reference, true);
    let hyp_loose = normalize(&text, true);
    Some(Measurement {
        name: name.to_string(),
        cold_ready_ms,
        warm_ready_ms,
        transcribe_ms,
        audio_seconds: wav.pcm_le.len() as f64 / 2.0 / wav.sample_rate as f64,
        cer_strict: cer(&ref_strict, &hyp_strict),
        cer_loose: cer(&ref_loose, &hyp_loose),
        stop,
        comma_count: text.matches('、').count(),
        period_count: text.matches('。').count(),
        ref_chars: ref_strict.len(),
        output_chars: hyp_strict.len(),
    })
}

/// worker を起動して ready までの時間を測る
fn worker_startup_ms(speech_exe: &str, model: &str) -> u128 {
    let started = Instant::now();
    let mut worker = asr::Worker::spawn(speech_exe, model).expect("worker を起動できません");
    worker
        .wait_ready(READY_TIMEOUT)
        .expect("worker が準備完了になりません");
    worker.shutdown();
    started.elapsed().as_millis()
}

fn write_report(results: &[Measurement], words: &str) {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("reports/perf");
    std::fs::create_dir_all(&dir).expect("reports/perf を作れません");
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let path = dir.join(format!("perf-live-{stamp}.md"));
    let mut report = String::new();
    report.push_str(&format!(
        "# perf_live の測定(エポック秒 {stamp})\n\nモデル: 設定の既定、キーワード: {words:?}\n\n"
    ));
    report.push_str("| 音声 | 長さ(秒) | 準備 1 回目(ms) | 準備 2 回目(ms) | 認識(ms) | CER 厳密 | CER 緩和 | stop | 、の数 | 。の数 | 正解文字数 | 出力文字数 |\n");
    report.push_str("|---|---|---|---|---|---|---|---|---|---|---|---|\n");
    for m in results {
        report.push_str(&format!(
            "| {} | {:.0} | {} | {} | {} | {:.1}% | {:.1}% | {} | {} | {} | {} | {} |\n",
            m.name,
            m.audio_seconds,
            m.cold_ready_ms,
            m.warm_ready_ms,
            m.transcribe_ms,
            m.cer_strict * 100.0,
            m.cer_loose * 100.0,
            m.stop,
            m.comma_count,
            m.period_count,
            m.ref_chars,
            m.output_chars
        ));
    }
    report.push_str("\n- 「準備 1 回目」は同じ実行内で初めて worker を起動したときの ready までの時間。「準備 2 回目」はその直後の再起動。\n");
    report.push_str("- 「認識」は transcribe を送ってから end までの時間。Enter 押下から結果までの待ち時間に相当する。\n");
    report.push_str(
        "- 「CER 厳密」は空白を除いた文字で比べ、「CER 緩和」はさらに句読点を除いて比べた値。\n",
    );
    std::fs::write(&path, report).expect("測定報告を書けません");
    println!("報告を書きました: {}", path.display());
}

#[test]
#[ignore = "本物の speech.exe と tests/corpus/ の音声が必要。--ignored 付きで実行する"]
fn live_worker_latency_and_accuracy_on_corpus() {
    let cfg = config::load().expect("設定を読めません");
    let speech_exe = std::env::var("JV_SPEECH_EXE").unwrap_or(cfg.speech_exe.clone());
    let model = std::env::var("JV_MODEL").unwrap_or(cfg.model.clone());
    let words = std::env::var("JV_ASR_WORDS").unwrap_or_default();

    let mut results = Vec::new();
    for name in ["short", "1min", "5min", "30min"] {
        if corpus_path(name, "wav").is_none() {
            println!("{name}.wav がないため飛ばします(gen_corpus を実行してください)");
            continue;
        }
        let started = Instant::now();
        let Some(measurement) = measure(name, &speech_exe, &model, &words) else {
            continue;
        };
        println!(
            "{}: 準備 {}ms(2 回目 {}ms) 認識 {}ms CER 厳密 {:.1}% 緩和 {:.1}% stop={} (測定全体 {:.0}s)",
            measurement.name,
            measurement.cold_ready_ms,
            measurement.warm_ready_ms,
            measurement.transcribe_ms,
            measurement.cer_strict * 100.0,
            measurement.cer_loose * 100.0,
            measurement.stop,
            started.elapsed().as_secs_f64()
        );
        results.push(measurement);
    }
    assert!(!results.is_empty(), "測定できる corpus がありません");
    write_report(&results, &words);
}
