#![windows_subsystem = "windows"]

mod asr;
mod chatgpt;
mod audio;
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
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS, LPARAM, WPARAM};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE, WM_QUIT,
};

enum FormatOutcome {
    Done(String),
    Failed(String),
}

struct Session {
    worker: Option<Worker>,
    recorder: Option<Recorder>,
    pending_pcm: Vec<Vec<u8>>,
    sample_rate: u32,
    stop_requested: bool,
    transcribe_sent: bool,
    started_at: Instant,
    // 録音開始の音がマイクに入るので、鳴り終わるまでに録れた音は捨てる
    skip_until: Instant,
}

impl Session {
    fn new() -> Session {
        Session {
            worker: None,
            recorder: None,
            pending_pcm: Vec::new(),
            sample_rate: 16000,
            stop_requested: false,
            transcribe_sent: false,
            started_at: Instant::now(),
            skip_until: Instant::now(),
        }
    }
}

struct App {
    cfg: Config,
    state: State,
    session: Session,
    format_tx: Sender<FormatOutcome>,
    transient_until: Option<Instant>,
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
        for action in actions {
            self.execute(action);
        }
    }

    fn fail_session(&mut self, message: String) {
        self.teardown_session();
        self.set_state(State::Idle);
        self.execute(Action::ShowOverlay(OverlayKind::Error { message }));
    }

    fn teardown_session(&mut self) {
        self.session.recorder = None;
        self.session.pending_pcm.clear();
        self.session.stop_requested = false;
        self.session.transcribe_sent = false;
        if let Some(worker) = self.session.worker.take() {
            worker.shutdown();
        }
    }

    fn execute(&mut self, action: Action) {
        match action {
            Action::StartSession => {
                self.session = Session::new();
                match Worker::spawn(&self.cfg.speech_exe, &self.cfg.model) {
                    Ok(worker) => {
                        self.session.worker = Some(worker);
                    }
                    Err(e) => {
                        self.fail_session(e.to_string());
                        return;
                    }
                }
                match Recorder::start() {
                    Ok(recorder) => {
                        self.session.sample_rate = recorder.sample_rate;
                        self.session.recorder = Some(recorder);
                        if self.cfg.sounds {
                            self.sfx(Sound::Start);
                            self.session.skip_until = Instant::now() + sound::duration(Sound::Start) + Duration::from_millis(40);
                        }
                    }
                    Err(e) => {
                        self.fail_session(e);
                        return;
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
                    if !samples.is_empty() {
                        self.session
                            .pending_pcm
                            .push(audio::samples_to_pcm_le(&samples));
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
                std::thread::spawn(move || {
                    let outcome = match format::format(&cfg, &raw) {
                        Ok(text) => FormatOutcome::Done(text),
                        Err(e) => FormatOutcome::Failed(e.to_string()),
                    };
                    let _ = tx.send(outcome);
                });
            }
            Action::Paste { text } => match paste::paste_text(&text) {
                Ok(()) => self.sfx(Sound::Done),
                Err(e) => self.execute(Action::ShowOverlay(OverlayKind::Error { message: e })),
            },
            Action::ShowOverlay(kind) => {
                self.show_overlay(kind);
            }
            Action::HideOverlay => {
                self.transient_until = None;
                overlay::hide();
            }
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
                overlay::show(Mode::Recording { started: self.session.started_at });
            }
            OverlayKind::Recognizing => {
                self.transient_until = None;
                overlay::show(Mode::Busy { label: "認識中".into(), hint: None });
            }
            OverlayKind::Formatting => {
                self.transient_until = None;
                overlay::show(Mode::Busy { label: "整形中".into(), hint: Some("Enter で生テキスト".into()) });
            }
            OverlayKind::FormatSkipped { reason } => {
                overlay::show(Mode::Notice { tone: Tone::Warn, text: format!("生テキストを貼りました（{reason}）") });
                self.transient_until = Some(Instant::now() + Duration::from_millis(2500));
            }
            OverlayKind::ApiKeyMissing => {
                overlay::show(Mode::Notice { tone: Tone::Warn, text: "APIキー未設定のため生テキストを貼りました".into() });
                self.transient_until = Some(Instant::now() + Duration::from_millis(2500));
            }
            OverlayKind::Error { message } => {
                self.sfx(Sound::Error);
                overlay::show(Mode::Notice { tone: Tone::Error, text: message });
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
            if !samples.is_empty() && Instant::now() >= self.session.skip_until {
                self.session
                    .pending_pcm
                    .push(audio::samples_to_pcm_le(&samples));
            }
        }

        if self.session.worker.is_none() {
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
            let pending = std::mem::take(&mut self.session.pending_pcm);
            let mut send_error: Option<String> = None;
            {
                let worker = self.session.worker.as_mut().unwrap();
                for chunk in &pending {
                    if let Err(e) = worker.send_chunk(chunk) {
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

        if in_recognizing && self.session.stop_requested && ready && !self.session.transcribe_sent {
            let sample_rate = self.session.sample_rate;
            let words = self.cfg.words.clone();
            let worker = self.session.worker.as_mut().unwrap();
            match worker.transcribe(sample_rate, &words) {
                Ok(()) => {
                    self.session.transcribe_sent = true;
                }
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
                        self.dispatch(Event::AsrDone {
                            text,
                            has_api_key,
                        });
                    }
                    Err(e) => {
                        let message = e.to_string();
                        self.dispatch(Event::AsrError { message });
                    }
                }
            }
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
            eprintln!("{e}");
            return;
        }
    };
    let hotkey = config::parse_hotkey(&cfg.hotkey).unwrap_or(config::Hotkey { mods: 0, vk: 0x77 });
    overlay::set_theme(&cfg.theme);
    let mut config_mtime = config_modified();
    let mut last_config_check = Instant::now();

    if let Err(e) = overlay::create() {
        eprintln!("{e}");
        return;
    }

    let (key_tx, key_rx) = channel::<hook::KeyEvent>();
    if let Err(e) = hook::install(key_tx, hotkey) {
        eprintln!("{e}");
        return;
    }

    let (format_tx, format_rx): (Sender<FormatOutcome>, Receiver<FormatOutcome>) = channel();
    let mut app = App {
        cfg,
        state: State::Idle,
        session: Session::new(),
        format_tx,
        transient_until: None,
    };
    hook::set_mirror(hook::MIRROR_IDLE);

    let mut msg = MSG::default();
    loop {
        if overlay::EXIT_REQUESTED.load(std::sync::atomic::Ordering::SeqCst) {
            break;
        }
        unsafe {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_QUIT {
                    overlay::EXIT_REQUESTED
                        .store(true, std::sync::atomic::Ordering::SeqCst);
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
            match outcome {
                FormatOutcome::Done(text) => app.dispatch(Event::FormatDone { text }),
                FormatOutcome::Failed(reason) => app.dispatch(Event::FormatFailed { reason }),
            }
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
                    Err(message) => app.execute(Action::ShowOverlay(OverlayKind::Error { message })),
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
    std::fs::metadata(config::config_path()).and_then(|m| m.modified()).ok()
}
