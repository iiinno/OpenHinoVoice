#![windows_subsystem = "windows"]

mod asr;
mod audio;
mod chatgpt;
mod config;
mod context;
mod format;
mod history;
mod hook;
mod lexicon;
mod overlay;
mod paste;
mod provider;
mod segment;
mod settings;
mod sound;
mod startup;
mod state;
mod uia;

use asr::Worker;
use audio::Recorder;
use config::Config;
use overlay::{Mode, Tone};
use sound::Sound;
use state::{Action, Event, OverlayKind, State};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
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
/// Edit で選べる文の上限。これを超える選択は Edit にしない
const MAX_EDIT_SELECTION_CHARS: usize = 6000;

/// 録音のあとに worker を残すか。残す時間の設定と、worker が ready かで決める
fn keep_worker_decision(minutes: u32, ready: bool) -> bool {
    minutes > 0 && ready
}

enum FormatOutcome {
    Done {
        generation: u64,
        text: String,
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
    /// 整形の応答が届いた分。帯に流すために、そこまでの全文を持つ
    Delta {
        generation: u64,
        text: String,
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
            | Self::Delta { generation, .. }
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
    pcm: Vec<u8>,
    chunks_sent: bool,
    transcribe_sent: bool,
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
    chunks: VecDeque<Vec<u8>>,
}

impl Sending {
    fn new(is_partial: bool, id: String, pcm: &[u8]) -> Sending {
        Sending {
            is_partial,
            id,
            chunks: pcm.chunks(ASR_CHUNK_BYTES).map(<[u8]>::to_vec).collect(),
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
    worker: Option<Worker>,
    recorder: Option<Recorder>,
    paste_target: Option<paste::PasteTarget>,
    input_app: Option<String>,
    recovery_id: Option<u64>,
    recovery_write_failed: bool,
    sample_rate: u32,
    stop_requested: bool,
    started_at: Instant,
    // 録音開始の音がマイクに入るので、鳴り終わるまでに録れた音は捨てる
    skip_until: Instant,
    segmenter: segment::Segmenter,
    intervals: Vec<Interval>,
    next_interval_index: usize,
    sending: Option<Sending>,
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
    fn new() -> Session {
        Session {
            worker: None,
            recorder: None,
            paste_target: None,
            input_app: None,
            recovery_id: None,
            recovery_write_failed: false,
            sample_rate: 16000,
            stop_requested: false,
            started_at: Instant::now(),
            skip_until: Instant::now(),
            segmenter: segment::Segmenter::new(16000),
            intervals: Vec::new(),
            next_interval_index: 0,
            sending: None,
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
        self.intervals.push(Interval {
            id,
            pcm,
            chunks_sent: false,
            transcribe_sent: false,
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
        self.confirmed_text.push_str(text);
        self.unaccounted_bytes = self.unaccounted_bytes.saturating_sub(interval.pcm.len());
        interval.pcm = Vec::new();
        self.asr_failures = 0;
        // 仮の文字があれば確定に置き換わる(仮は空に戻す)
        let partial = if self.partial.as_ref().is_some_and(|p| p.id != id) {
            String::new()
        } else {
            self.partial = None;
            String::new()
        };
        overlay::set_caption(&self.confirmed_text, &partial);
    }

    /// 順番に結果をつないだ文章。結果が出ていない区間は空になり、
    /// すべて出てから呼ぶことを想定する
    fn joined_text(&self) -> String {
        let texts: Vec<String> = self
            .intervals
            .iter()
            .filter(|i| i.done)
            .map(|i| i.text.clone())
            .collect();
        segment::join_intervals(&texts)
    }

    fn all_intervals_done(&self) -> bool {
        self.intervals.iter().all(|i| i.done)
    }
}

struct App {
    cfg: Config,
    state: State,
    session: Session,
    /// 録音のあとに残した認識エンジン。keep_worker_minutes の設定があるときだけ
    kept_worker: Option<(Worker, Instant)>,
    format_tx: Sender<FormatOutcome>,
    /// 整形の結果の受け口。main のループが読み、テストも使う
    format_rx: std::sync::Mutex<Option<Receiver<FormatOutcome>>>,
    format_generation: FormatGeneration,
    transient_until: Option<Instant>,
    failed_pastes: VecDeque<String>,
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
    fn set_state(&mut self, next: State) {
        hook::set_mirror(mirror_of(&next));
        self.state = next;
    }

    fn dispatch(&mut self, event: Event) {
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
    }

    fn fail_session(&mut self, message: String) {
        self.teardown_session();
        self.set_state(State::Idle);
        self.execute(Action::ShowOverlay(OverlayKind::Error { message }));
    }

    fn teardown_session(&mut self) {
        self.session.recorder = None;
        self.session.paste_target = None;
        self.session.stop_requested = false;
        if let Some(worker) = self.session.worker.take() {
            worker.shutdown();
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
        overlay::clear_caption();
    }

    /// Returns false if this action fails; subsequent actions in the transition must be skipped.
    fn execute(&mut self, action: Action) -> bool {
        match action {
            Action::StartSession => {
                // 前の録音の worker を、設定があればいったん残す
                if let Some(previous) = self.session.worker.take() {
                    if keep_worker_decision(self.cfg.keep_worker_minutes, previous.is_ready()) {
                        self.kept_worker = Some((
                            previous,
                            Instant::now()
                                + Duration::from_secs(u64::from(self.cfg.keep_worker_minutes) * 60),
                        ));
                    } else {
                        previous.shutdown();
                    }
                }
                // 残しておいた worker があれば、起動とモデルの読み込みを飛ばして使う
                let kept = match self.kept_worker.take() {
                    Some((worker, expires)) if Instant::now() < expires => {
                        let mut worker = worker;
                        worker.forget_ids();
                        Some(worker)
                    }
                    Some((worker, _)) => {
                        worker.shutdown();
                        None
                    }
                    None => None,
                };
                self.session = Session::new();
                self.session.worker = kept;
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
                if self.session.worker.is_none() {
                    match Worker::spawn(&self.cfg.speech_exe, &self.cfg.model) {
                        Ok(worker) => {
                            self.session.worker = Some(worker);
                        }
                        Err(e) => {
                            self.fail_session(e.to_string());
                            return false;
                        }
                    }
                }
                match Recorder::start_with_device(Some(self.cfg.microphone.as_str())) {
                    Ok(recorder) => {
                        self.session.sample_rate = recorder.sample_rate;
                        self.session.segmenter = segment::Segmenter::new(recorder.sample_rate);
                        self.session.recorder = Some(recorder);
                        if self.cfg.sounds {
                            self.sfx(Sound::Start);
                            self.session.skip_until = Instant::now()
                                + sound::duration(Sound::Start)
                                + Duration::from_millis(40);
                        }
                    }
                    Err(e) => {
                        self.fail_session(e);
                        return false;
                    }
                }
                if self.cfg.has_api_key() {
                    let cfg = self.cfg.format.clone();
                    std::thread::spawn(move || format::prewarm(&cfg));
                }
            }
            Action::StopRecordingAndTranscribe => {
                if let Some(recorder) = self.session.recorder.take() {
                    let samples = recorder.drain();
                    drop(recorder);
                    if !self.session.feed_segmenter(&samples) || !self.session.close_open_interval()
                    {
                        self.fail_session(PCM_LIMIT_MESSAGE.into());
                        return false;
                    }
                }
                // 仮の認識はもう要らない。指示の途中の文字がそのまま貼られるのを防ぐ
                if let (Some(worker), Some(partial)) =
                    (self.session.worker.as_mut(), self.session.partial.take())
                {
                    let _ = worker.cancel(&partial.id);
                }
                if self.cfg.live_caption {
                    overlay::set_caption(&self.session.confirmed_text, "");
                }
                self.session.stop_requested = true;
                self.sfx(Sound::Stop);
            }
            Action::DiscardSession => {
                self.teardown_session();
            }
            Action::CallFormat { raw } => {
                let mut cfg = self.cfg.format.clone();
                cfg.prompt = lexicon::prompt_with_terms(
                    &cfg.prompt,
                    &self.cfg.terms,
                    self.session.input_app.as_deref(),
                );
                let tx = self.format_tx.clone();
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
                            let _ = tx.send(FormatOutcome::Delta {
                                generation,
                                text: text.to_string(),
                            });
                        },
                    );
                    if !cancelled() {
                        // 履歴の保存が失敗しても、完成した整形テキストは失わない。
                        if let Err(e) = history::record(&cfg, &raw, &result, started.elapsed()) {
                            eprintln!("整形履歴を書き込めません: {e}");
                        }
                    }
                    let outcome = match result {
                        Ok(text) => FormatOutcome::Done { generation, text },
                        Err(e) => FormatOutcome::Failed {
                            generation,
                            reason: e.to_string(),
                        },
                    };
                    let _ = tx.send(outcome);
                });
            }
            Action::CallEditFormat {
                selection,
                instruction,
            } => {
                // 置き換える前の文と後の文を、復旧用の履歴に両方残す。
                // うまくいかなかったときに元へ戻せる
                match history::save_recognized(&selection) {
                    Ok(id) => self.session.recovery_id = Some(id),
                    Err(e) => {
                        eprintln!("Edit の復旧履歴を書き込めません: {e}");
                        self.session.recovery_write_failed = true;
                    }
                }
                let mut cfg = self.cfg.format.clone();
                cfg.prompt = lexicon::prompt_with_terms(
                    &cfg.prompt,
                    &self.cfg.terms,
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
                });
            }
            Action::Paste { text } => {
                // Only user-specified exact aliases are replaced, even if AI is off.
                overlay::clear_caption();
                let text = lexicon::replace_explicit(&text, &self.cfg.terms);
                if let Some(id) = self.session.recovery_id {
                    if let Err(e) = history::update_formatted(id, &text) {
                        eprintln!("復旧履歴の更新に失敗しました: {e}");
                    }
                }
                let result = self
                    .session
                    .paste_target
                    .take()
                    .ok_or_else(|| "録音開始時の入力先が不明です".to_string())
                    .and_then(|target| paste::paste_text(&text, target, overlay::hwnd()));
                match result {
                    Ok(()) => {
                        if let Some(id) = self.session.recovery_id {
                            if let Err(e) =
                                history::update_paste_status(id, history::PasteStatus::ShortcutSent)
                            {
                                eprintln!("貼り付け履歴の更新に失敗しました: {e}");
                            }
                        }
                        self.sfx(Sound::Done);
                        if self.session.recovery_write_failed {
                            self.show_overlay(OverlayKind::Error {
                                message: "履歴保存に失敗。原文はトレイから回収できます".into(),
                            });
                        }
                    }
                    Err(e) => {
                        if let Some(id) = self.session.recovery_id {
                            if let Err(err) =
                                history::update_paste_status(id, history::PasteStatus::Failed)
                            {
                                eprintln!("失敗履歴の更新に失敗しました: {err}");
                            }
                        }
                        self.failed_pastes.push_back(text);
                        overlay::set_recovery_count(self.failed_pastes.len());
                        self.show_overlay(OverlayKind::Error {
                            message: format!("入力失敗。トレイで未入力テキストをコピー（{e}）"),
                        });
                        return false;
                    }
                }
            }
            Action::ShowOverlay(kind) => {
                self.show_overlay(kind);
            }
            Action::HideOverlay => {
                // 貼付失敗直後の Error が後続 HideOverlay に消されないよう、
                // 一時通知が表示中なら期限満了まで維持する。
                if self.transient_until.is_none() {
                    overlay::hide();
                }
            }
        }
        true
    }

    /// An explicit tray action. Never discard recoverable text unless Windows accepted the copy.
    fn copy_failed_text(&mut self) {
        let Some(text) = self.failed_pastes.front() else {
            return;
        };
        let result = paste::copy_text_to_clipboard(text, overlay::hwnd());
        match result {
            Ok(()) => {
                self.failed_pastes.pop_front();
                overlay::set_recovery_count(self.failed_pastes.len());
                overlay::show(Mode::Notice {
                    tone: Tone::Warn,
                    text: if self.failed_pastes.is_empty() {
                        "未入力テキストをクリップボードにコピーしました".into()
                    } else {
                        format!(
                            "コピーしました。未入力テキストは残り{}件",
                            self.failed_pastes.len()
                        )
                    },
                });
                self.transient_until = Some(Instant::now() + Duration::from_millis(2500));
            }
            Err(e) => self.show_overlay(OverlayKind::Error {
                message: format!("コピーできませんでした（{e}）。トレイから再試行できます"),
            }),
        }
    }

    fn sfx(&self, s: Sound) {
        if self.cfg.sounds {
            sound::play(s, self.cfg.volume);
        }
    }

    fn show_overlay(&mut self, kind: OverlayKind) {
        match kind {
            OverlayKind::Recording => {
                self.transient_until = None;
                overlay::show(Mode::Recording {
                    started: self.session.started_at,
                });
            }
            OverlayKind::Recognizing => {
                self.transient_until = None;
                overlay::show(Mode::Busy {
                    label: "認識中".into(),
                    hint: None,
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
            let samples = recorder.drain();
            overlay::push_level(&samples);
            if Instant::now() >= self.session.skip_until && !self.session.feed_segmenter(&samples) {
                self.dispatch(Event::AsrError {
                    message: PCM_LIMIT_MESSAGE.into(),
                });
                return;
            }
        }

        if in_recording
            && self
                .session
                .recording_time_limit(Instant::now(), self.cfg.max_recording_minutes)
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

        let words = lexicon::asr_hints(&self.cfg.words, &self.cfg.terms);
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

        // 区間ごとの時間切れ。1 区間の認識が 90 秒を超えたら送り直す
        if !restart {
            let now = Instant::now();
            let stalled = self.session.intervals.iter().any(|i| {
                i.transcribe_sent
                    && !i.done
                    && i.transcribe_at.is_some_and(|at| {
                        now.saturating_duration_since(at) >= asr::TRANSCRIPTION_TIMEOUT
                    })
            });
            if stalled {
                restart = true;
                self.session.asr_failures += 1;
            }
        }

        // 終端メッセージを読み、区間の結果として記録する。仮の認識(p*)の
        // 終端は帯に映すだけで、確定の処理には影響しない
        if !restart {
            loop {
                let polled = self.session.worker.as_mut().unwrap().poll_terminal();
                let Some(result) = polled else { break };
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
                                asr::TerminalOutcome::Text(text) => overlay::set_caption(
                                    &self.session.confirmed_text,
                                    text.as_deref().unwrap_or(""),
                                ),
                                _ => overlay::set_caption(&self.session.confirmed_text, ""),
                            }
                        }
                    }
                    Ok(terminal) => match terminal.outcome {
                        asr::TerminalOutcome::Text(text) => {
                            self.session
                                .complete_interval(&terminal.id, text.as_deref().unwrap_or(""));
                            self.update_recovery_history();
                        }
                        asr::TerminalOutcome::Error(_) | asr::TerminalOutcome::Cancelled => {
                            restart = true;
                            self.session.asr_failures += 1;
                        }
                    },
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
            if let Some(worker) = self.session.worker.take() {
                worker.shutdown();
            }
            self.session.sending = None;
            self.session.partial = None;
            self.session.last_spawn_attempt = None;
            for interval in &mut self.session.intervals {
                if !interval.done {
                    interval.chunks_sent = false;
                    interval.transcribe_sent = false;
                    interval.transcribe_at = None;
                }
            }
        }

        // 送りパイプライン。1 区間ずつ chunk を送り、終わったら transcribe を送る。
        // 仮の認識も同じ仕組みで送る
        if ready && !restart && fatal.is_none() {
            let mut send_failed = false;
            let sending = self.session.sending.as_mut();
            if let Some(sending) = sending {
                let worker = self.session.worker.as_mut().unwrap();
                while let Some(chunk) = sending.chunks.front() {
                    match worker.send_chunk(&sending.id, chunk) {
                        Ok(true) => {
                            sending.chunks.pop_front();
                        }
                        Ok(false) => break, // 押し戻し。同じ chunk を次の tick で送る
                        Err(_) => {
                            send_failed = true;
                            break;
                        }
                    }
                }
                if sending.chunks.is_empty() && !send_failed {
                    let id = sending.id.clone();
                    match worker.transcribe(&id, self.session.sample_rate, &words) {
                        Ok(true) => {
                            if sending.is_partial {
                                self.session.partial = Some(Partial { id });
                            } else if let Some(interval) =
                                self.session.intervals.iter_mut().find(|i| i.id == id)
                            {
                                interval.chunks_sent = true;
                                interval.transcribe_sent = true;
                                interval.transcribe_at = Some(Instant::now());
                            }
                            self.session.sending = None;
                        }
                        Ok(false) => {} // 列が満杯。次の tick で送り直す
                        Err(_) => send_failed = true,
                    }
                }
            } else if let Some((id, pcm)) = self
                .session
                .intervals
                .iter()
                .find(|i| !i.chunks_sent && !i.done)
                .map(|i| (i.id.clone(), i.pcm.clone()))
            {
                self.session.sending = Some(Sending::new(false, id, &pcm));
            }
            if send_failed {
                self.session.asr_failures += 1;
                if self.session.asr_failures > MAX_ASR_FAILURES {
                    fatal = Some("認識エンジンとの通信に失敗しました".into());
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
            let has_api_key = self.cfg.has_api_key();
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
        let worker = match self.session.worker.as_mut() {
            Some(worker) => worker,
            None => return,
        };
        // 前の仮が飛行中なら止めてから送る。列が満杯のときは次の tick で送り直す
        if let Some(old) = self.session.partial.take() {
            match worker.cancel(&old.id) {
                Ok(true) => {}
                Ok(false) | Err(_) => {
                    self.session.partial = Some(old);
                    return;
                }
            }
        }
        let id = format!("p{}", self.session.next_partial_index);
        self.session.next_partial_index += 1;
        self.session.last_partial_at = Some(Instant::now());
        self.session.sending = Some(Sending::new(true, id, &pcm));
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
        match Worker::spawn(&self.cfg.speech_exe, &self.cfg.model) {
            Ok(worker) => {
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
        if !self.cfg.recovery_history {
            return;
        }
        let joined = self.session.joined_text();
        if joined.is_empty() {
            return;
        }
        match self.session.recovery_id {
            None => match history::save_recognized(&joined) {
                Ok(id) => self.session.recovery_id = Some(id),
                Err(e) => {
                    eprintln!("復旧履歴を書き込めません: {e}");
                    self.session.recovery_write_failed = true;
                }
            },
            Some(id) => {
                if let Err(e) = history::update_raw(id, &joined) {
                    eprintln!("復旧履歴の追記に失敗しました: {e}");
                    self.session.recovery_write_failed = true;
                }
            }
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
            FormatOutcome::Done { text, .. } => {
                if let Some(id) = self.session.recovery_id {
                    if let Err(e) = history::update_formatted(id, &text) {
                        eprintln!("整形結果の復旧履歴保存に失敗しました: {e}");
                    }
                }
                self.dispatch(Event::FormatDone { text });
            }
            FormatOutcome::EditDone { text, .. } => {
                if let Some(id) = self.session.recovery_id {
                    if let Err(e) = history::update_formatted(id, &text) {
                        eprintln!("Edit の復旧履歴の保存に失敗しました: {e}");
                    }
                }
                self.dispatch(Event::EditFormatDone { text });
            }
            FormatOutcome::Failed { reason, .. } => self.dispatch(Event::FormatFailed { reason }),
            FormatOutcome::Delta { text, .. } => {
                // 整えた文が届いた分だけ帯に流す
                if self.cfg.live_caption {
                    overlay::set_caption(&text, "");
                }
            }
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

fn main() {
    let settings_mode = std::env::args().any(|a| a == "--settings");
    // 二重に起動するとキーの横取りが重なるので、2 つ目は何もせずに終わる。設定ウィンドウも 1 つだけ開く
    let _single = unsafe {
        let name = if settings_mode {
            windows::core::w!("Local\\OpenHinoVoice.Settings")
        } else {
            windows::core::w!("Local\\OpenHinoVoice.Singleton")
        };
        let handle = CreateMutexW(None, true, name);
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
    let hotkey = config::parse_hotkey(&cfg.hotkey).unwrap_or(config::Hotkey { mods: 0, vk: 0x77 });
    overlay::set_theme(&cfg.theme);
    overlay::set_caption_anchor(&cfg.caption_position);
    let mut config_mtime = config_modified();
    let mut last_config_check = Instant::now();

    if let Err(e) = overlay::create() {
        overlay::show_error(&e);
        return;
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

    let (format_tx, format_rx): (Sender<FormatOutcome>, Receiver<FormatOutcome>) = channel();
    let mut app = App {
        cfg,
        state: State::Idle,
        session: Session::new(),
        kept_worker: None,
        format_tx,
        format_rx: std::sync::Mutex::new(Some(format_rx)),
        format_generation: FormatGeneration::default(),
        transient_until: None,
        failed_pastes: VecDeque::new(),
    };
    hook::set_mirror(hook::MIRROR_IDLE);

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
        if overlay::EXIT_REQUESTED.load(std::sync::atomic::Ordering::SeqCst) {
            break;
        }

        if overlay::take_recovery_request() {
            app.copy_failed_text();
        }
        // 残していた worker の時間が来たら殺す
        if let Some((worker, expires)) = app.kept_worker.take() {
            if Instant::now() < expires {
                app.kept_worker = Some((worker, expires));
            } else {
                worker.shutdown();
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
        if overlay::EXIT_REQUESTED.load(std::sync::atomic::Ordering::SeqCst) {
            break;
        }

        while let Ok(key) = key_rx.try_recv() {
            match key {
                hook::KeyEvent::Hotkey => {
                    // Edit モード。文を選んだ状態でいつもの録音開始キーを押すと
                    // Edit になる。選択の確認は録音の開始を遅らせるので、
                    // 100 ミリ秒で打ち切る。読めなければ通常の音声入力になる
                    let edit_selection = if app.cfg.edit_mode && matches!(app.state, State::Idle) {
                        match uia::probe_selected_text(Duration::from_millis(100)) {
                            uia::SelectionProbe::Selected(text) => {
                                if text.chars().count() > MAX_EDIT_SELECTION_CHARS {
                                    Some(Err("選択が長すぎるため Edit を使えません".into()))
                                } else {
                                    Some(Ok(text))
                                }
                            }
                            uia::SelectionProbe::Unavailable => {
                                Some(Err("このアプリでは Edit を使えません".into()))
                            }
                            uia::SelectionProbe::None => None,
                        }
                    } else {
                        None
                    };
                    app.dispatch(Event::F8);
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

        let receiver = app
            .format_rx
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        if let Some(receiver) = &receiver {
            while let Ok(outcome) = receiver.try_recv() {
                app.receive_format(outcome);
            }
        }
        if let Some(receiver) = receiver {
            app.format_rx
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .replace(receiver);
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
                        overlay::set_caption_anchor(&cfg.caption_position);
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
                    State::Recording | State::Editing { .. } => {
                        overlay::show(Mode::Recording {
                            started: app.session.started_at,
                        });
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
                idle_sleep_duration(
                    &app.state,
                    app.transient_until.is_some(),
                    overlay::is_visible(),
                )
                .as_millis() as u32,
                QS_ALLINPUT,
                MWMO_INPUTAVAILABLE,
            );
        }
    }

    app.teardown_session();
    if let Some((worker, _)) = app.kept_worker.take() {
        worker.shutdown();
    }
    hook::uninstall();
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
        let mut session = Session::new();
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
        let mut session = Session::new();
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
        app.session.intervals[1].chunks_sent = true;
        app.session.intervals[1].transcribe_sent = true;
        app.session.intervals[1].transcribe_at = Some(Instant::now());
        app.set_state(State::Recognizing {
            raw_requested: false,
        });
        app.session.stop_requested = true;
        app.tick_session();
        // s0 の文章は残り、s1 は送り直しのために印が戻る。worker は起動し直し待ち
        assert!(app.session.intervals[0].done);
        assert_eq!(app.session.joined_text(), "ひとつめ。");
        assert!(!app.session.intervals[1].chunks_sent);
        assert!(!app.session.intervals[1].transcribe_sent);
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
        assert_eq!(app.failed_pastes[0], "ひとつめ。ふたつめ。");
    }

    #[test]
    fn a_worker_that_dies_mid_recording_resends_undone_intervals() {
        let mut app = test_app();
        let mut audio = vec![0.3f32; 16000];
        audio.extend(std::iter::repeat_n(0.0f32, 16000));
        assert!(app.session.feed_segmenter(&audio));
        let (_, rx) = channel();
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
        // 失敗の回数は tick の回し方で 1 か 2 になるので、起こったことだけを見る
        assert!(app.session.worker.is_none(), "worker を起動し直す");
        assert!(app.session.asr_failures >= 1);
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

    fn wait_for_written(recorder: &std::sync::Arc<std::sync::Mutex<String>>, min_lines: usize) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if sent_lines(recorder).len() >= min_lines {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
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
        for _ in 0..50 {
            app.tick_session();
            std::thread::sleep(Duration::from_millis(5));
        }
        wait_for_written(&recorder, 3);
        let lines = sent_lines(&recorder);
        let cancelled = lines
            .iter()
            .any(|v| v["type"] == "cancel" && v["id"] == "p0");
        assert!(cancelled, "古い仮 p0 を cancel する: {lines:?}");
        // cancel のあとに、新しい仮 p1 の音と transcribe が続く
        let p1_index = lines
            .iter()
            .position(|v| v["type"] == "chunk" && v["id"] == "p1");
        assert!(p1_index.is_some(), "次の仮 p1 が送られる: {lines:?}");
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
        wait_for_written(&recorder, 1);
        let lines = sent_lines(&recorder);
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
        for _ in 0..50 {
            app.tick_session();
            std::thread::sleep(Duration::from_millis(2));
        }
        let sent = recorder
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .lines()
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>();
        let partial_sent = sent.iter().any(|line| {
            let v: serde_json::Value = serde_json::from_str(line).unwrap_or_default();
            v["type"] == "transcribe" && v["id"] == "p0"
        });
        assert!(partial_sent, "仮の認識は p0 の要求として送る: {sent:?}");
        assert!(
            sent.iter().any(|line| line.contains("\"id\":\"p0\"")),
            "仮の音も同じ要求に属する"
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
        for _ in 0..30 {
            app.tick_session();
            std::thread::sleep(Duration::from_millis(2));
        }
        let sent = recorder
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .lines()
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>();
        assert!(
            !sent.iter().any(|line| line.contains("\"id\":\"p")),
            "確定の処理中に仮を送らない: {sent:?}"
        );
        assert!(
            sent.iter().any(|line| line.contains("\"id\":\"s0\"")),
            "確定の区間は送られている"
        );
    }

    #[test]
    fn edit_flow_recognizes_an_instruction_and_formats_it() {
        let mut app = test_app();
        // 整形の結果の受け口を、本編と同じ仕組みで用意する
        let (fmt_tx, fmt_rx) = channel();
        app.format_tx = fmt_tx;
        *app.format_rx.lock().unwrap_or_else(|p| p.into_inner()) = Some(fmt_rx);
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
            let receiver = app
                .format_rx
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .take();
            let mut got = false;
            if let Some(receiver) = &receiver {
                loop {
                    match receiver.try_recv() {
                        Ok(outcome) => {
                            got = true;
                            app.receive_format(outcome);
                        }
                        Err(std::sync::mpsc::TryRecvError::Empty) => break,
                        Err(_) => break,
                    }
                }
            }
            if let Some(receiver) = receiver {
                app.format_rx
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .replace(receiver);
            }
            if got && app.state == State::Idle {
                break;
            }
        }
        assert_eq!(app.state, State::Idle);
        assert!(
            app.failed_pastes.is_empty(),
            "失敗したときは何も貼らない: {:?}",
            app.failed_pastes
        );
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
        let (format_tx, _) = channel();
        App {
            cfg,
            state: State::Idle,
            session: Session::new(),
            kept_worker: None,
            format_tx,
            format_rx: std::sync::Mutex::new(None),
            format_generation: FormatGeneration::default(),
            transient_until: None,
            failed_pastes: VecDeque::new(),
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
