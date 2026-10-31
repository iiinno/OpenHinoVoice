#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    Idle,
    Recording,
    Recognizing { raw_requested: bool },
    Formatting { raw: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    F8,
    Enter,
    Esc,
    AsrDone { text: String, has_api_key: bool },
    AsrError { message: String },
    FormatDone { text: String },
    FormatFailed { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverlayKind {
    Recording,
    Recognizing,
    Formatting,
    FormatSkipped { reason: String },
    ApiKeyMissing,
    Error { message: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    StartSession,
    StopRecordingAndTranscribe,
    DiscardSession,
    CallFormat { raw: String },
    Paste { text: String },
    ShowOverlay(OverlayKind),
    HideOverlay,
}

pub fn step(state: State, event: Event) -> (State, Vec<Action>) {
    match state {
        State::Idle => match event {
            Event::F8 => (
                State::Recording,
                vec![Action::StartSession, Action::ShowOverlay(OverlayKind::Recording)],
            ),
            _ => (State::Idle, Vec::new()),
        },
        State::Recording => match event {
            Event::Enter => (
                State::Recognizing { raw_requested: false },
                vec![
                    Action::StopRecordingAndTranscribe,
                    Action::ShowOverlay(OverlayKind::Recognizing),
                ],
            ),
            Event::Esc => (
                State::Idle,
                vec![Action::DiscardSession, Action::HideOverlay],
            ),
            Event::AsrError { message } => (
                State::Idle,
                vec![
                    Action::DiscardSession,
                    Action::ShowOverlay(OverlayKind::Error { message }),
                ],
            ),
            _ => (State::Recording, Vec::new()),
        },
        State::Recognizing { raw_requested } => match event {
            Event::Enter => (State::Recognizing { raw_requested: true }, Vec::new()),
            Event::Esc => (
                State::Idle,
                vec![Action::DiscardSession, Action::HideOverlay],
            ),
            Event::AsrDone { text, has_api_key } => {
                if raw_requested {
                    (
                        State::Idle,
                        vec![Action::Paste { text }, Action::HideOverlay],
                    )
                } else if has_api_key {
                    (
                        State::Formatting { raw: text.clone() },
                        vec![
                            Action::CallFormat { raw: text },
                            Action::ShowOverlay(OverlayKind::Formatting),
                        ],
                    )
                } else {
                    (
                        State::Idle,
                        vec![
                            Action::Paste { text },
                            Action::ShowOverlay(OverlayKind::ApiKeyMissing),
                        ],
                    )
                }
            }
            Event::AsrError { message } => (
                State::Idle,
                vec![
                    Action::DiscardSession,
                    Action::ShowOverlay(OverlayKind::Error { message }),
                ],
            ),
            _ => (State::Recognizing { raw_requested }, Vec::new()),
        },
        State::Formatting { raw } => match event {
            Event::Enter => (
                State::Idle,
                vec![Action::Paste { text: raw }, Action::HideOverlay],
            ),
            Event::Esc => (State::Idle, vec![Action::HideOverlay]),
            Event::FormatDone { text } => (
                State::Idle,
                vec![Action::Paste { text }, Action::HideOverlay],
            ),
            Event::AsrError { message } => (
                State::Idle,
                vec![Action::ShowOverlay(OverlayKind::Error { message })],
            ),
            Event::FormatFailed { reason } => (
                State::Idle,
                vec![
                    Action::Paste { text: raw },
                    Action::ShowOverlay(OverlayKind::FormatSkipped { reason }),
                ],
            ),
            _ => (State::Formatting { raw }, Vec::new()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec() -> State {
        State::Recording
    }
    fn recog(raw: bool) -> State {
        State::Recognizing { raw_requested: raw }
    }
    fn fmt() -> State {
        State::Formatting { raw: "raw".to_string() }
    }

    #[test]
    fn idle_f8_starts_recording() {
        let (s, a) = step(State::Idle, Event::F8);
        assert_eq!(s, State::Recording);
        assert!(a.contains(&Action::StartSession));
    }

    #[test]
    fn idle_enter_esc_pass_through() {
        assert_eq!(step(State::Idle, Event::Enter), (State::Idle, vec![]));
        assert_eq!(step(State::Idle, Event::Esc), (State::Idle, vec![]));
    }

    #[test]
    fn idle_late_events_ignored() {
        assert_eq!(
            step(State::Idle, Event::FormatDone { text: "x".into() }),
            (State::Idle, vec![])
        );
        assert_eq!(
            step(State::Idle, Event::AsrDone { text: "x".into(), has_api_key: true }),
            (State::Idle, vec![])
        );
    }

    #[test]
    fn recording_enter_goes_recognizing() {
        let (s, a) = step(rec(), Event::Enter);
        assert_eq!(s, recog(false));
        assert!(a.contains(&Action::StopRecordingAndTranscribe));
    }

    #[test]
    fn recording_esc_discards() {
        let (s, a) = step(rec(), Event::Esc);
        assert_eq!(s, State::Idle);
        assert!(a.contains(&Action::DiscardSession));
    }

    #[test]
    fn recording_f8_ignored() {
        assert_eq!(step(rec(), Event::F8), (rec(), vec![]));
    }

    #[test]
    fn recognizing_enter_sets_raw_flag() {
        assert_eq!(step(recog(false), Event::Enter), (recog(true), vec![]));
        assert_eq!(step(recog(true), Event::Enter), (recog(true), vec![]));
    }

    #[test]
    fn recognizing_esc_discards() {
        let (s, a) = step(recog(false), Event::Esc);
        assert_eq!(s, State::Idle);
        assert!(a.contains(&Action::DiscardSession));
    }

    #[test]
    fn recognizing_f8_ignored_keeps_flag() {
        assert_eq!(step(recog(true), Event::F8), (recog(true), vec![]));
        assert_eq!(step(recog(false), Event::F8), (recog(false), vec![]));
    }

    #[test]
    fn asr_done_with_raw_flag_pastes_raw() {
        let (s, a) = step(
            recog(true),
            Event::AsrDone { text: "hello".into(), has_api_key: true },
        );
        assert_eq!(s, State::Idle);
        assert_eq!(a[0], Action::Paste { text: "hello".into() });
    }

    #[test]
    fn asr_done_with_key_goes_formatting() {
        let (s, a) = step(
            recog(false),
            Event::AsrDone { text: "hello".into(), has_api_key: true },
        );
        assert_eq!(s, State::Formatting { raw: "hello".into() });
        assert!(a.contains(&Action::CallFormat { raw: "hello".into() }));
    }

    #[test]
    fn asr_done_without_key_pastes_raw_with_notice() {
        let (s, a) = step(
            recog(false),
            Event::AsrDone { text: "hello".into(), has_api_key: false },
        );
        assert_eq!(s, State::Idle);
        assert!(a.contains(&Action::Paste { text: "hello".into() }));
        assert!(a.contains(&Action::ShowOverlay(OverlayKind::ApiKeyMissing)));
    }

    #[test]
    fn asr_error_goes_idle_with_error_overlay() {
        let (s, a) = step(recog(false), Event::AsrError { message: "boom".into() });
        assert_eq!(s, State::Idle);
        assert!(a.contains(&Action::ShowOverlay(OverlayKind::Error { message: "boom".into() })));
    }

    #[test]
    fn formatting_enter_pastes_raw() {
        let (s, a) = step(fmt(), Event::Enter);
        assert_eq!(s, State::Idle);
        assert!(a.contains(&Action::Paste { text: "raw".into() }));
    }

    #[test]
    fn formatting_esc_discards() {
        let (s, a) = step(fmt(), Event::Esc);
        assert_eq!(s, State::Idle);
        assert!(!a.iter().any(|x| matches!(x, Action::Paste { .. })));
    }

    #[test]
    fn formatting_f8_ignored() {
        assert_eq!(step(fmt(), Event::F8), (fmt(), vec![]));
    }

    #[test]
    fn recording_asr_error_goes_idle() {
        let (s, a) = step(rec(), Event::AsrError { message: "dead".into() });
        assert_eq!(s, State::Idle);
        assert!(a.contains(&Action::ShowOverlay(OverlayKind::Error { message: "dead".into() })));
    }

    #[test]
    fn format_done_pastes_formatted() {
        let (s, a) = step(fmt(), Event::FormatDone { text: "clean".into() });
        assert_eq!(s, State::Idle);
        assert!(a.contains(&Action::Paste { text: "clean".into() }));
    }

    #[test]
    fn format_failed_pastes_raw_with_skip_notice() {
        let (s, a) = step(fmt(), Event::FormatFailed { reason: "timeout".into() });
        assert_eq!(s, State::Idle);
        assert!(a.contains(&Action::Paste { text: "raw".into() }));
        assert!(a.contains(&Action::ShowOverlay(OverlayKind::FormatSkipped {
            reason: "timeout".into()
        })));
    }
}
