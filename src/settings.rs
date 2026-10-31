//! トレイの「設定」から別プロセスで開く管理画面。保存すると常駐側が設定ファイルの変化に気づいて読み直す

use crate::chatgpt;
use crate::config::{self, Config};
use crate::sound::{self, Sound};
use crate::startup;
use eframe::egui::{self, Color32, RichText};
use std::sync::{Arc, Mutex};
use std::time::Instant;

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
        }
    }

    fn save(&mut self) -> Result<(), String> {
        use toml::Value as V;
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
            (None, "theme", V::from(self.cfg.theme.as_str())),
            (None, "speech_exe", V::from(speech_exe)),
            (None, "model", V::from(self.cfg.model.as_str())),
            (None, "words", V::from(self.cfg.words.trim())),
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
            (
                Some("format"),
                "timeout_ms",
                V::from((self.timeout_secs * 1000.0).round() as i64),
            ),
            (Some("format"), "prompt", V::from(f.prompt.as_str())),
        ];
        config::save_api_keys(f.api_key.trim(), f.openai_api_key.trim())?;
        config::save(&config::set_values(
            text.trim_start_matches('\u{feff}'),
            &edits,
        ))?;
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
            ui.label("整形を待つ時間");
            ui.vertical(|ui| {
                ui.add(
                    egui::Slider::new(&mut self.timeout_secs, 1.0..=15.0)
                        .step_by(0.5)
                        .suffix(" 秒"),
                );
                hint(ui, "この時間内に返らないときは、整形せずに貼ります");
            });
            ui.end_row();
        });

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
                    let r = crate::format::format(&f, SAMPLE).map_err(|e| e.to_string());
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
