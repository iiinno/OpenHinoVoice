use crate::{
    asr, audio, config, context, format, history, hook, lexicon, overlay, paste, recovery, segment,
    settings, sound, startup, state, uia, usage,
};

use crate::feedback::FeedbackSettings;
use asr::Worker;
use audio::Recorder;
use config::Config;
use overlay::{Mode, Tone};
use sound::Sound;
use state::{Action, Event, OverlayKind, State};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS, LPARAM, WPARAM};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, MsgWaitForMultipleObjectsEx, PeekMessageW, TranslateMessage, MSG,
    MWMO_INPUTAVAILABLE, PM_REMOVE, QS_ALLINPUT, WM_QUIT,
};

// 10 minutes at 96 kHz mono PCM16 is 115.2 MB. Higher sample rates can hit the cap early.
// 区間認識では、この上限は「まだ結果が出ていない区間の合計」に対して数える
const MAX_RECORDING_PCM_BYTES: usize = 128 * 1024 * 1024;
const PCM_LIMIT_MESSAGE: &str = "録音データが128 MiBの安全上限を超えました（高サンプルレートのマイクでは設定時間より早く終了する場合があります）";
/// worker の死亡や時間切れで送り直す限界。連続でこれを超えたら録音を諦める
const MAX_ASR_FAILURES: u32 = 4;

/// 録音のあとに worker を残すか。残す時間の設定と、worker が ready かで決める
fn keep_worker_decision(minutes: u32, ready: bool) -> bool {
    minutes > 0 && ready
}

enum FormatOutcome {
    Done {
        generation: u64,
        text: String,
        fallback_chunks: usize,
    },
    Failed {
        generation: u64,
        reason: String,
    },
    /// 一時的な失敗のあと、attempt 回目の送り直しを待っている
    Retrying {
        generation: u64,
        attempt: u32,
    },
    /// Edit の書き換え結果
    EditDone {
        generation: u64,
        text: String,
    },
}

impl FormatOutcome {
    fn generation(&self) -> u64 {
        match self {
            Self::Done { generation, .. }
            | Self::Failed { generation, .. }
            | Self::Retrying { generation, .. }
            | Self::EditDone { generation, .. } => *generation,
        }
    }
}

/// Identifies the one formatting request whose result may still be applied.
#[derive(Default)]
struct FormatGeneration {
    next: u64,
    active: Option<u64>,
    /// 裏で送り直している整形に、もう要らないと伝える印
    cancel: Arc<AtomicBool>,
}

impl FormatGeneration {
    fn start(&mut self) -> u64 {
        self.cancel.store(true, Ordering::SeqCst);
        self.cancel = Arc::new(AtomicBool::new(false));
        self.next = self
            .next
            .checked_add(1)
            .expect("format generation exhausted");
        self.active = Some(self.next);
        self.next
    }

    fn cancel_flag(&self) -> Arc<AtomicBool> {
        self.cancel.clone()
    }

    fn invalidate(&mut self) {
        self.active = None;
        self.cancel.store(true, Ordering::SeqCst);
    }

    fn accepts(&self, generation: u64, formatting: bool) -> bool {
        formatting && self.active == Some(generation)
    }
}

/// 区間 1 つの認識の進み具合。結果が出るまで PCM を保持し、worker が落ちたときに送り直す
struct Interval {
    id: String,
    pcm: Arc<[u8]>,
    transcribe_at: Option<Instant>,
    /// 認識文。空の区間は空文字
    text: String,
    /// 終端を受け取った
    done: bool,
}

/// 押し戻しで送りきれなかった区間。chunk を送り終えたら transcribe を送る。
/// 話している途中の仮の認識(Partial)も同じ送り仕組みを使う
struct Sending {
    is_partial: bool,
    id: String,
    pcm: Arc<[u8]>,
    offset: usize,
}

impl Sending {
    fn new(is_partial: bool, id: String, pcm: Arc<[u8]>) -> Sending {
        Sending {
            is_partial,
            id,
            pcm,
            offset: 0,
        }
    }
}

/// 送った仮の認識。結果を表示するまで覚えておく
struct Partial {
    id: String,
}

/// 1 tick で worker に送る塊。0.1 秒分。録音中の送り方と同じ大きさにする
const ASR_CHUNK_BYTES: usize = 16000 / 10 * 2;

struct Session {
    quiet: bool,
    cfg: Config,
    asr_hints: String,
    audio_buffer: Vec<f32>,
    fallback_chunks: usize,
    microphone_notice: Option<String>,
    engine_preparing: bool,
    worker: Option<Worker>,
    recorder: Option<Recorder>,
    paste_target: Option<paste::PasteTarget>,
    input_app: Option<String>,
    recovery_id: Option<u64>,
    sample_rate: u32,
    stop_requested: bool,
    started_at: Instant,
    // 録音開始の音がマイクに入るので、鳴り終わるまでに録れた音は捨てる
    skip_until: Instant,
    segmenter: segment::Segmenter,
    intervals: Vec<Interval>,
    next_interval_index: usize,
    sending: Option<Sending>,
    /// Enter を押した時点。Enter から結果までの時間を記録するのに使う
    enter_at: Option<Instant>,
    /// 送った仮の認識と、確定した文章。帯の表示に使う
    partial: Option<Partial>,
    next_partial_index: usize,
    last_partial_at: Option<Instant>,
    confirmed_text: String,
    /// まだ認識結果が出ていない区間の PCM の合計。これが上限を超えたら止める
    unaccounted_bytes: usize,
    worker_spawned_at: Instant,
    /// worker の死亡や時間切れで送り直す回数。限界を超えたら録音を諦める
    asr_failures: u32,
    last_spawn_attempt: Option<Instant>,
}

impl Session {
    fn new(cfg: &Config) -> Session {
        Session {
            quiet: false,
            cfg: cfg.clone(),
            asr_hints: lexicon::asr_hints(&cfg.words, &cfg.terms),
            audio_buffer: Vec::new(),
            fallback_chunks: 0,
            microphone_notice: None,
            engine_preparing: true,
            worker: None,
            recorder: None,
            paste_target: None,
            input_app: None,
            recovery_id: None,
            sample_rate: 16000,
            stop_requested: false,
            started_at: Instant::now(),
            skip_until: Instant::now(),
            segmenter: segment::Segmenter::new(16000),
            intervals: Vec::new(),
            next_interval_index: 0,
            sending: None,
            enter_at: None,
            partial: None,
            next_partial_index: 0,
            last_partial_at: None,
            confirmed_text: String::new(),
            unaccounted_bytes: 0,
            worker_spawned_at: Instant::now(),
            asr_failures: 0,
            last_spawn_attempt: None,
        }
    }

    fn recording_time_limit(&self, now: Instant, max_minutes: u32) -> bool {
        now.saturating_duration_since(self.started_at)
            >= Duration::from_secs(max_minutes as u64 * 60)
    }

    fn log(&self, event: &str, fields: serde_json::Value) {
        if !self.quiet {
            usage::log(event, fields);
        }
    }

    fn push_samples(&mut self, samples: &[f32], now: Instant) -> bool {
        now < self.skip_until || self.feed_segmenter(samples)
    }

    /// 録音の音を区切りに渡し、閉じた区間を認識の待ち行列に入れる
    fn feed_segmenter(&mut self, samples: &[f32]) -> bool {
        for interval_pcm in self.segmenter.push(samples) {
            if !self.open_interval(interval_pcm) {
                return false;
            }
        }
        true
    }

    /// Enter のあとに呼ぶ。開いている区間を最後の区間として認識に回す
    fn close_open_interval(&mut self) -> bool {
        if let Some(samples) = self.segmenter.finish() {
            if !samples.is_empty() {
                return self.open_interval(samples);
            }
        }
        true
    }

    /// 閉じた区間を登録する。未認識の PCM の合計が上限を超えたら false
    fn open_interval(&mut self, samples: Vec<f32>) -> bool {
        let pcm = audio::samples_to_pcm_le(&samples);
        let bytes = pcm.len();
        if bytes > MAX_RECORDING_PCM_BYTES.saturating_sub(self.unaccounted_bytes) {
            return false;
        }
        let id = format!("s{}", self.next_interval_index);
        self.next_interval_index += 1;
        self.unaccounted_bytes += bytes;
        self.log(
            "interval_closed",
            serde_json::json!({
                "id": id,
                "ms": bytes as u64 * 500 / self.sample_rate.max(1) as u64,
                "bytes": bytes,
            }),
        );
        self.intervals.push(Interval {
            id,
            pcm: pcm.into(),
            transcribe_at: None,
            text: String::new(),
            done: false,
        });
        true
    }

    /// 区間の認識が終わった。PCM を捨てて、確定の文章を帯に映す
    fn complete_interval(&mut self, id: &str, text: &str) {
        let Some(interval) = self.intervals.iter_mut().find(|i| i.id == id) else {
            return;
        };
        if interval.done {
            return;
        }
        interval.done = true;
        interval.text = text.to_string();
        self.unaccounted_bytes = self.unaccounted_bytes.saturating_sub(interval.pcm.len());
        interval.pcm = Arc::from([]);
        self.asr_failures = 0;
        let fields = serde_json::json!({
            "id": interval.id,
            "latency_ms": interval
                .transcribe_at
                .map(|at| at.elapsed().as_millis() as u64)
                .unwrap_or(0),
            "empty": text.is_empty(),
            "chars": text.chars().count(),
        });
        self.log("interval_result", fields);
        self.confirmed_text = self.joined_text();
    }

    /// 順番に結果をつないだ文章。結果が出ていない区間は空になり、
    /// すべて出てから呼ぶことを想定する
    fn joined_text(&self) -> String {
        let mut text = String::new();
        for interval in self.intervals.iter().filter(|interval| interval.done) {
            text.push_str(interval.text.trim());
        }
        text
    }

    fn all_intervals_done(&self) -> bool {
        self.intervals.iter().all(|i| i.done)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum OutputFeedback {
    Success,
    Warning(String),
    Failure(String),
    Cancelled,
}

impl OutputFeedback {
    fn event(&self) -> Sound {
        match self {
            Self::Success => Sound::Done,
            Self::Warning(_) => Sound::Warning,
            Self::Failure(_) => Sound::Error,
            Self::Cancelled => Sound::Cancel,
        }
    }

    fn presentation(&self) -> (Tone, String, Duration) {
        match self {
            Self::Success => (
                Tone::Success,
                "入力しました".into(),
                Duration::from_millis(600),
            ),
            Self::Warning(message) => (Tone::Warn, message.clone(), Duration::from_millis(2500)),
            Self::Failure(message) => (Tone::Error, message.clone(), Duration::from_millis(3500)),
            Self::Cancelled => (
                Tone::Info,
                "取り消しました".into(),
                Duration::from_millis(500),
            ),
        }
    }
}

fn output_feedback(
    error: Option<&str>,
    volatile: bool,
    fallback: bool,
    notice: Option<OverlayKind>,
) -> OutputFeedback {
    if let Some(error) = error {
        return OutputFeedback::Failure(format!("入力失敗。トレイでテキストをコピー（{error}）"));
    }
    if volatile {
        return OutputFeedback::Warning("入力しました。履歴はトレイから回収できます".into());
    }
    if fallback {
        return OutputFeedback::Warning("一部を原文のまま入力しました".into());
    }
    match notice {
        Some(OverlayKind::FormatSkipped { reason }) => {
            OutputFeedback::Warning(format!("生テキストを送りました（{reason}）"))
        }
        Some(OverlayKind::ApiKeyMissing) => {
            OutputFeedback::Warning("APIキー未設定のため生テキストを送りました".into())
        }
        Some(OverlayKind::Error { message } | OverlayKind::EditUnavailable { message }) => {
            OutputFeedback::Warning(message)
        }
        _ => OutputFeedback::Success,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct WorkerKey {
    executable: String,
    model: String,
}
impl WorkerKey {
    fn for_config(cfg: &Config) -> Self {
        Self {
            executable: cfg.speech_exe.clone(),
            model: cfg.model.clone(),
        }
    }
}
struct KeptWorker {
    worker: Worker,
    key: WorkerKey,
    expires: Instant,
}
struct PendingPaste {
    text: String,
    target: Option<paste::PasteTarget>,
    recovery_id: u64,
    deadline: Instant,
}

// shutdown may wait for a blocked writer; keep that wait away from the UI loop.
fn retire_worker(mut worker: Worker) {
    worker.request_stop();
    std::thread::spawn(move || worker.shutdown());
}

struct App {
    #[cfg(test)]
    fixture_recorder: Option<Recorder>,
    replay: Option<Box<ReplayCapture>>,
    cfg: Config,
    state: State,
    session: Session,
    /// 録音のあとに残した認識エンジン。keep_worker_minutes の設定があるときだけ
    kept_worker: Option<KeptWorker>,
    format_tx: Sender<FormatOutcome>,
    /// 整形の結果の受け口。main のループが読み、テストも使う
    format_rx: Receiver<FormatOutcome>,
    format_generation: FormatGeneration,
    transient_until: Option<Instant>,
    recoveries: recovery::Recoveries,
    last_paste_text: Option<String>,
    pending_paste: Option<PendingPaste>,
    paste_transaction: Option<paste::PasteTransaction>,
    latest_delta: Arc<Mutex<Option<(u64, String)>>>,
    quit_started: Option<Instant>,
    start_requested: bool,
    paste_notice: Option<OverlayKind>,
}

/// 整形の結果の記録に使う、成功かどうか
fn outcome_ok(outcome: &FormatOutcome) -> bool {
    matches!(
        outcome,
        FormatOutcome::Done { .. } | FormatOutcome::EditDone { .. }
    )
}

/// 整形の結果の記録に使う、失敗の理由
fn outcome_reason(outcome: &FormatOutcome) -> Option<String> {
    match outcome {
        FormatOutcome::Failed { reason, .. } => Some(reason.clone()),
        _ => None,
    }
}

fn mirror_of(state: &State) -> u8 {
    match state {
        State::Idle => hook::MIRROR_IDLE,
        State::Recording | State::Editing { .. } => hook::MIRROR_RECORDING,
        State::Recognizing { .. } | State::EditRecognizing { .. } => hook::MIRROR_RECOGNIZING,
        State::Formatting { .. } | State::EditFormatting { .. } => hook::MIRROR_FORMATTING,
    }
}

impl App {
    fn new(cfg: Config) -> Self {
        let (format_tx, format_rx) = channel();
        Self {
            replay: None,
            #[cfg(test)]
            fixture_recorder: None,
            session: Session::new(&cfg),
            cfg,
            state: State::Idle,
            kept_worker: None,
            format_tx,
            format_rx,
            format_generation: FormatGeneration::default(),
            transient_until: None,
            recoveries: recovery::Recoveries::default(),
            last_paste_text: None,
            pending_paste: None,
            paste_transaction: None,
            latest_delta: Arc::new(Mutex::new(None)),
            quit_started: None,
            start_requested: false,
            paste_notice: None,
        }
    }

    fn start_recorder(&mut self) -> Result<Recorder, String> {
        #[cfg(test)]
        if let Some(recorder) = self.fixture_recorder.take() {
            return Ok(recorder);
        }
        Recorder::start_with_device(Some(self.cfg.microphone.as_str()))
    }

    fn retire_worker(&mut self, mut worker: Worker) {
        if let Some(replay) = &mut self.replay {
            worker.request_stop();
            replay
                .retired
                .push(std::thread::spawn(move || worker.shutdown()));
        } else {
            retire_worker(worker);
        }
    }

    fn asr_caption(&mut self, partial: &str) {
        if let Some(replay) = &mut self.replay {
            if !partial.is_empty() {
                replay.first_partial_at.get_or_insert_with(Instant::now);
            }
        } else {
            overlay::set_caption(&self.session.confirmed_text, partial);
        }
    }

    fn set_state(&mut self, next: State) {
        self.state = next;
        self.sync_mirror();
    }

    fn sync_mirror(&self) {
        if self.replay.is_some() {
            return;
        }
        crate::preview_ipc::set_available(
            matches!(self.state, State::Idle)
                && self.pending_paste.is_none()
                && self.paste_transaction.is_none()
                && !self.start_requested
                && self.quit_started.is_none(),
        );
        hook::set_mirror(
            if self.pending_paste.is_some()
                || self.paste_transaction.is_some()
                || self.start_requested
            {
                hook::MIRROR_FORMATTING
            } else {
                mirror_of(&self.state)
            },
        );
    }

    fn dispatch(&mut self, event: Event) {
        let cancelled = matches!(&event, Event::Esc)
            && (!matches!(self.state, State::Idle)
                || self.pending_paste.is_some()
                || self.start_requested);
        if matches!(&event, Event::Esc) {
            self.paste_notice = None;
        }
        if let Some(replay) = &mut self.replay {
            match &event {
                Event::AsrDone { text, .. } => {
                    replay.completed_at = Some(Instant::now());
                    replay.outcome = Some(Ok(text.clone()));
                }
                Event::AsrError { message } => replay.outcome = Some(Err(message.clone())),
                _ => {}
            }
        }
        if self.pending_paste.is_some() || self.paste_transaction.is_some() {
            match event {
                Event::F8 => self.start_requested = true,
                Event::Esc => {
                    self.start_requested = false;
                    if let Some(pending) = self.pending_paste.take() {
                        self.recoveries.abandon(pending.recovery_id);
                    }
                }
                _ => {}
            }
            self.sync_mirror();
            if cancelled {
                self.present_output(OutputFeedback::Cancelled);
            }
            return;
        }
        let current = std::mem::replace(&mut self.state, State::Idle);
        let (next, actions) = state::step(current, event);
        self.set_state(next);
        // Abort an action batch when StartSession or Paste fails. Otherwise a
        // subsequent recording/success notification could overwrite the error.
        state::run_actions(actions, |action| self.execute(action));
        // Edit の整形も同じ世代の受け口で続くので、無効化しない
        if !matches!(
            self.state,
            State::Formatting { .. } | State::EditFormatting { .. }
        ) {
            self.format_generation.invalidate();
        }
        if matches!(self.state, State::Idle) && self.pending_paste.is_none() {
            if let Some(id) = self.session.recovery_id.take() {
                self.recoveries.abandon(id);
            }
        }
        self.sync_mirror();
        if cancelled {
            self.present_output(OutputFeedback::Cancelled);
        }
    }

    fn fail_session(&mut self, message: String) {
        if let Some(replay) = &mut self.replay {
            replay.outcome = Some(Err(message.clone()));
        }
        self.teardown_session();
        self.set_state(State::Idle);
        self.execute(Action::ShowOverlay(OverlayKind::Error { message }));
    }

    fn teardown_session(&mut self) {
        if let Some(id) = self.session.recovery_id.take() {
            self.recoveries.abandon(id);
        }
        self.session.recorder = None;
        self.session.paste_target = None;
        self.session.stop_requested = false;
        if let Some(worker) = self.session.worker.take() {
            self.retire_worker(worker);
        }
        self.session.segmenter = segment::Segmenter::new(self.session.sample_rate);
        self.session.intervals.clear();
        self.session.sending = None;
        self.session.partial = None;
        self.session.next_partial_index = 0;
        self.session.confirmed_text.clear();
        self.session.unaccounted_bytes = 0;
        self.session.next_interval_index = 0;
        self.session.asr_failures = 0;
        self.session.last_spawn_attempt = None;
        if self.replay.is_none() {
            overlay::clear_caption();
        }
    }

    /// Returns false if this action fails; subsequent actions in the transition must be skipped.
    fn execute(&mut self, action: Action) -> bool {
        if let Some(replay) = &mut self.replay {
            match &action {
                Action::Paste { .. } | Action::ShowOverlay(_) | Action::HideOverlay => return true,
                Action::CallFormat { .. } | Action::CallEditFormat { .. } => {
                    replay.outcome = Some(Err("認識検証にAI整形が要求されました".into()));
                    return false;
                }
                _ => {}
            }
        }
        match action {
            Action::StartSession => {
                self.paste_notice = None;
                self.transient_until = None;
                if self.replay.is_none() {
                    crate::preview_ipc::invalidate();
                }
                if let Err(message) = asr::check_offline_model(
                    &self.cfg.speech_exe,
                    &self.cfg.model,
                    self.cfg.format.offline,
                ) {
                    self.fail_session(message);
                    return false;
                }
                // 残しておいた worker があれば、起動とモデルの読み込みを飛ばして使う
                let mut reused_worker = false;
                let kept = match self.kept_worker.take() {
                    Some(KeptWorker {
                        mut worker,
                        key,
                        expires,
                    }) if Instant::now() < expires && key == WorkerKey::for_config(&self.cfg) => {
                        worker.forget_ids();
                        reused_worker = true;
                        Some(worker)
                    }
                    Some(KeptWorker { worker, .. }) => {
                        self.retire_worker(worker);
                        None
                    }
                    None => None,
                };
                let recovery_id = if self.replay.is_none() {
                    match self.recoveries.reserve(self.cfg.recovery_history) {
                        Ok(id) => Some(id),
                        Err(message) => {
                            self.fail_session(message);
                            return false;
                        }
                    }
                } else {
                    None
                };
                self.session = Session::new(&self.cfg);
                self.session.quiet = self.replay.is_some();
                self.session.recovery_id = recovery_id;
                self.session.worker = kept;
                if self.replay.is_none() {
                    match paste::capture_target() {
                        Ok(target) => {
                            self.session.input_app = context::process_name(target.process_id());
                            self.session.paste_target = Some(target);
                        }
                        Err(e) => {
                            self.fail_session(e);
                            return false;
                        }
                    }
                }
                if self.session.worker.is_none() {
                    match Worker::spawn(&self.session.cfg.speech_exe, &self.session.cfg.model) {
                        Ok(worker) => {
                            self.session.worker = Some(worker);
                        }
                        Err(e) => {
                            self.fail_session(e.to_string());
                            return false;
                        }
                    }
                }
                self.session.engine_preparing = !self.session.worker.as_ref().unwrap().is_ready();
                if let Some(replay) = &mut self.replay {
                    replay.worker_started(
                        self.session.worker.as_ref().unwrap().process_id(),
                        reused_worker,
                    );
                    self.session.sample_rate = replay.sample_rate;
                    self.session.segmenter = segment::Segmenter::new(replay.sample_rate);
                } else {
                    sound::stop();
                    match self.start_recorder() {
                        Ok(recorder) => {
                            if let Some(notice) = recorder.fallback_notice() {
                                self.session.microphone_notice = Some(notice.to_string());
                            }
                            self.session.sample_rate = recorder.sample_rate;
                            self.session.segmenter = segment::Segmenter::new(recorder.sample_rate);
                            self.session.recorder = Some(recorder);
                            let exclusion =
                                sound::start_recording(FeedbackSettings::from_config(&self.cfg));
                            self.session.skip_until = Instant::now() + exclusion;
                        }
                        Err(e) => {
                            self.fail_session(e);
                            return false;
                        }
                    }
                }
                self.session.log(
                    "session_start",
                    serde_json::json!({
                        "sample_rate": self.session.sample_rate,
                        "microphone": self.session.recorder.as_ref().map(Recorder::device_name),
                        "reused_worker": reused_worker,
                        "edit_mode": self.cfg.edit_mode,
                    }),
                );
                if self.cfg.has_api_key() {
                    let cfg = self.cfg.format.clone();
                    std::thread::spawn(move || format::prewarm(&cfg));
                }
            }
            Action::StopRecordingAndTranscribe => {
                if let Some(recorder) = self.session.recorder.take() {
                    let mut samples = std::mem::take(&mut self.session.audio_buffer);
                    recorder.drain_into(&mut samples);
                    let capture_error = recorder.take_error();
                    drop(recorder);
                    if let Some(message) = capture_error {
                        self.fail_session(message);
                        return false;
                    }
                    if !self.session.feed_segmenter(&samples) {
                        self.fail_session(PCM_LIMIT_MESSAGE.into());
                        return false;
                    }
                }
                if !self.session.close_open_interval() {
                    self.fail_session(PCM_LIMIT_MESSAGE.into());
                    return false;
                }
                // 仮の認識はもう要らない。指示の途中の文字がそのまま貼られるのを防ぐ
                let _ = self.cancel_partial();
                if self.cfg.live_caption {
                    self.asr_caption("");
                }
                self.session.enter_at = Some(Instant::now());
                self.session.stop_requested = true;
                self.sfx(Sound::Stop);
            }
            Action::DiscardSession => {
                self.teardown_session();
            }
            Action::CallFormat { raw } => {
                let mut cfg = self.session.cfg.format.clone();
                cfg.offline |= self.cfg.format.offline;
                cfg.prompt = lexicon::prompt_with_terms(
                    &cfg.prompt,
                    &self.session.cfg.terms,
                    self.session.input_app.as_deref(),
                );
                let tx = self.format_tx.clone();
                let latest = self.latest_delta.clone();
                let generation = self.format_generation.start();
                let cancel = self.format_generation.cancel_flag();
                // 整形の間は、録音中の帯の代わりに届いた整形文を流す
                if self.cfg.live_caption {
                    overlay::set_caption("", "");
                }
                std::thread::spawn(move || {
                    let cancelled = || cancel.load(Ordering::SeqCst);
                    let started = Instant::now();
                    // 長い文章は段落ごとに分けて送る。1 つが失敗したら、その部分だけ
                    // 整形前の文を使う。短い文章では 1 回の依頼にまとめられる。
                    // 応答が届いた分は帯に流す
                    let result = format::format_long(
                        &cfg,
                        &raw,
                        &cancelled,
                        |attempt| {
                            let _ = tx.send(FormatOutcome::Retrying {
                                generation,
                                attempt,
                            });
                        },
                        &|text: &str| {
                            *latest.lock().unwrap_or_else(|p| p.into_inner()) =
                                Some((generation, text.to_string()));
                            hook::wake_resident();
                        },
                    );
                    if !cancelled() {
                        // 履歴の保存が失敗しても、完成した整形テキストは失わない。
                        let legacy = result
                            .as_ref()
                            .map(|done| done.text.clone())
                            .map_err(|error| format::FormatError::Provider(error.to_string()));
                        if let Err(e) = history::record(&cfg, &raw, &legacy, started.elapsed()) {
                            eprintln!("整形履歴を書き込めません: {e}");
                        }
                    }
                    let outcome = match result {
                        Ok(done) => FormatOutcome::Done {
                            generation,
                            text: done.text,
                            fallback_chunks: done.fallback_chunks,
                        },
                        Err(e) => FormatOutcome::Failed {
                            generation,
                            reason: e.to_string(),
                        },
                    };
                    usage::log(
                        "format_result",
                        serde_json::json!({
                            "ok": outcome_ok(&outcome),
                            "ms": started.elapsed().as_millis() as u64,
                            "reason": outcome_reason(&outcome),
                        }),
                    );
                    let _ = tx.send(outcome);
                    hook::wake_resident();
                });
            }
            Action::CallEditFormat {
                selection,
                instruction,
            } => {
                // 置き換える前の文と後の文を、復旧用の履歴に両方残す。
                // うまくいかなかったときに元へ戻せる
                if let Some(id) = self.session.recovery_id {
                    self.recoveries.update_raw(id, selection.clone());
                }
                let mut cfg = self.session.cfg.format.clone();
                cfg.offline |= self.cfg.format.offline;
                cfg.prompt = lexicon::prompt_with_terms(
                    &cfg.prompt,
                    &self.session.cfg.terms,
                    self.session.input_app.as_deref(),
                );
                let tx = self.format_tx.clone();
                let generation = self.format_generation.start();
                let cancel = self.format_generation.cancel_flag();
                std::thread::spawn(move || {
                    let cancelled = || cancel.load(Ordering::SeqCst);
                    let started = Instant::now();
                    let result = format::format_edit(
                        &cfg,
                        &selection,
                        &instruction,
                        &cancelled,
                        |attempt| {
                            let _ = tx.send(FormatOutcome::Retrying {
                                generation,
                                attempt,
                            });
                        },
                    );
                    if !cancelled() {
                        if let Err(e) =
                            history::record(&cfg, &selection, &result, started.elapsed())
                        {
                            eprintln!("Edit の整形履歴を書き込めません: {e}");
                        }
                    }
                    let outcome = match result {
                        Ok(text) => FormatOutcome::EditDone { generation, text },
                        Err(e) => FormatOutcome::Failed {
                            generation,
                            reason: e.to_string(),
                        },
                    };
                    let _ = tx.send(outcome);
                    hook::wake_resident();
                });
            }
            Action::Paste { text } => {
                overlay::clear_caption();
                let text = lexicon::replace_explicit(&text, &self.session.cfg.terms);
                self.last_paste_text = Some(text.clone());
                overlay::set_last_text_available(true);
                let id = match self.session.recovery_id {
                    Some(id) => id,
                    None => match self.recoveries.reserve(self.cfg.recovery_history) {
                        Ok(id) => {
                            self.recoveries.update_raw(id, text.clone());
                            id
                        }
                        Err(message) => {
                            self.show_overlay(OverlayKind::Error { message });
                            return false;
                        }
                    },
                };
                self.recoveries.set_formatted(id, text.clone());
                self.pending_paste = Some(PendingPaste {
                    text,
                    target: self.session.paste_target.take(),
                    recovery_id: id,
                    deadline: Instant::now() + Duration::from_secs(5),
                });
                self.session.recovery_id = None;
                self.sync_mirror();
                return true;
            }
            Action::ShowOverlay(kind) => {
                if self.pending_paste.is_some() {
                    self.paste_notice = Some(kind);
                } else if matches!(kind, OverlayKind::Recording)
                    && self.session.microphone_notice.is_some()
                {
                    let message = self.session.microphone_notice.take().unwrap();
                    self.show_overlay(OverlayKind::Error { message });
                } else {
                    self.show_overlay(kind);
                }
            }
            Action::HideOverlay => {
                // 貼付失敗直後の Error が後続 HideOverlay に消されないよう、
                // 一時通知が表示中なら期限満了まで維持する。
                if self.transient_until.is_none() && self.pending_paste.is_none() {
                    overlay::hide();
                }
            }
        }
        true
    }

    /// Remove a RAM recovery only after Windows accepted the user's explicit copy.
    fn copy_failed_text(&mut self) {
        if self.paste_transaction.is_some() {
            return;
        }
        let Some((id, text)) = self.recoveries.first_text() else {
            return;
        };
        match paste::copy_text_to_clipboard(text, overlay::hwnd()) {
            Ok(()) => {
                self.recoveries.copied(id);
                overlay::set_recovery_count(self.recoveries.count());
                overlay::show(Mode::Notice {
                    tone: Tone::Warn,
                    text: format!(
                        "回収テキストをコピーしました（残り{}件）",
                        self.recoveries.count()
                    ),
                });
                self.transient_until = Some(Instant::now() + Duration::from_millis(2500));
            }
            Err(error) => self.show_overlay(OverlayKind::Error {
                message: format!("コピーできませんでした（{error}）。トレイから再試行できます"),
            }),
        }
    }

    fn copy_last_text(&mut self) {
        let Some(text) = self.last_paste_text.as_ref() else {
            return;
        };
        match paste::copy_text_to_clipboard(text, overlay::hwnd()) {
            Ok(()) => self.show_notice(
                Tone::Warn,
                "直近の認識テキストをコピーしました".into(),
                Duration::from_millis(2500),
            ),
            Err(error) => self.show_overlay(OverlayKind::Error {
                message: format!("コピーできませんでした（{error}）。トレイから再試行できます"),
            }),
        }
    }

    fn tick_output(&mut self, now: Instant) {
        let recoverable_before = self.recoveries.count();
        self.recoveries.tick(hook::wake_resident);
        if self.recoveries.count() > recoverable_before {
            self.show_notice(
                Tone::Warn,
                "履歴保存に失敗。原文はトレイから回収できます".into(),
                Duration::from_millis(3500),
            );
        }
        if self
            .paste_transaction
            .as_ref()
            .is_some_and(|transaction| transaction.deadline() <= now)
        {
            if let Err(message) = self.paste_transaction.take().unwrap().restore() {
                self.show_notice(Tone::Warn, message, Duration::from_millis(3500));
            }
        }
        if self.pending_paste.as_ref().is_some_and(|pending| {
            self.recoveries.ready(pending.recovery_id) || pending.deadline <= now
        }) {
            let pending = self.pending_paste.take().unwrap();
            if !self.recoveries.ready(pending.recovery_id) {
                self.recoveries
                    .mark_volatile(pending.recovery_id, "履歴保存が時間内に完了しませんでした");
            }
            let result = pending
                .target
                .ok_or_else(|| "録音開始時の入力先が不明です".to_string())
                .and_then(|target| {
                    paste::PasteTransaction::begin(&pending.text, target, overlay::hwnd(), now)
                });
            self.recoveries
                .mark_paste(pending.recovery_id, result.is_ok());
            usage::log(
                "paste",
                serde_json::json!({"ok": result.is_ok(), "error": result.as_ref().err()}),
            );
            let feedback = output_feedback(
                result.as_ref().err().map(String::as_str),
                self.recoveries.volatile(pending.recovery_id),
                self.session.fallback_chunks > 0,
                self.paste_notice.take(),
            );
            if let Ok(transaction) = result {
                self.paste_transaction = Some(transaction);
            }
            self.present_output(feedback);
            self.paste_notice = None;
        }
        self.recoveries.tick(hook::wake_resident);
        overlay::set_recovery_count(self.recoveries.count());
        let delta = self
            .latest_delta
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        if let Some((generation, text)) = delta {
            if self.cfg.live_caption
                && self
                    .format_generation
                    .accepts(generation, matches!(self.state, State::Formatting { .. }))
            {
                overlay::set_caption(&text, "");
            }
        }
        if self.start_requested
            && self.pending_paste.is_none()
            && self.paste_transaction.is_none()
            && self.quit_started.is_none()
        {
            self.start_requested = false;
            self.dispatch(Event::F8);
        }
        self.sync_mirror();
    }

    fn should_exit(&mut self, requested: bool, now: Instant) -> bool {
        requested && self.exit_ready(now)
    }

    fn exit_ready(&mut self, now: Instant) -> bool {
        if self.quit_started.is_none() {
            self.quit_started = Some(now);
            self.start_requested = false;
            self.format_generation.invalidate();
            if let Some(pending) = self.pending_paste.take() {
                self.recoveries.abandon(pending.recovery_id);
            }
            self.teardown_session();
            self.set_state(State::Idle);
        }
        if self.paste_transaction.is_some() {
            return false;
        }
        if self.recoveries.has_unfinished_writes() {
            if now.saturating_duration_since(self.quit_started.unwrap()) < Duration::from_secs(5) {
                return false;
            }
            self.recoveries.expire_pending();
        }
        if self.recoveries.count() > 0 {
            self.show_overlay(OverlayKind::Error {
                message: "終了前に、未回収の文章をトレイからコピーしてください".into(),
            });
            overlay::EXIT_REQUESTED.store(false, Ordering::SeqCst);
            self.quit_started = None;
            return false;
        }
        true
    }

    fn wait_duration(&self, now: Instant) -> Duration {
        let mut wait = idle_sleep_duration(&self.state, false, overlay::needs_animation());
        for deadline in [
            self.transient_until,
            self.pending_paste.as_ref().map(|p| p.deadline),
            self.paste_transaction.as_ref().map(|p| p.deadline()),
            self.quit_started.map(|at| at + Duration::from_secs(5)),
            crate::preview_ipc::deadline(),
            overlay::next_deadline(),
        ]
        .into_iter()
        .flatten()
        {
            wait = wait.min(deadline.saturating_duration_since(now));
        }
        wait
    }

    fn sfx(&self, event: Sound) {
        if self.replay.is_none() {
            sound::play_event(FeedbackSettings::from_config(&self.cfg), event);
        }
    }

    fn show_notice(&mut self, tone: Tone, text: String, duration: Duration) {
        if self.replay.is_some() {
            return;
        }
        overlay::show(Mode::Notice { tone, text });
        self.transient_until = Some(Instant::now() + duration);
    }

    fn present_output(&mut self, feedback: OutputFeedback) {
        self.sfx(feedback.event());
        let (tone, text, duration) = feedback.presentation();
        self.show_notice(tone, text, duration);
    }

    fn engine_preparing_update(&mut self, ready: bool) -> Option<bool> {
        let preparing = !ready;
        let changed = self.session.engine_preparing != preparing;
        self.session.engine_preparing = preparing;
        (changed
            && self.replay.is_none()
            && self.transient_until.is_none()
            && self.pending_paste.is_none())
        .then_some(preparing)
    }

    fn show_overlay(&mut self, kind: OverlayKind) {
        if self.transient_until.is_some()
            && matches!(
                &kind,
                OverlayKind::Recording | OverlayKind::Editing { .. } | OverlayKind::Recognizing
            )
        {
            return;
        }
        match kind {
            OverlayKind::Recording => {
                self.transient_until = None;
                overlay::show(Mode::Recording {
                    started: self.session.started_at,
                    preparing: self.session.engine_preparing,
                });
            }
            OverlayKind::Recognizing => {
                self.transient_until = None;
                overlay::show(Mode::Recognizing {
                    preparing: self.session.engine_preparing,
                });
            }
            OverlayKind::Formatting => {
                self.transient_until = None;
                overlay::show(Mode::Busy {
                    label: "整形中".into(),
                    hint: Some("Enter で生テキスト".into()),
                });
            }
            OverlayKind::FormatSkipped { reason } => {
                overlay::show(Mode::Notice {
                    tone: Tone::Warn,
                    text: format!("生テキストを貼りました（{reason}）"),
                });
                self.transient_until = Some(Instant::now() + Duration::from_millis(2500));
            }
            OverlayKind::ApiKeyMissing => {
                overlay::show(Mode::Notice {
                    tone: Tone::Warn,
                    text: "APIキー未設定のため生テキストを貼りました".into(),
                });
                self.transient_until = Some(Instant::now() + Duration::from_millis(2500));
            }
            OverlayKind::Error { message } => {
                usage::log("error", serde_json::json!({"message": message}));
                self.sfx(Sound::Error);
                overlay::show(Mode::Notice {
                    tone: Tone::Error,
                    text: message,
                });
                self.transient_until = Some(Instant::now() + Duration::from_millis(3500));
            }
            OverlayKind::Editing { head } => {
                self.transient_until = None;
                overlay::show(Mode::Editing {
                    started: self.session.started_at,
                    head,
                    preparing: self.session.engine_preparing,
                });
            }
            OverlayKind::EditUnavailable { message } => {
                overlay::show(Mode::Notice {
                    tone: Tone::Warn,
                    text: message,
                });
                self.transient_until = Some(Instant::now() + Duration::from_millis(2500));
            }
        }
    }

    fn tick_session(&mut self) {
        let in_recording = matches!(self.state, State::Recording | State::Editing { .. });
        let in_recognizing = matches!(
            self.state,
            State::Recognizing { .. } | State::EditRecognizing { .. }
        );
        if !in_recording && !in_recognizing {
            return;
        }

        if let Some(recorder) = &self.session.recorder {
            if let Some(message) = recorder.take_error() {
                self.dispatch(Event::AsrError { message });
                return;
            }
            let mut samples = std::mem::take(&mut self.session.audio_buffer);
            recorder.drain_into(&mut samples);
            overlay::push_level(&samples);
            let within_limit = self.session.push_samples(&samples, Instant::now());
            samples.clear();
            self.session.audio_buffer = samples;
            if !within_limit {
                self.dispatch(Event::AsrError {
                    message: PCM_LIMIT_MESSAGE.into(),
                });
                return;
            }
        }

        if in_recording
            && self
                .session
                .recording_time_limit(Instant::now(), self.session.cfg.max_recording_minutes)
        {
            // Reuse the manual Enter transition, including final microphone
            // drain, Stop sound and recognition; no new state or GUI.
            self.dispatch(Event::Enter);
            return;
        }

        // worker のいない時間(送り直しの途中)は、起動を試みて ready を待つ
        match self.ensure_worker() {
            Ok(true) => {}
            Ok(false) => return,
            Err(message) => {
                self.dispatch(Event::AsrError { message });
                return;
            }
        }

        let mut restart = false;
        let mut fatal: Option<String> = None;

        // Ack の確認は、worker が stdin を読まなくなっても止まらない
        if let Err(e) = self.session.worker.as_mut().unwrap().poll_writer() {
            restart = true;
            self.session.asr_failures += 1;
            let _ = e;
        }

        // ready の確認。時間切れは送り直しの対象にする
        let mut ready = false;
        if !restart {
            let worker = self.session.worker.as_mut().unwrap();
            ready = worker.is_ready();
            if !ready {
                match worker.try_ready() {
                    Ok(r) => ready = r,
                    Err(_) => {
                        restart = true;
                        self.session.asr_failures += 1;
                    }
                }
            }
        }
        if !restart
            && !ready
            && asr::ready_timed_out(self.session.worker_spawned_at, Instant::now())
        {
            restart = true;
            self.session.asr_failures += 1;
        }
        if let Some(preparing) = self.engine_preparing_update(ready && !restart) {
            overlay::set_engine_preparing(preparing);
        }
        if ready && !restart {
            if let Some(replay) = &mut self.replay {
                if replay.worker_ready_at.is_none() {
                    replay.worker_ready_at = Some(Instant::now());
                    replay.record(replay.worker_pid, None, replay::TraceKind::WorkerReady);
                }
            }
        }

        // 区間ごとの時間切れ。1 区間の認識が 90 秒を超えたら送り直す
        if !restart {
            let now = Instant::now();
            let stalled = self.session.intervals.iter().any(|i| {
                !i.done
                    && i.transcribe_at.is_some_and(|at| {
                        now.saturating_duration_since(at) >= asr::TRANSCRIPTION_TIMEOUT
                    })
            });
            if stalled {
                restart = true;
                self.session.asr_failures += 1;
            }
        }

        if ready && !restart && (!in_recording || self.session.intervals.iter().any(|i| !i.done)) {
            match self.cancel_partial() {
                Ok(true) => {}
                Ok(false) => return,
                Err(_) => {
                    restart = true;
                    self.session.asr_failures += 1;
                }
            }
        }

        // 終端メッセージを読み、区間の結果として記録する。仮の認識(p*)の
        // 終端は帯に映すだけで、確定の処理には影響しない。
        // ready 前は try_ready だけが受信する。ここで読むと ready を捨ててしまう。
        if ready && !restart {
            loop {
                let polled = self.session.worker.as_mut().unwrap().poll_terminal();
                let Some(result) = polled else { break };
                if let (Some(replay), Ok(terminal)) = (&mut self.replay, &result) {
                    replay.terminal(self.session.worker.as_ref().unwrap().process_id(), terminal);
                }
                match result {
                    Ok(terminal) if terminal.id.starts_with('p') => {
                        // 今の仮の結果だけを映す。止めた古い仮の終端は無視する
                        let is_current = self
                            .session
                            .partial
                            .as_ref()
                            .is_some_and(|p| p.id == terminal.id);
                        if !is_current {
                            continue;
                        }
                        self.session.partial = None;
                        if self.cfg.live_caption {
                            match terminal.outcome {
                                asr::TerminalOutcome::Text(text) => {
                                    self.asr_caption(text.as_deref().unwrap_or(""))
                                }
                                _ => self.asr_caption(""),
                            }
                        }
                    }
                    Ok(terminal) if terminal.stop == "model_limit" => {
                        fatal = Some("音声が認識モデルの上限に達しました。確定した原文はトレイから回収できます".into());
                    }
                    Ok(terminal) => {
                        match terminal.outcome {
                            asr::TerminalOutcome::Text(text) => {
                                self.session
                                    .complete_interval(&terminal.id, text.as_deref().unwrap_or(""));
                                if self.cfg.live_caption {
                                    self.asr_caption("");
                                }
                                if !matches!(
                                    self.state,
                                    State::Editing { .. } | State::EditRecognizing { .. }
                                ) {
                                    self.update_recovery_history();
                                    if self.session.confirmed_text.len() > 1024 * 1024 {
                                        fatal = Some("原文が保存上限に達しました。トレイからコピーしてください".into());
                                        break;
                                    }
                                }
                            }
                            asr::TerminalOutcome::Error(_) | asr::TerminalOutcome::Cancelled => {
                                restart = true;
                                self.session.asr_failures += 1;
                            }
                        }
                    }
                    Err(_) => {
                        restart = true;
                        self.session.asr_failures += 1;
                    }
                }
                if restart {
                    break;
                }
            }
        }

        // 送り直しの回数が限界を超えたら録音を諦める
        if self.session.asr_failures > MAX_ASR_FAILURES {
            fatal = Some("認識エンジンが繰り返し停止しました".into());
        }

        // 送り直し。worker を殺し、結果が出ていない区間を送り直すために印を戻す
        if restart && fatal.is_none() {
            self.restart_worker();
        }

        // 送りパイプライン。1 区間ずつ chunk を送り、終わったら transcribe を送る。
        // 仮の認識も同じ仕組みで送る
        if ready && !restart && fatal.is_none() {
            let mut send_failed = false;
            let sending = self.session.sending.as_mut();
            if let Some(sending) = sending {
                let worker = self.session.worker.as_mut().unwrap();
                while sending.offset < sending.pcm.len() {
                    let end = (sending.offset + ASR_CHUNK_BYTES).min(sending.pcm.len());
                    let chunk = &sending.pcm[sending.offset..end];
                    match worker.send_chunk(&sending.id, chunk) {
                        Ok(true) => {
                            sending.offset = end;
                        }
                        Ok(false) => break, // 押し戻し。同じ chunk を次の tick で送る
                        Err(_) => {
                            send_failed = true;
                            break;
                        }
                    }
                }
                if sending.offset == sending.pcm.len() && !send_failed {
                    let id = sending.id.clone();
                    match worker.transcribe(&id, self.session.sample_rate, &self.session.asr_hints)
                    {
                        Ok(true) => {
                            if let Some(replay) = &mut self.replay {
                                replay.record(
                                    Some(worker.process_id()),
                                    Some(&id),
                                    replay::TraceKind::TranscribeQueued {
                                        partial: sending.is_partial,
                                        pcm_bytes: sending.pcm.len(),
                                    },
                                );
                            }
                            if sending.is_partial {
                                self.session.partial = Some(Partial { id: id.clone() });
                                self.session.log(
                                    "partial_sent",
                                    serde_json::json!({
                                        "id": id,
                                        "open_ms": self.session.segmenter.open_samples().len()
                                            as u64
                                            * 1000
                                            / self.session.sample_rate.max(1) as u64,
                                    }),
                                );
                            } else if let Some(interval) =
                                self.session.intervals.iter_mut().find(|i| i.id == id)
                            {
                                interval.transcribe_at = Some(Instant::now());
                            }
                            self.session.sending = None;
                        }
                        Ok(false) => {} // 列が満杯。次の tick で送り直す
                        Err(_) => send_failed = true,
                    }
                }
            } else if !self
                .session
                .intervals
                .iter()
                .any(|i| !i.done && i.transcribe_at.is_some())
            {
                if let Some(interval) = self
                    .session
                    .intervals
                    .iter()
                    .find(|i| !i.done && i.transcribe_at.is_none())
                {
                    self.session.sending = Some(Sending::new(
                        false,
                        interval.id.clone(),
                        interval.pcm.clone(),
                    ));
                }
            }
            if send_failed {
                self.session.asr_failures += 1;
                if self.session.asr_failures > MAX_ASR_FAILURES {
                    fatal = Some("認識エンジンとの通信に失敗しました".into());
                } else {
                    self.restart_worker();
                }
            }
        }

        if let Some(message) = fatal {
            self.dispatch(Event::AsrError { message });
            return;
        }

        if in_recording {
            self.tick_partial();
        }

        if in_recognizing && self.session.stop_requested && self.session.all_intervals_done() {
            let text = self.session.joined_text();
            self.session.recorder = None;
            if text.is_empty() {
                self.dispatch(Event::AsrError {
                    message: "音声を認識できませんでした（結果が空です）".into(),
                });
                return;
            }
            self.session.log(
                "asr_done",
                serde_json::json!({
                    "enter_to_result_ms": self
                        .session
                        .enter_at
                        .map(|at| at.elapsed().as_millis() as u64)
                        .unwrap_or(0),
                    "intervals": self.session.intervals.len(),
                    "chars": text.chars().count(),
                }),
            );
            if let Some(worker) = self.session.worker.take() {
                if keep_worker_decision(self.cfg.keep_worker_minutes, worker.is_ready())
                    && WorkerKey::for_config(&self.cfg) == WorkerKey::for_config(&self.session.cfg)
                {
                    self.kept_worker = Some(KeptWorker {
                        worker,
                        key: WorkerKey::for_config(&self.session.cfg),
                        expires: Instant::now()
                            + Duration::from_secs(u64::from(self.cfg.keep_worker_minutes) * 60),
                    });
                } else {
                    self.retire_worker(worker);
                }
            }
            let has_api_key = self.session.cfg.has_api_key();
            self.dispatch(Event::AsrDone { text, has_api_key });
        }
    }

    /// 話している途中の仮の認識。1 秒ごとに、閉じていない区間のその時点までの音を
    /// 別の要求(p*)として送る。確定の処理を優先し、確定が残っている間は送らない。
    /// 新しい仮を送るときは、前の仮を cancel する
    fn tick_partial(&mut self) {
        if !self.cfg.live_caption || !self.cfg.partial_caption {
            return;
        }
        if self.session.sending.is_some() || self.session.intervals.iter().any(|i| !i.done) {
            return;
        }
        if self
            .session
            .last_partial_at
            .is_some_and(|at| at.elapsed() < Duration::from_secs(1))
        {
            return;
        }
        let audio = self.session.segmenter.open_samples();
        if audio.len() < self.session.sample_rate as usize / 4 {
            // 0.25 秒より短いうちは送らない。1 枠のクリックが仮の文字になるのを防ぐ
            return;
        }
        let pcm = audio::samples_to_pcm_le(audio);
        if self.session.worker.is_none() || !matches!(self.cancel_partial(), Ok(true)) {
            return;
        }
        let id = format!("p{}", self.session.next_partial_index);
        self.session.next_partial_index += 1;
        self.session.last_partial_at = Some(Instant::now());
        self.session.sending = Some(Sending::new(true, id, pcm.into()));
    }

    fn restart_worker(&mut self) {
        if let Some(preparing) = self.engine_preparing_update(false) {
            overlay::set_engine_preparing(preparing);
        }
        self.session.log(
            "asr_resend",
            serde_json::json!({"failures": self.session.asr_failures}),
        );
        if let Some(worker) = self.session.worker.take() {
            self.retire_worker(worker);
        }
        self.session.sending = None;
        self.session.partial = None;
        self.session.last_spawn_attempt = None;
        for interval in &mut self.session.intervals {
            if !interval.done {
                interval.transcribe_at = None;
            }
        }
    }

    fn cancel_partial(&mut self) -> Result<bool, asr::AsrError> {
        if self.session.sending.as_ref().is_some_and(|s| s.is_partial) {
            let sending = self.session.sending.take().unwrap();
            self.session.partial = Some(Partial { id: sending.id });
        }
        let Some(partial) = &self.session.partial else {
            return Ok(true);
        };
        let Some(worker) = self.session.worker.as_mut() else {
            return Ok(false);
        };
        if !worker.cancel(&partial.id)? {
            return Ok(false);
        }
        if let Some(replay) = &mut self.replay {
            replay.cancel_queued(worker.process_id(), &partial.id);
        }
        self.session.partial = None;
        Ok(true)
    }

    /// worker がいれば true を返す。いなければ起動し直しを試みる(1 秒に 1 回)。
    /// 起動に失敗し続けたときは Err
    fn ensure_worker(&mut self) -> Result<bool, String> {
        if self.session.worker.is_some() {
            return Ok(true);
        }
        if self
            .session
            .last_spawn_attempt
            .is_some_and(|at| at.elapsed() < Duration::from_secs(1))
        {
            return Ok(false);
        }
        self.session.last_spawn_attempt = Some(Instant::now());
        asr::check_offline_model(
            &self.session.cfg.speech_exe,
            &self.session.cfg.model,
            self.session.cfg.format.offline || self.cfg.format.offline,
        )?;
        match Worker::spawn(&self.session.cfg.speech_exe, &self.session.cfg.model) {
            Ok(worker) => {
                if let Some(replay) = &mut self.replay {
                    replay.worker_started(worker.process_id(), false);
                }
                self.session.worker = Some(worker);
                self.session.worker_spawned_at = Instant::now();
                Ok(true)
            }
            Err(e) => {
                self.session.asr_failures += 1;
                if self.session.asr_failures > MAX_ASR_FAILURES {
                    Err(e.to_string())
                } else {
                    Ok(false)
                }
            }
        }
    }

    /// 区間の認識が終わるたびに、復旧履歴を途中までの文章で書き足す。
    /// アプリが途中で落ちても、それまでの文章は残る
    fn update_recovery_history(&mut self) {
        if let Some(id) = self.session.recovery_id {
            self.recoveries
                .update_raw(id, self.session.confirmed_text.clone());
        }
    }

    fn receive_format(&mut self, outcome: FormatOutcome) {
        // Edit の整形(EditFormatting)も同じ受け口を使う
        let formatting = matches!(
            self.state,
            State::Formatting { .. } | State::EditFormatting { .. }
        );
        if !self
            .format_generation
            .accepts(outcome.generation(), formatting)
        {
            return;
        }
        match outcome {
            FormatOutcome::Done {
                text,
                fallback_chunks,
                ..
            } => {
                self.session.fallback_chunks = fallback_chunks;
                self.dispatch(Event::FormatDone { text });
            }
            FormatOutcome::EditDone { text, .. } => self.dispatch(Event::EditFormatDone { text }),
            FormatOutcome::Failed { reason, .. } => self.dispatch(Event::FormatFailed { reason }),
            FormatOutcome::Retrying { attempt, .. } => overlay::show(Mode::Busy {
                label: "整形中".into(),
                hint: Some(format!(
                    "再試行 {attempt}/{} · Enter で生テキスト",
                    format::MAX_ATTEMPTS - 1
                )),
            }),
        }
    }
}

#[derive(Default)]
struct ReplayCapture {
    sample_rate: u32,
    worker_ready_at: Option<Instant>,
    first_partial_at: Option<Instant>,
    completed_at: Option<Instant>,
    outcome: Option<Result<String, String>>,
    retired: Vec<std::thread::JoinHandle<()>>,
    run_started_at: Option<Instant>,
    session_index: u32,
    worker_pid: Option<u32>,
    spawn_count: u32,
    reused_worker: bool,
    trace: Vec<replay::TraceEvent>,
    dropped_trace_events: usize,
    pending_cancels: Vec<(u32, String, Instant, u32)>,
}

impl ReplayCapture {
    const TRACE_LIMIT: usize = 32_768;

    fn record(&mut self, pid: Option<u32>, id: Option<&str>, kind: replay::TraceKind) {
        let now = Instant::now();
        let epoch = *self.run_started_at.get_or_insert(now);
        if self.trace.len() >= Self::TRACE_LIMIT {
            self.dropped_trace_events += 1;
            return;
        }
        self.trace.push(replay::TraceEvent {
            run_elapsed_us: now.saturating_duration_since(epoch).as_micros(),
            session: self.session_index,
            pid,
            request_id: id.map(str::to_owned),
            kind,
        });
    }

    fn worker_started(&mut self, pid: u32, reused: bool) {
        self.worker_pid = Some(pid);
        self.reused_worker |= reused;
        if !reused {
            self.spawn_count += 1;
            self.worker_ready_at = None;
        }
        self.record(
            Some(pid),
            None,
            if reused {
                replay::TraceKind::WorkerReused
            } else {
                replay::TraceKind::WorkerSpawned
            },
        );
        if reused {
            self.worker_ready_at = Some(Instant::now());
            self.record(Some(pid), None, replay::TraceKind::WorkerReady);
        }
    }

    fn cancel_queued(&mut self, pid: u32, id: &str) {
        if self.pending_cancels.len() < Self::TRACE_LIMIT {
            self.pending_cancels
                .push((pid, id.to_owned(), Instant::now(), self.session_index));
        } else {
            self.dropped_trace_events += 1;
        }
        self.record(Some(pid), Some(id), replay::TraceKind::CancelQueued);
    }

    fn terminal(&mut self, pid: u32, terminal: &asr::Terminal) {
        let cancelled_in = self
            .pending_cancels
            .iter()
            .find(|(owner, id, _, _)| *owner == pid && *id == terminal.id)
            .map(|(_, _, _, session)| *session);
        self.record(
            Some(pid),
            Some(&terminal.id),
            replay::TraceKind::Terminal {
                cancel_requested_session: cancelled_in,
                stop: terminal.stop.clone(),
                outcome: match &terminal.outcome {
                    asr::TerminalOutcome::Text(_) => "text",
                    asr::TerminalOutcome::Error(_) => "error",
                    asr::TerminalOutcome::Cancelled => "cancelled",
                }
                .into(),
            },
        );
        if let Some(index) = self
            .pending_cancels
            .iter()
            .position(|(owner, id, _, _)| *owner == pid && *id == terminal.id)
        {
            let (_, id, at, session) = self.pending_cancels.remove(index);
            self.record(
                Some(pid),
                Some(&id),
                replay::TraceKind::CancelTerminalObserved {
                    requested_session: session,
                    wait_us: at.elapsed().as_micros(),
                },
            );
        }
    }
}

/// 検証専用。本番の状態遷移と認識処理を共有し、音声源と出力先だけを置き換える。
#[doc(hidden)]
pub mod replay {
    use super::*;
    use std::path::Path;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum WorkerPolicy {
        FreshProcess,
        ReuseReady,
    }

    #[derive(Debug, Clone, serde::Serialize)]
    #[serde(tag = "event", rename_all = "snake_case")]
    pub enum TraceKind {
        SessionStarted {
            sample_rate: u32,
        },
        WorkerSpawned,
        WorkerReused,
        WorkerReady,
        IntervalClosed {
            pcm_bytes: usize,
        },
        TranscribeQueued {
            partial: bool,
            pcm_bytes: usize,
        },
        Terminal {
            cancel_requested_session: Option<u32>,
            stop: String,
            outcome: String,
        },
        CancelQueued,
        CancelTerminalObserved {
            requested_session: u32,
            wait_us: u128,
        },
        Enter,
    }

    #[derive(Debug, Clone, serde::Serialize)]
    pub struct TraceEvent {
        pub run_elapsed_us: u128,
        pub session: u32,
        pub pid: Option<u32>,
        pub request_id: Option<String>,
        #[serde(flatten)]
        pub kind: TraceKind,
    }

    #[derive(Debug, serde::Serialize)]
    pub struct Diagnostics {
        pub events: Vec<TraceEvent>,
        pub dropped_events: usize,
        pub unresolved_cancel_requests: usize,
    }

    pub struct Replay {
        app: App,
        policy: WorkerPolicy,
    }

    pub struct Snapshot {
        pub session_index: u32,
        pub worker_pid: Option<u32>,
        pub worker_spawn_count: u32,
        pub reused_worker: bool,
        pub worker_ready_at: Option<Instant>,
        pub first_partial_at: Option<Instant>,
        pub enter_at: Option<Instant>,
        pub completed_at: Option<Instant>,
        /// 開いている音声と未認識の確定区間。送信コピーは重複加算しない。
        pub pending_pcm_bytes: usize,
    }

    impl Replay {
        pub fn new(speech: &Path, model: &Path, sample_rate: u32) -> Result<Self, String> {
            Self::new_with_policy(speech, model, sample_rate, WorkerPolicy::FreshProcess)
        }

        pub fn new_with_policy(
            speech: &Path,
            model: &Path,
            sample_rate: u32,
            policy: WorkerPolicy,
        ) -> Result<Self, String> {
            if !config::is_test_sandbox() {
                return Err("verify.ps1の隔離環境で実行してください".into());
            }
            if !speech.is_absolute()
                || !speech.is_file()
                || !model.is_absolute()
                || !model.is_file()
            {
                return Err("speechと取得済みモデルの絶対パスが必要です".into());
            }
            if !(8_000..=192_000).contains(&sample_rate) {
                return Err("検証音声のサンプルレートが範囲外です".into());
            }
            // load()を使わず、認証情報や利用者設定を参照しない。
            let mut cfg = config::parse(config::EXAMPLE_TOML)?;
            cfg.speech_exe = speech.to_str().ok_or("speechのパスが不正です")?.into();
            cfg.model = model.to_str().ok_or("モデルのパスが不正です")?.into();
            cfg.sounds = false;
            cfg.recovery_history = false;
            cfg.format.usage_log = false;
            cfg.live_caption = true;
            cfg.partial_caption = true;
            cfg.edit_mode = false;
            cfg.keep_worker_minutes = if policy == WorkerPolicy::ReuseReady {
                60
            } else {
                0
            };
            cfg.max_recording_minutes = 60;
            cfg.format.offline = true;
            cfg.format.save_history = false;
            cfg.format.api_key.clear();
            cfg.format.openai_api_key.clear();
            cfg.format.provider = "disabled".into();
            let mut app = App::new(cfg);
            app.replay = Some(Box::new(ReplayCapture {
                sample_rate,
                session_index: 1,
                run_started_at: Some(Instant::now()),
                ..ReplayCapture::default()
            }));
            app.replay.as_mut().unwrap().record(
                None,
                None,
                TraceKind::SessionStarted { sample_rate },
            );
            let mut replay = Self { app, policy };
            replay.app.dispatch(Event::F8);
            if let Some(Err(error)) = replay.outcome() {
                return Err(error.to_string());
            }
            Ok(replay)
        }

        /// 成功した直前のsessionが保持した同一workerだけを再利用する。
        pub fn next_session(&mut self, sample_rate: u32) -> Result<(), String> {
            if self.policy != WorkerPolicy::ReuseReady {
                return Err("FreshProcessでは新しいReplayを作成してください".into());
            }
            if !(8_000..=192_000).contains(&sample_rate)
                || !matches!(self.app.state, State::Idle)
                || !matches!(self.outcome(), Some(Ok(_)))
            {
                return Err("成功したsessionの完了後に有効なsample_rateで再開してください".into());
            }
            let capture = self.app.replay.as_ref().unwrap();
            let expected_pid = capture.worker_pid;
            let spawns = capture.spawn_count;
            if capture
                .pending_cancels
                .iter()
                .any(|(pid, _, _, _)| Some(*pid) == expected_pid)
            {
                return Err(
                    "前sessionの取消終端が未観測のため、同一IDを再使用する測定を続行できません"
                        .into(),
                );
            }
            let reusable = self.app.kept_worker.as_ref().is_some_and(|kept| {
                kept.worker.is_ready()
                    && Some(kept.worker.process_id()) == expected_pid
                    && kept.expires > Instant::now()
                    && kept.key == WorkerKey::for_config(&self.app.cfg)
            });
            if !reusable {
                return Err(
                    "同一PIDのready workerが保持されていないため再利用測定を続行できません".into(),
                );
            }
            let capture = self.app.replay.as_mut().unwrap();
            capture.sample_rate = sample_rate;
            capture.session_index += 1;
            capture.worker_ready_at = None;
            capture.first_partial_at = None;
            capture.completed_at = None;
            capture.outcome = None;
            capture.reused_worker = false;
            capture.record(
                expected_pid,
                None,
                TraceKind::SessionStarted { sample_rate },
            );
            self.app.dispatch(Event::F8);
            if let Some(Err(error)) = self.outcome() {
                return Err(error.into());
            }
            let capture = self.app.replay.as_ref().unwrap();
            if capture.worker_pid != expected_pid
                || capture.spawn_count != spawns
                || !capture.reused_worker
            {
                return Err("再利用条件が崩れました。PIDとspawn履歴を確認してください".into());
            }
            Ok(())
        }

        fn record_intervals_since(&mut self, index: usize) {
            let capture = self.app.replay.as_mut().unwrap();
            for interval in &self.app.session.intervals[index..] {
                capture.record(
                    capture.worker_pid,
                    Some(&interval.id),
                    TraceKind::IntervalClosed {
                        pcm_bytes: interval.pcm.len(),
                    },
                );
            }
        }

        pub fn take_diagnostics(&mut self) -> Diagnostics {
            let capture = self.app.replay.as_mut().unwrap();
            Diagnostics {
                events: std::mem::take(&mut capture.trace),
                dropped_events: std::mem::take(&mut capture.dropped_trace_events),
                unresolved_cancel_requests: capture.pending_cancels.len(),
            }
        }

        pub fn push_samples(&mut self, samples: &[f32]) -> Result<(), String> {
            if !matches!(self.app.state, State::Recording) || self.outcome().is_some() {
                return Err("録音状態のセッションだけに音声を供給できます".into());
            }
            let intervals_before = self.app.session.intervals.len();
            if !self.app.session.push_samples(samples, Instant::now()) {
                self.app.dispatch(Event::AsrError {
                    message: PCM_LIMIT_MESSAGE.into(),
                });
                return Err(PCM_LIMIT_MESSAGE.into());
            }
            self.record_intervals_since(intervals_before);
            Ok(())
        }

        pub fn tick(&mut self) {
            self.app.tick_session();
        }

        pub fn enter(&mut self) {
            let intervals_before = self.app.session.intervals.len();
            let capture = self.app.replay.as_mut().unwrap();
            capture.record(capture.worker_pid, None, TraceKind::Enter);
            self.app.dispatch(Event::Enter);
            if self.app.session.intervals.len() >= intervals_before {
                self.record_intervals_since(intervals_before);
            }
        }

        pub fn wait_duration(&self) -> Duration {
            self.app.wait_duration(Instant::now())
        }

        pub fn snapshot(&self) -> Snapshot {
            let replay = self.app.replay.as_ref().unwrap();
            Snapshot {
                session_index: replay.session_index,
                worker_pid: replay.worker_pid,
                worker_spawn_count: replay.spawn_count,
                reused_worker: replay.reused_worker,
                worker_ready_at: replay.worker_ready_at,
                first_partial_at: replay.first_partial_at,
                enter_at: self.app.session.enter_at,
                completed_at: replay.completed_at,
                pending_pcm_bytes: self.app.session.unaccounted_bytes.saturating_add(
                    self.app
                        .session
                        .segmenter
                        .open_samples()
                        .len()
                        .saturating_mul(2),
                ),
            }
        }

        pub fn outcome(&self) -> Option<Result<&str, &str>> {
            self.app
                .replay
                .as_ref()
                .unwrap()
                .outcome
                .as_ref()
                .map(|result| result.as_deref().map_err(String::as_str))
        }
    }

    impl Drop for Replay {
        fn drop(&mut self) {
            // 測定の時刻を確定したあと、自分が作ったworkerだけを回収する。
            if let Some(worker) = self.app.session.worker.take() {
                worker.shutdown();
            }
            if let Some(kept) = self.app.kept_worker.take() {
                kept.worker.shutdown();
            }
            for handle in self.app.replay.as_mut().unwrap().retired.drain(..) {
                let _ = handle.join();
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn driver(worker: Worker) -> Replay {
            let mut cfg = config::parse(config::EXAMPLE_TOML).unwrap();
            cfg.sounds = false;
            cfg.live_caption = true;
            cfg.partial_caption = true;
            cfg.format.offline = true;
            cfg.format.provider = "disabled".into();
            let mut app = App::new(cfg);
            app.replay = Some(Box::new(ReplayCapture {
                sample_rate: 16000,
                ..ReplayCapture::default()
            }));
            app.session.quiet = true;
            app.replay.as_mut().unwrap().session_index = 1;
            app.replay
                .as_mut()
                .unwrap()
                .worker_started(worker.process_id(), false);
            app.session.worker = Some(worker);
            app.state = State::Recording;
            Replay {
                app,
                policy: WorkerPolicy::FreshProcess,
            }
        }

        fn wait_for_transcribe(
            replay: &mut Replay,
            sent: &Arc<Mutex<String>>,
            id: &str,
            expected: usize,
        ) {
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                replay.tick();
                let count = sent
                    .lock()
                    .unwrap()
                    .lines()
                    .filter(|line| {
                        serde_json::from_str::<serde_json::Value>(line)
                            .is_ok_and(|value| value["type"] == "transcribe" && value["id"] == id)
                    })
                    .count();
                if count >= expected {
                    return;
                }
                assert!(
                    Instant::now() < deadline,
                    "transcribe must be written before its terminal is supplied"
                );
                std::thread::yield_now();
            }
        }

        #[test]
        fn replay_enter_drains_through_the_production_completion_transition() {
            let (tx, rx) = channel();
            let (worker, sent) = asr::test_worker_with_recorder(rx);
            let mut replay = driver(worker);
            assert!(replay.app.session.open_interval(vec![0.3; 1600]));
            replay.enter();
            assert!(matches!(replay.app.state, State::Recognizing { .. }));
            assert!(replay.app.session.stop_requested);
            wait_for_transcribe(&mut replay, &sent, "s0", 1);
            tx.send(r#"{"type":"end","id":"s0","text":"検証の文章","stop":"complete"}"#.into())
                .unwrap();
            replay.tick();
            assert_eq!(replay.outcome(), Some(Ok("検証の文章")));
            assert_eq!(replay.app.state, State::Idle);
            assert!(replay.app.pending_paste.is_none());
            assert!(replay.app.paste_transaction.is_none());
            assert!(replay.snapshot().completed_at.is_some());
            assert_eq!(replay.snapshot().pending_pcm_bytes, 0);
        }

        #[test]
        fn completed_sessions_reuse_the_same_pid_without_another_spawn() {
            let (tx, rx) = channel();
            let (worker, sent) = asr::test_worker_with_recorder(rx);
            let mut replay = driver(worker);
            replay.policy = WorkerPolicy::ReuseReady;
            replay.app.cfg.keep_worker_minutes = 60;
            replay.app.cfg.format.offline = false;
            let pid = replay.snapshot().worker_pid;
            for session in 1..=2 {
                if session > 1 {
                    replay.next_session(16_000).unwrap();
                    assert!(replay.snapshot().worker_ready_at.is_some());
                    assert!(replay.outcome().is_none());
                }
                replay.app.session.open_interval(vec![0.3; 1600]);
                replay.enter();
                wait_for_transcribe(&mut replay, &sent, "s0", session as usize);
                tx.send(format!(
                    r#"{{"type":"end","id":"s0","text":"session {session}","stop":"complete"}}"#
                ))
                .unwrap();
                replay.tick();
                assert!(replay.outcome().unwrap().is_ok());
                assert_eq!(replay.snapshot().worker_pid, pid);
                assert_eq!(replay.snapshot().worker_spawn_count, 1);
                assert_eq!(replay.snapshot().session_index, session);
                assert_eq!(replay.snapshot().reused_worker, session > 1);
                assert!(replay.snapshot().worker_ready_at.is_some());
                assert!(replay.app.session.recorder.is_none());
                assert!(replay.app.pending_paste.is_none());
            }
            replay
                .app
                .replay
                .as_mut()
                .unwrap()
                .cancel_queued(pid.unwrap(), "p99");
            assert!(replay
                .next_session(16_000)
                .unwrap_err()
                .contains("取消終端"));
            assert_eq!(replay.snapshot().worker_spawn_count, 1);
            let diagnostics = replay.take_diagnostics();
            assert_eq!(
                diagnostics
                    .events
                    .iter()
                    .filter(|e| matches!(e.kind, TraceKind::WorkerReused))
                    .count(),
                1
            );
        }

        #[test]
        fn session_reuse_rejects_an_active_session_and_a_fresh_process_policy() {
            let (_tx, rx) = channel();
            let mut replay = driver(asr::test_worker(rx, false));
            assert!(replay.next_session(16_000).is_err());
            replay.policy = WorkerPolicy::ReuseReady;
            assert!(replay.next_session(16_000).is_err());
            assert_eq!(replay.app.state, State::Recording);
            assert_eq!(replay.snapshot().worker_spawn_count, 1);
        }

        #[test]
        fn cancel_diagnostics_wait_for_an_observed_terminal_and_keep_its_origin_session() {
            let mut capture = ReplayCapture {
                session_index: 1,
                ..ReplayCapture::default()
            };
            capture.cancel_queued(99, "p0");
            assert_eq!(capture.pending_cancels.len(), 1);
            assert!(!capture
                .trace
                .iter()
                .any(|event| matches!(event.kind, TraceKind::CancelTerminalObserved { .. })));
            capture.session_index = 2;
            capture.terminal(
                99,
                &asr::Terminal {
                    id: "p0".into(),
                    stop: "cancelled".into(),
                    outcome: asr::TerminalOutcome::Cancelled,
                },
            );
            assert!(capture.pending_cancels.is_empty());
            assert!(capture.trace.iter().any(|event| matches!(
                event.kind,
                TraceKind::CancelTerminalObserved {
                    requested_session: 1,
                    ..
                }
            ) && event.session == 2));
        }

        #[test]
        fn diagnostic_trace_has_a_hard_bound_and_reports_dropped_events() {
            let mut capture = ReplayCapture::default();
            for _ in 0..=ReplayCapture::TRACE_LIMIT {
                capture.record(Some(1), None, TraceKind::WorkerReady);
            }
            assert_eq!(capture.trace.len(), ReplayCapture::TRACE_LIMIT);
            assert_eq!(capture.dropped_trace_events, 1);
        }

        #[test]
        fn replay_observes_only_the_current_nonempty_partial_result() {
            let (tx, rx) = channel();
            let mut replay = driver(asr::test_worker(rx, false));
            replay.app.session.partial = Some(Partial { id: "p1".into() });
            tx.send(r#"{"type":"end","id":"p0","text":"古い仮認識","stop":"complete"}"#.into())
                .unwrap();
            replay.tick();
            assert!(replay.snapshot().first_partial_at.is_none());
            tx.send(r#"{"type":"end","id":"p1","text":"現在の仮認識","stop":"complete"}"#.into())
                .unwrap();
            replay.tick();
            assert!(replay.snapshot().first_partial_at.is_some());
        }
    }
}

pub fn run() {
    let settings_mode = std::env::args().any(|a| a == "--settings");
    // 二重に起動するとキーの横取りが重なるので、2 つ目は何もせずに終わる。設定ウィンドウも 1 つだけ開く
    let _single = unsafe {
        let name = if settings_mode {
            "Local\\OpenHinoVoice.Settings"
        } else {
            "Local\\OpenHinoVoice.Singleton"
        };
        let handle = CreateMutexW(
            None,
            true,
            &windows::core::HSTRING::from(config::mutex_name(name)),
        );
        if GetLastError() == ERROR_ALREADY_EXISTS {
            return;
        }
        handle
    };
    // 高解像度の画面で表示がぼやけないよう、モニターごとの拡大率を自分で扱う
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    config::migrate_legacy_dir();
    if settings_mode {
        settings::run();
        return;
    }
    startup::migrate_legacy();
    let cfg = match config::load() {
        Ok(c) => c,
        Err(e) => {
            overlay::show_error(&e);
            return;
        }
    };
    usage::ENABLED.store(cfg.format.usage_log, std::sync::atomic::Ordering::Relaxed);
    let hotkey = config::parse_hotkey(&cfg.hotkey).unwrap_or(config::Hotkey { mods: 0, vk: 0x77 });
    overlay::set_theme(&cfg.theme);
    let feedback = FeedbackSettings::from_config(&cfg);
    overlay::set_feedback(feedback.hud_style, feedback.motion);
    overlay::set_caption_anchor(&cfg.caption_position);
    let mut config_mtime = config_modified();
    let mut last_config_check = Instant::now();

    if let Err(e) = overlay::create() {
        overlay::show_error(&e);
        return;
    }

    if let Err(error) = crate::preview_ipc::register_endpoint(overlay::hwnd()) {
        eprintln!("{error}");
    }

    let (key_tx, key_rx) = channel::<hook::KeyEvent>();
    if let Err(e) = hook::install(key_tx, hotkey) {
        overlay::show_error(&e);
        return;
    }

    // オフラインモードでは認識モデルの自動取得が起こらない。未取得のままなら
    // 起動時に 1 回だけ案内を出す(取得はしない)
    let model_notice = if cfg.format.offline {
        Some(asr::model_fetched(&cfg.speech_exe, &cfg.model))
    } else {
        None
    };

    let mut app = App::new(cfg);
    app.sync_mirror();
    if app.cfg.startup_sound {
        app.sfx(Sound::Startup);
        app.show_notice(
            Tone::Startup,
            "OpenHinoVoice を起動しました".into(),
            Duration::from_millis(600),
        );
    }

    if let Some(Ok(false)) = model_notice {
        overlay::show(Mode::Notice {
            tone: Tone::Warn,
            text: "認識モデルが未取得です。オンラインのときに 1 回取得してください".into(),
        });
        app.transient_until = Some(Instant::now() + Duration::from_secs(8));
    } else if let Some(Err(e)) = model_notice {
        eprintln!("認識モデルの取得状況を確認できません: {e}");
    }

    let mut msg = MSG::default();
    loop {
        // Cancellation must run before any queued input can be sent.
        if app.should_exit(
            overlay::EXIT_REQUESTED.load(Ordering::SeqCst),
            Instant::now(),
        ) {
            break;
        }
        app.tick_output(Instant::now());

        if overlay::take_recovery_request() {
            app.copy_failed_text();
        }
        if app.paste_transaction.is_none() && overlay::take_last_text_request() {
            app.copy_last_text();
        }
        // 残していた worker の時間が来たら殺す
        if let Some(kept) = app.kept_worker.take() {
            if Instant::now() < kept.expires {
                app.kept_worker = Some(kept);
            } else {
                retire_worker(kept.worker);
            }
        }
        unsafe {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_QUIT {
                    overlay::EXIT_REQUESTED.store(true, std::sync::atomic::Ordering::SeqCst);
                    break;
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        if app.should_exit(
            overlay::EXIT_REQUESTED.load(Ordering::SeqCst),
            Instant::now(),
        ) {
            break;
        }

        while let Ok(key) = key_rx.try_recv() {
            match key {
                hook::KeyEvent::Hotkey if app.quit_started.is_some() => {
                    hook::acknowledge_hotkey();
                }
                hook::KeyEvent::Hotkey => {
                    // Edit モード。文を選んだ状態でいつもの録音開始キーを押すと
                    // Edit になる。選択の確認は録音の開始を遅らせるので、
                    // 100 ミリ秒で打ち切る。読めなければ通常の音声入力になる
                    let edit_selection = if app.cfg.edit_mode
                        && matches!(app.state, State::Idle)
                        && app.pending_paste.is_none()
                        && app.paste_transaction.is_none()
                    {
                        match uia::probe_selected_text(Duration::from_millis(100)) {
                            uia::SelectionProbe::Selected(text) => {
                                usage::log(
                                    "edit_probe",
                                    serde_json::json!({
                                        "result": "selected",
                                        "chars": text.chars().count(),
                                    }),
                                );
                                if text.chars().count() > uia::MAX_SELECTION_CHARS {
                                    Some(Err("選択が長すぎるため Edit を使えません".into()))
                                } else {
                                    Some(Ok(text))
                                }
                            }
                            uia::SelectionProbe::Unavailable => {
                                usage::log(
                                    "edit_probe",
                                    serde_json::json!({"result": "unavailable"}),
                                );
                                Some(Err("このアプリでは Edit を使えません".into()))
                            }
                            uia::SelectionProbe::TimedOut => Some(Err(
                                "選択の確認が時間内に終わらなかったため、通常の録音を始めます"
                                    .into(),
                            )),
                            uia::SelectionProbe::None => {
                                usage::log("edit_probe", serde_json::json!({"result": "none"}));
                                None
                            }
                        }
                    } else {
                        None
                    };
                    app.dispatch(Event::F8);
                    hook::acknowledge_hotkey();
                    match edit_selection {
                        Some(Ok(text)) => {
                            if app.cfg.has_api_key() {
                                app.dispatch(Event::EditModeStarted { selection: text });
                            } else {
                                app.dispatch(Event::EditNotice {
                                    message: "Edit には整形(AI)の接続先が必要です".into(),
                                });
                            }
                        }
                        Some(Err(message)) => {
                            app.dispatch(Event::EditNotice { message });
                        }
                        None => {}
                    }
                }
                hook::KeyEvent::Enter => app.dispatch(Event::Enter),
                hook::KeyEvent::Esc => app.dispatch(Event::Esc),
            }
        }

        app.sync_mirror();
        crate::preview_ipc::tick(Instant::now());

        while let Ok(outcome) = app.format_rx.try_recv() {
            app.receive_format(outcome);
        }

        app.tick_session();
        overlay::tick();

        // 設定ウィンドウで保存された変更を、起動し直さずに反映する
        if last_config_check.elapsed() >= Duration::from_secs(1) {
            last_config_check = Instant::now();
            let mtime = config_modified();
            if mtime != config_mtime {
                config_mtime = mtime;
                match config::load() {
                    Ok(cfg) => {
                        if let Some(h) = config::parse_hotkey(&cfg.hotkey) {
                            hook::set_hotkey(h);
                        }
                        overlay::set_theme(&cfg.theme);
                        let feedback = FeedbackSettings::from_config(&cfg);
                        overlay::set_feedback(feedback.hud_style, feedback.motion);
                        overlay::set_caption_anchor(&cfg.caption_position);
                        usage::ENABLED
                            .store(cfg.format.usage_log, std::sync::atomic::Ordering::Relaxed);
                        if cfg.model != app.cfg.model
                            || cfg.speech_exe != app.cfg.speech_exe
                            || cfg.keep_worker_minutes != app.cfg.keep_worker_minutes
                        {
                            if let Some(kept) = app.kept_worker.take() {
                                retire_worker(kept.worker);
                            }
                        }
                        if !cfg.recovery_history {
                            app.recoveries.disable();
                        }
                        if cfg.format.offline
                            && !app.cfg.format.offline
                            && matches!(
                                app.state,
                                State::Formatting { .. } | State::EditFormatting { .. }
                            )
                        {
                            app.format_generation.invalidate();
                            app.dispatch(Event::FormatFailed {
                                reason: "オフラインモードに切り替わりました".into(),
                            });
                        }
                        app.cfg = cfg;
                    }
                    Err(message) => {
                        app.execute(Action::ShowOverlay(OverlayKind::Error { message }));
                    }
                }
            }
        }

        if let Some(until) = app.transient_until {
            if Instant::now() >= until {
                app.transient_until = None;
                match app.state {
                    State::Idle => overlay::hide(),
                    // 録音中の一時的な注意が消えたら、録音の表示に戻す
                    State::Editing { ref selection } => app.show_overlay(OverlayKind::Editing {
                        head: selection.chars().take(24).collect(),
                    }),
                    State::Recording => app.show_overlay(OverlayKind::Recording),
                    State::Recognizing { .. } | State::EditRecognizing { .. } => {
                        app.show_overlay(OverlayKind::Recognizing);
                    }
                    _ => {}
                }
            }
        }

        // 何も表示していない待機中は、キーの通知(ウィンドウメッセージ)が来るまで
        // 寝る。設定の更新を見落とさないよう 1 秒で起きる。録音と表示の応答は
        // 従来どおり短い間隔で回す
        unsafe {
            MsgWaitForMultipleObjectsEx(
                None,
                app.wait_duration(Instant::now()).as_millis().max(1) as u32,
                QS_ALLINPUT,
                MWMO_INPUTAVAILABLE,
            );
        }
    }

    app.teardown_session();
    if let Some(kept) = app.kept_worker.take() {
        retire_worker(kept.worker);
    }
    hook::uninstall();
    crate::preview_ipc::unregister_endpoint(overlay::hwnd());
    sound::stop();
    overlay::remove_tray();
    let _ = LPARAM(0);
    let _ = WPARAM(0);
}

/// 待機中に次に起きるまでの長さ。待機中はメッセージで起こされるので、
/// 設定の更新の確認(1 秒ごと)だけを狙って 1 秒まで待つ。
/// 録音と表示の応答は従来どおり 10 ミリ秒
fn idle_sleep_duration(state: &State, transient: bool, overlay_visible: bool) -> Duration {
    if matches!(state, State::Idle) && !transient && !overlay_visible {
        Duration::from_secs(1)
    } else {
        Duration::from_millis(10)
    }
}

fn config_modified() -> Option<std::time::SystemTime> {
    std::fs::metadata(config::config_path())
        .and_then(|m| m.modified())
        .ok()
}

#[cfg(test)]
mod flow_tests {
    use super::*;

    #[test]
    fn output_feedback_selects_one_result_and_keeps_warnings_distinct() {
        assert_eq!(
            output_feedback(None, false, false, None),
            OutputFeedback::Success
        );
        assert_eq!(
            output_feedback(None, false, false, None).event(),
            Sound::Done
        );
        for result in [
            output_feedback(None, true, false, None),
            output_feedback(None, false, true, None),
            output_feedback(None, false, false, Some(OverlayKind::ApiKeyMissing)),
            output_feedback(
                None,
                false,
                false,
                Some(OverlayKind::FormatSkipped {
                    reason: "接続失敗".into(),
                }),
            ),
        ] {
            assert!(matches!(result, OutputFeedback::Warning(_)));
            assert_eq!(result.event(), Sound::Warning);
            assert_eq!(result.presentation().0, Tone::Warn);
        }
        let failure = output_feedback(
            Some("入力先なし"),
            true,
            true,
            Some(OverlayKind::ApiKeyMissing),
        );
        assert!(matches!(failure, OutputFeedback::Failure(_)));
        assert_eq!(failure.event(), Sound::Error);
        assert_eq!(failure.presentation().0, Tone::Error);
        assert_eq!(OutputFeedback::Cancelled.event(), Sound::Cancel);
        assert_eq!(OutputFeedback::Cancelled.presentation().0, Tone::Info);
    }

    #[test]
    fn idle_loop_slows_only_when_nothing_is_on_screen() {
        assert_eq!(
            idle_sleep_duration(&State::Idle, false, false),
            Duration::from_secs(1)
        );
        assert_eq!(
            idle_sleep_duration(&State::Idle, true, false),
            Duration::from_millis(10)
        );
        assert_eq!(
            idle_sleep_duration(&State::Idle, false, true),
            Duration::from_millis(10)
        );
        assert_eq!(
            idle_sleep_duration(&State::Recording, false, false),
            Duration::from_millis(10)
        );
    }

    #[test]
    fn recording_limits_keep_pending_pcm_bounded_and_use_existing_stop_transition() {
        let mut session = Session::new(&toml::from_str("").unwrap());
        let started = session.started_at;
        for minutes in [1, 3, 5, 10] {
            let limit = Duration::from_secs(minutes as u64 * 60);
            assert!(
                !session.recording_time_limit(started + limit - Duration::from_millis(1), minutes,)
            );
            assert!(session.recording_time_limit(started + limit, minutes));
        }
        assert!(!session.recording_time_limit(started + Duration::from_secs(5 * 60), 10));
        assert!(session.recording_time_limit(started + Duration::from_secs(10 * 60), 10));
        // 未認識の区間の合計に対して上限が効く。結果が出たら PCM は捨てて枠が戻る
        let mut loud = vec![0.3f32; 16000 * 3];
        loud.extend(std::iter::repeat_n(0.0f32, 16000));
        assert!(session.feed_segmenter(&loud));
        assert_eq!(session.intervals.len(), 1);
        let used = session.unaccounted_bytes;
        assert!(used > 0);
        let first = session.intervals[0].id.clone();
        session.complete_interval(&first, "できた");
        assert_eq!(session.unaccounted_bytes, 0);
        assert!(session.intervals[0].pcm.is_empty());
        // 上限に達したら区間を登録しない
        session.unaccounted_bytes = MAX_RECORDING_PCM_BYTES;
        assert!(!session.open_interval(loud));
        assert_eq!(session.intervals.len(), 1);
        let (next, actions) = state::step(State::Recording, Event::Enter);
        assert!(matches!(next, State::Recognizing { .. }));
        assert!(actions.contains(&Action::StopRecordingAndTranscribe));
    }

    #[test]
    fn intervals_are_joined_in_order_and_empty_results_are_skipped() {
        let mut session = Session::new(&toml::from_str("").unwrap());
        let utterances: [Vec<f32>; 3] = [
            vec![0.3f32; 16000],
            vec![0.3f32; 16000],
            vec![0.3f32; 16000],
        ];
        for samples in &utterances {
            // 閉じた区間として返るように、発話のあとに無音を足す
            let mut audio = samples.clone();
            audio.extend(std::iter::repeat_n(0.0f32, 16000));
            assert!(session.feed_segmenter(&audio));
        }
        assert_eq!(session.intervals.len(), 3);
        let ids: Vec<String> = session.intervals.iter().map(|i| i.id.clone()).collect();
        session.complete_interval(&ids[0], "ひとつめ");
        session.complete_interval(&ids[1], "");
        assert!(!session.all_intervals_done());
        assert_eq!(session.joined_text(), "ひとつめ");
        session.complete_interval(&ids[2], "ふたつめ");
        assert!(session.all_intervals_done());
        assert_eq!(session.joined_text(), "ひとつめふたつめ");
    }

    /// 偽の worker を使って、区間ごとの認識と送り直しを確かめる
    #[test]
    fn intervals_are_recognized_by_id_and_a_failed_one_is_marked_for_resend() {
        let mut app = test_app();
        for _ in 0..2 {
            let mut audio = vec![0.3f32; 16000];
            audio.extend(std::iter::repeat_n(0.0f32, 16000));
            assert!(app.session.feed_segmenter(&audio));
        }
        assert_eq!(app.session.intervals.len(), 2);
        // s0 は認識が終わり、s1 は worker がエラーで死んだ状態
        let (tx, rx) = channel();
        tx.send(r#"{"type":"end","id":"s0","text":"ひとつめ。","stop":"complete"}"#.to_string())
            .unwrap();
        tx.send(
            r#"{"type":"error","id":"s1","error":{"code":"worker","message":"crashed"}}"#
                .to_string(),
        )
        .unwrap();
        app.session.worker = Some(asr::test_worker(rx, false));
        app.session.intervals[1].transcribe_at = Some(Instant::now());
        app.set_state(State::Recognizing {
            raw_requested: false,
        });
        app.session.stop_requested = true;
        app.tick_session();
        // s0 の文章は残り、s1 は送り直しのために印が戻る。worker は起動し直し待ち
        assert!(app.session.intervals[0].done);
        assert_eq!(app.session.joined_text(), "ひとつめ。");
        assert!(app.session.intervals[1].transcribe_at.is_none());
        assert!(app.session.worker.is_none());
        assert_eq!(app.session.asr_failures, 1);
        assert_eq!(
            app.state,
            State::Recognizing {
                raw_requested: false
            }
        );
    }

    #[test]
    fn recognizing_ends_with_joined_text_when_every_interval_is_done() {
        let mut app = test_app();
        for _ in 0..2 {
            let mut audio = vec![0.3f32; 16000];
            audio.extend(std::iter::repeat_n(0.0f32, 16000));
            assert!(app.session.feed_segmenter(&audio));
        }
        let (tx, rx) = channel();
        tx.send(r#"{"type":"end","id":"s0","text":"ひとつめ。","stop":"complete"}"#.to_string())
            .unwrap();
        tx.send(r#"{"type":"end","id":"s1","text":"ふたつめ。","stop":"complete"}"#.to_string())
            .unwrap();
        app.session.worker = Some(asr::test_worker(rx, false));
        app.set_state(State::Recognizing {
            raw_requested: false,
        });
        app.session.stop_requested = true;
        app.tick_session();
        // 整形の接続先がないので生テキストを貼る。貼り先がなく失敗するが、
        // 文は順番につながっている
        assert_eq!(app.state, State::Idle);
        app.tick_output(Instant::now());
        assert_eq!(
            app.recoveries.first_text().unwrap().1,
            "ひとつめ。ふたつめ。"
        );
    }

    #[test]
    fn a_worker_that_dies_mid_recording_resends_undone_intervals() {
        let mut app = test_app();
        let mut audio = vec![0.3f32; 16000];
        audio.extend(std::iter::repeat_n(0.0f32, 16000));
        assert!(app.session.feed_segmenter(&audio));
        let (_tx, rx) = channel();
        app.session.worker = Some(asr::test_worker(rx, true));
        app.set_state(State::Recording);
        // 書き込みの失敗は送信スレッドが非同期に気づくので、tick を繰り返して待つ
        for _ in 0..100 {
            app.tick_session();
            if app.session.worker.is_none() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        // 送信の失敗は録音を続けたまま、送り直しとして扱う。
        assert!(app.session.worker.is_none(), "worker を起動し直す");
        assert_eq!(app.session.asr_failures, 1);
        assert!(!app.session.intervals[0].done);
        assert_eq!(app.state, State::Recording);
    }

    /// 送られた JSON 行を、型ごとに確かめられる形で読む
    fn sent_lines(recorder: &std::sync::Arc<std::sync::Mutex<String>>) -> Vec<serde_json::Value> {
        recorder
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| serde_json::from_str(l).unwrap_or(serde_json::Value::Null))
            .collect()
    }

    fn wait_for_sent(
        recorder: &std::sync::Arc<std::sync::Mutex<String>>,
        kind: &str,
        id: &str,
        mut tick: impl FnMut(),
    ) -> Vec<serde_json::Value> {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            tick();
            let lines = sent_lines(recorder);
            if lines.iter().any(|v| v["type"] == kind && v["id"] == id) {
                return lines;
            }
            assert!(
                Instant::now() < deadline,
                "writer did not emit {kind} for {id}: {lines:?}"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn a_new_partial_cancels_the_old_partial_request_first() {
        let mut app = test_app();
        let (_tx, rx) = channel();
        let (worker, recorder) = asr::test_worker_with_recorder(rx);
        app.session.worker = Some(worker);
        app.set_state(State::Recording);
        // 開いている区間が仮の認識の対象になる
        let speech = vec![0.3f32; 16000];
        assert!(app.session.feed_segmenter(&speech));
        // p0 が飛行中で、前回の送信から 1 秒以上たった状態
        app.session.partial = Some(Partial {
            id: "p0".to_string(),
        });
        app.session.next_partial_index = 1;
        app.session.last_partial_at = Some(Instant::now() - Duration::from_secs(2));
        let lines = wait_for_sent(&recorder, "transcribe", "p1", || app.tick_session());
        let cancelled = lines
            .iter()
            .position(|v| v["type"] == "cancel" && v["id"] == "p0")
            .expect("古い仮 p0 を cancel する");
        // cancel のあとに、新しい仮 p1 の音と transcribe が続く
        let p1_index = lines
            .iter()
            .position(|v| v["type"] == "chunk" && v["id"] == "p1");
        assert!(cancelled < p1_index.expect("次の仮 p1 が送られる"));
        assert!(lines
            .iter()
            .any(|v| v["type"] == "transcribe" && v["id"] == "p1"));
    }

    #[test]
    fn enter_cancels_the_partial_in_flight() {
        let mut app = test_app();
        let (_tx, rx) = channel();
        let (worker, recorder) = asr::test_worker_with_recorder(rx);
        app.session.worker = Some(worker);
        app.set_state(State::Recording);
        app.session.partial = Some(Partial {
            id: "p3".to_string(),
        });
        // Enter は録音を止めて認識へ進む。指示の途中の仮の文字がそのまま
        // 貼られるのを防ぐため、飛行中の仮を cancel する
        app.dispatch(Event::Enter);
        assert!(matches!(app.state, State::Recognizing { .. }));
        assert!(app.session.partial.is_none());
        let lines = wait_for_sent(&recorder, "cancel", "p3", || {});
        assert!(
            lines
                .iter()
                .any(|v| v["type"] == "cancel" && v["id"] == "p3"),
            "Enter で飛行中の仮 p3 を cancel する: {lines:?}"
        );
    }

    #[test]
    fn partial_recognition_sends_open_audio_and_waits_for_confirmed_work() {
        let mut app = test_app();
        // tx は最後まで保持する。落とすと worker が死んだものとして扱われる
        let (_tx, rx) = channel();
        let (worker, recorder) = asr::test_worker_with_recorder(rx);
        app.session.worker = Some(worker);
        app.set_state(State::Recording);
        // 開いている区間(最後に無音を足さない)が仮の認識の対象になる
        let speech = vec![0.3f32; 16000];
        assert!(app.session.feed_segmenter(&speech));
        // Queueing must not depend on when the writer thread gets scheduled.
        // Hold its sink until the application has queued the partial request.
        let blocked_writer = recorder.lock().unwrap_or_else(|p| p.into_inner());
        let deadline = Instant::now() + Duration::from_secs(2);
        while app.session.partial.is_none() && Instant::now() < deadline {
            app.tick_session();
            std::thread::yield_now();
        }
        assert_eq!(
            app.session
                .partial
                .as_ref()
                .map(|partial| partial.id.as_str()),
            Some("p0"),
            "仮の要求を writer の実書込みを待たずに queue できる"
        );
        assert!(blocked_writer.is_empty(), "実書込みはまだ完了していない");
        drop(blocked_writer);

        let sent = wait_for_sent(&recorder, "transcribe", "p0", || app.tick_session());
        assert!(
            sent.iter().any(|v| v["type"] == "chunk" && v["id"] == "p0"),
            "仮の音も同じ要求に属する: {sent:?}"
        );
    }

    #[test]
    fn partial_recognition_is_held_back_while_a_confirmed_interval_is_in_flight() {
        let mut app = test_app();
        // tx は最後まで保持する。落とすと worker が死んだものとして扱われる
        let (_tx, rx) = channel();
        let (worker, recorder) = asr::test_worker_with_recorder(rx);
        app.session.worker = Some(worker);
        app.set_state(State::Recording);
        // 閉じた区間(まだ結果が出ていない)があるときは、確定を優先して仮を送らない
        let mut audio = vec![0.3f32; 16000];
        audio.extend(std::iter::repeat_n(0.0f32, 16000));
        assert!(app.session.feed_segmenter(&audio));
        let speech = vec![0.3f32; 16000];
        assert!(app.session.feed_segmenter(&speech));
        let sent = wait_for_sent(&recorder, "transcribe", "s0", || app.tick_session());
        assert!(
            !sent
                .iter()
                .any(|v| v["id"].as_str().is_some_and(|id| id.starts_with('p'))),
            "確定の処理中に仮を送らない: {sent:?}"
        );
        assert!(app.session.partial.is_none());
    }

    #[test]
    fn edit_flow_recognizes_an_instruction_and_formats_it() {
        let mut app = test_app();
        // 整形の結果の受け口を、本編と同じ仕組みで用意する
        let (fmt_tx, fmt_rx) = channel();
        app.format_tx = fmt_tx;
        app.format_rx = fmt_rx;
        let (tx, rx) = channel();
        // 認識の終端として、指示の文を返す worker
        tx.send(r#"{"type":"end","id":"s0","text":"丁寧にして","stop":"complete"}"#.to_string())
            .unwrap();
        // 指示を話した区間(閉じた区間として返るように無音を足す)
        let mut audio = vec![0.3f32; 16000];
        audio.extend(std::iter::repeat_n(0.0f32, 16000));
        assert!(app.session.feed_segmenter(&audio));
        app.session.worker = Some(asr::test_worker(rx, false));
        // オフラインモードなので Edit の整形はすぐ失敗する。指示の文は貼られない
        app.cfg.format.offline = true;
        // Enter を押したあとの Edit の認識として進める
        app.set_state(State::EditRecognizing {
            selection: "えーと明日です".into(),
        });
        app.session.stop_requested = true;
        app.tick_session();
        // 指示の認識が終わると Edit の整形に入る
        assert_eq!(
            app.state,
            State::EditFormatting {
                selection: "えーと明日です".into(),
                instruction: "丁寧にして".into(),
            }
        );
        for _ in 0..50 {
            std::thread::sleep(Duration::from_millis(50));
            let mut got = false;
            while let Ok(outcome) = app.format_rx.try_recv() {
                got = true;
                app.receive_format(outcome);
            }
            if got && app.state == State::Idle {
                break;
            }
        }
        assert_eq!(app.state, State::Idle);
        assert!(app.pending_paste.is_none(), "指示を貼り付けてはいけない");
        assert!(app.paste_transaction.is_none());
    }

    #[test]
    fn confirmed_requests_wait_for_the_previous_result_before_starting_their_timeout() {
        let mut app = test_app();
        let (_tx, rx) = channel();
        let (worker, recorder) = asr::test_worker_with_recorder(rx);
        app.session.worker = Some(worker);
        app.session.open_interval(vec![0.3; 1600]);
        app.session.open_interval(vec![0.3; 1600]);
        app.set_state(State::Recording);
        let lines = wait_for_sent(&recorder, "transcribe", "s0", || app.tick_session());
        assert_eq!(
            lines.iter().filter(|v| v["type"] == "transcribe").count(),
            1
        );
        assert!(app.session.intervals[1].transcribe_at.is_none());
    }

    #[test]
    fn engine_preparation_updates_only_on_transitions_and_preserves_transient_notices() {
        let mut app = test_app();
        app.set_state(State::Recording);
        assert_eq!(app.engine_preparing_update(false), None);
        assert_eq!(app.engine_preparing_update(true), Some(false));
        assert_eq!(app.engine_preparing_update(true), None);

        let until = Instant::now() + Duration::from_secs(3);
        app.transient_until = Some(until);
        assert_eq!(app.engine_preparing_update(false), None);
        assert!(app.session.engine_preparing);
        for kind in [
            OverlayKind::Recording,
            OverlayKind::Editing {
                head: "選んだ文".into(),
            },
            OverlayKind::Recognizing,
        ] {
            app.show_overlay(kind);
            assert_eq!(app.transient_until, Some(until));
        }
        assert_eq!(app.engine_preparing_update(true), None);
        assert!(
            !app.session.engine_preparing,
            "通知の期限後に復帰する表示には最新の ready 状態を使う"
        );
        app.transient_until = None;
        assert_eq!(app.engine_preparing_update(true), None);
        assert_eq!(app.engine_preparing_update(false), Some(true));
    }

    #[test]
    fn startup_progress_does_not_let_terminal_poll_consume_ready() {
        for state in [
            State::Recording,
            State::Recognizing {
                raw_requested: false,
            },
        ] {
            let mut app = test_app();
            let (tx, rx) = channel();
            tx.send(r#"{"type":"progress","done":0.5}"#.into()).unwrap();
            tx.send(r#"{"type":"ready","protocol":4}"#.into()).unwrap();
            app.session.worker = Some(asr::test_starting_worker(rx));
            app.set_state(state);

            app.tick_session();
            assert!(!app.session.worker.as_ref().unwrap().is_ready());
            app.tick_session();
            assert!(
                app.session.worker.as_ref().unwrap().is_ready(),
                "progress に続く ready を終端処理が捨ててはいけない"
            );
            assert_eq!(app.session.asr_failures, 0);
        }
    }

    #[test]
    fn completed_recognition_releases_or_keeps_the_worker_immediately() {
        for minutes in [0, 3] {
            let mut app = test_app();
            app.cfg.keep_worker_minutes = minutes;
            app.session.open_interval(vec![0.3; 1600]);
            let (tx, rx) = channel();
            tx.send(r#"{"type":"end","id":"s0","text":"認識しました","stop":"complete"}"#.into())
                .unwrap();
            app.session.worker = Some(asr::test_worker(rx, false));
            app.set_state(State::Recognizing {
                raw_requested: false,
            });
            app.session.stop_requested = true;
            app.tick_session();
            assert!(
                app.session.worker.is_none(),
                "認識終了時にsessionから解放する"
            );
            assert_eq!(app.kept_worker.is_some(), minutes > 0);
        }
    }

    #[test]
    fn enter_cancels_a_partial_that_is_still_uploading() {
        let mut app = test_app();
        let (_tx, rx) = channel();
        let (worker, recorder) = asr::test_worker_with_recorder(rx);
        app.session.worker = Some(worker);
        app.session.open_interval(vec![0.3; 1600]);
        app.session.sending = Some(Sending::new(true, "p0".into(), Arc::from([0; 6400])));
        app.set_state(State::Recording);
        app.dispatch(Event::Enter);
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            app.tick_session();
            let lines = sent_lines(&recorder);
            if lines
                .iter()
                .any(|v| v["type"] == "cancel" && v["id"] == "p0")
                && lines
                    .iter()
                    .any(|v| v["type"] == "chunk" && v["id"] == "s0")
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        let lines = sent_lines(&recorder);
        assert!(
            !lines
                .iter()
                .any(|v| v["type"] == "transcribe" && v["id"] == "p0"),
            "{lines:?}"
        );
        let cancel = lines
            .iter()
            .position(|v| v["type"] == "cancel" && v["id"] == "p0")
            .unwrap();
        let confirmed = lines
            .iter()
            .position(|v| v["type"] == "chunk" && v["id"] == "s0")
            .unwrap();
        assert!(cancel < confirmed, "{lines:?}");
    }

    #[test]
    fn confirmed_work_cancels_the_partial_before_sending_audio() {
        let mut app = test_app();
        let (_tx, rx) = channel();
        let (worker, recorder) = asr::test_worker_with_recorder(rx);
        app.session.worker = Some(worker);
        app.session.partial = Some(Partial { id: "p0".into() });
        app.session.open_interval(vec![0.3; 1600]);
        app.set_state(State::Recording);
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            app.tick_session();
            let lines = sent_lines(&recorder);
            if lines
                .iter()
                .any(|v| v["type"] == "cancel" && v["id"] == "p0")
                && lines
                    .iter()
                    .any(|v| v["type"] == "chunk" && v["id"] == "s0")
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        let lines = sent_lines(&recorder);
        let cancel = lines
            .iter()
            .position(|v| v["type"] == "cancel" && v["id"] == "p0")
            .expect("確定を優先する");
        let confirmed = lines
            .iter()
            .position(|v| v["type"] == "chunk" && v["id"] == "s0")
            .unwrap();
        assert!(cancel < confirmed, "{lines:?}");
    }

    #[test]
    fn confirmed_caption_uses_interval_order_even_when_results_arrive_out_of_order() {
        let mut session = Session::new(&toml::from_str("").unwrap());
        session.open_interval(vec![0.3; 1600]);
        session.open_interval(vec![0.3; 1600]);
        session.complete_interval("s1", " 後半。 ");
        session.complete_interval("s0", " 前半。 ");
        assert_eq!(session.confirmed_text, "前半。後半。");
    }

    #[test]
    fn a_ready_worker_is_kept_when_the_setting_says_so() {
        // 残す時間が 0(初期値)のときは殺し、1 以上で ready なら残す
        let _app = test_app();
        assert!(!keep_worker_decision(0, true));
        assert!(!keep_worker_decision(3, false));
        assert!(keep_worker_decision(3, true));
    }

    fn test_app() -> App {
        let mut cfg: Config = toml::from_str("").unwrap();
        cfg.speech_exe = "__openhinovoice_missing_test_worker__.exe".into();
        cfg.sounds = false;
        cfg.recovery_history = false;
        let (format_tx, format_rx) = channel();
        let mut recoveries = recovery::Recoveries::default();
        let mut session = Session::new(&cfg);
        session.recovery_id = Some(recoveries.reserve(false).unwrap());
        App {
            fixture_recorder: None,
            replay: None,
            session,
            cfg,
            state: State::Idle,
            kept_worker: None,
            format_tx,
            format_rx,
            format_generation: FormatGeneration::default(),
            transient_until: None,
            recoveries,
            last_paste_text: None,
            pending_paste: None,
            paste_transaction: None,
            latest_delta: Arc::new(Mutex::new(None)),
            quit_started: None,
            start_requested: false,
            paste_notice: None,
        }
    }

    #[test]
    fn late_format_result_cannot_apply_to_a_new_request() {
        let mut generations = FormatGeneration::default();
        let old = generations.start();
        let old_cancel = generations.cancel_flag();
        assert!(generations.accepts(old, true));
        // The user presses Enter for raw text or Esc to discard the old result.
        generations.invalidate();
        assert!(
            old_cancel.load(Ordering::SeqCst),
            "background retries must stop"
        );
        assert!(!generations.accepts(old, false));
        let current = generations.start();
        assert_ne!(old, current);
        assert!(!generations.accepts(old, true));
        assert!(generations.accepts(current, true));
        assert!(!generations.accepts(current, false));
    }

    #[test]
    fn both_format_success_and_failure_carry_their_origin_generation() {
        let done = FormatOutcome::Done {
            generation: 4,
            text: "clean".into(),
            fallback_chunks: 0,
        };
        let failed = FormatOutcome::Failed {
            generation: 3,
            reason: "timeout".into(),
        };
        assert_eq!(done.generation(), 4);
        assert_eq!(failed.generation(), 3);
    }

    #[test]
    fn old_format_success_or_error_cannot_finish_new_formatting() {
        let mut app = test_app();
        let previous = app.format_generation.start();
        app.format_generation.invalidate();
        let current = app.format_generation.start();
        app.set_state(State::Formatting {
            raw: "current raw".into(),
        });
        app.receive_format(FormatOutcome::Done {
            generation: previous,
            text: "old formatted".into(),
            fallback_chunks: 0,
        });
        app.receive_format(FormatOutcome::Failed {
            generation: previous,
            reason: "old failure".into(),
        });
        assert_eq!(
            app.state,
            State::Formatting {
                raw: "current raw".into()
            }
        );
        assert!(app.format_generation.accepts(current, true));
    }

    #[test]
    fn session_keeps_model_dictionary_and_format_settings_from_start() {
        let mut cfg: Config = toml::from_str("").unwrap();
        cfg.speech_exe = "old.exe".into();
        cfg.model = "old-model".into();
        cfg.words = "old word".into();
        cfg.format.model = "old-format".into();
        let session = Session::new(&cfg);
        let identity = WorkerKey::for_config(&cfg);
        cfg.speech_exe = "new.exe".into();
        cfg.model = "new-model".into();
        cfg.words = "new word".into();
        cfg.format.model = "new-format".into();
        assert_eq!(session.cfg.model, "old-model");
        assert_eq!(session.cfg.format.model, "old-format");
        assert!(session.asr_hints.contains("old word"));
        assert!(!session.asr_hints.contains("new word"));
        assert_ne!(identity, WorkerKey::for_config(&cfg));
        assert_eq!(identity, WorkerKey::for_config(&session.cfg));
    }

    #[test]
    fn sending_shares_pcm_and_advances_without_allocating_chunks() {
        let pcm: Arc<[u8]> = vec![7; ASR_CHUNK_BYTES * 3].into();
        let sending = Sending::new(false, "s0".into(), pcm.clone());
        assert!(Arc::ptr_eq(&pcm, &sending.pcm));
        assert_eq!(sending.offset, 0);
    }

    #[test]
    fn a_pending_paste_preserves_raw_and_reserves_only_one_next_start() {
        let mut app = test_app();
        let id = app.session.recovery_id.unwrap();
        app.recoveries.update_raw(id, "原文".into());
        app.execute(Action::Paste {
            text: "入力文".into(),
        });
        app.dispatch(Event::F8);
        app.dispatch(Event::F8);
        assert!(app.start_requested);
        assert!(app.pending_paste.is_some());
        app.dispatch(Event::Esc);
        assert!(!app.start_requested);
        assert!(app.pending_paste.is_none());
        assert_eq!(app.recoveries.first_text(), Some((id, "入力文")));
    }

    #[test]
    fn replay_cancel_discards_pending_output_notice_before_the_next_result() {
        for notice in [
            OverlayKind::FormatSkipped {
                reason: "previous failure".into(),
            },
            OverlayKind::ApiKeyMissing,
        ] {
            let mut app = test_app();
            app.replay = Some(Box::new(ReplayCapture {
                sample_rate: 16_000,
                ..ReplayCapture::default()
            }));
            app.cfg.sounds = true;
            let id = app.session.recovery_id.unwrap();
            app.recoveries.update_raw(id, "cancelled text".into());
            app.pending_paste = Some(PendingPaste {
                text: "cancelled text".into(),
                target: None,
                recovery_id: id,
                deadline: Instant::now() + Duration::from_secs(5),
            });
            app.paste_notice = Some(notice);
            app.dispatch(Event::Esc);
            assert!(app.pending_paste.is_none());
            assert!(app.paste_notice.is_none());
            assert_eq!(
                output_feedback(None, false, false, app.paste_notice.take()),
                OutputFeedback::Success
            );
            assert!(
                app.transient_until.is_none(),
                "replay does not show a native notice"
            );
        }
    }

    #[test]
    fn replay_new_session_cannot_inherit_a_previous_output_warning() {
        let mut app = test_app();
        app.replay = Some(Box::new(ReplayCapture {
            sample_rate: 16_000,
            ..ReplayCapture::default()
        }));
        app.cfg.sounds = true;
        app.cfg.format.provider = "disabled".into();
        let (_tx, rx) = channel();
        app.kept_worker = Some(KeptWorker {
            worker: asr::test_worker(rx, false),
            key: WorkerKey::for_config(&app.cfg),
            expires: Instant::now() + Duration::from_secs(5),
        });
        app.paste_notice = Some(OverlayKind::FormatSkipped {
            reason: "previous failure".into(),
        });
        app.dispatch(Event::F8);
        assert_eq!(app.state, State::Recording);
        assert!(app.paste_notice.is_none());
        assert!(app.session.recorder.is_none());
        app.dispatch(Event::Enter);
        app.dispatch(Event::Enter);
        app.dispatch(Event::AsrDone {
            text: "current success".into(),
            has_api_key: false,
        });
        assert_eq!(app.state, State::Idle);
        assert_eq!(
            output_feedback(None, false, false, app.paste_notice.take()),
            OutputFeedback::Success
        );
        assert!(app.pending_paste.is_none(), "replay does not paste");
        assert!(
            app.transient_until.is_none(),
            "replay does not show a native notice"
        );
    }

    #[test]
    fn the_wait_ends_at_the_pending_output_deadline() {
        let mut app = test_app();
        let now = Instant::now();
        app.pending_paste = Some(PendingPaste {
            text: "a".into(),
            target: None,
            recovery_id: 0,
            deadline: now + Duration::from_millis(4),
        });
        assert!(app.wait_duration(now) <= Duration::from_millis(4));
    }

    #[test]
    fn an_exit_request_cancels_pending_input_before_output_advances() {
        let _exit_guard = overlay::EXIT_TEST_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let mut app = test_app();
        let id = app.session.recovery_id.unwrap();
        app.recoveries.update_raw(id, "保存したい原文".into());
        app.execute(Action::Paste {
            text: "終了後に送ってはいけない文".into(),
        });
        assert!(app.pending_paste.is_some());
        assert!(!app.should_exit(true, Instant::now()));
        assert!(app.pending_paste.is_none());
        assert!(app.paste_transaction.is_none());
        assert_eq!(app.recoveries.count(), 1);
        app.tick_output(Instant::now());
        assert!(app.paste_transaction.is_none());
        assert_eq!(
            app.recoveries.first_text(),
            Some((id, "終了後に送ってはいけない文"))
        );
    }

    #[test]
    fn an_exit_request_waits_for_unsaved_raw_without_discarding_it() {
        let mut app = test_app();
        let id = app.recoveries.reserve(true).unwrap();
        app.recoveries
            .update_raw(id, "まだ保存されていない原文".into());
        let now = Instant::now();
        assert!(!app.should_exit(false, now));
        assert!(app.quit_started.is_none());
        assert!(!app.should_exit(true, now));
        assert!(app.quit_started.is_some());
        assert!(app.recoveries.has_unfinished_writes());
        assert!(!app.should_exit(true, now + Duration::from_millis(250)));
        assert!(app.recoveries.has_unfinished_writes());
    }

    #[test]
    fn session_start_failure_must_prevent_following_recording_overlay() {
        let mut app = test_app();
        app.dispatch(Event::F8);
        assert_eq!(app.state, State::Idle);
        // Error notices set a dismissal deadline; Recording overlays clear it.
        assert!(app.transient_until.is_some());
        assert!(app.session.worker.is_none());
        assert!(app.session.recorder.is_none());
    }
}

#[cfg(test)]
mod core_e2e;
