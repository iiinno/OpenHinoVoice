//! 本番の録音セッションへWAVを実時間で供給する認識経路の検証。
//! マイク、画面描画、AI整形、履歴、貼り付けは対象に含めない。
//! JV_REALTIME_WORKER_MODE=fresh|reuse、JV_REALTIME_REPETITIONS=1..10。
//! freshもOS/model cacheは消去しない。reuseの先頭1sessionは初回起動として別記する。
//! 同一PID、追加spawnなし、取消終端の観測済みを満たさなければ残りを未実施として止める。
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
    repetition: u32,
    worker_condition: String,
    session_index: Option<u32>,
    worker_pid_at_start: Option<u32>,
    worker_pid_at_end: Option<u32>,
    worker_spawn_count: u32,
    worker_spawns_during_session: u32,
    reused_worker: bool,
    diagnostics_complete: bool,
    request_timings: Vec<RequestTiming>,
    unmatched_terminal_count: usize,
    untranscribed_interval_count: usize,
    unfinished_request_count: usize,
    diagnostics: Option<replay::Diagnostics>,
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

#[derive(Debug, Serialize)]
struct RequestTiming {
    session: u32,
    pid: u32,
    id: String,
    partial: bool,
    pcm_bytes: usize,
    transcribe_queued_run_us: u128,
    terminal_run_us: Option<u128>,
    queue_to_terminal_us: Option<u128>,
    stop: Option<String>,
    outcome: Option<String>,
}

#[derive(Default, Debug)]
struct RequestAudit {
    timings: Vec<RequestTiming>,
    unmatched_terminals: usize,
    untranscribed_intervals: usize,
    unfinished_requests: usize,
}

fn request_audit(events: &[replay::TraceEvent]) -> RequestAudit {
    let mut audit = RequestAudit::default();
    let mut intervals = Vec::new();
    let mut cancellations = Vec::new();
    for event in events {
        let (Some(pid), Some(id)) = (event.pid, event.request_id.as_deref()) else {
            if matches!(event.kind, replay::TraceKind::Terminal { .. }) {
                audit.unmatched_terminals += 1;
            }
            continue;
        };
        match &event.kind {
            replay::TraceKind::IntervalClosed { .. } => intervals.push((event.session, pid, id)),
            replay::TraceKind::CancelQueued => cancellations.push((event.session, pid, id)),
            replay::TraceKind::TranscribeQueued { partial, pcm_bytes } => {
                audit.timings.push(RequestTiming {
                    session: event.session,
                    pid,
                    id: id.into(),
                    partial: *partial,
                    pcm_bytes: *pcm_bytes,
                    transcribe_queued_run_us: event.run_elapsed_us,
                    terminal_run_us: None,
                    queue_to_terminal_us: None,
                    stop: None,
                    outcome: None,
                })
            }
            replay::TraceKind::Terminal {
                stop,
                outcome,
                cancel_requested_session,
            } => {
                if cancel_requested_session.is_some_and(|session| session != event.session) {
                    audit.unmatched_terminals += 1;
                    continue;
                }
                let cancelled_upload = cancellations
                    .iter()
                    .position(|entry| *entry == (event.session, pid, id));
                if let Some(index) = cancelled_upload {
                    cancellations.remove(index);
                }
                if let Some(timing) = audit.timings.iter_mut().rev().find(|timing| {
                    timing.session == event.session
                        && timing.pid == pid
                        && timing.id == id
                        && timing.terminal_run_us.is_none()
                }) {
                    timing.terminal_run_us = Some(event.run_elapsed_us);
                    timing.queue_to_terminal_us = Some(
                        event
                            .run_elapsed_us
                            .saturating_sub(timing.transcribe_queued_run_us),
                    );
                    timing.stop = Some(stop.clone());
                    timing.outcome = Some(outcome.clone());
                } else if outcome != "cancelled" || cancelled_upload.is_none() {
                    // Upload cancellation can finish before transcribe is queued; a text result cannot.
                    audit.unmatched_terminals += 1;
                }
            }
            _ => {}
        }
    }
    audit.untranscribed_intervals = intervals
        .iter()
        .filter(|(session, pid, id)| {
            !audit.timings.iter().any(|timing| {
                !timing.partial
                    && timing.session == *session
                    && timing.pid == *pid
                    && timing.id == *id
            })
        })
        .count();
    audit.unfinished_requests = audit
        .timings
        .iter()
        .filter(|timing| timing.terminal_run_us.is_none())
        .count();
    audit
}

fn worker_mode(value: &str) -> Result<replay::WorkerPolicy, String> {
    match value {
        "fresh" => Ok(replay::WorkerPolicy::FreshProcess),
        "reuse" => Ok(replay::WorkerPolicy::ReuseReady),
        _ => Err("JV_REALTIME_WORKER_MODEはfreshかreuseを指定してください".into()),
    }
}

fn repetitions(value: &str) -> Result<u32, String> {
    value
        .parse::<u32>()
        .ok()
        .filter(|n| (1..=10).contains(n))
        .ok_or_else(|| "JV_REALTIME_REPETITIONSは1〜10を指定してください".into())
}

fn apply_diagnostic_status(
    status: &mut String,
    error: &mut Option<String>,
    diagnostics: &replay::Diagnostics,
    audit: &RequestAudit,
) -> bool {
    let complete = diagnostics.dropped_events == 0
        && diagnostics.unresolved_cancel_requests == 0
        && audit.unmatched_terminals == 0
        && audit.untranscribed_intervals == 0
        && audit.unfinished_requests == 0;
    if !complete && status.as_str() == "completed" {
        *status = "diagnostics_incomplete".into();
        *error = Some(format!(
            "診断が不完全です（未観測の取消終端: {}件、記録上限による欠落: {}件、送信と対応しない終端: {}件、未送信区間: {}件、未完了要求: {}件）",
            diagnostics.unresolved_cancel_requests, diagnostics.dropped_events, audit.unmatched_terminals, audit.untranscribed_intervals, audit.unfinished_requests
        ));
    }
    complete
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

fn measure(
    case: &Case,
    speech: &Path,
    model: &Path,
    policy: replay::WorkerPolicy,
    repetition: u32,
    shared_driver: &mut Option<replay::Replay>,
) -> Measurement {
    if policy == replay::WorkerPolicy::FreshProcess {
        drop(shared_driver.take());
    }
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
    let reusing = shared_driver.is_some();
    let spawns_before = shared_driver
        .as_ref()
        .map_or(0, |driver| driver.snapshot().worker_spawn_count);
    let mut measurement = Measurement {
        case: case.name.clone(),
        repetition,
        worker_condition: if reusing {
            "same_pid_ready_worker"
        } else {
            "fresh_process_cache_unflushed"
        }
        .into(),
        session_index: None,
        worker_pid_at_start: None,
        worker_pid_at_end: None,
        worker_spawn_count: spawns_before,
        worker_spawns_during_session: 0,
        reused_worker: false,
        diagnostics_complete: false,
        request_timings: Vec::new(),
        unmatched_terminal_count: 0,
        untranscribed_interval_count: 0,
        unfinished_request_count: 0,
        diagnostics: None,
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
    let initialized = if let Some(driver) = shared_driver {
        driver.next_session(sample_rate)
    } else {
        replay::Replay::new_with_policy(speech, model, sample_rate, policy)
            .map(|driver| *shared_driver = Some(driver))
    };
    if let Err(error) = initialized {
        measurement.status = "session_start_failed".into();
        measurement.error = Some(error);
        measurement.elapsed_ms = started.elapsed().as_millis();
        if let Some(driver) = shared_driver {
            measurement.diagnostics = Some(driver.take_diagnostics());
        }
        return measurement;
    }
    let driver = shared_driver.as_mut().unwrap();
    measurement.worker_pid_at_start = driver.snapshot().worker_pid;
    let audio_epoch = Instant::now();
    let mut frame = 0;
    while measurement.supplied_samples < samples.len() && driver.outcome().is_none() {
        let end = frame_end(frame, sample_rate, samples.len());
        let due = audio_epoch + Duration::from_secs_f64(end as f64 / f64::from(sample_rate));
        while Instant::now() < due && driver.outcome().is_none() {
            driver.tick();
            observe(driver, audio_epoch, &mut measurement);
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
        observe(driver, audio_epoch, &mut measurement);
        driver.tick();
        observe(driver, audio_epoch, &mut measurement);
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
            observe(driver, audio_epoch, &mut measurement);
            if driver.outcome().is_none() {
                std::thread::sleep(driver.wait_duration().min(Duration::from_millis(25)));
            }
        }
    }
    let snapshot = driver.snapshot();
    measurement.session_index = Some(snapshot.session_index);
    measurement.worker_pid_at_end = snapshot.worker_pid;
    measurement.worker_spawn_count = snapshot.worker_spawn_count;
    measurement.worker_spawns_during_session =
        snapshot.worker_spawn_count.saturating_sub(spawns_before);
    measurement.reused_worker = snapshot.reused_worker;
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
    let diagnostics = driver.take_diagnostics();
    let audit = request_audit(&diagnostics.events);
    measurement.unmatched_terminal_count = audit.unmatched_terminals;
    measurement.untranscribed_interval_count = audit.untranscribed_intervals;
    measurement.unfinished_request_count = audit.unfinished_requests;
    measurement.diagnostics_complete = apply_diagnostic_status(
        &mut measurement.status,
        &mut measurement.error,
        &diagnostics,
        &audit,
    );
    measurement.request_timings = audit.timings;
    measurement.diagnostics = Some(diagnostics);
    let expected_spawns = u32::from(!reusing);
    if measurement.status == "completed"
        && (measurement.worker_spawns_during_session != expected_spawns
            || measurement.worker_pid_at_start != measurement.worker_pid_at_end
            || measurement.reused_worker != reusing)
    {
        measurement.status = "worker_condition_changed".into();
        measurement.error = Some("測定中にworkerのPIDまたはspawn条件が変化しました".into());
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
    let policy =
        worker_mode(&std::env::var("JV_REALTIME_WORKER_MODE").unwrap_or_else(|_| "fresh".into()))
            .unwrap();
    let repeat =
        repetitions(&std::env::var("JV_REALTIME_REPETITIONS").unwrap_or_else(|_| "1".into()))
            .unwrap();
    let schedule: Vec<_> = (1..=repeat)
        .flat_map(|round| cases.iter().map(move |case| (round, case)))
        .collect();
    let mut shared_driver = None;
    let mut measurements = Vec::new();
    let mut not_run = Vec::new();
    for (index, (round, case)) in schedule.iter().enumerate() {
        let row = measure(case, &speech, &model, policy, *round, &mut shared_driver);
        let failed = row.status != "completed" || !row.diagnostics_complete;
        measurements.push(row);
        if failed {
            not_run = schedule[index + 1..]
                .iter()
                .map(|(round, case)| {
                    serde_json::json!({
                        "case": case.name, "repetition": round, "status": "not_run_after_failure"
                    })
                })
                .collect();
            break;
        }
    }
    // 前条件のworker回収は次の測定開始前に完了させる。
    drop(shared_driver);
    let reuse_observed = measurements.iter().any(|row| row.reused_worker);
    let condition_error = (policy == replay::WorkerPolicy::ReuseReady && !reuse_observed)
        .then_some("同一workerの再利用sessionを観測できませんでした。測定計画と前sessionの結果を確認してください");
    let success = condition_error.is_none()
        && !measurements.is_empty()
        && measurements.len() == schedule.len()
        && measurements
            .iter()
            .all(|row| row.status == "completed" && row.diagnostics_complete);
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
        "schema_version": 2,
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
        "worker_policy": match policy { replay::WorkerPolicy::FreshProcess => "fresh_process", replay::WorkerPolicy::ReuseReady => "reuse_ready_worker" },
        "repetitions": repeat,
        "planned_sessions": schedule.len(),
        "condition_error": condition_error,
        "reuse_observed": reuse_observed,
        "not_run": not_run,
        "trace_clock": "microseconds from Replay construction; preserved across reused sessions",
        "request_id_limitations": "production s0/p0 IDs are reused; a stale terminal arriving after a new same-ID send cannot be distinguished without a production protocol/ID change",
        "request_timing_scope": "transcribe queue acceptance to observed terminal; includes writer/engine queue and inference, not isolated inference",
        "cancel_timing_scope": "cancel queue acceptance to matching observed terminal; missing terminal remains unresolved, not zero",
        "cache_condition": "OS page/model cache not flushed; fresh process is not a cold-cache benchmark",
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
fn realtime_worker_conditions_and_repetition_counts_are_explicit() {
    assert_eq!(
        worker_mode("fresh").unwrap(),
        replay::WorkerPolicy::FreshProcess
    );
    assert_eq!(
        worker_mode("reuse").unwrap(),
        replay::WorkerPolicy::ReuseReady
    );
    assert!(worker_mode("cold").is_err());
    assert_eq!(repetitions("3").unwrap(), 3);
    for value in ["0", "11", "", "abc"] {
        assert!(repetitions(value).is_err());
    }
}

#[test]
fn request_timings_preserve_unfinished_requests_and_ignore_old_session_cancel_results() {
    use replay::{TraceEvent, TraceKind};
    let event = |at, kind| TraceEvent {
        run_elapsed_us: at,
        session: 2,
        pid: Some(10),
        request_id: Some("p0".into()),
        kind,
    };
    let events = vec![
        event(
            100,
            TraceKind::TranscribeQueued {
                partial: true,
                pcm_bytes: 3200,
            },
        ),
        event(
            110,
            TraceKind::Terminal {
                cancel_requested_session: Some(1),
                stop: "cancelled".into(),
                outcome: "cancelled".into(),
            },
        ),
    ];
    let audit = request_audit(&events);
    assert_eq!(audit.unmatched_terminals, 1);
    let unfinished = audit.timings;
    assert_eq!(unfinished.len(), 1);
    assert!(unfinished[0].terminal_run_us.is_none());
    let mut finished = events;
    finished.push(event(
        250,
        TraceKind::Terminal {
            cancel_requested_session: None,
            stop: "complete".into(),
            outcome: "text".into(),
        },
    ));
    assert_eq!(
        request_audit(&finished).timings[0].queue_to_terminal_us,
        Some(150)
    );
}

#[test]
fn terminal_before_transcribe_and_untranscribed_intervals_cannot_pass_diagnostics() {
    use replay::{TraceEvent, TraceKind};
    let event = |at, kind| TraceEvent {
        run_elapsed_us: at,
        session: 1,
        pid: Some(10),
        request_id: Some("s0".into()),
        kind,
    };
    let closed = event(10, TraceKind::IntervalClosed { pcm_bytes: 3200 });
    let sent = event(
        20,
        TraceKind::TranscribeQueued {
            partial: false,
            pcm_bytes: 3200,
        },
    );
    let terminal = event(
        30,
        TraceKind::Terminal {
            cancel_requested_session: None,
            stop: "complete".into(),
            outcome: "text".into(),
        },
    );
    let mut early_terminal = terminal.clone();
    early_terminal.run_elapsed_us = 15;
    for events in [
        vec![closed.clone(), early_terminal, sent.clone()],
        vec![closed.clone(), terminal.clone()],
    ] {
        let audit = request_audit(&events);
        assert_eq!(audit.unmatched_terminals, 1);
        let diagnostics = replay::Diagnostics {
            events,
            dropped_events: 0,
            unresolved_cancel_requests: 0,
        };
        let mut status = "completed".to_owned();
        let mut error = None;
        assert!(!apply_diagnostic_status(
            &mut status,
            &mut error,
            &diagnostics,
            &audit
        ));
        assert_eq!(status, "diagnostics_incomplete");
    }
    let events = vec![closed, sent, terminal];
    let audit = request_audit(&events);
    let diagnostics = replay::Diagnostics {
        events,
        dropped_events: 0,
        unresolved_cancel_requests: 0,
    };
    let mut status = "completed".to_owned();
    let mut error = None;
    assert!(apply_diagnostic_status(
        &mut status,
        &mut error,
        &diagnostics,
        &audit
    ));
}

#[test]
fn cancelled_upload_may_finish_without_transcribe_but_duplicate_terminals_are_incomplete() {
    use replay::{TraceEvent, TraceKind};
    let event = |at, kind| TraceEvent {
        run_elapsed_us: at,
        session: 1,
        pid: Some(10),
        request_id: Some("p0".into()),
        kind,
    };
    let mut events = vec![
        event(10, TraceKind::CancelQueued),
        event(
            20,
            TraceKind::Terminal {
                cancel_requested_session: Some(1),
                stop: "cancelled".into(),
                outcome: "cancelled".into(),
            },
        ),
    ];
    assert_eq!(request_audit(&events).unmatched_terminals, 0);
    events.push(events[1].clone());
    assert_eq!(request_audit(&events).unmatched_terminals, 1);
}

#[test]
fn final_session_with_an_unobserved_cancel_terminal_is_diagnostically_incomplete() {
    let diagnostics = replay::Diagnostics {
        events: Vec::new(),
        dropped_events: 0,
        unresolved_cancel_requests: 1,
    };
    let mut status = "completed".to_owned();
    let mut error = None;
    assert!(!apply_diagnostic_status(
        &mut status,
        &mut error,
        &diagnostics,
        &RequestAudit::default(),
    ));
    assert_eq!(status, "diagnostics_incomplete");
    assert!(error.unwrap().contains("未観測の取消終端: 1件"));
}

#[test]
fn incomplete_diagnostics_preserve_an_existing_recognition_failure() {
    let diagnostics = replay::Diagnostics {
        events: Vec::new(),
        dropped_events: 1,
        unresolved_cancel_requests: 0,
    };
    let mut status = "session_failed".to_owned();
    let mut error = Some("worker stopped".to_owned());
    assert!(!apply_diagnostic_status(
        &mut status,
        &mut error,
        &diagnostics,
        &RequestAudit::default(),
    ));
    assert_eq!(status, "session_failed");
    assert_eq!(error.as_deref(), Some("worker stopped"));
    let complete = replay::Diagnostics {
        events: Vec::new(),
        dropped_events: 0,
        unresolved_cancel_requests: 0,
    };
    let mut status = "completed".to_owned();
    let mut error = None;
    assert!(apply_diagnostic_status(
        &mut status,
        &mut error,
        &complete,
        &RequestAudit::default()
    ));
    assert_eq!(status, "completed");
    assert!(error.is_none());
}

#[test]
fn realtime_requires_each_requested_corpus() {
    let missing = std::env::temp_dir().join(format!("ohv-realtime-missing-{}", std::process::id()));
    assert!(cases(&missing, "").is_err());
    assert!(cases(&missing, "../short").is_err());
    assert!(cases(&missing, "short,1min").is_err());
}
