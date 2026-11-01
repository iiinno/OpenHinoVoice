// Integration crates compile full production modules but use only a subset of their APIs.
#![allow(dead_code)]

#[path = "../src/paste.rs"]
mod paste;

#[test]
#[ignore = "needs a focused text field in the interactive session; run explicitly"]
fn pastes_into_focused_app() {
    std::thread::sleep(std::time::Duration::from_secs(4));
    let target = paste::capture_target().expect("focused text field must exist");
    let owner = unsafe { windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow() };
    paste::paste_text("OpenHinoVoice paste test 123", target, owner).expect("paste must succeed");
    std::thread::sleep(std::time::Duration::from_millis(500));
}
