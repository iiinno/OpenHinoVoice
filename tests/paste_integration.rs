#[path = "../src/paste.rs"]
mod paste;

#[test]
#[ignore = "needs a focused text field in the interactive session; run explicitly"]
fn pastes_into_focused_app() {
    std::thread::sleep(std::time::Duration::from_secs(4));
    paste::paste_text("OpenHinoVoice paste test 123").expect("paste must succeed");
    std::thread::sleep(std::time::Duration::from_millis(500));
}
