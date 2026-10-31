#![windows_subsystem = "windows"]

mod asr;
mod audio;
mod chatgpt;
mod config;
mod format;
mod hook;
mod overlay;
mod paste;
mod settings;
mod sound;
mod startup;
mod state;

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
    DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE, WM_QUIT,
};

// 10 minutes at 96 kHz mono PCM16 is 115.2 MB. Higher sample rates can hit the cap early.
const MAX_RECORDING_PCM_BYTES: usize = 128 * 1024 * 1024;
const PCM_LIMIT_MESSAGE: &str = "録音データが128 MiBの安全上限を超えました（高サンプルレートのマイクでは設定時間より早く終了する場合があります）";

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
}

impl FormatOutcome {
    fn generation(&self) -> u64 {
        match self {
            Self::Done { generation, .. }
            | Self::Failed { generation, .. }
            | Self::Retrying { generation, .. } => *generation,
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

struct Session {
    worker: Option<Worker>,
    recorder: Option<Recorder>,
    paste_target: Option<paste::PasteTarget>,
    pending_pcm: VecDeque<Vec<u8>>,
    recorded_pcm_bytes: usize,
    sample_rate: u32,
    stop_requested: bool,
    transcribe_sent: bool,
    transcribe_started_at: Option<Instant>,
    started_at: Instant,
    // 録音開始の音がマイクに入るので、鳴り終わるまでに録れた音は捨てる
    skip_until: Instant,
}

impl Session {
    fn new() -> Session {
        Session {
            worker: None,
            recorder: None,
            paste_target: None,
            pending_pcm: VecDeque::new(),
            recorded_pcm_bytes: 0,
            sample_rate: 16000,
            stop_requested: false,
            transcribe_sent: false,
            transcribe_started_at: None,
            started_at: Instant::now(),
            skip_until: Instant::now(),
        }
    }

    fn queue_audio(&mut self, samples: &[f32]) -> bool {
        let bytes = samples.len().saturating_mul(2);
        if bytes > MAX_RECORDING_PCM_BYTES.saturating_sub(self.recorded_pcm_bytes) {
            return false;
        }
        if !samples.is_empty() {
            self.pending_pcm
                .push_back(audio::samples_to_pcm_le(samples));
            self.recorded_pcm_bytes += bytes;
        }
        true
    }

    fn recording_time_limit(&self, now: Instant, max_minutes: u32) -> bool {
        now.saturating_duration_since(self.started_at)
            >= Duration::from_secs(max_minutes as u64 * 60)
    }
}

struct App {
    cfg: Config,
    state: State,
    session: Session,
    format_tx: Sender<FormatOutcome>,
    format_generation: FormatGeneration,
    transient_until: Option<Instant>,
    failed_pastes: VecDeque<String>,
}

fn mirror_of(state: &State) -> u8 {
    match state {
        State::Idle => hook::MIRROR_IDLE,
        State::Recording => hook::MIRROR_RECORDING,
        State::Recognizing { .. } => hook::MIRROR_RECOGNIZING,
        State::Formatting { .. } => hook::MIRROR_FORMATTING,
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
        if !matches!(self.state, State::Formatting { .. }) {
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
        self.session.pending_pcm.clear();
        self.session.stop_requested = false;
        self.session.transcribe_sent = false;
        self.session.transcribe_started_at = None;
        if let Some(worker) = self.session.worker.take() {
            worker.shutdown();
        }
    }

    /// Returns false if this action fails; subsequent actions in the transition must be skipped.
    fn execute(&mut self, action: Action) -> bool {
        match action {
            Action::StartSession => {
                self.session = Session::new();
                match paste::capture_target() {
                    Ok(target) => self.session.paste_target = Some(target),
                    Err(e) => {
                        self.fail_session(e);
                        return false;
                    }
                }
                match Worker::spawn(&self.cfg.speech_exe, &self.cfg.model) {
                    Ok(worker) => {
                        self.session.worker = Some(worker);
                    }
                    Err(e) => {
                        self.fail_session(e.to_string());
                        return false;
                    }
                }
                match Recorder::start() {
                    Ok(recorder) => {
                        self.session.sample_rate = recorder.sample_rate;
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
                    if !self.session.queue_audio(&samples) {
                        self.fail_session(PCM_LIMIT_MESSAGE.into());
                        return false;
                    }
                }
                self.session.stop_requested = true;
                self.sfx(Sound::Stop);
            }
            Action::DiscardSession => {
                self.teardown_session();
            }
            Action::CallFormat { raw } => {
                let cfg = self.cfg.format.clone();
                let tx = self.format_tx.clone();
                let generation = self.format_generation.start();
                let cancel = self.format_generation.cancel_flag();
                std::thread::spawn(move || {
                    let cancelled = || cancel.load(Ordering::SeqCst);
                    let result = format::format_with_retry(&cfg, &raw, &cancelled, |attempt| {
                        let _ = tx.send(FormatOutcome::Retrying {
                            generation,
                            attempt,
                        });
                    });
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
            Action::Paste { text } => {
                let result = self
                    .session
                    .paste_target
                    .take()
                    .ok_or_else(|| "録音開始時の入力先が不明です".to_string())
                    .and_then(|target| paste::paste_text(&text, target));
                match result {
                    Ok(()) => self.sfx(Sound::Done),
                    Err(e) => {
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
        }
    }

    fn tick_session(&mut self) {
        let in_recording = matches!(self.state, State::Recording);
        let in_recognizing = matches!(self.state, State::Recognizing { .. });
        if !in_recording && !in_recognizing {
            return;
        }

        if let Some(recorder) = &self.session.recorder {
            let samples = recorder.drain();
            overlay::push_level(&samples);
            if Instant::now() >= self.session.skip_until && !self.session.queue_audio(&samples) {
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

        if self.session.worker.is_none() {
            return;
        }

        // Ack / timeout checks are nonblocking even if speech.exe has stopped
        // reading stdin. The error path kills the stalled child and resets UI.
        if let Err(e) = self.session.worker.as_mut().unwrap().poll_writer() {
            self.dispatch(Event::AsrError {
                message: e.to_string(),
            });
            return;
        }

        let worker_ready = self.session.worker.as_ref().unwrap().is_ready();
        if let Some(phase) = asr::timeout_phase(
            self.session.started_at,
            worker_ready,
            self.session.transcribe_started_at,
            Instant::now(),
        ) {
            self.dispatch(Event::AsrError {
                message: format!("認識が時間切れです: {phase}"),
            });
            return;
        }

        let ready = {
            let worker = self.session.worker.as_mut().unwrap();
            if worker.is_ready() {
                true
            } else {
                match worker.try_ready() {
                    Ok(r) => r,
                    Err(e) => {
                        let message = e.to_string();
                        self.dispatch(Event::AsrError { message });
                        return;
                    }
                }
            }
        };

        if ready {
            let mut send_error: Option<String> = None;
            while let Some(chunk) = self.session.pending_pcm.front() {
                match self.session.worker.as_mut().unwrap().send_chunk(chunk) {
                    Ok(true) => {
                        self.session.pending_pcm.pop_front();
                    }
                    Ok(false) => break, // backpressure; retry without losing this chunk
                    Err(e) => {
                        send_error = Some(e.to_string());
                        break;
                    }
                }
            }
            if let Some(message) = send_error {
                self.dispatch(Event::AsrError { message });
                return;
            }
        }

        if in_recording {
            return;
        }

        if in_recognizing
            && self.session.stop_requested
            && ready
            && self.session.pending_pcm.is_empty()
            && !self.session.transcribe_sent
        {
            let sample_rate = self.session.sample_rate;
            let words = self.cfg.words.clone();
            let worker = self.session.worker.as_mut().unwrap();
            match worker.transcribe(sample_rate, &words) {
                Ok(true) => {
                    self.session.transcribe_sent = true;
                    self.session.transcribe_started_at = Some(Instant::now());
                }
                Ok(false) => {} // queue full; next tick will retry
                Err(e) => {
                    let message = e.to_string();
                    self.dispatch(Event::AsrError { message });
                    return;
                }
            }
        }

        if in_recognizing && self.session.transcribe_sent {
            let polled = {
                let worker = self.session.worker.as_mut().unwrap();
                worker.poll_end()
            };
            if let Some(result) = polled {
                if let Some(worker) = self.session.worker.take() {
                    worker.shutdown();
                }
                self.session.recorder = None;
                self.session.pending_pcm.clear();
                match result {
                    Ok(text) => {
                        let has_api_key = self.cfg.has_api_key();
                        self.dispatch(Event::AsrDone { text, has_api_key });
                    }
                    Err(e) => {
                        let message = e.to_string();
                        self.dispatch(Event::AsrError { message });
                    }
                }
            }
        }
    }

    fn receive_format(&mut self, outcome: FormatOutcome) {
        if !self.format_generation.accepts(
            outcome.generation(),
            matches!(self.state, State::Formatting { .. }),
        ) {
            return;
        }
        match outcome {
            FormatOutcome::Done { text, .. } => self.dispatch(Event::FormatDone { text }),
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

    let (format_tx, format_rx): (Sender<FormatOutcome>, Receiver<FormatOutcome>) = channel();
    let mut app = App {
        cfg,
        state: State::Idle,
        session: Session::new(),
        format_tx,
        format_generation: FormatGeneration::default(),
        transient_until: None,
        failed_pastes: VecDeque::new(),
    };
    hook::set_mirror(hook::MIRROR_IDLE);

    let mut msg = MSG::default();
    loop {
        if overlay::EXIT_REQUESTED.load(std::sync::atomic::Ordering::SeqCst) {
            break;
        }

        if overlay::take_recovery_request() {
            app.copy_failed_text();
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
            let event = match key {
                hook::KeyEvent::Hotkey => Event::F8,
                hook::KeyEvent::Enter => Event::Enter,
                hook::KeyEvent::Esc => Event::Esc,
            };
            app.dispatch(event);
        }

        while let Ok(outcome) = format_rx.try_recv() {
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
                if matches!(app.state, State::Idle) {
                    overlay::hide();
                }
            }
        }

        std::thread::sleep(Duration::from_millis(10));
    }

    app.teardown_session();
    hook::uninstall();
    overlay::remove_tray();
    let _ = LPARAM(0);
    let _ = WPARAM(0);
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
    fn recording_limits_keep_pcm_bounded_and_use_existing_stop_transition() {
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
        session.recorded_pcm_bytes = MAX_RECORDING_PCM_BYTES - 4;
        assert!(session.queue_audio(&[0.0, 1.0]));
        assert_eq!(session.recorded_pcm_bytes, MAX_RECORDING_PCM_BYTES);
        assert!(!session.queue_audio(&[0.0]));
        assert_eq!(session.pending_pcm.len(), 1);
        let (next, actions) = state::step(State::Recording, Event::Enter);
        assert!(matches!(next, State::Recognizing { .. }));
        assert!(actions.contains(&Action::StopRecordingAndTranscribe));
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
            format_tx,
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
