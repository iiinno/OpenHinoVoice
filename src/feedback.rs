use crate::config::Config;
use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SoundSet {
    Classic,
    Soft,
    Minimal,
    Rich,
}

impl SoundSet {
    pub const ALL: [Self; 4] = [Self::Classic, Self::Soft, Self::Minimal, Self::Rich];

    pub fn from_id(id: &str) -> Option<Self> {
        match id {
            "classic" => Some(Self::Classic),
            "soft" => Some(Self::Soft),
            "minimal" => Some(Self::Minimal),
            "rich" => Some(Self::Rich),
            _ => None,
        }
    }

    pub const fn id(self) -> &'static str {
        match self {
            Self::Classic => "classic",
            Self::Soft => "soft",
            Self::Minimal => "minimal",
            Self::Rich => "rich",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Classic => "標準",
            Self::Soft => "ソフト",
            Self::Minimal => "ミニマル",
            Self::Rich => "リッチ",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HudStyle {
    Standard,
    Soft,
    Minimal,
    Rich,
}

impl HudStyle {
    pub const ALL: [Self; 4] = [Self::Standard, Self::Soft, Self::Minimal, Self::Rich];

    pub fn from_id(id: &str) -> Option<Self> {
        match id {
            "standard" => Some(Self::Standard),
            "soft" => Some(Self::Soft),
            "minimal" => Some(Self::Minimal),
            "rich" => Some(Self::Rich),
            _ => None,
        }
    }

    pub const fn id(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::Soft => "soft",
            Self::Minimal => "minimal",
            Self::Rich => "rich",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Standard => "標準",
            Self::Soft => "ソフト",
            Self::Minimal => "ミニマル",
            Self::Rich => "リッチ",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedbackMotion {
    System,
    Reduced,
}

impl FeedbackMotion {
    pub const ALL: [Self; 2] = [Self::System, Self::Reduced];

    pub fn from_id(id: &str) -> Option<Self> {
        match id {
            "system" => Some(Self::System),
            "reduced" => Some(Self::Reduced),
            _ => None,
        }
    }

    pub const fn id(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Reduced => "reduced",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::System => "Windows に合わせる",
            Self::Reduced => "動きを減らす",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SoundEvent {
    Startup,
    Start,
    Stop,
    Done,
    Warning,
    Error,
    Cancel,
}

impl SoundEvent {
    pub const ALL: [Self; 7] = [
        Self::Startup,
        Self::Start,
        Self::Stop,
        Self::Done,
        Self::Warning,
        Self::Error,
        Self::Cancel,
    ];

    pub const fn id(self) -> &'static str {
        match self {
            Self::Startup => "startup",
            Self::Start => "start",
            Self::Stop => "stop",
            Self::Done => "done",
            Self::Warning => "warning",
            Self::Error => "error",
            Self::Cancel => "cancel",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Startup => "アプリ起動",
            Self::Start => "録音開始",
            Self::Stop => "録音終了",
            Self::Done => "入力完了",
            Self::Warning => "注意付き完了",
            Self::Error => "エラー",
            Self::Cancel => "取消",
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct SoundEvents {
    pub start: bool,
    pub stop: bool,
    pub done: bool,
    pub warning: bool,
    pub error: bool,
    pub cancel: bool,
}

impl Default for SoundEvents {
    fn default() -> Self {
        Self {
            start: true,
            stop: true,
            done: true,
            warning: true,
            error: true,
            cancel: false,
        }
    }
}

impl SoundEvents {
    pub const fn enabled(self, event: SoundEvent, startup_sound: bool) -> bool {
        match event {
            SoundEvent::Startup => startup_sound,
            SoundEvent::Start => self.start,
            SoundEvent::Stop => self.stop,
            SoundEvent::Done => self.done,
            SoundEvent::Warning => self.warning,
            SoundEvent::Error => self.error,
            SoundEvent::Cancel => self.cancel,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeedbackSettings {
    pub sounds: bool,
    pub volume: u8,
    pub sound_set: SoundSet,
    pub sound_events: SoundEvents,
    pub startup_sound: bool,
    pub hud_style: HudStyle,
    pub motion: FeedbackMotion,
}

impl FeedbackSettings {
    pub fn from_config(cfg: &Config) -> Self {
        Self {
            sounds: cfg.sounds,
            volume: cfg.volume.min(100),
            sound_set: SoundSet::from_id(&cfg.sound_set).unwrap_or(SoundSet::Classic),
            sound_events: cfg.sound_events,
            startup_sound: cfg.startup_sound,
            hud_style: HudStyle::from_id(&cfg.hud_style).unwrap_or(HudStyle::Standard),
            motion: FeedbackMotion::from_id(&cfg.feedback_motion).unwrap_or(FeedbackMotion::System),
        }
    }

    pub const fn sound_enabled(self, event: SoundEvent) -> bool {
        self.sounds && self.volume > 0 && self.sound_events.enabled(event, self.startup_sound)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedbackWarning {
    pub key: &'static str,
    pub value: String,
    pub fallback: &'static str,
}

impl FeedbackWarning {
    pub fn message(&self) -> String {
        format!(
            "{} の値「{}」は未対応のため「{}」を使用します。保存前の値は保持しています。",
            self.key, self.value, self.fallback
        )
    }
}

pub fn warnings(cfg: &Config) -> Vec<FeedbackWarning> {
    [
        (
            "sound_set",
            cfg.sound_set.as_str(),
            SoundSet::from_id(&cfg.sound_set).is_some(),
            SoundSet::Classic.id(),
        ),
        (
            "hud_style",
            cfg.hud_style.as_str(),
            HudStyle::from_id(&cfg.hud_style).is_some(),
            HudStyle::Standard.id(),
        ),
        (
            "feedback_motion",
            cfg.feedback_motion.as_str(),
            FeedbackMotion::from_id(&cfg.feedback_motion).is_some(),
            FeedbackMotion::System.id(),
        ),
    ]
    .into_iter()
    .filter(|(_, _, known, _)| !known)
    .map(|(key, value, _, fallback)| FeedbackWarning {
        key,
        value: value.to_owned(),
        fallback,
    })
    .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preset {
    Standard,
    Soft,
    Minimal,
    Silent,
    Rich,
}

impl Preset {
    pub const ALL: [Self; 5] = [
        Self::Standard,
        Self::Soft,
        Self::Minimal,
        Self::Silent,
        Self::Rich,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Standard => "標準",
            Self::Soft => "ソフト",
            Self::Minimal => "ミニマル",
            Self::Silent => "サイレント",
            Self::Rich => "リッチ",
        }
    }

    fn values(self) -> (bool, SoundSet, SoundEvents, HudStyle, FeedbackMotion) {
        let (sounds, sound_set, hud_style) = match self {
            Self::Standard => (true, SoundSet::Classic, HudStyle::Standard),
            Self::Soft => (true, SoundSet::Soft, HudStyle::Soft),
            Self::Minimal => (true, SoundSet::Minimal, HudStyle::Minimal),
            Self::Silent => (false, SoundSet::Classic, HudStyle::Standard),
            Self::Rich => (true, SoundSet::Rich, HudStyle::Rich),
        };
        let sound_events = SoundEvents {
            stop: self != Self::Minimal,
            ..SoundEvents::default()
        };
        (
            sounds,
            sound_set,
            sound_events,
            hud_style,
            FeedbackMotion::System,
        )
    }

    pub fn apply(self, cfg: &mut Config) {
        let (sounds, sound_set, sound_events, hud_style, motion) = self.values();
        cfg.sounds = sounds;
        cfg.sound_set = sound_set.id().to_owned();
        cfg.sound_events = sound_events;
        cfg.hud_style = hud_style.id().to_owned();
        cfg.feedback_motion = motion.id().to_owned();
    }

    pub fn detect(cfg: &Config) -> Option<Self> {
        let sound_set = SoundSet::from_id(&cfg.sound_set)?;
        let hud_style = HudStyle::from_id(&cfg.hud_style)?;
        let motion = FeedbackMotion::from_id(&cfg.feedback_motion)?;
        Self::ALL.into_iter().find(|preset| {
            preset.values() == (cfg.sounds, sound_set, cfg.sound_events, hud_style, motion)
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedbackSnapshot {
    pub sounds: bool,
    pub volume: u8,
    pub sound_set: String,
    pub sound_events: SoundEvents,
    pub startup_sound: bool,
    pub hud_style: String,
    pub feedback_motion: String,
}

impl FeedbackSnapshot {
    pub fn capture(cfg: &Config) -> Self {
        Self {
            sounds: cfg.sounds,
            volume: cfg.volume,
            sound_set: cfg.sound_set.clone(),
            sound_events: cfg.sound_events,
            startup_sound: cfg.startup_sound,
            hud_style: cfg.hud_style.clone(),
            feedback_motion: cfg.feedback_motion.clone(),
        }
    }

    pub fn restore(&self, cfg: &mut Config) {
        cfg.sounds = self.sounds;
        cfg.volume = self.volume;
        cfg.sound_set.clone_from(&self.sound_set);
        cfg.sound_events = self.sound_events;
        cfg.startup_sound = self.startup_sound;
        cfg.hud_style.clone_from(&self.hud_style);
        cfg.feedback_motion.clone_from(&self.feedback_motion);
    }

    pub fn edits(&self) -> Vec<(Option<&'static str>, &'static str, toml::Value)> {
        use toml::Value as V;
        vec![
            (None, "sounds", V::from(self.sounds)),
            (None, "volume", V::from(self.volume as i64)),
            (None, "sound_set", V::from(self.sound_set.as_str())),
            (None, "startup_sound", V::from(self.startup_sound)),
            (None, "hud_style", V::from(self.hud_style.as_str())),
            (
                None,
                "feedback_motion",
                V::from(self.feedback_motion.as_str()),
            ),
            (
                Some("sound_events"),
                "start",
                V::from(self.sound_events.start),
            ),
            (
                Some("sound_events"),
                "stop",
                V::from(self.sound_events.stop),
            ),
            (
                Some("sound_events"),
                "done",
                V::from(self.sound_events.done),
            ),
            (
                Some("sound_events"),
                "warning",
                V::from(self.sound_events.warning),
            ),
            (
                Some("sound_events"),
                "error",
                V::from(self.sound_events.error),
            ),
            (
                Some("sound_events"),
                "cancel",
                V::from(self.sound_events.cancel),
            ),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;

    #[test]
    fn feedback_legacy_values_keep_their_meaning() {
        let cfg = config::parse("sounds = false\nvolume = 23\ntheme = 'auto'\n").unwrap();
        let feedback = FeedbackSettings::from_config(&cfg);
        assert!(!feedback.sounds);
        assert_eq!(feedback.volume, 23);
        assert_eq!(feedback.sound_set, SoundSet::Classic);
        assert_eq!(feedback.hud_style, HudStyle::Standard);
        assert_eq!(feedback.motion, FeedbackMotion::System);
        assert_eq!(feedback.sound_events, SoundEvents::default());
        assert!(!feedback.startup_sound);
        assert!(!feedback.sound_events.cancel);
        assert!(warnings(&cfg).is_empty());
    }

    #[test]
    fn feedback_unknown_tokens_warn_without_rewriting_or_claiming_a_preset() {
        let source = "sound_set = 'future' # keep this choice\nhud_style = 'hologram'\nfeedback_motion = 'fluid'\n";
        let cfg = config::parse(source).unwrap();
        let feedback = FeedbackSettings::from_config(&cfg);
        assert_eq!(feedback.sound_set, SoundSet::Classic);
        assert_eq!(feedback.hud_style, HudStyle::Standard);
        assert_eq!(feedback.motion, FeedbackMotion::System);
        assert_eq!(warnings(&cfg).len(), 3);
        assert_eq!(warnings(&cfg)[0].value, "future");
        assert_eq!(Preset::detect(&cfg), None);
        let saved = config::set_values(source, &FeedbackSnapshot::capture(&cfg).edits()).unwrap();
        assert!(saved.contains("sound_set = 'future' # keep this choice"));
        assert_eq!(config::parse(&saved).unwrap(), cfg);
    }

    #[test]
    fn feedback_presets_round_trip_without_changing_independent_preferences() {
        for preset in Preset::ALL {
            let mut cfg = config::parse("").unwrap();
            cfg.volume = 39;
            cfg.startup_sound = true;
            cfg.theme = "auto".into();
            cfg.live_caption = false;
            cfg.partial_caption = false;
            cfg.caption_position = "caret".into();
            cfg.hotkey = "F9".into();
            cfg.format.api_key = "preserve-test-value".into();
            let original = cfg.clone();
            preset.apply(&mut cfg);
            assert_eq!(Preset::detect(&cfg), Some(preset));
            assert_eq!(cfg.volume, original.volume);
            assert_eq!(cfg.startup_sound, original.startup_sound);
            assert_eq!(cfg.theme, original.theme);
            assert_eq!(cfg.live_caption, original.live_caption);
            assert_eq!(cfg.partial_caption, original.partial_caption);
            assert_eq!(cfg.caption_position, original.caption_position);
            assert_eq!(cfg.hotkey, original.hotkey);
            assert_eq!(cfg.format, original.format);
            let saved = config::set_values("", &FeedbackSnapshot::capture(&cfg).edits()).unwrap();
            let loaded = config::parse(&saved).unwrap();
            assert_eq!(Preset::detect(&loaded), Some(preset));
            assert_eq!(
                FeedbackSettings::from_config(&loaded),
                FeedbackSettings::from_config(&cfg)
            );
        }
    }

    #[test]
    fn feedback_mixed_sound_and_display_are_custom_and_survive_a_save() {
        let mut cfg = config::parse("").unwrap();
        Preset::Rich.apply(&mut cfg);
        cfg.sounds = false;
        assert_eq!(Preset::detect(&cfg), None);
        let saved = config::set_values("", &FeedbackSnapshot::capture(&cfg).edits()).unwrap();
        let loaded = config::parse(&saved).unwrap();
        assert_eq!(loaded.hud_style, "rich");
        assert!(!loaded.sounds);
        assert_eq!(Preset::detect(&loaded), None);
        cfg.sounds = true;
        cfg.hud_style = HudStyle::Minimal.id().into();
        assert_eq!(
            FeedbackSettings::from_config(&cfg).sound_set,
            SoundSet::Rich
        );
        assert_eq!(Preset::detect(&cfg), None);
        cfg.feedback_motion = FeedbackMotion::Reduced.id().into();
        assert_eq!(
            FeedbackSettings::from_config(&cfg).motion,
            FeedbackMotion::Reduced
        );
    }

    #[test]
    fn feedback_mute_and_individual_events_gate_every_sound() {
        let mut cfg = config::parse("").unwrap();
        let feedback = FeedbackSettings::from_config(&cfg);
        assert!(feedback.sound_enabled(SoundEvent::Start));
        assert!(!feedback.sound_enabled(SoundEvent::Startup));
        assert!(!feedback.sound_enabled(SoundEvent::Cancel));
        cfg.startup_sound = true;
        cfg.sound_events.cancel = true;
        cfg.sound_events.start = false;
        let selected = FeedbackSettings::from_config(&cfg);
        assert!(selected.sound_enabled(SoundEvent::Startup));
        assert!(selected.sound_enabled(SoundEvent::Cancel));
        assert!(!selected.sound_enabled(SoundEvent::Start));
        cfg.volume = 0;
        assert!(SoundEvent::ALL
            .into_iter()
            .all(|event| !FeedbackSettings::from_config(&cfg).sound_enabled(event)));
        cfg.volume = 100;
        cfg.sounds = false;
        assert!(SoundEvent::ALL
            .into_iter()
            .all(|event| !FeedbackSettings::from_config(&cfg).sound_enabled(event)));
    }

    #[test]
    fn feedback_restore_only_restores_the_sound_and_hud_editing_scope() {
        let mut cfg = config::parse("").unwrap();
        let saved = FeedbackSnapshot::capture(&cfg);
        Preset::Rich.apply(&mut cfg);
        cfg.volume = 12;
        cfg.startup_sound = true;
        cfg.theme = "auto".into();
        cfg.caption_position = "caret".into();
        cfg.live_caption = false;
        cfg.partial_caption = false;
        cfg.hotkey = "F10".into();
        cfg.format.api_key = "other-page-edit".into();
        let independently_edited = cfg.clone();
        saved.restore(&mut cfg);
        assert_eq!(FeedbackSnapshot::capture(&cfg), saved);
        assert_eq!(cfg.theme, independently_edited.theme);
        assert_eq!(cfg.caption_position, independently_edited.caption_position);
        assert_eq!(cfg.live_caption, independently_edited.live_caption);
        assert_eq!(cfg.partial_caption, independently_edited.partial_caption);
        assert_eq!(cfg.hotkey, independently_edited.hotkey);
        assert_eq!(cfg.format, independently_edited.format);
    }

    #[test]
    fn feedback_event_edits_keep_table_comments_and_unknown_keys() {
        for source in [
            "# preferences\nvolume = 42 # volume\n[sound_events] # event table\nstart = false # start\nfuture_event = false # future\n[format]\nmodel = 'custom' # model\n",
            "# preferences\nvolume = 42 # volume\nsound_events = { start = false, future_event = false } # event table\n[format]\nmodel = 'custom' # model\n",
        ] {
            let mut cfg = config::parse(source).unwrap();
            assert!(!cfg.sound_events.start);
            assert!(cfg.sound_events.done, "omitted event values keep legacy defaults");
            Preset::Rich.apply(&mut cfg);
            let saved = config::set_values(source, &FeedbackSnapshot::capture(&cfg).edits()).unwrap();
            for comment in ["# preferences", "# volume", "# event table", "# model"] {
                assert!(saved.contains(comment), "{comment}: {saved}");
            }
            if source.contains("# start") {
                assert!(saved.contains("# start"));
                assert!(saved.contains("# future"));
            }
            let value: toml::Value = toml::from_str(&saved).unwrap();
            assert_eq!(value["sound_events"]["future_event"].as_bool(), Some(false));
            assert_eq!(value["format"]["model"].as_str(), Some("custom"));
            assert_eq!(Preset::detect(&config::parse(&saved).unwrap()), Some(Preset::Rich));
        }
    }
}
