//! トレイの「設定」から別プロセスで開く管理画面。保存すると常駐側が設定ファイルの変化に気づいて読み直す

mod preview;
mod setup;
mod test_capture;

use crate::asr;
use crate::audio;
use crate::chatgpt;
use crate::config::{self, Config};
use crate::feedback::{
    self, FeedbackMotion, FeedbackSettings, FeedbackSnapshot, HudStyle, Preset, SoundEvent,
    SoundSet,
};
use crate::history;
use crate::paste;
use crate::startup;
use eframe::egui::{self, Color32, RichText};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

const HINOMARU_RED: Color32 = Color32::from_rgb(188, 0, 45);
const OK_GREEN: Color32 = Color32::from_rgb(40, 140, 70);
const SAMPLE: &str = "えーと、明日の会議は午後3時からに変更になりました。資料は、あのー、共有フォルダにおいてあります。";
const MIC_TEST_DURATION: Duration = Duration::from_secs(10);
const ASR_MODELS: [(&str, &str); 4] = [
    ("haas-ja-v1.0:q4_k", "Q4_K(0.35GB、既定)"),
    ("haas-ja-v1.0:q5_k", "Q5_K(0.43GB)"),
    ("haas-ja-v1.0:q6_k", "Q6_K(0.51GB)"),
    ("haas-ja-v1.0", "Q8_0(0.66GB、最も正確)"),
];

pub fn run() {
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("OpenHinoVoice")
        .with_active(!config::is_test_sandbox())
        .with_inner_size(initial_size())
        .with_min_inner_size([600.0, 420.0]);
    if let Ok(icon) =
        eframe::icon_data::from_png_bytes(include_bytes!("../assets/branding/settings.png"))
    {
        viewport = viewport.with_icon(icon);
    }
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    if let Err(e) = eframe::run_native(
        "OpenHinoVoice",
        options,
        Box::new(|cc| {
            use_japanese_font(&cc.egui_ctx);
            // 選択中の項目やチェックの色を日の丸の赤に寄せる
            cc.egui_ctx.all_styles_mut(|style| {
                style.visuals.selection.bg_fill = Color32::from_rgb(150, 0, 36);
                style.visuals.selection.stroke.color = Color32::WHITE;
                style.spacing.item_spacing = egui::vec2(10.0, 8.0);
                style.spacing.button_padding = egui::vec2(10.0, 6.0);
            });
            let mut settings = Settings::load();
            settings.preview.observe_environment(cc);
            settings._test_capture = test_capture::Capture::install(cc);
            Ok(Box::new(settings))
        }),
    ) {
        crate::overlay::show_error(&format!("設定画面を開けません: {e}"));
    }
}

fn initial_size() -> [f32; 2] {
    if config::is_test_sandbox() {
        match std::env::var("OHV_TEST_SETTINGS_SIZE").as_deref() {
            Ok("600x420") => return [600.0, 420.0],
            Ok("720x520") => return [720.0, 520.0],
            _ => {}
        }
    }
    [720.0, 520.0]
}

fn test_page(name: &str) -> Option<Page> {
    match name {
        "start" => Some(Page::Start),
        "home" => Some(Page::Home),
        "input" => Some(Page::Input),
        "display" => Some(Page::Display),
        "asr" => Some(Page::Asr),
        "ai" => Some(Page::Ai),
        "edit" => Some(Page::Edit),
        "history" => Some(Page::History),
        "about" => Some(Page::About),
        _ => None,
    }
}

fn use_japanese_font(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    for path in [
        r"C:\Windows\Fonts\YuGothM.ttc",
        r"C:\Windows\Fonts\meiryo.ttc",
    ] {
        if let Ok(bytes) = std::fs::read(path) {
            fonts
                .font_data
                .insert("ja".into(), Arc::new(egui::FontData::from_owned(bytes)));
            for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
                fonts
                    .families
                    .entry(family)
                    .or_default()
                    .insert(0, "ja".into());
            }
            break;
        }
    }
    ctx.set_fonts(fonts);
}

#[derive(Clone, Copy, PartialEq)]
enum Page {
    Start,
    Home,
    Input,
    Display,
    Asr,
    Ai,
    Edit,
    History,
    About,
}

/// 別スレッドで走らせる処理の結果の受け皿
type Job<T> = Arc<Mutex<Option<T>>>;

fn spawn<T: Send + 'static>(
    ctx: &egui::Context,
    work: impl FnOnce() -> T + Send + 'static,
) -> Job<T> {
    let slot: Job<T> = Arc::new(Mutex::new(None));
    let (out, ctx) = (slot.clone(), ctx.clone());
    std::thread::spawn(move || {
        let result = work();
        *out.lock().unwrap_or_else(|p| p.into_inner()) = Some(result);
        ctx.request_repaint();
    });
    slot
}

fn take<T>(job: &mut Option<Job<T>>) -> Option<T> {
    let result = job
        .as_ref()?
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .take();
    if result.is_some() {
        *job = None;
    }
    result
}

/// 設定画面上のボタンからのみ呼ぶ。実在する自プロセスのウィンドウを
/// clipboard owner に渡す（HWND::default() では SetClipboardData が失敗する）。
fn settings_clipboard_owner() -> Result<HWND, String> {
    let window = unsafe { GetForegroundWindow() };
    check_settings_clipboard_owner(window)?;
    Ok(window)
}

fn check_settings_clipboard_owner(window: HWND) -> Result<(), String> {
    if window.0.is_null() {
        return Err("設定ウィンドウを取得できません".into());
    }
    let mut pid = 0;
    let thread = unsafe { GetWindowThreadProcessId(window, Some(&mut pid)) };
    if thread == 0 || pid != std::process::id() {
        return Err("設定画面が前面にありません。設定画面を選び直してください".into());
    }
    Ok(())
}

/// 3秒の待機は別スレッド、実際の貼り付けは設定画面のUIスレッド(STA)で行う。
/// OLEクリップボードの参照を一時ワーカースレッドに残さないため。
fn repaste_after_switch(text: &str, owner: HWND) -> Result<paste::PasteTransaction, String> {
    // 待機中に設定ウィンドウが閉じられていたら、安全に中断する。
    check_settings_clipboard_owner(owner)?;
    let target = paste::capture_target()?;
    if target.process_id() == std::process::id() {
        return Err("貼り付け先のアプリに切り替えてください".into());
    }
    paste::PasteTransaction::begin(text, target, owner, Instant::now())
}

type ModelList = Vec<(String, String)>;

struct MicrophoneTest {
    recorder: audio::Recorder,
    started: Instant,
    samples: Vec<f32>,
    peak: f32,
    level: f32,
}

struct TranscribedFile {
    text: String,
    history_warning: Option<String>,
}

struct PreparationNotice<'a> {
    heading: &'static str,
    detail: &'a str,
    ready: bool,
}

fn preparation_notice(
    exe_ready: bool,
    status: Option<&setup::ModelStatus>,
    offline: bool,
) -> PreparationNotice<'_> {
    if !exe_ready {
        return PreparationNotice {
            heading: "speech.cpp が見つかりません",
            detail: "セットアップを開き、導入方法や speech.exe の場所を確認してください。",
            ready: false,
        };
    }
    let (heading, detail, ready) = match status {
        Some(setup::ModelStatus::Ready) => (
            "認識モデルは取得済みです",
            "マイクと録音キーはセットアップで確認できます。",
            true,
        ),
        Some(setup::ModelStatus::Missing) if offline => (
            "認識モデルが未取得です",
            "オフラインモードでは取得できません。セットアップで準備方法を確認してください。",
            false,
        ),
        Some(setup::ModelStatus::Missing) => (
            "認識モデルが未取得です",
            "セットアップでモデルを取得すると、音声を認識できるようになります。",
            false,
        ),
        Some(setup::ModelStatus::Running { message, .. }) => {
            ("認識モデルを準備しています", message.as_str(), false)
        }
        Some(setup::ModelStatus::Failed(error)) => {
            ("認識モデルを確認できません", error.as_str(), false)
        }
        Some(setup::ModelStatus::Cancelled) => (
            "モデルの処理を中断しました",
            "セットアップで状態を確認し、必要ならもう一度取得してください。",
            false,
        ),
        None if offline => (
            "認識モデルは未確認です",
            "オフラインでも取得済みか確認できます。セットアップを開いて確認してください。",
            false,
        ),
        None => (
            "認識モデルは未確認です",
            "初回はセットアップで、選択中のモデルが取得済みか確認してください。",
            false,
        ),
    };
    PreparationNotice {
        heading,
        detail,
        ready,
    }
}

#[derive(Clone)]
struct Draft {
    config: Config,
    startup: bool,
    timeout_secs: f32,
    extra_body: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DisplaySnapshot {
    feedback: FeedbackSnapshot,
    theme: String,
    live_caption: bool,
    partial_caption: bool,
    caption_position: String,
}

impl DisplaySnapshot {
    fn capture(cfg: &Config) -> Self {
        Self {
            feedback: FeedbackSnapshot::capture(cfg),
            theme: cfg.theme.clone(),
            live_caption: cfg.live_caption,
            partial_caption: cfg.partial_caption,
            caption_position: cfg.caption_position.clone(),
        }
    }

    fn restore(&self, cfg: &mut Config) {
        self.feedback.restore(cfg);
        cfg.theme.clone_from(&self.theme);
        cfg.live_caption = self.live_caption;
        cfg.partial_caption = self.partial_caption;
        cfg.caption_position.clone_from(&self.caption_position);
    }
}

struct Settings {
    cfg: Config,
    page: Page,
    show_key: bool,
    startup: bool,
    timeout_secs: f32,
    message: Option<(bool, String)>,
    models: Vec<(String, String)>,
    models_job: Option<Job<Result<ModelList, String>>>,
    test_job: Option<Job<(Result<String, String>, f32)>>,
    test_result: Option<(Result<String, String>, f32)>,
    chatgpt_status: chatgpt::Status,
    chatgpt_job: Option<Job<Result<chatgpt::Status, String>>>,
    file_path: String,
    file_job: Option<Job<Result<TranscribedFile, String>>>,
    file_result: Option<Result<String, String>>,
    history_message: Option<String>,
    history_paste_job: Option<Job<(String, isize)>>,
    microphones: Vec<String>,
    history_entries: Vec<history::RecoveryEntry>,
    history_loaded: bool,
    history_clear_confirm: bool,
    /// リクエストの追加項目(extra_body)の編集用。JSON の形で書いてもらう
    extra_body_text: String,
    saved: Draft,
    close_confirm: bool,
    allow_close: bool,
    closing: bool,
    paste_transaction: Option<paste::PasteTransaction>,
    home_job: Option<Job<Result<history::HomeSnapshot, String>>>,
    home_snapshot: Option<Result<history::HomeSnapshot, String>>,
    history_job: Option<Job<Result<Vec<history::RecoveryEntry>, String>>>,
    history_clear_job: Option<Job<Result<(), String>>>,
    model_job: Option<setup::ModelJob>,
    retiring_jobs: Vec<setup::ModelJob>,
    model_status: Option<setup::ModelStatus>,
    model_selection: (String, String),
    mic_test: Option<MicrophoneTest>,
    mic_result: Option<(bool, String)>,
    preview: preview::Preview,
    _test_capture: Option<test_capture::Capture>,
    brand_icon: Option<egui::TextureHandle>,
    validation_focus: Option<Page>,
}

impl Settings {
    fn load() -> Settings {
        let (cfg, message) = match config::load() {
            Ok(cfg) => (cfg, None),
            Err(e) => (
                config::parse(config::EXAMPLE_TOML).expect("example parses"),
                Some((false, e)),
            ),
        };
        Self::from_loaded(
            cfg,
            message,
            startup::is_enabled(),
            chatgpt::status(),
            audio::input_devices(),
        )
    }

    fn from_loaded(
        cfg: Config,
        message: Option<(bool, String)>,
        startup: bool,
        chatgpt_status: chatgpt::Status,
        microphones: Vec<String>,
    ) -> Settings {
        let extra_body_text =
            serde_json::to_string_pretty(&cfg.format.extra_body).unwrap_or_else(|_| "{}".into());
        let page = if Self::needs_setup(&cfg) {
            Page::Start
        } else {
            Page::Home
        };
        let page = if config::is_test_sandbox() {
            std::env::var("OHV_TEST_SETTINGS_PAGE")
                .ok()
                .and_then(|name| test_page(&name))
                .unwrap_or(page)
        } else {
            page
        };
        let saved = Draft {
            config: cfg.clone(),
            startup,
            timeout_secs: cfg.format.timeout_ms as f32 / 1000.0,
            extra_body: extra_body_text.clone(),
        };
        let model_selection = (cfg.speech_exe.clone(), cfg.model.clone());
        Settings {
            timeout_secs: cfg.format.timeout_ms as f32 / 1000.0,
            cfg,
            page,
            show_key: false,
            startup,
            message,
            models: Vec::new(),
            models_job: None,
            test_job: None,
            test_result: None,
            chatgpt_status,
            chatgpt_job: None,
            file_path: String::new(),
            file_job: None,
            file_result: None,
            history_message: None,
            history_paste_job: None,
            microphones,
            history_entries: Vec::new(),
            history_loaded: false,
            history_clear_confirm: false,
            extra_body_text,
            saved,
            close_confirm: false,
            allow_close: false,
            closing: false,
            paste_transaction: None,
            home_job: None,
            home_snapshot: None,
            history_job: None,
            history_clear_job: None,
            model_job: None,
            retiring_jobs: Vec::new(),
            model_status: None,
            model_selection,
            mic_test: None,
            mic_result: None,
            preview: preview::Preview::default(),
            _test_capture: None,
            brand_icon: None,
            validation_focus: None,
        }
    }

    /// speech.cpp が見つからないときは、始め方の案内を最初に出す
    fn needs_setup(cfg: &config::Config) -> bool {
        let detected;
        let exe = if cfg.speech_exe.trim().is_empty() {
            detected = config::find_speech_exe();
            detected.trim()
        } else {
            cfg.speech_exe.trim()
        };
        if exe.is_empty() {
            return true;
        }
        let path = std::path::Path::new(exe);
        if path.is_file() {
            return false;
        }
        if path.is_absolute() || path.components().count() > 1 {
            return true;
        }
        !std::env::var_os("PATH")
            .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(path).is_file()))
    }

    fn validation_issue(&self) -> Option<(Page, String)> {
        if config::parse_hotkey(&self.cfg.hotkey).is_none() {
            return Some((Page::Input, "録音開始キーを確認してください。".into()));
        }
        if let Err(error) = crate::lexicon::validate(&self.cfg.terms) {
            return Some((Page::Asr, format!("用語辞書を確認してください。{error}")));
        }
        config::parse_extra_body(&self.extra_body_text)
            .err()
            .map(|error| (Page::Ai, format!("AIの追加項目を確認してください。{error}")))
    }

    fn show_validation_issue(&mut self, ctx: &egui::Context, page: Page, message: String) {
        self.select_page(page);
        self.message = Some((false, message));
        self.validation_focus = Some(page);
        let field = match page {
            Page::Input => Some("recording_hotkey"),
            Page::Ai => Some("format_extra_body"),
            _ => None,
        };
        if let Some(field) = field {
            ctx.memory_mut(|memory| memory.request_focus(egui::Id::unique(field)));
        }
    }

    fn save_from_ui(&mut self, ctx: &egui::Context) {
        if let Some((page, message)) = self.validation_issue() {
            self.show_validation_issue(ctx, page, message);
            return;
        }
        self.message = Some(match self.save() {
            Ok(()) => (true, "保存しました。次の録音から反映されます。".into()),
            Err(error) => (false, error),
        });
    }

    fn save(&mut self) -> Result<(), String> {
        use toml::Value as V;
        if config::parse_hotkey(&self.cfg.hotkey).is_none() {
            return Err("「入力」で録音開始キーを確認してください".into());
        }
        crate::lexicon::validate(&self.cfg.terms)?;
        let f = &self.cfg.format;
        let path = config::config_path();
        let text =
            std::fs::read_to_string(&path).map_err(|e| format!("設定ファイルを読めません: {e}"))?;
        // 自動検出したパスは保存しない。空のまま残すと、speech.cpp を更新したときに新しい版を使える
        let speech_exe = if self.cfg.speech_exe.trim() == config::find_speech_exe() {
            ""
        } else {
            self.cfg.speech_exe.trim()
        };
        let mut edits = vec![
            (None, "hotkey", V::from(self.cfg.hotkey.trim())),
            (
                None,
                "max_recording_minutes",
                V::from(self.cfg.max_recording_minutes as i64),
            ),
            (None, "theme", V::from(self.cfg.theme.as_str())),
            (None, "speech_exe", V::from(speech_exe)),
            (None, "model", V::from(self.cfg.model.as_str())),
            (None, "words", V::from(self.cfg.words.trim())),
            (None, "microphone", V::from(self.cfg.microphone.trim())),
            (None, "recovery_history", V::from(self.cfg.recovery_history)),
            (None, "sounds", V::from(self.cfg.sounds)),
            (None, "volume", V::from(self.cfg.volume as i64)),
            (None, "live_caption", V::from(self.cfg.live_caption)),
            (None, "partial_caption", V::from(self.cfg.partial_caption)),
            (
                None,
                "caption_position",
                V::from(self.cfg.caption_position.as_str()),
            ),
            (None, "edit_mode", V::from(self.cfg.edit_mode)),
            (
                None,
                "keep_worker_minutes",
                V::from(self.cfg.keep_worker_minutes as i64),
            ),
            (Some("format"), "provider", V::from(f.provider.as_str())),
            (Some("format"), "offline", V::from(f.offline)),
            (
                Some("format"),
                "edit_prompt",
                V::from(f.edit_prompt.as_str()),
            ),
            (Some("format"), "usage_log", V::from(f.usage_log)),
            (Some("format"), "base_url", V::from(f.base_url.trim())),
            (Some("format"), "api_key", V::from("")),
            (Some("format"), "model", V::from(f.model.trim())),
            (Some("format"), "openai_api_key", V::from("")),
            (
                Some("format"),
                "openai_model",
                V::from(f.openai_model.trim()),
            ),
            (
                Some("format"),
                "chatgpt_model",
                V::from(f.chatgpt_model.trim()),
            ),
            (Some("format"), "fast", V::from(f.fast)),
            (Some("format"), "save_history", V::from(f.save_history)),
            (
                Some("format"),
                "timeout_ms",
                V::from((self.timeout_secs * 1000.0).round() as i64),
            ),
            (Some("format"), "prompt", V::from(f.prompt.as_str())),
        ];
        edits.extend(FeedbackSnapshot::capture(&self.cfg).edits());
        let extra_body = config::parse_extra_body(&self.extra_body_text)?;
        let updated = config::set_values(text.trim_start_matches('\u{feff}'), &edits)?;
        let mut doc: toml_edit::DocumentMut = updated
            .parse()
            .map_err(|e| format!("設定ファイルの更新に失敗しました: {e}"))?;
        // 用語辞書だけは配列テーブルのため別途更新。その他のTOMLは保持する。
        doc.remove("terms");
        if !self.cfg.terms.is_empty() {
            let mut tables = toml_edit::ArrayOfTables::new();
            for term in &self.cfg.terms {
                let mut table = toml_edit::Table::new();
                table["canonical"] = toml_edit::value(&term.canonical);
                if !term.replace_from.is_empty() {
                    let mut aliases = toml_edit::Array::new();
                    for alias in &term.replace_from {
                        aliases.push(alias.as_str());
                    }
                    table["replace_from"] =
                        toml_edit::Item::Value(toml_edit::Value::Array(aliases));
                }
                if !term.hint.is_empty() {
                    table["hint"] = toml_edit::value(&term.hint);
                }
                tables.push(table);
            }
            doc["terms"] = toml_edit::Item::ArrayOfTables(tables);
        }
        config::update_extra_body(&mut doc, &extra_body)?;
        config::save_api_keys(f.api_key.trim(), f.openai_api_key.trim())?;
        config::save(&doc.to_string())?;
        startup::set_enabled(self.startup)?;
        self.saved = self.draft();
        self.close_confirm = false;
        Ok(())
    }

    /// 編集中の内容で整形の設定を作る。接続テストとモデル一覧に使う
    fn draft_format(&self) -> Result<config::FormatConfig, String> {
        let mut f = self.cfg.format.clone();
        f.timeout_ms = (self.timeout_secs * 1000.0).round() as u64;
        f.extra_body = config::parse_extra_body(&self.extra_body_text)?;
        Ok(f)
    }

    fn draft(&self) -> Draft {
        Draft {
            config: self.cfg.clone(),
            startup: self.startup,
            timeout_secs: self.timeout_secs,
            extra_body: self.extra_body_text.clone(),
        }
    }

    fn is_dirty(&self) -> bool {
        self.cfg != self.saved.config
            || self.startup != self.saved.startup
            || self.timeout_secs != self.saved.timeout_secs
            || self.extra_body_text != self.saved.extra_body
    }

    fn select_page(&mut self, page: Page) {
        if self.page == page {
            return;
        }
        self.preview.stop();
        self.validation_focus = None;
        self.mic_test = None;
        self.mic_result = None;
        self.page = page;
        if page == Page::Home {
            self.home_snapshot = None;
        }
        if page == Page::History {
            self.history_loaded = false;
        }
    }

    fn refresh_home(&mut self, ctx: &egui::Context) {
        if self.home_job.is_none() {
            self.home_job = Some(spawn(ctx, || {
                history::home_snapshot().map_err(|e| e.to_string())
            }));
        }
    }

    fn retire_model_job(&mut self) {
        if let Some(job) = self.model_job.take() {
            job.cancel();
            if !job.is_finished() {
                self.retiring_jobs.push(job);
            }
        }
    }

    fn poll_retiring_jobs(&mut self, ctx: &egui::Context) {
        self.retiring_jobs.retain(|job| !job.is_finished());
        if !self.retiring_jobs.is_empty() {
            ctx.request_repaint_after(Duration::from_millis(50));
        }
    }

    fn request_close(&mut self, ctx: &egui::Context) {
        self.preview.stop();
        if self.is_dirty() && !self.allow_close {
            self.closing = false;
            self.close_confirm = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            return;
        }
        self.mic_test = None;
        self.history_paste_job = None;
        self.retire_model_job();
        self.poll_retiring_jobs(ctx);
        if self.paste_transaction.is_some() || !self.retiring_jobs.is_empty() {
            self.closing = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            if let Some(tx) = &self.paste_transaction {
                ctx.request_repaint_after(tx.deadline().saturating_duration_since(Instant::now()));
            }
        } else {
            self.closing = false;
            self.allow_close = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    fn update_model_jobs(&mut self, ctx: &egui::Context) {
        let selection = (self.cfg.speech_exe.clone(), self.cfg.model.clone());
        if selection != self.model_selection {
            self.retire_model_job();
            self.model_status = None;
            self.model_selection = selection;
        }
        if let Some(job) = &self.model_job {
            if self.cfg.format.offline && job.is_download() {
                job.cancel();
            }
            let status = job.status();
            let done = !matches!(status, setup::ModelStatus::Running { .. });
            self.model_status = Some(status);
            if done {
                self.retire_model_job();
            }
        }
        self.poll_retiring_jobs(ctx);
        if self.closing {
            self.request_close(ctx);
        }
    }

    fn update_microphone(&mut self, ctx: &egui::Context) {
        let Some(test) = self.mic_test.as_mut() else {
            return;
        };
        if let Some(error) = test.recorder.take_error() {
            self.mic_result = Some((false, error));
            self.mic_test = None;
            return;
        }
        test.recorder.drain_into(&mut test.samples);
        test.level = audio::rms(&test.samples);
        test.peak = test.peak.max(test.level);
        if test.started.elapsed() >= MIC_TEST_DURATION {
            self.finish_microphone();
        } else {
            ctx.request_repaint_after(Duration::from_millis(50));
        }
    }

    fn finish_microphone(&mut self) {
        if let Some(test) = self.mic_test.take() {
            let detected = test.peak >= 0.001;
            self.mic_result = Some((detected, if detected {
                "マイクの入力を検出しました。保存後、録音開始キー → 話す → Enter で音声入力を試せます"
            } else {
                "入力がほとんどありません。マイクの選択、ミュート、Windows のマイク許可を確認してください"
            }.into()));
        }
    }
}

impl eframe::App for Settings {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if ctx.input(|input| input.viewport().visible() == Some(false)) {
            self.preview.stop();
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let hidden = ui
            .ctx()
            .input(|input| input.viewport().visible() == Some(false));
        if hidden || self.page != Page::Display {
            self.preview.stop();
        }
        if self.preview.running()
            && ui
                .ctx()
                .input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
        {
            self.preview.stop();
        }
        if !self.closing && !self.close_confirm && save_shortcut(ui.ctx()) {
            self.save_from_ui(ui.ctx());
        }
        self.update_microphone(ui.ctx());
        if ui.ctx().input(|i| i.viewport().close_requested()) {
            self.request_close(ui.ctx());
        }
        if let Some(tx) = &self.paste_transaction {
            let remaining = tx.deadline().saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                let result = self.paste_transaction.take().unwrap().restore();
                self.history_message = Some(match result {
                    Ok(()) => "入力を送信しました。貼り付け先で内容を確認してください".into(),
                    Err(e) => {
                        format!("入力は送信しましたが、クリップボードの復元を確認できません: {e}")
                    }
                });
                if self.closing {
                    self.request_close(ui.ctx());
                }
            } else {
                ui.ctx().request_repaint_after(remaining);
            }
        }
        if let Some(result) = take(&mut self.home_job) {
            self.home_snapshot = Some(result);
        }
        if let Some(result) = take(&mut self.history_job) {
            match result {
                Ok(mut entries) => {
                    entries.sort_by_key(|entry| std::cmp::Reverse(entry.id));
                    self.history_entries = entries;
                    self.history_message = None;
                }
                Err(e) => {
                    self.history_message = Some(format!(
                        "履歴を読めません: {e}。履歴を更新して再試行できます"
                    ))
                }
            }
        }
        if let Some(result) = take(&mut self.history_clear_job) {
            self.history_message = Some(match result {
                Ok(()) => {
                    self.history_entries.clear();
                    self.history_job = None;
                    self.home_snapshot = None;
                    self.home_job = None;
                    "復旧履歴を消去しました".into()
                }
                Err(e) => format!("消去できません: {e}"),
            });
        }
        self.update_model_jobs(ui.ctx());
        if let Some(r) = take(&mut self.models_job) {
            match r {
                Ok(m) => self.models = m,
                Err(e) => self.message = Some((false, e)),
            }
        }
        if let Some(r) = take(&mut self.test_job) {
            self.test_result = Some(r);
        }
        if let Some(r) = take(&mut self.chatgpt_job) {
            match r {
                Ok(s) => self.chatgpt_status = s,
                Err(e) => {
                    self.chatgpt_status = chatgpt::status();
                    self.message = Some((false, e));
                }
            }
        }
        if let Some(r) = take(&mut self.file_job) {
            self.file_result = Some(r.map(|file| {
                if let Some(warning) = file.history_warning {
                    self.message = Some((false, warning));
                }
                self.home_snapshot = None;
                self.home_job = None;
                self.history_loaded = false;
                file.text
            }));
        }
        if let Some((text, owner)) = take(&mut self.history_paste_job) {
            let owner = HWND(owner as *mut std::ffi::c_void);
            match repaste_after_switch(&text, owner) {
                Ok(tx) => {
                    ui.ctx().request_repaint_after(
                        tx.deadline().saturating_duration_since(Instant::now()),
                    );
                    self.paste_transaction = Some(tx);
                    self.history_message = Some("入力を送信しました".into());
                }
                Err(message) => {
                    self.history_message = Some(format!(
                        "貼り直せませんでした: {message}。履歴は残っています"
                    ))
                }
            }
        }

        egui::Panel::left("nav")
            .resizable(false)
            .exact_size(170.0)
            .frame(egui::Frame::side_top_panel(ui.style()).inner_margin(egui::Margin::same(14)))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    if self.brand_icon.is_none() {
                        if let Ok(icon) = eframe::icon_data::from_png_bytes(include_bytes!(
                            "../assets/branding/settings.png"
                        )) {
                            self.brand_icon = Some(ui.ctx().load_texture(
                                "openhinovoice-brand",
                                egui::ColorImage::from_rgba_unmultiplied(
                                    [icon.width as usize, icon.height as usize],
                                    &icon.rgba,
                                ),
                                egui::TextureOptions::LINEAR,
                            ));
                        }
                    }
                    if let Some(icon) = &self.brand_icon {
                        ui.image((icon.id(), egui::vec2(22.0, 22.0)));
                    }
                    ui.label(RichText::new("OpenHinoVoice").size(15.0).strong());
                });
                ui.add_space(16.0);
                for (page, label) in [
                    (Page::Start, "はじめに"),
                    (Page::Home, "ホーム"),
                    (Page::Input, "入力"),
                    (Page::Display, "音と表示"),
                    (Page::Asr, "認識"),
                    (Page::Ai, "AI"),
                    (Page::Edit, "Edit"),
                    (Page::History, "履歴と再入力"),
                    (Page::About, "情報"),
                ] {
                    let selected = self.page == page;
                    let text = if selected {
                        RichText::new(label).strong()
                    } else {
                        RichText::new(label)
                    };
                    if ui
                        .add_sized(
                            [ui.available_width(), 30.0],
                            egui::Button::selectable(selected, text),
                        )
                        .clicked()
                    {
                        self.select_page(page);
                    }
                }
            });

        egui::Panel::bottom("actions")
            .frame(
                egui::Frame::side_top_panel(ui.style())
                    .inner_margin(egui::Margin::symmetric(16, 10)),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let valid = self.validation_issue().is_none();
                    let save =
                        egui::Button::new(RichText::new("保存 (Ctrl+S)").color(Color32::WHITE))
                            .fill(HINOMARU_RED)
                            .min_size(egui::vec2(84.0, 30.0));
                    if ui.add_enabled(valid, save).clicked() {
                        self.save_from_ui(ui.ctx());
                    }
                    if ui
                        .add(egui::Button::new("閉じる").min_size(egui::vec2(84.0, 30.0)))
                        .clicked()
                    {
                        self.request_close(ui.ctx());
                    }
                    if self.is_dirty() {
                        ui.label(RichText::new("未保存の変更があります").strong());
                    }
                });
                if let Some((page, message)) = self.validation_issue() {
                    ui.colored_label(HINOMARU_RED, &message);
                    let label = match page {
                        Page::Input => "入力を開く",
                        Page::Asr => "認識を開く",
                        _ => "AIを開く",
                    };
                    if ui.small_button(label).clicked() {
                        self.show_validation_issue(ui.ctx(), page, message);
                    }
                }
                if let Some((ok, text)) = &self.message {
                    ui.add_space(8.0);
                    ui.colored_label(if *ok { OK_GREEN } else { HINOMARU_RED }, text);
                }
            });

        egui::CentralPanel::default()
            .frame(egui::Frame::central_panel(ui.style()).inner_margin(egui::Margin::same(20)))
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| match self.page {
                        Page::Start => self.start(ui),
                        Page::Home => self.home(ui),
                        Page::Input => self.input(ui),
                        Page::Display => self.display(ui),
                        Page::Asr => self.asr(ui),
                        Page::Ai => self.format(ui),
                        Page::Edit => self.edit_page(ui),
                        Page::History => self.history(ui),
                        Page::About => self.about(ui),
                    });
            });
        if self.close_confirm {
            let modal =
                egui::Modal::new(egui::Id::unique("unsaved_settings")).show(ui.ctx(), |ui| {
                    ui.heading("変更を保存しますか？");
                    ui.label("保存していない設定があります。");
                    ui.horizontal(|ui| {
                        if ui.button("保存して閉じる").clicked() {
                            match self.save() {
                                Ok(()) => {
                                    self.allow_close = true;
                                    self.request_close(ui.ctx());
                                }
                                Err(e) => {
                                    self.message = Some((false, e));
                                    self.close_confirm = false;
                                }
                            }
                        }
                        if ui.button("保存せず閉じる").clicked() {
                            self.allow_close = true;
                            self.close_confirm = false;
                            self.request_close(ui.ctx());
                        }
                        if ui.button("編集を続ける").clicked() {
                            self.close_confirm = false;
                        }
                    });
                });
            if modal.should_close() {
                self.close_confirm = false;
            }
        }
    }
}

fn save_shortcut(ctx: &egui::Context) -> bool {
    ctx.input_mut(|input| {
        input.consume_shortcut(&egui::KeyboardShortcut::new(
            egui::Modifiers::CTRL,
            egui::Key::S,
        ))
    })
}

fn section(ui: &mut egui::Ui, title: &str) {
    ui.heading(title);
    ui.add_space(10.0);
}

fn hint(ui: &mut egui::Ui, text: &str) {
    ui.label(RichText::new(text).weak());
}

fn grid(ui: &mut egui::Ui, id: &str, body: impl FnOnce(&mut egui::Ui)) {
    egui::Grid::new(id)
        .num_columns(2)
        .spacing([18.0, 14.0])
        .min_col_width(120.0)
        .show(ui, body);
}

impl Settings {
    fn start(&mut self, ui: &mut egui::Ui) {
        section(ui, "はじめに");
        hint(ui, "認識モデルとマイクを確かめれば、すぐに話し始められます。音声はこの PC の中で処理します。");
        ui.add_space(16.0);
        let exe_ready = !Self::needs_setup(&self.cfg);
        ui.strong("1. 音声認識を準備する");
        if exe_ready {
            ui.colored_label(OK_GREEN, "speech.cpp が見つかりました");
        } else {
            ui.colored_label(HINOMARU_RED, "speech.cpp が見つかりません");
            hint(
                ui,
                "speech.cpp を導入済みなら、「認識」で speech.exe の場所を指定してください。",
            );
        }
        self.model_controls(ui);
        ui.add_space(18.0);
        ui.strong("2. マイクを確かめる");
        self.microphone_controls(ui);
        ui.add_space(18.0);
        ui.strong("3. 話して入力する");
        ui.label(format!(
            "{} で録音開始 → 話す → Enter で確定",
            self.cfg.hotkey
        ));
        hint(
            ui,
            "Esc で取り消せます。途中の字幕は画面にだけ表示し、入力欄には確定後に送ります。",
        );
        ui.horizontal(|ui| {
            if ui.button("録音キーを変更").clicked() {
                self.select_page(Page::Input);
            }
            if ui.button("マイク・モデルを変更").clicked() {
                self.select_page(Page::Asr);
            }
        });
        ui.add_space(18.0);
        ui.strong("文章を整える・選択文を編集する");
        hint(ui, "AI の接続先は任意です。接続テストで確かめてから設定を保存してください。API キーやサインインはこの画面で設定できます。");
        if ui.button("AI の接続先を設定").clicked() {
            self.select_page(Page::Ai);
        }
    }

    fn model_controls(&mut self, ui: &mut egui::Ui) {
        let exe_ready = !Self::needs_setup(&self.cfg);
        if exe_ready
            && self.model_job.is_none()
            && self.model_status.is_none()
            && self.retiring_jobs.is_empty()
            && !self.closing
            && !self.close_confirm
            && !ui.ctx().egui_wants_keyboard_input()
        {
            self.start_model_job(ui.ctx(), false);
        }
        ui.label(format!("認識モデル: {}", self.cfg.model));
        match &self.model_status {
            Some(setup::ModelStatus::Ready) => {
                ui.colored_label(OK_GREEN, "取得済み・使用できます");
            }
            Some(setup::ModelStatus::Missing) => {
                hint(ui, "まだ取得していません。初回だけダウンロードが必要です。");
            }
            Some(setup::ModelStatus::Cancelled) => {
                hint(
                    ui,
                    "処理を中断しました。状態を確認するか、もう一度取得してください。",
                );
            }
            Some(setup::ModelStatus::Failed(e)) => {
                ui.colored_label(HINOMARU_RED, e);
            }
            Some(setup::ModelStatus::Running { message, fraction }) => {
                if let Some(fraction) = fraction {
                    ui.add(egui::ProgressBar::new(*fraction).show_percentage());
                }
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(message);
                });
            }
            None => {
                hint(ui, "モデルはまだ確認していません");
            }
        }
        let busy = self.model_job.is_some()
            || !self.retiring_jobs.is_empty()
            || self.closing
            || self.close_confirm;
        ui.horizontal(|ui| {
            if ui
                .add_enabled(exe_ready && !busy, egui::Button::new("状態を確認"))
                .clicked()
            {
                self.start_model_job(ui.ctx(), false);
            }
            let fetched = matches!(self.model_status, Some(setup::ModelStatus::Ready));
            let catalog_model = ASR_MODELS.iter().any(|(id, _)| *id == self.cfg.model);
            if ui
                .add_enabled(
                    exe_ready && !busy && !fetched && !self.cfg.format.offline && catalog_model,
                    egui::Button::new("モデルを取得"),
                )
                .clicked()
            {
                self.start_model_job(ui.ctx(), true);
            }
            if busy && ui.button("中断").clicked() {
                if let Some(job) = &self.model_job {
                    job.cancel();
                }
            }
        });
        if self.cfg.format.offline {
            hint(ui, "オフラインモード中は取得しません。取得する場合は「AI」でオフにしてから操作してください。");
        }
    }

    fn start_model_job(&mut self, ctx: &egui::Context, fetch: bool) {
        if self.model_job.is_some()
            || !self.retiring_jobs.is_empty()
            || self.closing
            || self.close_confirm
            || (fetch && self.cfg.format.offline)
        {
            return;
        }
        let exe = if self.cfg.speech_exe.trim().is_empty() {
            config::find_speech_exe()
        } else {
            self.cfg.speech_exe.clone()
        };
        self.model_job = Some(setup::ModelJob::start(
            ctx,
            exe,
            self.cfg.model.clone(),
            fetch,
        ));
        self.model_status = self.model_job.as_ref().map(setup::ModelJob::status);
    }

    fn microphone_controls(&mut self, ui: &mut egui::Ui) {
        if let Some(test) = &self.mic_test {
            ui.strong("マイクテスト中");
            ui.label(format!("入力: {}", test.recorder.device_name()));
            if let Some(notice) = test.recorder.fallback_notice() {
                ui.label(notice);
            }
            let level = ((20.0 * test.level.max(1e-6).log10() + 60.0) / 60.0).clamp(0.0, 1.0);
            ui.add(egui::ProgressBar::new(level).text("入力レベル"));
            hint(
                ui,
                &format!(
                    "あと {} 秒。普段の声で話してください",
                    MIC_TEST_DURATION
                        .saturating_sub(test.started.elapsed())
                        .as_secs()
                        + 1
                ),
            );
            if ui.button("テストを終了").clicked() {
                self.finish_microphone();
            }
        } else {
            ui.label(format!(
                "マイク: {}",
                if self.cfg.microphone.is_empty() {
                    "Windows の既定デバイス"
                } else {
                    &self.cfg.microphone
                }
            ));
            if ui.button("マイクを10秒テスト").clicked() {
                self.mic_result = None;
                match audio::Recorder::start_with_device(Some(&self.cfg.microphone)) {
                    Ok(recorder) => {
                        self.mic_test = Some(MicrophoneTest {
                            recorder,
                            started: Instant::now(),
                            samples: Vec::new(),
                            peak: 0.0,
                            level: 0.0,
                        });
                        ui.ctx().request_repaint();
                    }
                    Err(e) => self.mic_result = Some((false, e)),
                }
            }
        }
        if let Some((ok, message)) = &self.mic_result {
            ui.colored_label(if *ok { OK_GREEN } else { HINOMARU_RED }, message);
        }
        hint(
            ui,
            "音量だけを確認します。テストの音声は保存せず、外部にも送りません。",
        );
    }

    /// 今日と今週の入力文字数、最近の履歴、状態
    fn home(&mut self, ui: &mut egui::Ui) {
        section(ui, "ホーム");
        let exe_ready = !Self::needs_setup(&self.cfg);
        let preparation = preparation_notice(
            exe_ready,
            self.model_status.as_ref(),
            self.cfg.format.offline,
        );
        if preparation.ready {
            ui.colored_label(OK_GREEN, preparation.heading);
        } else {
            ui.strong(preparation.heading);
        }
        ui.label(preparation.detail);
        hint(ui, &format!("選択中のモデル: {}", self.cfg.model));
        if ui.button("セットアップを開く").clicked() {
            self.select_page(Page::Start);
        }
        ui.add_space(10.0);
        ui.separator();
        ui.add_space(10.0);
        if self.home_snapshot.is_none() {
            self.refresh_home(ui.ctx());
        }
        ui.horizontal(|ui| {
            if ui
                .add_enabled(self.home_job.is_none(), egui::Button::new("更新"))
                .clicked()
            {
                self.refresh_home(ui.ctx());
            }
            if self.home_job.is_some() {
                ui.spinner();
                ui.label("履歴を読み込んでいます");
            }
        });
        if let Some(Err(e)) = &self.home_snapshot {
            ui.colored_label(HINOMARU_RED, format!("履歴を読めません: {e}"));
        }
        let snapshot = self
            .home_snapshot
            .as_ref()
            .and_then(|result| result.as_ref().ok());
        grid(ui, "home", |ui| {
            ui.label("今日の入力");
            ui.label(
                snapshot.map_or_else(|| "未取得".into(), |s| format!("{} 文字", s.today_chars)),
            );
            ui.end_row();
            ui.label("今週の入力");
            ui.label(
                snapshot.map_or_else(|| "未取得".into(), |s| format!("{} 文字", s.week_chars)),
            );
            ui.end_row();
            ui.label("認識エンジン");
            ui.label(if !exe_ready {
                "speech.exe が見つかりません".to_string()
            } else if self.cfg.speech_exe.is_empty() {
                "speech.cpp を自動検出済み".to_string()
            } else {
                self.cfg.speech_exe.clone()
            });
            ui.end_row();
            ui.label("接続先");
            ui.label(match crate::provider::find(&self.cfg.format.provider) {
                Some(preset) => preset.label.to_string(),
                None => self.cfg.format.provider.clone(),
            });
            ui.end_row();
            ui.label("オフライン");
            ui.label(if self.cfg.format.offline {
                "オン(PC の外へ出ない)"
            } else {
                "オフ"
            });
            ui.end_row();
        });
        ui.add_space(14.0);
        section(ui, "最近の履歴");
        if snapshot.is_some_and(|s| s.recent.is_empty()) {
            hint(
                ui,
                "まだありません。録音開始キーを押して話し、Enter を押すとここに残ります",
            );
        }
        for entry in snapshot.into_iter().flat_map(|s| &s.recent) {
            let preview: String = entry.final_text().chars().take(48).collect();
            ui.label(RichText::new(preview).weak());
        }
    }

    fn input(&mut self, ui: &mut egui::Ui) {
        section(ui, "入力");
        grid(ui, "input", |ui| {
            ui.label("録音開始キー");
            ui.vertical(|ui| {
                let response = ui.add(
                    egui::TextEdit::singleline(&mut self.cfg.hotkey)
                        .id(egui::Id::unique("recording_hotkey"))
                        .desired_width(180.0),
                );
                if self.validation_focus == Some(Page::Input) {
                    response.request_focus();
                    response.scroll_to_me(Some(egui::Align::Center));
                    self.validation_focus = None;
                }
                if config::parse_hotkey(&self.cfg.hotkey).is_none() {
                    ui.colored_label(HINOMARU_RED, "書き方が正しくありません(例: Ctrl+Alt+J、F8)");
                } else {
                    hint(
                        ui,
                        "マウスのボタンに割り当てるときは、同じ組み合わせを設定アプリにも入れます",
                    );
                }
            });
            ui.end_row();

            ui.label("最大録音時間");
            ui.vertical(|ui| {
                egui::ComboBox::from_id_salt("max_recording_minutes")
                    .selected_text(format!("{}分", self.cfg.max_recording_minutes))
                    .show_ui(ui, |ui| {
                        for minutes in [1, 3, 5, 10, 30, 60] {
                            ui.selectable_value(
                                &mut self.cfg.max_recording_minutes,
                                minutes,
                                format!("{minutes}分"),
                            );
                        }
                    });
                hint(
                    ui,
                    "時間になると自動で録音を終了します。発話の切れ目ごとに認識を進め、Enter の後は残っている区間の処理を待ちます",
                );
            });
            ui.end_row();

            ui.label("認識エンジンを残す時間");
            ui.vertical(|ui| {
                egui::ComboBox::from_id_salt("keep_worker_minutes")
                    .selected_text(if self.cfg.keep_worker_minutes == 0 {
                        "残さない(今までどおり)".to_string()
                    } else {
                        format!("{}分", self.cfg.keep_worker_minutes)
                    })
                    .show_ui(ui, |ui| {
                        for minutes in [0, 1, 3, 5, 10] {
                            ui.selectable_value(
                                &mut self.cfg.keep_worker_minutes,
                                minutes,
                                if minutes == 0 {
                                    "残さない".to_string()
                                } else {
                                    format!("{minutes}分")
                                },
                            );
                        }
                    });
                hint(ui, "続けて話すとき、認識エンジンの起動とモデル読み込みの待ち時間を減らせます。残している間もエンジンのメモリを使います。使用量はモデルと PC によって変わります");
            });
            ui.end_row();

            ui.label("自動起動");
            ui.checkbox(&mut self.startup, "Windows にサインインしたときに起動する");
            ui.end_row();
        });
    }

    fn display(&mut self, ui: &mut egui::Ui) {
        section(ui, "音と表示");
        hint(ui, "好みの組合せを選び、保存する前に試せます。");
        let before = DisplaySnapshot::capture(&self.cfg);
        let selected = Preset::detect(&self.cfg);
        ui.horizontal_wrapped(|ui| {
            for preset in Preset::ALL {
                if ui
                    .add(egui::Button::selectable(
                        selected == Some(preset),
                        preset.label(),
                    ))
                    .clicked()
                {
                    self.preview.stop();
                    preset.apply(&mut self.cfg);
                }
            }
        });
        hint(
            ui,
            match Preset::detect(&self.cfg) {
                Some(Preset::Standard) => "いつもの音と、状態が分かる標準の表示。",
                Some(Preset::Soft) => "やわらかな音と控えめな動き。",
                Some(Preset::Minimal) => "必要な音だけ、表示もコンパクトに。",
                Some(Preset::Silent) => "音を鳴らさず、文字と形で状態を伝えます。",
                Some(Preset::Rich) => "重なりのある音色と、波紋・光彩・粒の演出。",
                None => "カスタム。音と表示を個別に調整しています。",
            },
        );
        ui.add_space(10.0);
        let mut requested = None;
        let mut toggle = false;
        ui.horizontal_wrapped(|ui| {
            if ui
                .button(if self.preview.running() {
                    "プレビューを停止"
                } else {
                    "試す"
                })
                .clicked()
            {
                if self.preview.running() {
                    self.preview.stop();
                } else {
                    toggle = true;
                }
            }
            if ui
                .add_enabled(
                    before != DisplaySnapshot::capture(&self.saved.config),
                    egui::Button::new("音と表示を保存済みに戻す"),
                )
                .clicked()
            {
                self.preview.stop();
                DisplaySnapshot::capture(&self.saved.config).restore(&mut self.cfg);
            }
        });
        ui.add_space(6.0);
        ui.label(RichText::new("プレビュー").strong());
        self.preview.ui(ui, &self.cfg);
        hint(
            ui,
            "録音から入力完了までの合成プレビューです。マイクや入力先は操作しません。",
        );
        if let Some(message) = &self.preview.message {
            ui.label(message);
        }
        for warning in feedback::warnings(&self.cfg) {
            ui.colored_label(HINOMARU_RED, warning.message());
        }

        ui.add_space(16.0);
        ui.label(RichText::new("音").strong());
        ui.checkbox(&mut self.cfg.sounds, "効果音を鳴らす");
        ui.horizontal_wrapped(|ui| {
            let label = ui.label("音セット");
            egui::ComboBox::from_id_salt("sound_set")
                .selected_text(
                    SoundSet::from_id(&self.cfg.sound_set)
                        .unwrap_or(SoundSet::Classic)
                        .label(),
                )
                .show_ui(ui, |ui| {
                    for set in SoundSet::ALL {
                        ui.selectable_value(&mut self.cfg.sound_set, set.id().into(), set.label());
                    }
                })
                .response
                .labelled_by(label.id);
        });
        ui.horizontal(|ui| {
            ui.label("音量");
            ui.add(
                egui::Slider::new(&mut self.cfg.volume, 0..=100)
                    .suffix(" %")
                    .text("効果音の音量"),
            );
        });
        egui::CollapsingHeader::new("場面ごとの音")
            .id_salt("feedback_events")
            .show(ui, |ui| {
                hint(ui, "チェックした場面で鳴ります。各音を単独で試せます。");
                for event in SoundEvent::ALL {
                    let can_play = FeedbackSettings::from_config(&self.cfg).sound_enabled(event);
                    ui.horizontal(|ui| {
                        let enabled = match event {
                            SoundEvent::Startup => &mut self.cfg.startup_sound,
                            SoundEvent::Start => &mut self.cfg.sound_events.start,
                            SoundEvent::Stop => &mut self.cfg.sound_events.stop,
                            SoundEvent::Done => &mut self.cfg.sound_events.done,
                            SoundEvent::Warning => &mut self.cfg.sound_events.warning,
                            SoundEvent::Error => &mut self.cfg.sound_events.error,
                            SoundEvent::Cancel => &mut self.cfg.sound_events.cancel,
                        };
                        ui.add_sized([140.0, 24.0], egui::Checkbox::new(enabled, event.label()));
                        if ui
                            .add_enabled(
                                can_play,
                                egui::Button::new(format!("{}を試聴", event.label())),
                            )
                            .clicked()
                        {
                            requested = Some(event);
                        }
                    });
                }
                hint(ui, "起動音は、ここで有効にした場合だけ鳴ります。");
            });

        ui.add_space(16.0);
        ui.label(RichText::new("表示と動き").strong());
        ui.horizontal_wrapped(|ui| {
            let label = ui.label("表示スタイル");
            egui::ComboBox::from_id_salt("hud_style")
                .selected_text(
                    HudStyle::from_id(&self.cfg.hud_style)
                        .unwrap_or(HudStyle::Standard)
                        .label(),
                )
                .show_ui(ui, |ui| {
                    for style in HudStyle::ALL {
                        ui.selectable_value(
                            &mut self.cfg.hud_style,
                            style.id().into(),
                            style.label(),
                        );
                    }
                })
                .response
                .labelled_by(label.id);
        });
        ui.horizontal_wrapped(|ui| {
            for motion in FeedbackMotion::ALL {
                ui.radio_value(
                    &mut self.cfg.feedback_motion,
                    motion.id().into(),
                    motion.label(),
                );
            }
        });
        hint(
            ui,
            "音とは別に選べます。Windowsで動きを無効にしている場合も、静かな表示になります。",
        );

        ui.add_space(14.0);
        egui::CollapsingHeader::new("テーマとリアルタイム字幕")
            .id_salt("caption_display")
            .show(ui, |ui| {
                ui.label(RichText::new("表示のテーマ").strong());
                ui.radio_value(
                    &mut self.cfg.theme,
                    "hinomaru".to_string(),
                    "日の丸（白地に赤い点）",
                );
                ui.radio_value(
                    &mut self.cfg.theme,
                    "auto".to_string(),
                    "Windows のライト、ダークに合わせる",
                );
                ui.add_space(10.0);
                ui.checkbox(
                    &mut self.cfg.live_caption,
                    "話している途中の文字を画面上の帯に表示する",
                );
                ui.add_enabled(
                    self.cfg.live_caption,
                    egui::Checkbox::new(
                        &mut self.cfg.partial_caption,
                        "途中の音を認識して仮の文字を表示する",
                    ),
                );
                hint(
                    ui,
                    "仮の文字は1秒ごとに更新します。処理が遅いPCではオフにできます。",
                );
                ui.add_enabled_ui(self.cfg.live_caption, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.label("帯の場所");
                        ui.radio_value(
                            &mut self.cfg.caption_position,
                            "top".to_string(),
                            "画面の上端",
                        );
                        ui.radio_value(
                            &mut self.cfg.caption_position,
                            "caret".to_string(),
                            "入力中の位置の近く",
                        );
                    });
                });
            });
        if before != DisplaySnapshot::capture(&self.cfg) {
            self.preview.stop();
        }
        if let Some(event) = requested {
            self.preview.start(preview::Kind::Event(event), &self.cfg);
            ui.ctx().request_repaint();
        } else if toggle {
            self.preview.start(preview::Kind::Sequence, &self.cfg);
            ui.ctx().request_repaint();
        }
    }

    fn edit_page(&mut self, ui: &mut egui::Ui) {
        section(ui, "Edit");
        ui.checkbox(
            &mut self.cfg.edit_mode,
            "文を選んだ状態で録音を始めると、話した指示で選んだ部分を書き換える",
        );
        hint(ui, "専用のキーはありません。選択の確認は画面の部品の情報(UI Automation)だけで行い、キー操作を送らず、クリップボードにも触れません。選択を読めないアプリでは Edit を使えないことを表示して、通常の音声入力になります。整形(AI)の接続先が必要です。オフのときは選択の確認そのものを行いません");
        ui.add_space(14.0);
        grid(ui, "edit", |ui| {
            ui.label("書き換え方の追加の指示");
            ui.vertical(|ui| {
                ui.add(
                    egui::TextEdit::multiline(&mut self.cfg.format.edit_prompt)
                        .desired_rows(3)
                        .desired_width(f32::INFINITY),
                );
                hint(ui, "例: 「ですます調にする」「技術用語は原文のまま」。空なら既定の指示だけを使います");
            });
            ui.end_row();
        });
        ui.add_space(14.0);
        section(ui, "Edit を使えるアプリの対応表");
        hint(ui, "実機での確認結果です。cargo run --release --example uia_probe で読み取りの可否を調べられます。結果は README.md の対応表に記載します");
    }
    fn format(&mut self, ui: &mut egui::Ui) {
        section(ui, "整形(AI)");
        hint(ui, "Enter を 1 回押したときに、書き起こしを AI で整えます。音声は送らず、文字だけを送ります。");
        ui.add_space(10.0);
        let before = self.cfg.format.provider.clone();
        grid(ui, "provider", |ui| {
            ui.label("接続先");
            ui.vertical(|ui| {
                let f = &mut self.cfg.format.provider;
                for preset in crate::provider::all() {
                    ui.radio_value(f, preset.id.to_string(), preset.label);
                }
            });
            ui.end_row();
        });
        if before != self.cfg.format.provider {
            // 接続先を変えたら、その接続先の初期値にそろえる
            if let Some(preset) = crate::provider::find(&self.cfg.format.provider) {
                if !preset.base_url.is_empty() {
                    self.cfg.format.base_url = preset.base_url.to_string();
                }
                if !preset.default_model.is_empty() {
                    self.cfg.format.model = preset.default_model.to_string();
                }
                // ローカルのエンジンは、最初の要求でモデルを読み込むので待ち時間を長めに
                if preset.id == "local" && self.timeout_secs < 60.0 {
                    self.timeout_secs = 60.0;
                }
            }
            self.models.clear();
            self.test_result = None;
            if !self.extra_body_text.trim().is_empty() && self.extra_body_text.trim() != "{}" {
                self.message = Some((false, "接続先を変更しました。追加項目は引き継いでいるため、この接続先で使える内容か確認してください".into()));
            }
        }
        ui.add_space(14.0);

        let provider = self.cfg.format.provider.clone();
        match provider.as_str() {
            "chatgpt" => self.chatgpt_fields(ui),
            "openai" => {
                let f = &mut self.cfg.format;
                let show_key = &mut self.show_key;
                grid(ui, "openai", |ui| {
                    ui.label("API キー");
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut f.openai_api_key)
                                .password(!*show_key)
                                .desired_width(300.0),
                        );
                        ui.checkbox(show_key, "表示");
                    });
                    ui.end_row();
                });
                self.model_picker(ui);
                ui.checkbox(
                    &mut self.cfg.format.fast,
                    "Fast モード(応答が速くなる代わりに料金が 2 倍)",
                );
            }
            provider => {
                let editable_url = provider == "custom" || provider == "local";
                let key_optional = provider == "local";
                let anthropic = provider == "anthropic";
                let f = &mut self.cfg.format;
                let show_key = &mut self.show_key;
                grid(ui, "compat", |ui| {
                    if editable_url {
                        ui.label("ベース URL");
                        let url_hint = if provider == "local" {
                            "http://127.0.0.1:8080/v1"
                        } else {
                            "https://example.com/v1"
                        };
                        ui.add(
                            egui::TextEdit::singleline(&mut f.base_url)
                                .hint_text(url_hint)
                                .desired_width(340.0),
                        );
                        if provider == "local" {
                            hint(ui, "OpenAI 互換の API を話すローカルのエンジンの URL です。/v1 まで入れてください");
                        }
                        ui.end_row();
                    }
                    if !(key_optional && f.api_key.is_empty()) {
                        ui.label(if key_optional {
                            "API キー(任意)"
                        } else {
                            "API キー"
                        });
                        ui.horizontal(|ui| {
                            ui.add(
                                egui::TextEdit::singleline(&mut f.api_key)
                                    .password(!*show_key)
                                    .desired_width(300.0),
                            );
                            ui.checkbox(show_key, "表示");
                        });
                        ui.end_row();
                    }
                });
                if !editable_url {
                    if let Some(preset) = crate::provider::find(provider) {
                        if !preset.base_url.is_empty() {
                            f.base_url = preset.base_url.to_string();
                        }
                    }
                }
                if anthropic {
                    hint(ui, "初期のモデルは claude-haiku-5-5 です。 Anthropic の Messages API を使います");
                } else {
                    self.model_picker(ui);
                }
            }
        }

        ui.add_space(10.0);
        ui.checkbox(
            &mut self.cfg.format.offline,
            "オフラインモード(この PC の外へ一切通信しない)",
        );
        hint(ui, "オンにすると、整形の接続先は localhost だけを受け付け、ChatGPT のトークンの更新もしません。認識モデルが未取得のままでは音声入力が動かないので、オンラインのときに 1 回取得してください");

        ui.add_space(14.0);
        grid(ui, "timeout", |ui| {
            ui.label("応答の停止を待つ時間");
            ui.vertical(|ui| {
                ui.add(
                    egui::Slider::new(&mut self.timeout_secs, 3.0..=120.0)
                        .step_by(0.5)
                        .suffix(" 秒"),
                );
                hint(ui, "応答が途切れたときの待ち時間です。処理全体の待機目安は90秒です。受信待ちの状況によって終了まで時間がかかる場合があります");
            });
            ui.end_row();
        });

        let mut effective_config = self.cfg.format.clone();
        effective_config.timeout_ms = (self.timeout_secs * 1000.0).round() as u64;
        let effective_wait = crate::format::response_wait(&effective_config);
        if effective_wait.as_millis() != effective_config.timeout_ms as u128 {
            hint(
                ui,
                &format!(
                    "この接続先では初回読み込みのため、応答がないときの待ち時間を{}秒にしています",
                    effective_wait.as_secs()
                ),
            );
        }

        ui.add_space(8.0);
        ui.checkbox(
            &mut self.cfg.format.save_history,
            "原文とAI整形結果をこのPCに保存する（改善用）",
        );
        hint(ui, "オフが初期値。オンにすると %APPDATA%\\OpenHinoVoice\\format-history.jsonl に原文・結果・エラーとモデル情報が平文で保存されます。外部には履歴を送信しません");

        ui.add_space(10.0);
        egui::CollapsingHeader::new("指示文(整形のしかた)").show(ui, |ui| {
            ui.add(
                egui::TextEdit::multiline(&mut self.cfg.format.prompt)
                    .desired_rows(7)
                    .desired_width(f32::INFINITY),
            );
            if ui.small_button("初期の指示文に戻す").clicked() {
                self.cfg.format.prompt = config::DEFAULT_PROMPT.to_string();
            }
        });

        ui.add_space(10.0);
        egui::CollapsingHeader::new("リクエストの追加項目(extra_body)")
            .open((self.validation_focus == Some(Page::Ai)).then_some(true))
            .show(ui, |ui| {
            hint(ui, "リクエストの本体にそのまま混ぜる項目を JSON で書きます。推論(考える過程)の止め方はエンジンごとに違うので、ここで指定します。空なら送りません");
            let response = ui.add(
                egui::TextEdit::multiline(&mut self.extra_body_text).id(egui::Id::unique("format_extra_body"))
                    .desired_rows(4)
                    .desired_width(f32::INFINITY)
                    .code_editor(),
            );
            if self.validation_focus == Some(Page::Ai) {
                response.request_focus();
                response.scroll_to_me(Some(egui::Align::Center));
                self.validation_focus = None;
            }
            ui.horizontal(|ui| {
                if ui.small_button("空にする").clicked() {
                    self.extra_body_text.clear();
                }
                if ui
                    .small_button("推論を止める例(OpenAI)")
                    .clicked()
                {
                    self.extra_body_text = r#"{"reasoning_effort": "minimal"}"#.into();
                }
                if ui
                    .small_button("推論を止める例(llama.cpp 系)")
                    .clicked()
                {
                    self.extra_body_text =
                        r#"{"chat_template_kwargs": {"enable_thinking": false}}"#.into();
                }
            });
        });

        ui.add_space(14.0);
        ui.horizontal(|ui| {
            let busy = self.test_job.is_some();
            if ui
                .add_enabled(!busy, egui::Button::new("接続テスト"))
                .clicked()
            {
                let f = self.draft_format();
                self.test_result = None;
                self.test_job = Some(spawn(ui.ctx(), move || {
                    let t = Instant::now();
                    let r = f.and_then(|f| {
                        crate::format::format_with_retry(&f, SAMPLE, &|| false, |_| {})
                            .and_then(|text| {
                                if text == SAMPLE
                                    || text.contains("えーと")
                                    || text.contains("あのー")
                                {
                                    Err(crate::format::FormatError::BadResponse(
                                        "接続しましたが、例文の言い淀みを整形できませんでした"
                                            .into(),
                                    ))
                                } else {
                                    Ok(text)
                                }
                            })
                            .map_err(|e| e.to_string())
                    });
                    (r, t.elapsed().as_secs_f32())
                }));
            }
            if busy {
                ui.spinner();
            }
            hint(ui, "例文を 1 回整形して、結果と時間を表示します");
        });
        if let Some((r, secs)) = &self.test_result {
            ui.add_space(6.0);
            match r {
                Ok(text) => {
                    ui.colored_label(OK_GREEN, format!("成功({secs:.2} 秒)"));
                    ui.label(text);
                }
                Err(e) => {
                    ui.colored_label(HINOMARU_RED, format!("失敗({secs:.2} 秒): {e}"));
                }
            }
        }
    }

    fn model_picker(&mut self, ui: &mut egui::Ui) {
        let provider = self.cfg.format.provider.clone();
        let current = match provider.as_str() {
            "chatgpt" => self.cfg.format.chatgpt_model.clone(),
            "openai" => self.cfg.format.openai_model.clone(),
            _ => self.cfg.format.model.clone(),
        };
        let mut chosen = current.clone();
        let mut fetch = false;
        let busy = self.models_job.is_some();
        let models = &self.models;
        grid(ui, "model", |ui| {
            ui.label("モデル");
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut chosen).desired_width(220.0));
                if !models.is_empty() {
                    egui::ComboBox::from_id_salt("models")
                        .selected_text("一覧から選ぶ")
                        .show_ui(ui, |ui| {
                            for (id, label) in models {
                                if ui.selectable_label(*id == chosen, label).clicked() {
                                    chosen = id.clone();
                                }
                            }
                        });
                }
                fetch = ui
                    .add_enabled(!busy, egui::Button::new("一覧を取得"))
                    .clicked();
                if busy {
                    ui.spinner();
                }
            });
            ui.end_row();
        });
        if fetch {
            let f = self.draft_format();
            self.models_job = Some(spawn(ui.ctx(), move || {
                f.and_then(|f| crate::format::list_models(&f))
            }));
        }
        if chosen != current {
            match provider.as_str() {
                "chatgpt" => self.cfg.format.chatgpt_model = chosen,
                "openai" => self.cfg.format.openai_model = chosen,
                _ => self.cfg.format.model = chosen,
            }
        }
    }

    fn chatgpt_fields(&mut self, ui: &mut egui::Ui) {
        let busy = self.chatgpt_job.is_some();
        match self.chatgpt_status.clone() {
            chatgpt::Status::SignedOut => {
                hint(ui, "ChatGPT の Plus か Pro のプランの範囲で整形します。OpenHinoVoice の料金はかかりません。");
                ui.add_space(8.0);
                let button = egui::Button::new(
                    RichText::new("Continue with ChatGPT")
                        .color(Color32::WHITE)
                        .strong(),
                )
                .fill(Color32::BLACK)
                .min_size(egui::vec2(220.0, 34.0));
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(!busy && !self.cfg.format.offline, button)
                        .clicked()
                    {
                        self.chatgpt_job = Some(spawn(ui.ctx(), chatgpt::sign_in));
                    }
                    if busy {
                        ui.spinner();
                        hint(ui, "ブラウザでサインインを続けてください");
                    }
                });
            }
            chatgpt::Status::SignedIn { email, plan_usage } => {
                ui.horizontal(|ui| {
                    ui.label(format!("{email} でサインイン中"));
                    if ui
                        .add_enabled(!busy, egui::Button::new("サインアウト"))
                        .clicked()
                    {
                        self.chatgpt_job = Some(spawn(ui.ctx(), || {
                            chatgpt::sign_out().map(|()| chatgpt::Status::SignedOut)
                        }));
                    }
                });
                if plan_usage {
                    ui.horizontal(|ui| {
                        ui.colored_label(OK_GREEN, "Using ChatGPT plan");
                        if ui
                            .add_enabled(!self.cfg.format.offline, egui::Link::new("Manage usage"))
                            .clicked()
                        {
                            chatgpt::open_browser(chatgpt::USAGE_URL);
                        }
                    });
                } else {
                    ui.colored_label(HINOMARU_RED, "ChatGPT プランの利用が許可されていません。サインインし直して、プランの利用を許可してください");
                }
                ui.add_space(8.0);
                self.model_picker(ui);
                ui.checkbox(
                    &mut self.cfg.format.fast,
                    "Fast モード(応答が速くなる代わりにプランの利用枠を多く使う)",
                );
            }
        }
    }

    fn asr(&mut self, ui: &mut egui::Ui) {
        section(ui, "認識");
        hint(
            ui,
            "音声はこの PC の中だけで文字にします。外には送りません。",
        );
        ui.add_space(10.0);
        let detected = config::find_speech_exe();
        let microphone_before = self.cfg.microphone.clone();
        grid(ui, "asr", |ui| {
            ui.label("マイク");
            ui.horizontal(|ui| {
                let current = if self.cfg.microphone.trim().is_empty() {
                    "Windowsの既定デバイス".to_string()
                } else {
                    self.cfg.microphone.clone()
                };
                egui::ComboBox::from_id_salt("asr_microphone")
                    .selected_text(current)
                    .width(290.0)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut self.cfg.microphone,
                            String::new(),
                            "Windowsの既定デバイス",
                        );
                        for name in &self.microphones {
                            ui.selectable_value(&mut self.cfg.microphone, name.clone(), name);
                        }
                    });
                if ui.small_button("再検出").clicked() {
                    self.microphones = audio::input_devices();
                }
            });
            ui.end_row();

            ui.label("認識モデル");
            let label = ASR_MODELS
                .iter()
                .find(|(id, _)| *id == self.cfg.model)
                .map_or(self.cfg.model.clone(), |(_, l)| l.to_string());
            egui::ComboBox::from_id_salt("asr_model")
                .selected_text(label)
                .width(260.0)
                .show_ui(ui, |ui| {
                    for (id, label) in ASR_MODELS {
                        ui.selectable_value(&mut self.cfg.model, id.to_string(), label);
                    }
                });
            ui.end_row();

            ui.label("語彙ヒント");
            ui.vertical(|ui| {
                ui.add(
                    egui::TextEdit::multiline(&mut self.cfg.words)
                        .desired_rows(3)
                        .hint_text("共有フォルダ\nOpenHinoVoice")
                        .desired_width(340.0),
                );
                hint(
                    ui,
                    "1行に1語。読みを指定する場合は Haas=ハース,ハウス のようにカンマで区切ります",
                );
            });
            ui.end_row();

            ui.label("speech.exe");
            ui.vertical(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.cfg.speech_exe)
                        .hint_text(detected.as_str())
                        .desired_width(340.0),
                );
                hint(
                    ui,
                    "空なら自動で探します。下の「モデルを取得」で事前に準備できます",
                );
            });
            ui.end_row();
        });
        if microphone_before != self.cfg.microphone {
            self.mic_test = None;
            self.mic_result = None;
        }
        ui.add_space(14.0);
        self.model_controls(ui);
        ui.add_space(14.0);
        self.microphone_controls(ui);
        ui.add_space(14.0);
        self.dictionary(ui);
        ui.add_space(14.0);
        self.file_transcription(ui);
    }

    fn dictionary(&mut self, ui: &mut egui::Ui) {
        section(ui, "用語辞書・表記の統一");
        hint(ui, "推奨表記をAIに伝え、明示した別表記だけを確定置換します。一般的な同音異義語は文脈から判断します。");
        let mut delete_term = None;
        for (i, term) in self.cfg.terms.iter_mut().enumerate() {
            ui.group(|ui| {
                ui.horizontal(|ui| {
                    ui.strong(format!("用語 {}", i + 1));
                    if ui.small_button("この用語を削除").clicked() {
                        delete_term = Some(i);
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("正式表記");
                    let response = ui
                        .add(egui::TextEdit::singleline(&mut term.canonical).desired_width(220.0));
                    if i == 0 && self.validation_focus == Some(Page::Asr) {
                        response.request_focus();
                        response.scroll_to_me(Some(egui::Align::Center));
                        self.validation_focus = None;
                    }
                    ui.label("文脈");
                    ui.add(egui::TextEdit::singleline(&mut term.hint).desired_width(240.0));
                });
                let mut delete_alias = None;
                for (j, alias) in term.replace_from.iter_mut().enumerate() {
                    ui.horizontal(|ui| {
                        ui.label(if j == 0 {
                            "確定置換する別表記"
                        } else {
                            "　　　　　　　　"
                        });
                        ui.add(egui::TextEdit::singleline(alias).desired_width(240.0));
                        if ui.small_button("削除").clicked() {
                            delete_alias = Some(j);
                        }
                    });
                }
                if let Some(j) = delete_alias {
                    term.replace_from.remove(j);
                }
                if ui.small_button("別表記を追加").clicked() {
                    term.replace_from.push(String::new());
                }
            });
            ui.add_space(6.0);
        }
        if let Some(i) = delete_term {
            self.cfg.terms.remove(i);
        }
        if ui.button("用語を追加").clicked() {
            self.cfg.terms.push(crate::lexicon::Term::default());
        }
        if let Err(e) = crate::lexicon::validate(&self.cfg.terms) {
            ui.colored_label(HINOMARU_RED, e);
        }
    }

    fn file_transcription(&mut self, ui: &mut egui::Ui) {
        section(ui, "ファイルの文字起こし");
        hint(ui, "WAVファイルをこのPCのspeech.cppで認識。音声はアップロードしません。ドラッグ＆ドロップにも対応しています。");
        if let Some(path) = ui.ctx().input(|i| {
            i.raw.dropped_files.iter().find_map(|file| {
                let path = file.path();
                (!path.as_os_str().is_empty()).then(|| path.to_path_buf())
            })
        }) {
            self.file_path = path.display().to_string();
        }
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.file_path)
                    .hint_text("WAVファイルのフルパス（ここにドロップも可）")
                    .desired_width(400.0),
            );
            let busy = self.file_job.is_some();
            if ui
                .add_enabled(
                    !busy && !self.file_path.trim().is_empty(),
                    egui::Button::new("文字起こし"),
                )
                .clicked()
            {
                let path = std::path::PathBuf::from(self.file_path.trim().trim_matches('"'));
                let exe = self.cfg.speech_exe.clone();
                let model = self.cfg.model.clone();
                let words = crate::lexicon::asr_hints(&self.cfg.words, &self.cfg.terms);
                let offline = self.cfg.format.offline;
                let terms = self.cfg.terms.clone();
                let recovery_history = self.cfg.recovery_history;
                self.file_result = None;
                self.file_job = Some(spawn(ui.ctx(), move || {
                    let raw = asr::transcribe_file_async(path, exe, model, words, offline)
                        .recv()
                        .unwrap_or_else(|_| {
                            Err("ファイル認識の結果を受信できませんでした".into())
                        })?;
                    let normalized = crate::lexicon::replace_explicit(&raw, &terms);
                    let saved = history::save_recognized_if_enabled(&raw, recovery_history)
                        .and_then(|id| {
                            if let Some(id) = id {
                                if normalized != raw {
                                    history::update_formatted(id, &normalized)?;
                                }
                                history::update_paste_status(id, history::PasteStatus::Pending)?;
                            }
                            Ok(())
                        });
                    Ok(TranscribedFile {
                        text: normalized,
                        history_warning: saved.err().map(|e| {
                            format!("文字起こしは完了しましたが、履歴に保存できません: {e}")
                        }),
                    })
                }));
            }
            if busy {
                ui.spinner();
            }
        });
        if let Some(result) = &self.file_result {
            match result {
                Ok(text) => {
                    ui.label(RichText::new("認識完了").color(OK_GREEN));
                    ui.horizontal(|ui| {
                        if ui.button("全文をコピー").clicked() {
                            self.message = match settings_clipboard_owner()
                                .and_then(|owner| paste::copy_text_to_clipboard(text, owner))
                            {
                                Ok(()) => {
                                    Some((true, "文章をクリップボードにコピーしました".into()))
                                }
                                Err(e) => Some((false, format!("コピーに失敗: {e}"))),
                            };
                        }
                        hint(ui, "内容は下の欄から確認できます");
                    });
                    egui::ScrollArea::vertical()
                        .max_height(160.0)
                        .show(ui, |ui| {
                            ui.label(text);
                        });
                }
                Err(message) => {
                    ui.colored_label(HINOMARU_RED, message);
                }
            }
        }
    }

    fn reload_history(&mut self, ctx: &egui::Context) {
        if self.history_job.is_some() || self.history_clear_job.is_some() {
            return;
        }
        self.history_loaded = true;
        self.history_job = Some(spawn(ctx, || {
            history::recent(30).map_err(|e| e.to_string())
        }));
    }

    fn history(&mut self, ui: &mut egui::Ui) {
        if !self.history_loaded {
            self.reload_history(ui.ctx());
        }
        section(ui, "履歴と再入力");
        ui.checkbox(
            &mut self.cfg.recovery_history,
            "認識した文章を復旧用にローカル保存する",
        );
        hint(ui, "既定でオン。文章はWindowsのユーザーに紐づく暗号化で保存。最大200件・16MiBの短期履歴です。設定を保存すると反映されます。");
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    self.history_job.is_none() && self.history_clear_job.is_none(),
                    egui::Button::new("履歴を更新"),
                )
                .clicked()
            {
                self.reload_history(ui.ctx());
                self.home_snapshot = None;
            }
            if !self.history_clear_confirm {
                if ui
                    .add_enabled(
                        self.history_clear_job.is_none() && self.history_job.is_none(),
                        egui::Button::new("履歴を消去"),
                    )
                    .clicked()
                {
                    self.history_clear_confirm = true;
                }
            } else {
                ui.label("すべて消去しますか？");
                if ui.button("消去する").clicked() {
                    self.history_clear_job = Some(spawn(ui.ctx(), || {
                        history::clear_recovery().map_err(|e| e.to_string())
                    }));
                    self.history_clear_confirm = false;
                }
                if ui.button("戻る").clicked() {
                    self.history_clear_confirm = false;
                }
            }
        });
        if let Some(msg) = &self.history_message {
            ui.label(msg);
        }
        if self.history_job.is_some() || self.history_clear_job.is_some() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("履歴を処理しています");
            });
        }
        if self.history_paste_job.is_some() {
            ui.horizontal(|ui| {
                ui.spinner();
                hint(ui, "3秒以内に貼り付け先のウィンドウを選んでください");
            });
        }
        ui.add_space(8.0);
        if self.history_entries.is_empty()
            && self.history_job.is_none()
            && self.history_message.is_none()
        {
            hint(ui, "復旧できる文章はまだありません。");
        }
        for entry in &self.history_entries {
            let text = entry.final_text();
            let age = std::time::SystemTime::now()
                .duration_since(
                    std::time::UNIX_EPOCH + Duration::from_millis(entry.timestamp_unix_ms),
                )
                .unwrap_or_default()
                .as_secs();
            let when = if age < 60 {
                "たった今".to_owned()
            } else if age < 3600 {
                format!("{}分前", age / 60)
            } else if age < 86400 {
                format!("{}時間前", age / 3600)
            } else {
                format!("{}日前", age / 86400)
            };
            ui.group(|ui| {
                ui.horizontal(|ui| {
                    ui.label(format!(
                        "{} · {}",
                        when,
                        match entry.paste_status {
                            history::PasteStatus::Pending => "未入力",
                            history::PasteStatus::ShortcutSent => "入力送信済み",
                            history::PasteStatus::Failed => "入力失敗",
                        }
                    ));
                    if ui.small_button("コピー").clicked() {
                        self.history_message = Some(
                            match settings_clipboard_owner()
                                .and_then(|owner| paste::copy_text_to_clipboard(text, owner))
                            {
                                Ok(()) => "文章をクリップボードにコピーしました".into(),
                                Err(e) => format!("コピーに失敗: {e}"),
                            },
                        );
                    }
                    if ui
                        .add_enabled(
                            self.history_paste_job.is_none() && self.paste_transaction.is_none(),
                            egui::Button::new("貼り直す（3秒）"),
                        )
                        .clicked()
                    {
                        let text = text.to_owned();
                        match settings_clipboard_owner() {
                            Ok(owner) => {
                                self.history_message =
                                    Some("貼り付け先へ切り替えてください".into());
                                let owner = owner.0 as isize;
                                self.history_paste_job = Some(spawn(ui.ctx(), move || {
                                    std::thread::sleep(Duration::from_secs(3));
                                    (text, owner)
                                }));
                            }
                            Err(e) => {
                                self.history_message = Some(format!("貼り直せませんでした: {e}"))
                            }
                        }
                    }
                });
                let preview: String = text.chars().take(170).collect();
                ui.label(preview);
                egui::CollapsingHeader::new("全文・元の認識")
                    .id_salt(entry.id)
                    .show(ui, |ui| {
                        ui.label(text);
                        if entry.formatted.is_some() {
                            ui.separator();
                            ui.label("認識原文");
                            ui.label(&entry.raw);
                        }
                    });
            });
            ui.add_space(4.0);
        }
    }

    fn about(&mut self, ui: &mut egui::Ui) {
        section(ui, "情報");
        ui.label(format!("OpenHinoVoice {}", env!("CARGO_PKG_VERSION")));
        ui.add_space(10.0);
        ui.checkbox(
            &mut self.cfg.format.usage_log,
            "この PC での動きを記録する(音声は含めず、外にも送らない)",
        );
        hint(ui, "区間の長さ、Enter からの待ち時間、失敗の理由を %APPDATA%\\OpenHinoVoice\\usage-log.jsonl に書きます。不具合の調査用です。cargo run --release --example usage_report で集計を読めます");
        ui.add_space(10.0);
        grid(ui, "about", |ui| {
            ui.label("設定ファイル");
            ui.horizontal(|ui| {
                ui.label(config::config_path().display().to_string());
                if ui.small_button("開く").clicked() {
                    chatgpt::open_browser(&config::config_path().display().to_string());
                }
            });
            ui.end_row();
        });
        ui.add_space(14.0);
        ui.label(RichText::new("性能の確認").strong());
        hint(ui, "性能は PC と認識モデルによって変わります。現行版の録音中の待ち時間とメモリ予算は、実機での確認待ちです");
        hint(
            ui,
            "測定の手順と版ごとの記録は、ソースの reports/perf/ にあります",
        );
        ui.add_space(14.0);
        ui.label(RichText::new("使っているもの").strong());
        hint(
            ui,
            "音声認識: speech.cpp と Haas-ja-v1.0(Third Intelligence、Apache-2.0)",
        );
        hint(ui, "効果音: earshot(MIT)で作成");
        hint(ui, "画面: egui(MIT / Apache-2.0)");
    }
}

#[cfg(test)]
mod clipboard_tests {
    use super::*;
    use windows::core::w;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, WINDOW_EX_STYLE, WS_POPUP,
    };

    fn settings_without_devices_or_user_files() -> Settings {
        let mut cfg: Config = toml::from_str("").unwrap();
        cfg.speech_exe = "__missing_setup_fixture__.exe".into();
        Settings::from_loaded(cfg, None, false, chatgpt::Status::SignedOut, Vec::new())
    }

    fn has_close(output: egui::FullOutput) -> bool {
        let close = output.viewport_output.values().any(|viewport| {
            viewport
                .commands
                .iter()
                .any(|command| matches!(command, egui::ViewportCommand::Close))
        });
        output.drop_without_applying_deltas();
        close
    }

    fn wait_for_retirement(settings: &mut Settings, ctx: &egui::Context) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !settings.retiring_jobs.is_empty() {
            settings.poll_retiring_jobs(ctx);
            assert!(
                Instant::now() < deadline,
                "cancelled worker must finish before settings can close"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn closing_settings_keeps_a_starting_worker_until_it_finishes() {
        let mut settings = settings_without_devices_or_user_files();
        let ctx = egui::Context::default();
        let (release, wait) = std::sync::mpsc::channel();
        let (job, cancelled) = setup::ModelJob::blocked_for_test(wait);
        settings.model_job = Some(job);
        let mut close_elapsed = Duration::ZERO;
        let pending = ctx.run_pass(Default::default(), |ctx| {
            let started = Instant::now();
            settings.request_close(ctx);
            close_elapsed = started.elapsed();
        });
        assert!(
            close_elapsed < Duration::from_millis(250),
            "UI must not join the worker"
        );
        assert!(!has_close(pending));
        assert!(settings.closing);
        assert!(settings.model_job.is_none());
        assert_eq!(settings.retiring_jobs.len(), 1);
        assert!(cancelled.load(std::sync::atomic::Ordering::Acquire));
        settings.start_model_job(&ctx, false);
        assert!(
            settings.model_job.is_none(),
            "closing cannot start another worker"
        );
        release.send(()).unwrap();
        wait_for_retirement(&mut settings, &ctx);
        let finished = ctx.run_pass(Default::default(), |ctx| settings.update_model_jobs(ctx));
        assert!(has_close(finished));
        assert!(!settings.closing);
    }

    #[test]
    fn closing_also_waits_for_a_worker_cancelled_by_a_model_change() {
        let mut settings = settings_without_devices_or_user_files();
        let ctx = egui::Context::default();
        let (release, wait) = std::sync::mpsc::channel();
        let (job, cancelled) = setup::ModelJob::blocked_for_test(wait);
        settings.model_job = Some(job);
        settings.cfg.model = "changed-model".into();
        settings.update_model_jobs(&ctx);
        assert!(settings.model_job.is_none());
        assert_eq!(settings.retiring_jobs.len(), 1);
        assert!(cancelled.load(std::sync::atomic::Ordering::Acquire));
        settings.start_model_job(&ctx, false);
        assert!(
            settings.model_job.is_none(),
            "replacement waits for the prior process to stop"
        );
        let confirmation = ctx.run_pass(Default::default(), |ctx| settings.request_close(ctx));
        assert!(!has_close(confirmation));
        assert!(
            settings.close_confirm,
            "unsaved model changes still need the user's close choice"
        );
        settings.allow_close = true;
        settings.close_confirm = false;
        let pending = ctx.run_pass(Default::default(), |ctx| settings.request_close(ctx));
        assert!(!has_close(pending));
        assert!(settings.closing);
        release.send(()).unwrap();
        wait_for_retirement(&mut settings, &ctx);
        let finished = ctx.run_pass(Default::default(), |ctx| settings.update_model_jobs(ctx));
        assert!(has_close(finished));
    }

    #[test]
    fn save_shortcut_consumes_ctrl_s_without_matching_plain_s() {
        let ctx = egui::Context::default();
        for (modifiers, expected) in [
            (egui::Modifiers::NONE, false),
            (egui::Modifiers::CTRL, true),
        ] {
            let input = egui::RawInput {
                events: vec![egui::Event::Key {
                    key: egui::Key::S,
                    physical_key: Some(egui::Key::S),
                    pressed: true,
                    repeat: false,
                    modifiers,
                }],
                ..Default::default()
            };
            let output = ctx.run_pass(input, |ctx| {
                assert_eq!(save_shortcut(ctx), expected);
                assert!(!save_shortcut(ctx), "a shortcut is consumed once");
            });
            output.drop_without_applying_deltas();
        }
    }

    #[test]
    fn validation_errors_route_to_the_field_without_discarding_the_draft() {
        let mut settings = settings_without_devices_or_user_files();
        settings.cfg.hotkey = "invalid shortcut".into();
        settings.extra_body_text = "{invalid}".into();
        let (page, message) = settings.validation_issue().unwrap();
        assert!(page == Page::Input);
        let before = settings.cfg.clone();
        let ctx = egui::Context::default();
        let output = ctx.run_pass(Default::default(), |ctx| {
            settings.show_validation_issue(ctx, page, message.clone());
        });
        output.drop_without_applying_deltas();
        assert!(settings.page == Page::Input);
        assert_eq!(settings.cfg, before);
        assert_eq!(settings.message, Some((false, message)));
        settings.cfg.hotkey = "F8".into();
        assert!(settings.validation_issue().unwrap().0 == Page::Ai);
        settings.extra_body_text = "{}".into();
        assert!(settings.validation_issue().is_none());
    }

    #[test]
    fn sandbox_page_names_only_select_known_pages() {
        assert!(test_page("display") == Some(Page::Display));
        assert!(test_page("input") == Some(Page::Input));
        assert!(test_page("start-recording").is_none());
    }

    #[test]
    fn home_preparation_never_reports_ready_without_an_engine_or_verified_model() {
        let ready = setup::ModelStatus::Ready;
        let no_engine = preparation_notice(false, Some(&ready), false);
        assert!(!no_engine.ready);
        assert!(no_engine.heading.contains("speech.cpp"));
        let unchecked = preparation_notice(true, None, false);
        assert!(!unchecked.ready);
        assert!(unchecked.heading.contains("未確認"));
        assert!(unchecked.detail.contains("セットアップ"));
        let missing_state = setup::ModelStatus::Missing;
        let missing = preparation_notice(true, Some(&missing_state), false);
        assert!(!missing.ready);
        assert!(missing.heading.contains("未取得"));
        let checked = preparation_notice(true, Some(&ready), true);
        assert!(checked.ready, "a fetched model stays usable offline");
    }

    #[test]
    fn home_preparation_explains_offline_recovery_without_claiming_a_download() {
        let missing_state = setup::ModelStatus::Missing;
        let missing = preparation_notice(true, Some(&missing_state), true);
        assert!(missing.detail.contains("オフライン"));
        assert!(missing.detail.contains("取得できません"));
        let unchecked = preparation_notice(true, None, true);
        assert!(unchecked.detail.contains("取得済みか確認"));
        assert!(!unchecked.ready);
    }

    #[test]
    fn home_preparation_preserves_progress_errors_and_cancellation() {
        let running = setup::ModelStatus::Running {
            message: "モデルを取得しています 42%".into(),
            fraction: Some(0.42),
        };
        let progress = preparation_notice(true, Some(&running), false);
        assert!(!progress.ready);
        assert_eq!(progress.detail, "モデルを取得しています 42%");
        let failed = setup::ModelStatus::Failed("モデル一覧を読めません".into());
        let failure = preparation_notice(true, Some(&failed), false);
        assert!(!failure.ready);
        assert_eq!(failure.detail, "モデル一覧を読めません");
        let cancelled_state = setup::ModelStatus::Cancelled;
        let cancelled = preparation_notice(true, Some(&cancelled_state), false);
        assert!(!cancelled.ready);
        assert!(cancelled.heading.contains("中断"));
        assert!(cancelled.detail.contains("もう一度"));
    }

    #[test]
    fn setup_detects_a_missing_configured_executable() {
        let mut cfg: config::Config = toml::from_str("").unwrap();
        cfg.speech_exe = r"C:\存在しない場所\speech.exe".into();
        assert!(Settings::needs_setup(&cfg));
        cfg.speech_exe = std::env::current_exe()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(!Settings::needs_setup(&cfg));
    }

    #[test]
    fn clipboard_owner_requires_a_live_window_in_this_process() {
        assert!(check_settings_clipboard_owner(HWND::default()).is_err());
        // The built-in STATIC class allows a hidden test window without a GUI event loop.
        let owner = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("STATIC"),
                w!("OpenHinoVoice clipboard owner test"),
                WS_POPUP,
                0,
                0,
                1,
                1,
                None,
                None,
                None,
                None,
            )
        }
        .expect("create a hidden owner window");
        assert!(check_settings_clipboard_owner(owner).is_ok());
        unsafe { DestroyWindow(owner) }.expect("destroy the owner window");
        assert!(check_settings_clipboard_owner(owner).is_err());
    }

    #[test]
    fn display_restore_covers_theme_and_captions_without_other_page_edits() {
        let saved = config::parse("").unwrap();
        let snapshot = DisplaySnapshot::capture(&saved);
        let mut edited = saved.clone();
        edited.theme = "dark".into();
        edited.live_caption = !saved.live_caption;
        edited.partial_caption = !saved.partial_caption;
        edited.caption_position = "caret".into();
        assert_ne!(DisplaySnapshot::capture(&edited), snapshot);
        edited.sound_set = "rich".into();
        edited.hotkey = "Ctrl+Alt+J".into();
        edited.format.model = "other-model".into();
        snapshot.restore(&mut edited);
        assert_eq!(DisplaySnapshot::capture(&edited), snapshot);
        assert_eq!(edited.hotkey, "Ctrl+Alt+J");
        assert_eq!(edited.format.model, "other-model");
        edited.theme = "dark".into();
        Preset::Rich.apply(&mut edited);
        assert_eq!(edited.theme, "dark", "preset selection keeps the theme");
    }
}
