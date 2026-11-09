//! 実workerの診断測定。録音・仮字幕・AI・貼り付けを含む操作全体とは分ける。
//! scripts/verify.ps1 -Mode Live が入力、モデル、保存先を明示して実行する。
use openhinovoice::{asr, audio, config, segment};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const READY_TIMEOUT: Duration = Duration::from_secs(300);
const END_TIMEOUT: Duration = Duration::from_secs(900);
const CASES: &[&str] = &["short", "1min", "5min", "30min"];
const PUNCTUATION: &[char] = &['、', '。', '！', '？', '，', '．', ',', '.', '!', '?', '・'];

struct CorpusCase {
    name: String,
    wav: PathBuf,
    reference: PathBuf,
}
fn required_cases(directory: &Path, requested: &[&str]) -> Result<Vec<CorpusCase>, String> {
    if requested.is_empty() {
        return Err("測定するcorpusを指定してください".into());
    }
    requested
        .iter()
        .map(|name| {
            if !CASES.contains(name) {
                return Err(format!("不明なcorpusです: {name}"));
            }
            let wav = directory.join(format!("{name}.wav"));
            let reference = directory.join(format!("{name}.ref.txt"));
            if !wav.is_file() || !reference.is_file() {
                return Err(format!("{name}.wav と {name}.ref.txt の両方が必要です"));
            }
            Ok(CorpusCase {
                name: (*name).to_owned(),
                wav,
                reference,
            })
        })
        .collect()
}
fn corpus_cases() -> Vec<CorpusCase> {
    let directory = std::env::var_os("JV_CORPUS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus"));
    let requested = std::env::var("JV_PERF_CASES").unwrap_or_else(|_| CASES.join(","));
    let requested: Vec<_> = requested.split(',').map(str::trim).collect();
    required_cases(&directory, &requested).expect("corpusの事前確認に失敗しました")
}
struct Run {
    speech_exe: String,
    model: String,
    words: String,
}
impl Run {
    fn from_environment() -> Self {
        // 実アカウントや設定の移行を起こさず、既定値と明示した引数だけを使う。
        let cfg = config::parse(config::EXAMPLE_TOML).expect("既定設定を読めません");
        let run = Self {
            speech_exe: std::env::var("JV_SPEECH_EXE").unwrap_or(cfg.speech_exe),
            model: std::env::var("JV_MODEL").unwrap_or(cfg.model),
            words: std::env::var("JV_ASR_WORDS").unwrap_or_default(),
        };
        asr::check_offline_model(&run.speech_exe, &run.model, true)
            .expect("取得済みのモデルが必要です。測定中のダウンロードは行いません");
        run
    }
    fn start_worker(&self) -> (asr::Worker, u128) {
        let started = Instant::now();
        let mut worker =
            asr::Worker::spawn(&self.speech_exe, &self.model).expect("workerを起動できません");
        worker
            .wait_ready(READY_TIMEOUT)
            .expect("workerが準備完了になりません");
        (worker, started.elapsed().as_millis())
    }
}
fn feed_wav(worker: &mut asr::Worker, id: &str, sample_rate: u32, pcm: &[u8]) {
    let chunk_bytes = (sample_rate as usize / 10) * 2;
    for chunk in pcm.chunks(chunk_bytes.max(2)) {
        while !worker.send_chunk(id, chunk).expect("chunkを送れません") {
            worker.poll_writer().expect("workerへの送信が進みません");
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}
fn transcribe(worker: &mut asr::Worker, id: &str, rate: u32, words: &str) {
    while !worker
        .transcribe(id, rate, words)
        .expect("認識を要求できません")
    {
        worker.poll_writer().expect("workerへの送信が進みません");
        std::thread::sleep(Duration::from_millis(1));
    }
}
fn drain_until_end(worker: &mut asr::Worker, id: &str) -> (String, String) {
    let started = Instant::now();
    loop {
        worker.poll_writer().expect("workerへの送信が進みません");
        match worker.poll_terminal() {
            Some(Ok(terminal)) if terminal.id == id => {
                return match terminal.outcome {
                    asr::TerminalOutcome::Text(text) => (text.unwrap_or_default(), terminal.stop),
                    asr::TerminalOutcome::Error(message) => (String::new(), message),
                    asr::TerminalOutcome::Cancelled => (String::new(), "cancelled".into()),
                }
            }
            Some(Err(error)) => return (String::new(), error.to_string()),
            _ if started.elapsed() >= END_TIMEOUT => return (String::new(), "timeout".into()),
            _ => std::thread::sleep(Duration::from_millis(10)),
        }
    }
}
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
    for (i, reference_char) in reference.iter().enumerate() {
        current[0] = i as u32 + 1;
        for (j, hypothesis_char) in hypothesis.iter().enumerate() {
            current[j + 1] = (prev[j + 1] + 1)
                .min(current[j] + 1)
                .min(prev[j] + u32::from(reference_char != hypothesis_char));
        }
        std::mem::swap(&mut prev, &mut current);
    }
    prev[hypothesis.len()] as f64 / reference.len() as f64
}
// 再現条件の比較用。改ざん検知用ではなく、verify.ps1ではSHA-256も記録する。
fn fingerprint(bytes: &[u8]) -> String {
    let hash = bytes.iter().fold(0xcbf29ce484222325u64, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
    });
    format!("{hash:016x}")
}
#[derive(Serialize)]
struct Measurement {
    name: String,
    audio_seconds: f64,
    wav_fingerprint_fnv1a64: String,
    reference_fingerprint_fnv1a64: String,
    first_ready_ms: u128,
    restarted_ready_ms: Option<u128>,
    recognition_ms: u128,
    last_interval_ms: Option<u128>,
    intervals: usize,
    cer_strict: f64,
    cer_loose: f64,
    stop: String,
    reference_chars: usize,
    output_chars: usize,
}
fn measure(case: &CorpusCase, run: &Run, segmented: bool) -> Measurement {
    let bytes = std::fs::read(&case.wav).expect("WAVを読めません");
    let wav_hash = fingerprint(&bytes);
    let wav = asr::parse_wav(&bytes).expect("WAVが不正です");
    drop(bytes);
    let reference = std::fs::read_to_string(&case.reference).expect("正解を読めません");
    assert!(!reference.trim().is_empty(), "正解の文章が空です");
    let (mut worker, first_ready_ms) = run.start_worker();
    let audio_seconds = wav.pcm_le.len() as f64 / 2.0 / wav.sample_rate as f64;
    let (text, stop, recognition_ms, last_interval_ms, intervals) = if segmented {
        // 全音声を先に区切る診断。録音時の到着速度や未処理量は再現しない。
        let samples: Vec<f32> = wav
            .pcm_le
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| i16::from_le_bytes(*b) as f32 / 32768.0)
            .collect();
        let mut segmenter = segment::Segmenter::new(wav.sample_rate);
        let mut intervals = segmenter.push(&samples);
        if let Some(rest) = segmenter.finish().filter(|rest| !rest.is_empty()) {
            intervals.push(rest);
        }
        assert!(!intervals.is_empty(), "発話区間が見つかりません");
        let mut texts = Vec::with_capacity(intervals.len());
        let mut last_interval_ms = None;
        let mut outcome = String::from("complete");
        let started = Instant::now();
        for (index, interval) in intervals.iter().enumerate() {
            let id = format!("s{index}");
            feed_wav(
                &mut worker,
                &id,
                wav.sample_rate,
                &audio::samples_to_pcm_le(interval),
            );
            let requested_at = Instant::now();
            transcribe(&mut worker, &id, wav.sample_rate, &run.words);
            let (text, stop) = drain_until_end(&mut worker, &id);
            if stop != "complete" {
                outcome = stop;
                break;
            }
            texts.push(text);
            if index + 1 == intervals.len() {
                last_interval_ms = Some(requested_at.elapsed().as_millis());
            }
        }
        (
            segment::join_intervals(&texts),
            outcome,
            started.elapsed().as_millis(),
            last_interval_ms,
            intervals.len(),
        )
    } else {
        feed_wav(&mut worker, "r", wav.sample_rate, &wav.pcm_le);
        let started = Instant::now();
        transcribe(&mut worker, "r", wav.sample_rate, &run.words);
        let (text, stop) = drain_until_end(&mut worker, "r");
        (text, stop, started.elapsed().as_millis(), None, 1)
    };
    worker.shutdown();
    let restarted_ready_ms = if segmented {
        None
    } else {
        let (worker, ready_ms) = run.start_worker();
        worker.shutdown();
        Some(ready_ms)
    };
    let reference_strict = normalize(&reference, false);
    let hypothesis_strict = normalize(&text, false);
    Measurement {
        name: case.name.clone(),
        audio_seconds,
        wav_fingerprint_fnv1a64: wav_hash,
        reference_fingerprint_fnv1a64: fingerprint(reference.as_bytes()),
        first_ready_ms,
        restarted_ready_ms,
        recognition_ms,
        last_interval_ms,
        intervals,
        cer_strict: cer(&reference_strict, &hypothesis_strict),
        cer_loose: cer(&normalize(&reference, true), &normalize(&text, true)),
        stop,
        reference_chars: reference_strict.len(),
        output_chars: hypothesis_strict.len(),
    }
}
fn write_report(run: &Run, results: &[Measurement], segmented: bool) {
    let directory = std::env::var_os("JV_PERF_OUTPUT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("target/qa/perf"));
    std::fs::create_dir_all(&directory).expect("測定報告の保存先を作れません");
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let kind = if segmented {
        "segmented-diagnostic"
    } else {
        "worker-diagnostic"
    };
    let base = directory.join(format!("{kind}-{stamp}-{}", std::process::id()));
    let all_complete = results.iter().all(|row| row.stop == "complete");
    let model_label = Path::new(&run.model)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let metadata = serde_json::json!({
        "schemaVersion": 1, "measurement": kind,
        "status": if all_complete { "passed" } else { "failed" },
        "version": env!("CARGO_PKG_VERSION"), "revision": std::env::var("OHV_REVISION").ok(),
        "target": { "os": std::env::consts::OS, "arch": std::env::consts::ARCH },
        "logicalCpuCount": std::thread::available_parallelism().map(usize::from).ok(),
        "processor": std::env::var("PROCESSOR_IDENTIFIER").ok(), "model": model_label,
        "keywordsChars": run.words.chars().count(), "cacheFlushed": false,
        "includes": if segmented { "serial interval recognition including PCM feed" } else { "transcribe request to terminal response" },
        "excludes": ["real-time microphone arrival", "ASR backlog at Enter", "partial captions", "AI formatting", "paste", "worker shutdown"],
        "results": results,
    });
    std::fs::write(
        base.with_extension("json"),
        serde_json::to_vec_pretty(&metadata).unwrap(),
    )
    .expect("測定JSONを書けません");
    let mut report = format!("# {kind}\n\nモデル: {model_label}\n\n");
    report.push_str("| 音声 | 長さ(秒) | 区間 | 準備1回目(ms) | 再起動準備(ms) | 認識(ms) | 最後の区間(ms) | CER厳密 | CER緩和 | stop |\n|---|---|---|---|---|---|---|---|---|---|\n");
    for row in results {
        report.push_str(&format!(
            "| {} | {:.1} | {} | {} | {} | {} | {} | {:.2}% | {:.2}% | {} |\n",
            row.name,
            row.audio_seconds,
            row.intervals,
            row.first_ready_ms,
            row.restarted_ready_ms
                .map_or_else(|| "対象外".into(), |n| n.to_string()),
            row.recognition_ms,
            row.last_interval_ms
                .map_or_else(|| "対象外".into(), |n| n.to_string()),
            row.cer_strict * 100.0,
            row.cer_loose * 100.0,
            row.stop.replace(['\n', '\r', '|'], " ")
        ));
    }
    report.push_str("\n準備時間は起動要求からreadyまで。終了処理は含まない。OSのキャッシュは消しておらず、再起動は保持workerの再利用とは異なる。\n\n区間版は全音声を事前に分割して逐次認識する診断。最後の区間の時間はEnterから入力完了までの時間ではない。実時間の音声到着、未処理区間、仮字幕、AI、貼り付けを含まない。\n");
    std::fs::write(base.with_extension("md"), report).expect("測定報告を書けません");
    println!("測定結果: {}", base.with_extension("json").display());
    assert!(
        all_complete,
        "完了しなかった認識があります。測定JSONを確認してください"
    );
}
#[test]
#[ignore = "取得済みspeech.cppモデルとcorpusが必要。verify.ps1 -Mode Liveで明示実行"]
fn live_segmented_latency_and_accuracy_on_corpus() {
    let cases = corpus_cases();
    let run = Run::from_environment();
    let measurements: Vec<_> = cases.iter().map(|case| measure(case, &run, true)).collect();
    write_report(&run, &measurements, true);
}
#[test]
#[ignore = "取得済みspeech.cppモデルとcorpusが必要。verify.ps1 -Mode Liveで明示実行"]
fn live_worker_latency_and_accuracy_on_corpus() {
    let cases = corpus_cases();
    let run = Run::from_environment();
    let measurements: Vec<_> = cases
        .iter()
        .map(|case| measure(case, &run, false))
        .collect();
    write_report(&run, &measurements, false);
}
#[test]
fn missing_or_empty_corpus_selection_is_an_error() {
    let missing = std::env::temp_dir().join(format!("ohv-missing-corpus-{}", std::process::id()));
    assert!(required_cases(&missing, &[]).is_err());
    assert!(required_cases(&missing, &["short"]).is_err());
    assert!(required_cases(&missing, &["../other"]).is_err());
}
#[test]
fn each_requested_corpus_requires_audio_and_reference() {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let directory = std::env::temp_dir().join(format!(
        "ohv-corpus-contract-{}-{nonce}",
        std::process::id()
    ));
    std::fs::create_dir(&directory).unwrap();
    std::fs::write(directory.join("short.wav"), b"fixture").unwrap();
    assert!(required_cases(&directory, &["short"]).is_err());
    std::fs::write(directory.join("short.ref.txt"), "正解").unwrap();
    assert_eq!(required_cases(&directory, &["short"]).unwrap().len(), 1);
    assert!(required_cases(&directory, &["short", "1min"]).is_err());
    std::fs::remove_dir_all(directory).unwrap();
}
#[test]
fn character_error_rate_counts_substitution_insertion_and_deletion() {
    assert_eq!(
        cer(&normalize("あいう", false), &normalize("あいう", false)),
        0.0
    );
    assert!((cer(&normalize("あいう", false), &normalize("あえ", false)) - 2.0 / 3.0).abs() < 1e-9);
    assert_eq!(cer(&[], &normalize("あ", false)), 1.0);
    assert_eq!(normalize("あ、 い。", true), vec!['あ', 'い']);
}
