//! トレイの「設定」から別プロセスで開く管理画面。保存すると常駐側が設定ファイルの変化に気づいて読み直す

use crate::asr;
use crate::audio;
use crate::chatgpt;
use crate::config::{self, Config};
use crate::history;
use crate::paste;
use crate::sound::{self, Sound};
use crate::startup;
use eframe::egui::{self, Color32, RichText};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

const HINOMARU_RED: Color32 = Color32::from_rgb(188, 0, 45);
const OK_GREEN: Color32 = Color32::from_rgb(40, 140, 70);
const SAMPLE: &str = "えーと、明日の会議は午後3時からに変更になりました。資料は、あのー、共有フォルダにおいてあります。";
const ASR_MODELS: [(&str, &str); 4] = [
    ("haas-ja-v1.0:q4_k", "Q4_K(0.35GB、既定)"),
    ("haas-ja-v1.0:q5_k", "Q5_K(0.43GB)"),
    ("haas-ja-v1.0:q6_k", "Q6_K(0.51GB)"),
    ("haas-ja-v1.0", "Q8_0(0.66GB、最も正確)"),
];

pub fn run() {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("OpenHinoVoice")
            .with_inner_size([720.0, 520.0])
            .with_min_inner_size([600.0, 420.0]),
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
            });
            Ok(Box::new(Settings::load()))
        }),
    ) {
        crate::overlay::show_error(&format!("設定画面を開けません: {e}"));
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
    General,
    Format,
    Asr,
    History,
    Sound,
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

/// 履歴を選んだあとユーザーが貼り付け先へ切り替える時間を確保する。
/// 設定ウィンドウ自身が前面のままなら入力しない。失敗しても元データは履歴に残る。
fn repaste_after_switch(text: String) -> Result<String, String> {
    std::thread::sleep(Duration::from_secs(3));
    let window = unsafe { GetForegroundWindow() };
    if window.0.is_null() {
        return Err("前面の入力先を取得できません".into());
    }
    let mut pid = 0;
    unsafe { GetWindowThreadProcessId(window, Some(&mut pid)) };
    if pid == std::process::id() {
        return Err("貼り付け先のアプリに切り替えてください".into());
    }
    let target = paste::capture_target()?;
    paste::paste_text(&text, target, HWND::default())?;
    Ok("履歴のテキストを貼り付けました".into())
}

type ModelList = Vec<(String, String)>;

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
    file_job: Option<Job<Result<String, String>>>,
    file_result: Option<Result<String, String>>,
    history_message: Option<String>,
    history_paste_job: Option<Job<Result<String, String>>>,
    microphones: Vec<String>,
    history_entries: Vec<history::RecoveryEntry>,
    history_loaded: bool,
    history_clear_confirm: bool,
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
        Settings {
            timeout_secs: cfg.format.timeout_ms as f32 / 1000.0,
            cfg,
            page: Page::General,
            show_key: false,
            startup: startup::is_enabled(),
            message,
            models: Vec::new(),
            models_job: None,
            test_job: None,
            test_result: None,
            chatgpt_status: chatgpt::status(),
            chatgpt_job: None,
            file_path: String::new(),
            file_job: None,
            file_result: None,
            history_message: None,
            history_paste_job: None,
            microphones: audio::input_devices(),
            history_entries: Vec::new(),
            history_loaded: false,
            history_clear_confirm: false,
        }
    }

    fn save(&mut self) -> Result<(), String> {
        use toml::Value as V;
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
        let edits = [
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
            (Some("format"), "provider", V::from(f.provider.as_str())),
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
        config::save_api_keys(f.api_key.trim(), f.openai_api_key.trim())?;
        config::save(&doc.to_string())?;
        startup::set_enabled(self.startup)
    }

    /// 編集中の内容で整形の設定を作る。接続テストとモデル一覧に使う
    fn draft_format(&self) -> config::FormatConfig {
        let mut f = self.cfg.format.clone();
        f.timeout_ms = (self.timeout_secs * 1000.0).round() as u64;
        f
    }
}

impl eframe::App for Settings {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
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
            self.file_result = Some(r.map(|raw| {
                let normalized = crate::lexicon::replace_explicit(&raw, &self.cfg.terms);
                if let Ok(Some(id)) =
                    history::save_recognized_if_enabled(&raw, self.cfg.recovery_history)
                {
                    if normalized != raw {
                        let _ = history::update_formatted(id, &normalized);
                    }
                    let _ = history::update_paste_status(id, history::PasteStatus::Pending);
                }
                normalized
            }));
        }
        if let Some(r) = take(&mut self.history_paste_job) {
            self.history_message = Some(match r {
                Ok(message) => message,
                Err(message) => format!("貼り直せませんでした: {message}。履歴は残っています"),
            });
        }

        egui::Panel::left("nav")
            .resizable(false)
            .exact_size(170.0)
            .frame(egui::Frame::side_top_panel(ui.style()).inner_margin(egui::Margin::same(14)))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let (rect, _) =
                        ui.allocate_exact_size(egui::vec2(14.0, 14.0), egui::Sense::hover());
                    ui.painter().circle_filled(rect.center(), 6.0, HINOMARU_RED);
                    ui.heading("OpenHinoVoice");
                });
                ui.add_space(16.0);
                for (page, label) in [
                    (Page::General, "一般"),
                    (Page::Format, "整形(AI)"),
                    (Page::Asr, "認識"),
                    (Page::History, "履歴と再入力"),
                    (Page::Sound, "サウンド"),
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
                        self.page = page;
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
                    let valid = config::parse_hotkey(&self.cfg.hotkey).is_some();
                    let save = egui::Button::new(RichText::new("保存").color(Color32::WHITE))
                        .fill(HINOMARU_RED)
                        .min_size(egui::vec2(84.0, 30.0));
                    if ui.add_enabled(valid, save).clicked() {
                        self.message = Some(match self.save() {
                            Ok(()) => (true, "保存しました。すぐに反映されます。".into()),
                            Err(e) => (false, e),
                        });
                    }
                    if ui
                        .add(egui::Button::new("閉じる").min_size(egui::vec2(84.0, 30.0)))
                        .clicked()
                    {
                        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                    if let Some((ok, text)) = &self.message {
                        ui.add_space(8.0);
                        ui.colored_label(if *ok { OK_GREEN } else { HINOMARU_RED }, text);
                    }
                });
            });

        egui::CentralPanel::default()
            .frame(egui::Frame::central_panel(ui.style()).inner_margin(egui::Margin::same(20)))
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| match self.page {
                        Page::General => self.general(ui),
                        Page::Format => self.format(ui),
                        Page::Asr => self.asr(ui),
                        Page::History => self.history(ui),
                        Page::Sound => self.sound(ui),
                        Page::About => self.about(ui),
                    });
            });
    }
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
    fn general(&mut self, ui: &mut egui::Ui) {
        section(ui, "一般");
        grid(ui, "general", |ui| {
            ui.label("録音開始キー");
            ui.vertical(|ui| {
                ui.add(egui::TextEdit::singleline(&mut self.cfg.hotkey).desired_width(180.0));
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
                        for minutes in [1, 3, 5, 10] {
                            ui.selectable_value(
                                &mut self.cfg.max_recording_minutes,
                                minutes,
                                format!("{minutes}分"),
                            );
                        }
                    });
                hint(
                    ui,
                    "時間になると自動で録音を終了して認識します（データ上限128 MiB）",
                );
            });
            ui.end_row();

            ui.label("表示のテーマ");
            ui.vertical(|ui| {
                ui.radio_value(
                    &mut self.cfg.theme,
                    "hinomaru".to_string(),
                    "日の丸(白地に赤い点)",
                );
                ui.radio_value(
                    &mut self.cfg.theme,
                    "auto".to_string(),
                    "Windows のライト、ダークに合わせる",
                );
            });
            ui.end_row();

            ui.label("自動起動");
            ui.checkbox(&mut self.startup, "Windows にサインインしたときに起動する");
            ui.end_row();
        });
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
                ui.radio_value(f, "opencode-go".into(), "OpenCode Go");
                ui.radio_value(f, "chatgpt".into(), "ChatGPT のプラン(Plus、Pro)");
                ui.radio_value(f, "openai".into(), "OpenAI の API キー");
                ui.radio_value(f, "custom".into(), "その他の OpenAI 互換サービス");
            });
            ui.end_row();
        });
        if before != self.cfg.format.provider {
            self.models.clear();
            self.test_result = None;
        }
        ui.add_space(14.0);

        match self.cfg.format.provider.as_str() {
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
                let custom = provider == "custom";
                let f = &mut self.cfg.format;
                let show_key = &mut self.show_key;
                grid(ui, "compat", |ui| {
                    if custom {
                        ui.label("ベース URL");
                        ui.add(
                            egui::TextEdit::singleline(&mut f.base_url)
                                .hint_text("https://example.com/v1")
                                .desired_width(340.0),
                        );
                        ui.end_row();
                    }
                    ui.label("API キー");
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut f.api_key)
                                .password(!*show_key)
                                .desired_width(300.0),
                        );
                        ui.checkbox(show_key, "表示");
                    });
                    ui.end_row();
                });
                if !custom {
                    f.base_url = "https://opencode.ai/inference/go/openai/v1".into();
                }
                self.model_picker(ui);
            }
        }

        ui.add_space(14.0);
        grid(ui, "timeout", |ui| {
            ui.label("応答の停止を待つ時間");
            ui.vertical(|ui| {
                ui.add(
                    egui::Slider::new(&mut self.timeout_secs, 3.0..=30.0)
                        .step_by(0.5)
                        .suffix(" 秒"),
                );
                hint(ui, "AI の応答がこの時間ずっと途切れたら、自動でやり直します。長く話しても、返っている間は待ちます");
            });
            ui.end_row();
        });

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
                    let r = crate::format::format_with_retry(&f, SAMPLE, &|| false, |_| {})
                        .and_then(|text| {
                            if text == SAMPLE || text.contains("えーと") || text.contains("あのー")
                            {
                                Err(crate::format::FormatError::BadResponse(
                                    "接続しましたが、例文の言い淀みを整形できませんでした".into(),
                                ))
                            } else {
                                Ok(text)
                            }
                        })
                        .map_err(|e| e.to_string());
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
            self.models_job = Some(spawn(ui.ctx(), move || match f.provider.as_str() {
                "chatgpt" => chatgpt::list_models()
                    .map(|m| m.into_iter().map(|m| (m.slug, m.display_name)).collect()),
                "openai" => {
                    crate::format::list_models("https://api.openai.com/v1", &f.openai_api_key)
                }
                // OpenCode Go の一覧は整形の窓口(/openai/v1)ではなく /inference/go/v1 にある
                "opencode-go" => {
                    crate::format::list_models("https://opencode.ai/inference/go/v1", &f.api_key)
                }
                _ => crate::format::list_models(&f.base_url, &f.api_key),
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
                    if ui.add_enabled(!busy, button).clicked() {
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
                        if ui.link("Manage usage").clicked() {
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
                    egui::TextEdit::singleline(&mut self.cfg.words)
                        .hint_text("共有フォルダ,OpenHinoVoice")
                        .desired_width(340.0),
                );
                hint(ui, "聞き間違えやすい言葉をカンマで区切って並べます");
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
                    "空なら自動で探します。認識モデルは初回に自動で取得します",
                );
            });
            ui.end_row();
        });
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
                    ui.add(egui::TextEdit::singleline(&mut term.canonical).desired_width(220.0));
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
                self.file_result = None;
                self.file_job = Some(spawn(ui.ctx(), move || {
                    asr::transcribe_file_async(path, exe, model, words)
                        .recv()
                        .unwrap_or_else(|_| Err("ファイル認識の結果を受信できませんでした".into()))
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
                            self.message =
                                match paste::copy_text_to_clipboard(text, HWND::default()) {
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

    fn reload_history(&mut self) {
        self.history_loaded = true;
        match history::recent(30) {
            Ok(mut entries) => {
                entries.sort_by_key(|a| std::cmp::Reverse(a.id));
                self.history_entries = entries;
                self.history_message = None;
            }
            Err(e) => self.history_message = Some(format!("復旧履歴の読み込みに失敗: {e}")),
        }
    }

    fn history(&mut self, ui: &mut egui::Ui) {
        if !self.history_loaded {
            self.reload_history();
        }
        section(ui, "履歴と再入力");
        ui.checkbox(
            &mut self.cfg.recovery_history,
            "認識した文章を復旧用にローカル保存する",
        );
        hint(ui, "既定でオン。文章はWindowsのユーザーに紐づく暗号化で保存。最大200件・16MiBの短期履歴です。設定を保存すると反映されます。");
        ui.horizontal(|ui| {
            if ui.button("履歴を更新").clicked() {
                self.reload_history();
            }
            if !self.history_clear_confirm {
                if ui.button("履歴を消去").clicked() {
                    self.history_clear_confirm = true;
                }
            } else {
                ui.label("すべて消去しますか？");
                if ui.button("消去する").clicked() {
                    match history::clear_recovery() {
                        Ok(()) => {
                            self.history_loaded = false;
                            self.history_entries.clear();
                            self.history_message = Some("復旧履歴を消去しました".into());
                        }
                        Err(e) => self.history_message = Some(format!("消去できません: {e}")),
                    }
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
        if self.history_paste_job.is_some() {
            ui.horizontal(|ui| {
                ui.spinner();
                hint(ui, "3秒以内に貼り付け先のウィンドウを選んでください");
            });
        }
        ui.add_space(8.0);
        if self.history_entries.is_empty() {
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
                        "記録 #{} · {} · {:?}",
                        entry.id, when, entry.paste_status
                    ));
                    if ui.small_button("コピー").clicked() {
                        self.history_message =
                            Some(match paste::copy_text_to_clipboard(text, HWND::default()) {
                                Ok(()) => "文章をクリップボードにコピーしました".into(),
                                Err(e) => format!("コピーに失敗: {e}"),
                            });
                    }
                    if ui
                        .add_enabled(
                            self.history_paste_job.is_none(),
                            egui::Button::new("貼り直す（3秒）"),
                        )
                        .clicked()
                    {
                        let text = text.to_owned();
                        self.history_message = Some("貼り付け先へ切り替えてください".into());
                        self.history_paste_job =
                            Some(spawn(ui.ctx(), move || repaste_after_switch(text)));
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

    fn sound(&mut self, ui: &mut egui::Ui) {
        section(ui, "サウンド");
        ui.checkbox(&mut self.cfg.sounds, "録音開始、確定、エラーで音を鳴らす");
        ui.add_space(10.0);
        grid(ui, "sound", |ui| {
            ui.label("音量");
            ui.add(egui::Slider::new(&mut self.cfg.volume, 0..=100).suffix(" %"));
            ui.end_row();

            ui.label("試し聞き");
            ui.horizontal(|ui| {
                for (s, label) in [
                    (Sound::Start, "録音開始"),
                    (Sound::Stop, "確定"),
                    (Sound::Done, "貼り付け"),
                    (Sound::Error, "エラー"),
                ] {
                    if ui.button(label).clicked() {
                        sound::play(s, self.cfg.volume);
                    }
                }
            });
            ui.end_row();
        });
    }

    fn about(&mut self, ui: &mut egui::Ui) {
        section(ui, "情報");
        ui.label(format!("OpenHinoVoice {}", env!("CARGO_PKG_VERSION")));
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
        ui.label(RichText::new("使っているもの").strong());
        hint(
            ui,
            "音声認識: speech.cpp と Haas-ja-v1.0(Third Intelligence、Apache-2.0)",
        );
        hint(ui, "効果音: earshot(MIT)で作成");
        hint(ui, "画面: egui(MIT / Apache-2.0)");
    }
}
