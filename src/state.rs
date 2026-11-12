#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    Idle,
    Recording,
    Recognizing {
        raw_requested: bool,
    },
    Formatting {
        raw: String,
    },
    /// Edit モード。選んだ文を書き換える指示を録音している
    Editing {
        selection: String,
    },
    /// Edit モードの認識。2 回目の Enter(生テキストで貼る)は無効
    EditRecognizing {
        selection: String,
    },
    /// Edit モードの整形。失敗しても指示の文は貼らない
    EditFormatting {
        selection: String,
        instruction: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    F8,
    Enter,
    Esc,
    /// 録音の開始時に選んだ文があった。Recording から Edit に入る
    EditModeStarted {
        selection: String,
    },
    /// Edit を使えない理由。Recording のまま注意を表示する
    EditNotice {
        message: String,
    },
    AsrDone {
        text: String,
        has_api_key: bool,
    },
    AsrError {
        message: String,
    },
    FormatDone {
        text: String,
    },
    FormatFailed {
        reason: String,
    },
    EditFormatDone {
        text: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverlayKind {
    Recording,
    Recognizing,
    Formatting,
    FormatSkipped {
        reason: String,
    },
    ApiKeyMissing,
    Error {
        message: String,
    },
    /// Edit の録音中。「編集」と選んだ文の先頭を出す
    Editing {
        head: String,
    },
    /// Edit を使えないことと、その理由
    EditUnavailable {
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    StartSession,
    StopRecordingAndTranscribe,
    DiscardSession,
    CallFormat {
        raw: String,
    },
    CallEditFormat {
        selection: String,
        instruction: String,
    },
    Paste {
        text: String,
    },
    ShowOverlay(OverlayKind),
    HideOverlay,
}

/// Stop the current transition when an action fails. In particular, a failed Paste must not
/// be followed by HideOverlay or a notice claiming that text was pasted successfully.
pub fn run_actions(actions: Vec<Action>, mut execute: impl FnMut(Action) -> bool) {
    for action in actions {
        if !execute(action) {
            break;
        }
    }
}

pub fn step(state: State, event: Event) -> (State, Vec<Action>) {
    match state {
        State::Idle => match event {
            Event::F8 => (
                State::Recording,
                vec![
                    Action::StartSession,
                    Action::ShowOverlay(OverlayKind::Recording),
                ],
            ),
            _ => (State::Idle, Vec::new()),
        },
        State::Recording => match event {
            Event::EditModeStarted { selection } => {
                let head = selection.chars().take(24).collect();
                (
                    State::Editing { selection },
                    vec![Action::ShowOverlay(OverlayKind::Editing { head })],
                )
            }
            Event::EditNotice { message } => (
                State::Recording,
                vec![Action::ShowOverlay(OverlayKind::EditUnavailable {
                    message,
                })],
            ),
            Event::Enter => (
                State::Recognizing {
                    raw_requested: false,
                },
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
            Event::Enter => (
                State::Recognizing {
                    raw_requested: true,
                },
                Vec::new(),
            ),
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
        State::Editing { selection } => match event {
            Event::Enter => (
                State::EditRecognizing { selection },
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
            _ => (State::Editing { selection }, Vec::new()),
        },
        State::EditRecognizing { selection } => match event {
            // 指示の文をそのまま貼ってしまうので、2 回目の Enter は無効にする
            Event::Enter | Event::F8 => (State::EditRecognizing { selection }, Vec::new()),
            Event::Esc => (
                State::Idle,
                vec![Action::DiscardSession, Action::HideOverlay],
            ),
            Event::AsrDone { text, .. } => (
                State::EditFormatting {
                    selection: selection.clone(),
                    instruction: text.clone(),
                },
                vec![Action::CallEditFormat {
                    selection,
                    instruction: text,
                }],
            ),
            Event::AsrError { message } => (
                State::Idle,
                vec![
                    Action::DiscardSession,
                    Action::ShowOverlay(OverlayKind::Error { message }),
                ],
            ),
            _ => (State::EditRecognizing { selection }, Vec::new()),
        },
        State::EditFormatting {
            selection,
            instruction,
        } => match event {
            // 指示の文をそのまま貼ってしまうので、Enter は無効にする
            Event::Enter | Event::F8 => (
                State::EditFormatting {
                    selection,
                    instruction,
                },
                Vec::new(),
            ),
            Event::Esc => (State::Idle, vec![Action::HideOverlay]),
            Event::EditFormatDone { text } => (
                State::Idle,
                vec![Action::Paste { text }, Action::HideOverlay],
            ),
            Event::FormatFailed { reason } => (
                State::Idle,
                vec![Action::ShowOverlay(OverlayKind::EditUnavailable {
                    message: format!("Edit を行えませんでした（{reason}）"),
                })],
            ),
            Event::AsrError { message } => (
                State::Idle,
                vec![Action::ShowOverlay(OverlayKind::Error { message })],
            ),
            _ => (
                State::EditFormatting {
                    selection,
                    instruction,
                },
                Vec::new(),
            ),
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
        State::Formatting {
            raw: "raw".to_string(),
        }
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
            step(
                State::Idle,
                Event::AsrDone {
                    text: "x".into(),
                    has_api_key: true
                }
            ),
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
            Event::AsrDone {
                text: "hello".into(),
                has_api_key: true,
            },
        );
        assert_eq!(s, State::Idle);
        assert_eq!(
            a[0],
            Action::Paste {
                text: "hello".into()
            }
        );
    }

    #[test]
    fn asr_done_with_key_goes_formatting() {
        let (s, a) = step(
            recog(false),
            Event::AsrDone {
                text: "hello".into(),
                has_api_key: true,
            },
        );
        assert_eq!(
            s,
            State::Formatting {
                raw: "hello".into()
            }
        );
        assert!(a.contains(&Action::CallFormat {
            raw: "hello".into()
        }));
    }

    #[test]
    fn asr_done_without_key_pastes_raw_with_notice() {
        let (s, a) = step(
            recog(false),
            Event::AsrDone {
                text: "hello".into(),
                has_api_key: false,
            },
        );
        assert_eq!(s, State::Idle);
        assert!(a.contains(&Action::Paste {
            text: "hello".into()
        }));
        assert!(a.contains(&Action::ShowOverlay(OverlayKind::ApiKeyMissing)));
    }

    #[test]
    fn asr_error_goes_idle_with_error_overlay() {
        let (s, a) = step(
            recog(false),
            Event::AsrError {
                message: "boom".into(),
            },
        );
        assert_eq!(s, State::Idle);
        assert!(a.contains(&Action::ShowOverlay(OverlayKind::Error {
            message: "boom".into()
        })));
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
        let (s, a) = step(
            rec(),
            Event::AsrError {
                message: "dead".into(),
            },
        );
        assert_eq!(s, State::Idle);
        assert!(a.contains(&Action::ShowOverlay(OverlayKind::Error {
            message: "dead".into()
        })));
    }

    #[test]
    fn format_done_pastes_formatted() {
        let (s, a) = step(
            fmt(),
            Event::FormatDone {
                text: "clean".into(),
            },
        );
        assert_eq!(s, State::Idle);
        assert!(a.contains(&Action::Paste {
            text: "clean".into()
        }));
    }

    #[test]
    fn format_failed_pastes_raw_with_skip_notice() {
        let (s, a) = step(
            fmt(),
            Event::FormatFailed {
                reason: "timeout".into(),
            },
        );
        assert_eq!(s, State::Idle);
        assert!(a.contains(&Action::Paste { text: "raw".into() }));
        assert!(a.contains(&Action::ShowOverlay(OverlayKind::FormatSkipped {
            reason: "timeout".into()
        })));
    }

    // ---- Edit モードの遷移(先にテストを書く) ----

    fn edit() -> State {
        State::Editing {
            selection: "選んだ文".to_string(),
        }
    }
    fn edit_recog() -> State {
        State::EditRecognizing {
            selection: "選んだ文".to_string(),
        }
    }
    fn edit_fmt() -> State {
        State::EditFormatting {
            selection: "選んだ文".to_string(),
            instruction: "丁寧にして".to_string(),
        }
    }

    #[test]
    fn edit_mode_starts_from_recording_and_shows_the_head() {
        let (s, a) = step(
            rec(),
            Event::EditModeStarted {
                selection: "選んだ文".into(),
            },
        );
        assert_eq!(
            s,
            State::Editing {
                selection: "選んだ文".into()
            }
        );
        assert!(a.contains(&Action::ShowOverlay(OverlayKind::Editing {
            head: "選んだ文".into()
        })));
    }

    #[test]
    fn edit_recording_enter_goes_to_edit_recognizing() {
        let (s, a) = step(edit(), Event::Enter);
        assert_eq!(s, edit_recog());
        assert!(a.contains(&Action::StopRecordingAndTranscribe));
    }

    #[test]
    fn edit_recognizing_ignores_the_second_enter() {
        // 指示の文をそのまま貼ってしまうので、2 回目の Enter は無効にする
        assert_eq!(step(edit_recog(), Event::Enter), (edit_recog(), vec![]));
    }

    #[test]
    fn edit_recognizing_done_goes_to_edit_formatting() {
        let (s, a) = step(
            edit_recog(),
            Event::AsrDone {
                text: "丁寧にして".into(),
                has_api_key: true,
            },
        );
        assert_eq!(s, edit_fmt());
        assert!(a.contains(&Action::CallEditFormat {
            selection: "選んだ文".into(),
            instruction: "丁寧にして".into(),
        }));
    }

    #[test]
    fn edit_recognizing_done_updates_the_formatting_status() {
        let (next, actions) = step(
            edit_recog(),
            Event::AsrDone {
                text: "丁寧にして".into(),
                has_api_key: true,
            },
        );
        assert_eq!(next, edit_fmt());
        assert_eq!(
            actions,
            vec![
                Action::CallEditFormat {
                    selection: "選んだ文".into(),
                    instruction: "丁寧にして".into(),
                },
                Action::ShowOverlay(OverlayKind::Formatting),
            ]
        );
    }

    #[test]
    fn edit_format_done_replaces_the_selection() {
        let (s, a) = step(
            edit_fmt(),
            Event::EditFormatDone {
                text: "丁寧な文".into(),
            },
        );
        assert_eq!(s, State::Idle);
        assert!(a.contains(&Action::Paste {
            text: "丁寧な文".into()
        }));
    }

    #[test]
    fn edit_format_failure_does_not_paste_anything() {
        // 指示の文や失敗した応答を選択部分に貼ってはいけない
        let (s, a) = step(
            edit_fmt(),
            Event::FormatFailed {
                reason: "timeout".into(),
            },
        );
        assert_eq!(s, State::Idle);
        assert!(!a.iter().any(|x| matches!(x, Action::Paste { .. })));
        assert!(a
            .iter()
            .any(|x| matches!(x, Action::ShowOverlay(OverlayKind::EditUnavailable { .. }))));
    }

    #[test]
    fn edit_formatting_enter_and_second_enter_do_not_paste_the_instruction() {
        assert_eq!(
            step(edit_fmt(), Event::Enter),
            (
                State::EditFormatting {
                    selection: "選んだ文".into(),
                    instruction: "丁寧にして".into(),
                },
                vec![]
            )
        );
    }

    #[test]
    fn edit_states_escape_to_idle_and_discard() {
        for state in [edit(), edit_recog()] {
            let (s, a) = step(state.clone(), Event::Esc);
            assert_eq!(s, State::Idle);
            assert!(a.contains(&Action::DiscardSession));
        }
        // 整形の途中の Esc は貼り付けも破棄もしない。整形中の通常の Esc と同じ
        let (s, a) = step(edit_fmt(), Event::Esc);
        assert_eq!(s, State::Idle);
        assert!(!a.iter().any(|x| matches!(x, Action::Paste { .. })));
        assert!(!a.iter().any(|x| matches!(x, Action::DiscardSession)));
    }

    #[test]
    fn edit_recognizing_asr_error_discards() {
        let (s, a) = step(
            edit_recog(),
            Event::AsrError {
                message: "boom".into(),
            },
        );
        assert_eq!(s, State::Idle);
        assert!(a.contains(&Action::DiscardSession));
    }

    #[test]
    fn editing_ignores_late_edit_mode_started_and_f8() {
        assert_eq!(step(edit(), Event::F8), (edit(), vec![]));
        assert_eq!(
            step(
                edit(),
                Event::EditModeStarted {
                    selection: "別の文".into()
                }
            ),
            (edit(), vec![])
        );
    }

    #[test]
    fn edit_notice_keeps_recording_and_shows_the_reason() {
        // TextPattern を読めないアプリでは、Edit を使えないことを出して通常録音
        let (s, a) = step(
            rec(),
            Event::EditNotice {
                message: "このアプリでは Edit を使えません".into(),
            },
        );
        assert_eq!(s, State::Recording);
        assert!(
            a.contains(&Action::ShowOverlay(OverlayKind::EditUnavailable {
                message: "このアプリでは Edit を使えません".into(),
            }))
        );
    }

    #[test]
    fn failed_paste_stops_following_success_notices_and_hides() {
        let cases = [
            (
                recog(true),
                Event::AsrDone {
                    text: "raw".into(),
                    has_api_key: true,
                },
            ),
            (
                recog(false),
                Event::AsrDone {
                    text: "raw".into(),
                    has_api_key: false,
                },
            ),
            (fmt(), Event::Enter),
            (
                fmt(),
                Event::FormatDone {
                    text: "clean".into(),
                },
            ),
            (
                fmt(),
                Event::FormatFailed {
                    reason: "timeout".into(),
                },
            ),
        ];
        for (state, event) in cases {
            let (_, actions) = step(state, event);
            let mut executed = Vec::new();
            run_actions(actions, |action| {
                let success = !matches!(action, Action::Paste { .. });
                executed.push(action);
                success
            });
            assert_eq!(executed.len(), 1, "a failed Paste must stop its transition");
            assert!(matches!(executed[0], Action::Paste { .. }));
        }
    }

    #[test]
    fn successful_paste_retains_normal_notice_and_hide_behavior() {
        let cases = [
            (
                recog(true),
                Event::AsrDone {
                    text: "raw".into(),
                    has_api_key: true,
                },
            ),
            (
                recog(false),
                Event::AsrDone {
                    text: "raw".into(),
                    has_api_key: false,
                },
            ),
            (
                fmt(),
                Event::FormatFailed {
                    reason: "timeout".into(),
                },
            ),
        ];
        for (state, event) in cases {
            let (_, actions) = step(state, event);
            let mut executed = Vec::new();
            run_actions(actions.clone(), |action| {
                executed.push(action);
                true
            });
            assert_eq!(executed, actions);
        }
    }
}
