//! 本番の録音セッションへWAVを実時間で供給する認識経路の検証。
//! マイク、画面描画、AI整形、履歴、貼り付けは対象に含めない。
use openhinovoice::{asr, replay};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const FRAME_MS: u64 = 25;
const DRAIN_LIMIT: Duration = Duration::from_secs(900);
const CASES: &[&str] = &["short", "1min", "5min", "30min"];

struct Case {
    name: String,
    wav: PathBuf,
    reference: PathBuf,
}

fn cases(directory: &Path, requested: &str) -> Result<Vec<Case>, String> {
    requested
        .split(',')
        .map(str::trim)
        .map(|name| {
            if !CASES.contains(&name) {
                return Err("測定する既存corpusを指定してください".into());
            }
            let wav = directory.join(format!("{name}.wav"));
            let reference = directory.join(format!("{name}.ref.txt"));
            if !wav.is_file() || !reference.is_file() {
                return Err(format!("{name}の音声と正解文が必要です"));
            }
            Ok(Case {
                name: name.to_owned(),
                wav,
                reference,
            })
        })
        .collect()
}

fn explicit_file(name: &str) -> PathBuf {
    let path = PathBuf::from(std::env::var_os(name).expect("speechとモデルを明示してください"));
    assert!(
        path.is_absolute() && path.is_file(),
        "{name}には既存ファイルの絶対パスが必要です"
    );
    path
}

fn frame_end(frame: u64, sample_rate: u32, total_samples: usize) -> usize {
    (((frame + 1) * u64::from(sample_rate) * FRAME_MS / 1000) as usize).min(total_samples)
}

fn fingerprint(bytes: &[u8]) -> String {
    let hash = bytes.iter().fold(0xcbf29ce484222325u64, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
    });
    format!("{hash:016x}")
}

fn normalized(text: &str, punctuation: bool) -> Vec<char> {
    text.chars()
        .filter(|c| !c.is_whitespace())
        .filter(|c| !punctuation || !"、。！？，．,.!?・".contains(*c))
        .collect()
}

fn cer(reference: &[char], hypothesis: &[char]) -> f64 {
    if reference.is_empty() {
        return f64::from(u8::from(!hypothesis.is_empty()));
    }
    let mut previous: Vec<usize> = (0..=hypothesis.len()).collect();
    let mut current = vec![0; hypothesis.len() + 1];
    for (i, wanted) in reference.iter().enumerate() {
        current[0] = i + 1;
        for (j, actual) in hypothesis.iter().enumerate() {
            current[j + 1] = (previous[j + 1] + 1)
                .min(current[j] + 1)
                .min(previous[j] + usize::from(wanted != actual));
        }
        std::mem::swap(&mut current, &mut previous);
    }
    previous[hypothesis.len()] as f64 / reference.len() as f64
}

#[derive(Serialize)]
struct Backlog {
    elapsed_ms: u128,
    pending_pcm_bytes: usize,
}

#[derive(Serialize)]
struct Measurement {
    case: String,
    status: String,
    error: Option<String>,
    audio_seconds: f64,
    sample_rate: u32,
    supplied_samples: usize,
    wav_fingerprint_fnv1a64: String,
    reference_fingerprint_fnv1a64: String,
    worker_ready_ms: Option<u128>,
    first_partial_result_ms: Option<u128>,
    backlog_at_enter_pcm_bytes: Option<usize>,
    max_backlog_pcm_bytes: usize,
    enter_to_asr_done_ms: Option<u128>,
    audio_start_to_asr_done_ms: Option<u128>,
    elapsed_ms: u128,
    max_feed_lateness_ms: u128,
    cer_strict: Option<f64>,
    cer_loose: Option<f64>,
    output_chars: Option<usize>,
    backlog: Vec<Backlog>,
}

fn observe(driver: &replay::Replay, epoch: Instant, measurement: &mut Measurement) {
    let snapshot = driver.snapshot();
    measurement.max_backlog_pcm_bytes = measurement
        .max_backlog_pcm_bytes
        .max(snapshot.pending_pcm_bytes);
    let elapsed_ms = epoch.elapsed().as_millis();
    if measurement
        .backlog
        .last()
        .is_none_or(|last| elapsed_ms.saturating_sub(last.elapsed_ms) >= 1000)
    {
        measurement.backlog.push(Backlog {
            elapsed_ms,
            pending_pcm_bytes: snapshot.pending_pcm_bytes,
        });
    }
}

fn measure(case: &Case, speech: &Path, model: &Path) -> Measurement {
    let bytes = std::fs::read(&case.wav).expect("WAVを読めません");
    let wav_hash = fingerprint(&bytes);
    let wav = asr::parse_wav(&bytes).expect("WAVが不正です");
    drop(bytes);
    let reference = std::fs::read_to_string(&case.reference).expect("正解文を読めません");
    assert!(!reference.trim().is_empty(), "正解文が空です");
    let samples: Vec<f32> = wav
        .pcm_le
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| i16::from_le_bytes(*b) as f32 / 32768.0)
        .collect();
    let sample_rate = wav.sample_rate;
    drop(wav);
    let started = Instant::now();
    let mut driver =
        replay::Replay::new(speech, model, sample_rate).expect("検証用セッションを開始できません");
    let audio_epoch = Instant::now();
    let mut measurement = Measurement {
        case: case.name.clone(),
        status: "running".into(),
        error: None,
        audio_seconds: samples.len() as f64 / f64::from(sample_rate),
        sample_rate,
        supplied_samples: 0,
        wav_fingerprint_fnv1a64: wav_hash,
        reference_fingerprint_fnv1a64: fingerprint(reference.as_bytes()),
        worker_ready_ms: None,
        first_partial_result_ms: None,
        backlog_at_enter_pcm_bytes: None,
        max_backlog_pcm_bytes: 0,
        enter_to_asr_done_ms: None,
        audio_start_to_asr_done_ms: None,
        elapsed_ms: 0,
        max_feed_lateness_ms: 0,
        cer_strict: None,
        cer_loose: None,
        output_chars: None,
        backlog: Vec::new(),
    };
    let mut frame = 0;
    while measurement.supplied_samples < samples.len() && driver.outcome().is_none() {
        let end = frame_end(frame, sample_rate, samples.len());
        let due = audio_epoch + Duration::from_secs_f64(end as f64 / f64::from(sample_rate));
        while Instant::now() < due && driver.outcome().is_none() {
            driver.tick();
            observe(&driver, audio_epoch, &mut measurement);
            let remaining = due.saturating_duration_since(Instant::now());
            std::thread::sleep(driver.wait_duration().min(remaining));
        }
        if driver.outcome().is_some() {
            break;
        }
        measurement.max_feed_lateness_ms = measurement
            .max_feed_lateness_ms
            .max(Instant::now().saturating_duration_since(due).as_millis());
        if let Err(error) = driver.push_samples(&samples[measurement.supplied_samples..end]) {
            measurement.status = "input_failed".into();
            measurement.error = Some(error);
            break;
        }
        measurement.supplied_samples = end;
        observe(&driver, audio_epoch, &mut measurement);
        driver.tick();
        observe(&driver, audio_epoch, &mut measurement);
        frame += 1;
    }
    let mut enter_requested_at = None;
    if measurement.supplied_samples == samples.len() && driver.outcome().is_none() {
        let entered = Instant::now();
        enter_requested_at = Some(entered);
        driver.enter();
        measurement.backlog_at_enter_pcm_bytes = Some(driver.snapshot().pending_pcm_bytes);
        while driver.outcome().is_none() && entered.elapsed() < DRAIN_LIMIT {
            driver.tick();
            observe(&driver, audio_epoch, &mut measurement);
            if driver.outcome().is_none() {
                std::thread::sleep(driver.wait_duration().min(Duration::from_millis(25)));
            }
        }
    }
    let snapshot = driver.snapshot();
    measurement.worker_ready_ms = snapshot
        .worker_ready_at
        .map(|at| at.saturating_duration_since(started).as_millis());
    measurement.first_partial_result_ms = snapshot
        .first_partial_at
        .map(|at| at.saturating_duration_since(audio_epoch).as_millis());
    measurement.enter_to_asr_done_ms = enter_requested_at
        .zip(snapshot.completed_at)
        .map(|(enter, done)| done.saturating_duration_since(enter).as_millis());
    measurement.audio_start_to_asr_done_ms = snapshot
        .completed_at
        .map(|at| at.saturating_duration_since(audio_epoch).as_millis());
    measurement.elapsed_ms = started.elapsed().as_millis();
    match driver.outcome() {
        Some(Ok(text)) if measurement.supplied_samples == samples.len() => {
            measurement.status = "completed".into();
            measurement.cer_strict = Some(cer(
                &normalized(&reference, false),
                &normalized(text, false),
            ));
            measurement.cer_loose =
                Some(cer(&normalized(&reference, true), &normalized(text, true)));
            measurement.output_chars = Some(text.chars().count());
        }
        Some(Err(error)) => {
            measurement.status = "session_failed".into();
            measurement.error = Some(error.to_string());
        }
        Some(Ok(_)) => {
            measurement.status = "session_failed".into();
            measurement.error =
                Some("すべての音声を供給する前に認識セッションが終了しました".into());
        }
        None if measurement.status == "running" => {
            measurement.status = "drain_timeout".into();
            measurement.error = Some(format!(
                "Enter後{}秒以内に完了しませんでした（未認識PCM: {} bytes）",
                DRAIN_LIMIT.as_secs(),
                snapshot.pending_pcm_bytes
            ));
        }
        None => {}
    }
    measurement
}

#[test]
#[ignore = "本番セッションと既存モデル・音声を実時間で検証。verify.ps1 -Mode Live -LiveTest Realtimeで明示実行"]
fn live_realtime_session_on_corpus() {
    let speech = explicit_file("JV_SPEECH_EXE");
    let model = explicit_file("JV_MODEL");
    let directory =
        PathBuf::from(std::env::var_os("JV_CORPUS_DIR").expect("corpusの場所を明示してください"));
    let requested = std::env::var("JV_PERF_CASES").expect("測定するcorpusを明示してください");
    let cases = cases(&directory, &requested).expect("corpusの事前確認に失敗しました");
    let measurements: Vec<_> = cases
        .iter()
        .map(|case| measure(case, &speech, &model))
        .collect();
    let success = measurements.iter().all(|row| row.status == "completed");
    let output = PathBuf::from(
        std::env::var_os("JV_PERF_OUTPUT_DIR").expect("結果の保存先を明示してください"),
    );
    std::fs::create_dir_all(&output).expect("結果の保存先を作れません");
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    let path = output.join(format!("realtime-{stamp}-{}.json", std::process::id()));
    let report = serde_json::json!({
        "schema_version": 1,
        "scope": "production_recognition_session_realtime_audio",
        "status": if success { "completed" } else { "failed" },
        "version": env!("CARGO_PKG_VERSION"),
        "build_profile": if cfg!(debug_assertions) { "debug" } else { "release" },
        "revision": std::env::var("OHV_REVISION").ok(),
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "cpu": std::env::var("PROCESSOR_IDENTIFIER").ok(),
        "logical_cpus": std::thread::available_parallelism().map(|n| n.get()).ok(),
        "model": model.file_name().and_then(|name| name.to_str()),
        "frame_ms": FRAME_MS,
        "pump_wait": "production App::wait_duration limited by the next audio frame",
        "cache_flushed": false,
        "includes": ["production segmentation", "partial recognition", "confirmed priority", "Enter drain", "AsrDone transition"],
        "excludes": ["microphone capture", "Windows event loop", "painted subtitle", "AI formatting", "history", "clipboard and paste"],
        "measurements": measurements,
    });
    std::fs::write(&path, serde_json::to_vec_pretty(&report).unwrap()).expect("結果を書けません");
    println!("実時間検証の結果: {}", path.display());
    assert!(
        success,
        "完了しなかったセッションがあります。JSONを確認してください"
    );
}

#[test]
fn realtime_audio_frames_keep_fractional_samples_without_drift() {
    let total = 44100 * 3 + 7;
    let mut previous = 0;
    for frame in 0..121 {
        let end = frame_end(frame, 44100, total);
        assert!(end > previous && end - previous <= 1103);
        previous = end;
    }
    assert_eq!(previous, total);
}

#[test]
fn realtime_requires_each_requested_corpus() {
    let missing = std::env::temp_dir().join(format!("ohv-realtime-missing-{}", std::process::id()));
    assert!(cases(&missing, "").is_err());
    assert!(cases(&missing, "../short").is_err());
    assert!(cases(&missing, "short,1min").is_err());
}
