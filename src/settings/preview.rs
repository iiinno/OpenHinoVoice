use crate::config::Config;
use crate::feedback::{FeedbackSettings, SoundEvent};
use crate::overlay::{self, Mode, Tone};
use eframe::egui::{self, Color32, TextureHandle, TextureOptions};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const FRAME_INTERVAL: Duration = Duration::from_nanos(16_666_667);
const SEQUENCE_END: Duration = Duration::from_millis(3900);
const EVENT_END: Duration = Duration::from_millis(900);

const ENVIRONMENT_SUBCLASS: usize = 0x4F48_5645;

struct EnvironmentState {
    ctx: egui::Context,
    installed: AtomicBool,
}

struct EnvironmentObserver {
    window: windows::Win32::Foundation::HWND,
    state: Arc<EnvironmentState>,
}

impl EnvironmentObserver {
    fn install(cc: &eframe::CreationContext<'_>) -> Option<Self> {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::Shell::SetWindowSubclass;
        let RawWindowHandle::Win32(handle) = cc.window_handle().ok()?.as_raw() else {
            return None;
        };
        let window = HWND(handle.hwnd.get() as *mut std::ffi::c_void);
        let state = Arc::new(EnvironmentState {
            ctx: cc.egui_ctx.clone(),
            installed: AtomicBool::new(true),
        });
        let callback_state = Arc::into_raw(Arc::clone(&state));
        let installed = unsafe {
            SetWindowSubclass(
                window,
                Some(environment_proc),
                ENVIRONMENT_SUBCLASS,
                callback_state as usize,
            )
            .as_bool()
        };
        if !installed {
            unsafe {
                drop(Arc::from_raw(callback_state));
            }
            return None;
        }
        Some(Self { window, state })
    }
}

impl Drop for EnvironmentObserver {
    fn drop(&mut self) {
        use windows::Win32::UI::Shell::RemoveWindowSubclass;
        use windows::Win32::UI::WindowsAndMessaging::IsWindow;
        if !self.state.installed.swap(false, Ordering::AcqRel) {
            return;
        }
        let removed = unsafe {
            RemoveWindowSubclass(self.window, Some(environment_proc), ENVIRONMENT_SUBCLASS)
                .as_bool()
                || !IsWindow(self.window).as_bool()
        };
        if removed {
            // Release the callback's strong reference only after it cannot run again.
            unsafe {
                drop(Arc::from_raw(Arc::as_ptr(&self.state)));
            }
        } else {
            // WM_NCDESTROY will release it if Windows could not remove the callback yet.
            self.state.installed.store(true, Ordering::Release);
        }
    }
}

unsafe extern "system" fn environment_proc(
    window: windows::Win32::Foundation::HWND,
    message: u32,
    wparam: windows::Win32::Foundation::WPARAM,
    lparam: windows::Win32::Foundation::LPARAM,
    _id: usize,
    data: usize,
) -> windows::Win32::Foundation::LRESULT {
    use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass};
    use windows::Win32::UI::WindowsAndMessaging::{
        WM_NCDESTROY, WM_SETTINGCHANGE, WM_SYSCOLORCHANGE, WM_THEMECHANGED,
    };
    let state = &*(data as *const EnvironmentState);
    if matches!(
        message,
        WM_SETTINGCHANGE | WM_SYSCOLORCHANGE | WM_THEMECHANGED
    ) {
        state.ctx.request_repaint();
    }
    let release = message == WM_NCDESTROY && state.installed.swap(false, Ordering::AcqRel);
    if release {
        let _ = RemoveWindowSubclass(window, Some(environment_proc), ENVIRONMENT_SUBCLASS);
    }
    let result = DefSubclassProc(window, message, wparam, lparam);
    if release {
        drop(Arc::from_raw(data as *const EnvironmentState));
    }
    result
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    Sequence,
    Event(SoundEvent),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scene {
    Recording,
    Recognizing,
    Formatting,
    Success,
    Startup,
    Warning,
    Error,
    Cancelled,
}

impl Scene {
    fn mode(self, now: Instant) -> Mode {
        match self {
            Self::Recording => Mode::Recording {
                started: now,
                preparing: false,
            },
            Self::Recognizing => Mode::Recognizing { preparing: false },
            Self::Formatting => Mode::Busy {
                label: "文章を整えています".into(),
                hint: Some("これはプレビューです".into()),
            },
            Self::Success => Mode::Notice {
                tone: Tone::Success,
                text: "入力しました".into(),
            },
            Self::Startup => Mode::Notice {
                tone: Tone::Startup,
                text: "OpenHinoVoiceを起動しました".into(),
            },
            Self::Warning => Mode::Notice {
                tone: Tone::Warn,
                text: "一部を原文のまま入力しました".into(),
            },
            Self::Error => Mode::Notice {
                tone: Tone::Error,
                text: "入力できませんでした。再試行できます".into(),
            },
            Self::Cancelled => Mode::Notice {
                tone: Tone::Info,
                text: "取り消しました".into(),
            },
        }
    }

    fn event(self) -> Option<SoundEvent> {
        match self {
            Self::Recording => Some(SoundEvent::Start),
            Self::Recognizing => Some(SoundEvent::Stop),
            Self::Formatting => None,
            Self::Success => Some(SoundEvent::Done),
            Self::Startup => Some(SoundEvent::Startup),
            Self::Warning => Some(SoundEvent::Warning),
            Self::Error => Some(SoundEvent::Error),
            Self::Cancelled => Some(SoundEvent::Cancel),
        }
    }

    fn from_event(event: SoundEvent) -> Self {
        match event {
            SoundEvent::Startup => Self::Startup,
            SoundEvent::Start => Self::Recording,
            SoundEvent::Stop => Self::Recognizing,
            SoundEvent::Done => Self::Success,
            SoundEvent::Warning => Self::Warning,
            SoundEvent::Error => Self::Error,
            SoundEvent::Cancel => Self::Cancelled,
        }
    }
}

struct Session {
    kind: Kind,
    started: Instant,
    token: u64,
    settings: FeedbackSettings,
    last_scene: Option<Scene>,
}

#[derive(Default)]
struct Timeline {
    session: Option<Session>,
}

struct Step {
    scene: Scene,
    elapsed: Duration,
    level: f32,
    sound: Option<(FeedbackSettings, SoundEvent, u64)>,
    ended: Option<u64>,
    next_transition: Option<Duration>,
}

impl Timeline {
    fn start(&mut self, kind: Kind, now: Instant, token: u64, settings: FeedbackSettings) {
        self.session = Some(Session {
            kind,
            started: now,
            token,
            settings,
            last_scene: None,
        });
    }

    fn running(&self) -> bool {
        self.session.is_some()
    }

    fn stop(&mut self) -> Option<u64> {
        self.session.take().map(|session| session.token)
    }

    fn advance(&mut self, now: Instant) -> Step {
        let idle = || Step {
            scene: Scene::Recording,
            elapsed: Duration::from_millis(800),
            level: 0.38,
            sound: None,
            ended: None,
            next_transition: None,
        };
        let Some(session) = &mut self.session else {
            return idle();
        };
        let elapsed = now.saturating_duration_since(session.started);
        let end = match session.kind {
            Kind::Sequence => SEQUENCE_END,
            Kind::Event(_) => EVENT_END,
        };
        if elapsed >= end {
            let token = self.stop();
            return Step {
                ended: token,
                ..idle()
            };
        }
        let (scene, since, next) = match session.kind {
            Kind::Event(event) => (Scene::from_event(event), Duration::ZERO, end),
            Kind::Sequence if elapsed < Duration::from_millis(1500) => (
                Scene::Recording,
                Duration::ZERO,
                Duration::from_millis(1500),
            ),
            Kind::Sequence if elapsed < Duration::from_millis(2300) => (
                Scene::Recognizing,
                Duration::from_millis(1500),
                Duration::from_millis(2300),
            ),
            Kind::Sequence if elapsed < Duration::from_millis(3300) => (
                Scene::Formatting,
                Duration::from_millis(2300),
                Duration::from_millis(3300),
            ),
            Kind::Sequence => (Scene::Success, Duration::from_millis(3300), SEQUENCE_END),
        };
        let sound = if session.last_scene != Some(scene) {
            scene.event().and_then(|event| {
                session.settings.sound_enabled(event).then_some((
                    session.settings,
                    event,
                    session.token,
                ))
            })
        } else {
            None
        };
        session.last_scene = Some(scene);
        let phase = elapsed.as_secs_f32();
        Step {
            scene,
            elapsed: elapsed.saturating_sub(since),
            level: if scene == Scene::Recording {
                (0.4 + (phase * 7.0).sin() * 0.17 + (phase * 3.1).cos() * 0.1).clamp(0.0, 1.0)
            } else {
                0.0
            },
            sound,
            ended: None,
            next_transition: Some(next.saturating_sub(elapsed)),
        }
    }
}

#[derive(Default)]
pub(super) struct Preview {
    timeline: Timeline,
    texture: Option<TextureHandle>,
    image_key: Option<(
        String,
        crate::feedback::HudStyle,
        crate::feedback::FeedbackMotion,
        u32,
        overlay::PreviewEnvironment,
    )>,
    pub message: Option<String>,
    audio_available: bool,
    _environment_observer: Option<EnvironmentObserver>,
}

impl Preview {
    pub fn observe_environment(&mut self, cc: &eframe::CreationContext<'_>) {
        self._environment_observer = EnvironmentObserver::install(cc);
    }

    pub fn running(&self) -> bool {
        self.timeline.running()
    }

    pub fn stop(&mut self) {
        if let Some(token) = self.timeline.stop() {
            let _ = crate::preview_ipc::request_stop(token);
        }
        self.image_key = None;
    }

    pub fn start(&mut self, kind: Kind, cfg: &Config) {
        self.stop();
        self.message = None;
        self.audio_available = true;
        self.timeline.start(
            kind,
            Instant::now(),
            crate::preview_ipc::new_token(),
            FeedbackSettings::from_config(cfg),
        );
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, cfg: &Config) {
        let (rect, _) =
            ui.allocate_exact_size(egui::vec2(ui.available_width(), 76.0), egui::Sense::hover());
        if ui
            .ctx()
            .input(|input| input.viewport().visible() == Some(false))
        {
            self.stop();
            return;
        }
        let visible = ui.is_rect_visible(rect);
        let now = Instant::now();
        let step = self.timeline.advance(now);
        if let Some(token) = step.ended {
            let _ = crate::preview_ipc::request_stop(token);
            self.image_key = None;
        }
        if self.audio_available {
            if let Some((settings, event, token)) = step.sound {
                if let Err(message) = crate::preview_ipc::request_play(
                    settings.sound_set,
                    event,
                    settings.volume,
                    token,
                ) {
                    self.audio_available = false;
                    self.message = Some(format!("表示だけを試しています。{message}"));
                }
            }
        }
        if !visible {
            if let Some(next) = step.next_transition {
                ui.ctx().request_repaint_after(next);
            }
            return;
        }
        let settings = FeedbackSettings::from_config(cfg);
        let scale = ui.ctx().pixels_per_point().clamp(1.0, 3.0);
        let key = (
            cfg.theme.clone(),
            settings.hud_style,
            settings.motion,
            scale.to_bits(),
            overlay::preview_environment(&cfg.theme, settings.motion),
        );
        if self.timeline.running() || self.image_key.as_ref() != Some(&key) {
            let frame = overlay::preview_frame(
                &step.scene.mode(now),
                &cfg.theme,
                settings.hud_style,
                settings.motion,
                step.elapsed,
                step.level,
                scale,
            );
            let image =
                egui::ColorImage::from_rgba_unmultiplied([frame.width, frame.height], &frame.rgba);
            match &mut self.texture {
                Some(texture) => texture.set(image, TextureOptions::LINEAR),
                None => {
                    self.texture = Some(ui.ctx().load_texture(
                        "feedback-preview",
                        image,
                        TextureOptions::LINEAR,
                    ));
                }
            }
            self.image_key = Some(key);
        }
        if let Some(texture) = &self.texture {
            let native = texture.size_vec2() / scale;
            let factor = (rect.width() / native.x)
                .min(rect.height() / native.y)
                .min(1.0);
            let image_rect = egui::Rect::from_center_size(rect.center(), native * factor);
            ui.painter().image(
                texture.id(),
                image_rect,
                egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
                Color32::WHITE,
            );
        }
        if let Some(next) = step.next_transition {
            ui.ctx()
                .request_repaint_after(if overlay::motion_enabled(settings.motion) {
                    FRAME_INTERVAL.min(next)
                } else {
                    next
                });
        }
    }
}

impl Drop for Preview {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feedback::Preset;

    fn settings(preset: Preset) -> FeedbackSettings {
        let mut cfg = crate::config::parse("").unwrap();
        preset.apply(&mut cfg);
        FeedbackSettings::from_config(&cfg)
    }

    #[test]
    fn preview_sequence_is_synthetic_bounded_and_plays_each_cue_once() {
        let now = Instant::now();
        let mut timeline = Timeline::default();
        timeline.start(Kind::Sequence, now, 7, settings(Preset::Rich));
        let start = timeline.advance(now);
        assert_eq!(start.scene, Scene::Recording);
        assert_eq!(start.sound.unwrap().1, SoundEvent::Start);
        assert!(timeline.advance(now + FRAME_INTERVAL).sound.is_none());
        let stop = timeline.advance(now + Duration::from_millis(1500));
        assert_eq!(stop.sound.unwrap().1, SoundEvent::Stop);
        let formatting = timeline.advance(now + Duration::from_millis(2300));
        assert_eq!(formatting.scene, Scene::Formatting);
        assert!(formatting.sound.is_none());
        assert_eq!(
            timeline
                .advance(now + Duration::from_millis(3300))
                .sound
                .unwrap()
                .1,
            SoundEvent::Done
        );
        let finished = timeline.advance(now + SEQUENCE_END);
        assert_eq!(finished.ended, Some(7));
        assert!(!timeline.running());
        assert!(finished.next_transition.is_none());
        assert!(timeline.advance(now + SEQUENCE_END).ended.is_none());
    }

    #[test]
    fn preview_mute_and_disabled_events_never_emit_sound() {
        let now = Instant::now();
        for preset in [Preset::Silent, Preset::Minimal] {
            let mut timeline = Timeline::default();
            timeline.start(Kind::Event(SoundEvent::Stop), now, 8, settings(preset));
            assert!(timeline.advance(now).sound.is_none());
        }
        let mut zero_volume = settings(Preset::Rich);
        zero_volume.volume = 0;
        let mut timeline = Timeline::default();
        timeline.start(Kind::Sequence, now, 9, zero_volume);
        for ms in [0, 1500, 3300] {
            assert!(timeline
                .advance(now + Duration::from_millis(ms))
                .sound
                .is_none());
        }
    }

    #[test]
    fn preview_stop_invalidates_all_future_cues_and_returns_only_its_token() {
        let now = Instant::now();
        let mut timeline = Timeline::default();
        timeline.start(Kind::Sequence, now, 17, settings(Preset::Rich));
        timeline.advance(now);
        assert_eq!(timeline.stop(), Some(17));
        assert_eq!(timeline.stop(), None);
        let after = timeline.advance(now + Duration::from_millis(3300));
        assert!(after.sound.is_none());
        assert!(after.next_transition.is_none());
    }

    #[test]
    fn preview_late_frame_does_not_replay_stale_cues() {
        let now = Instant::now();
        let mut timeline = Timeline::default();
        timeline.start(Kind::Sequence, now, 21, settings(Preset::Rich));
        let delayed = timeline.advance(now + Duration::from_millis(2400));
        assert_eq!(delayed.scene, Scene::Formatting);
        assert!(delayed.sound.is_none());
        assert_eq!(
            timeline
                .advance(now + Duration::from_millis(3400))
                .sound
                .unwrap()
                .1,
            SoundEvent::Done
        );
    }

    #[test]
    fn preview_single_event_uses_the_requested_state_and_expires() {
        let now = Instant::now();
        let mut cfg = settings(Preset::Standard);
        cfg.startup_sound = true;
        cfg.sound_events.cancel = true;
        for event in SoundEvent::ALL {
            let mut timeline = Timeline::default();
            timeline.start(Kind::Event(event), now, 25, cfg);
            let step = timeline.advance(now);
            assert_eq!(step.scene, Scene::from_event(event));
            assert_eq!(step.sound.unwrap().1, event);
            assert_eq!(timeline.advance(now + EVENT_END).ended, Some(25));
        }
    }
}
