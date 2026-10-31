//! Send Unicode text to the original focused control without touching the clipboard.
//!
//! Clipboard contents may be delayed-rendered OLE data, images, file lists, HTML, or private
//! formats that cannot be safely duplicated. Replacing even CF_UNICODETEXT and restoring it
//! later can destroy that data or overwrite another application's clipboard update.

use std::ffi::c_void;
use std::thread;
use std::time::Duration;
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP,
    KEYEVENTF_UNICODE, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetGUIThreadInfo, GetWindowThreadProcessId, GUITHREADINFO,
};

const CF_UNICODETEXT: u32 = 13;
const GMEM_MOVEABLE: u32 = 0x0002;

#[link(name = "user32")]
extern "system" {
    fn OpenClipboard(hwnd: *mut c_void) -> i32;
    fn CloseClipboard() -> i32;
    fn EmptyClipboard() -> i32;
    fn SetClipboardData(format: u32, mem: *mut c_void) -> *mut c_void;
}

#[link(name = "kernel32")]
extern "system" {
    fn GlobalAlloc(flags: u32, bytes: usize) -> *mut c_void;
    fn GlobalLock(mem: *mut c_void) -> *mut c_void;
    fn GlobalUnlock(mem: *mut c_void) -> i32;
    fn GlobalFree(mem: *mut c_void) -> *mut c_void;
}

struct AllocatedClipboardMemory(*mut c_void);

impl Drop for AllocatedClipboardMemory {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                GlobalFree(self.0);
            }
        }
    }
}

struct OpenedClipboard;

impl Drop for OpenedClipboard {
    fn drop(&mut self) {
        unsafe {
            CloseClipboard();
        }
    }
}

fn clipboard_utf16(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Called ONLY after the user explicitly selects the recovery action from the tray.
/// Allocate all memory before EmptyClipboard so allocation failures preserve existing content.
pub fn copy_text_to_clipboard(text: &str, owner: HWND) -> Result<(), String> {
    if owner.0.is_null() {
        return Err("クリップボードの所有者ウィンドウがありません".into());
    }
    let utf16 = clipboard_utf16(text);
    let bytes = utf16
        .len()
        .checked_mul(std::mem::size_of::<u16>())
        .ok_or("クリップボード用テキストが大きすぎます")?;
    let mut mem = AllocatedClipboardMemory(unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes) });
    if mem.0.is_null() {
        return Err("クリップボード用メモリを確保できません".into());
    }
    unsafe {
        let ptr = GlobalLock(mem.0) as *mut u16;
        if ptr.is_null() {
            return Err("クリップボード用メモリをロックできません".into());
        }
        std::ptr::copy_nonoverlapping(utf16.as_ptr(), ptr, utf16.len());
        GlobalUnlock(mem.0);

        let mut opened = false;
        for _ in 0..5 {
            if OpenClipboard(owner.0) != 0 {
                opened = true;
                break;
            }
            thread::sleep(Duration::from_millis(30));
        }
        if !opened {
            return Err("クリップボードが使用中のためコピーできません".into());
        }
        let _clipboard = OpenedClipboard;
        if EmptyClipboard() == 0 {
            return Err("クリップボードを空にできません".into());
        }
        if SetClipboardData(CF_UNICODETEXT, mem.0).is_null() {
            // Even when SetClipboardData fails, retain the failed text in App for another attempt.
            return Err("クリップボードへテキストを書き込めません".into());
        }
        // Windows now owns the moveable memory handle; never free it in this process.
        mem.0 = std::ptr::null_mut();
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PasteTarget {
    foreground: isize,
    focused: isize,
    thread_id: u32,
    process_id: u32,
}

fn active_target() -> Result<PasteTarget, String> {
    unsafe {
        let foreground = GetForegroundWindow();
        if foreground.0.is_null() {
            return Err("入力先ウィンドウを取得できません".into());
        }
        let mut process_id = 0;
        let thread_id = GetWindowThreadProcessId(foreground, Some(&mut process_id));
        if thread_id == 0 || process_id == 0 {
            return Err("入力先ウィンドウを識別できません".into());
        }
        let mut info = GUITHREADINFO {
            cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
            ..Default::default()
        };
        GetGUIThreadInfo(thread_id, &mut info)
            .map_err(|_| "入力先のフォーカスを取得できません".to_string())?;
        if info.hwndFocus.0.is_null() {
            return Err("入力先にフォーカスされたコントロールがありません".into());
        }
        Ok(PasteTarget {
            foreground: foreground.0 as isize,
            focused: info.hwndFocus.0 as isize,
            thread_id,
            process_id,
        })
    }
}

/// Record the original text destination before showing the recording overlay.
pub fn capture_target() -> Result<PasteTarget, String> {
    active_target()
}

fn unchanged_target(expected: PasteTarget, current: PasteTarget) -> bool {
    expected == current
}

fn check_target(expected: PasteTarget) -> Result<(), String> {
    let current = active_target()?;
    if !unchanged_target(expected, current) {
        return Err(
            "録音開始時と入力先またはフォーカスが変わったため、貼り付けを中止しました".into(),
        );
    }
    Ok(())
}

fn no_modifiers_pressed() -> bool {
    // Modifiers could turn input into a shortcut, particularly if the user is still holding
    // the start hotkey. Never synthesize text while Ctrl/Alt/Shift/Win are depressed.
    [0x10, 0x11, 0x12, 0x5B, 0x5C]
        .iter()
        .all(|&key| unsafe { GetAsyncKeyState(key) as u16 & 0x8000 == 0 })
}

fn unicode_inputs(text: &str) -> Result<Vec<INPUT>, String> {
    if text.contains('\0') {
        return Err("入力文字列に NUL 文字が含まれています".into());
    }
    let units: Vec<u16> = text.encode_utf16().collect();
    let mut inputs = Vec::with_capacity(units.len().saturating_mul(2));
    for unit in units {
        for flags in [KEYEVENTF_UNICODE, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP] {
            inputs.push(INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: VIRTUAL_KEY(0),
                        wScan: unit,
                        dwFlags: flags,
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            });
        }
    }
    Ok(inputs)
}

fn validate_injected_count(expected: usize, inserted: u32) -> Result<(), String> {
    if inserted as usize == expected {
        Ok(())
    } else {
        Err(format!(
            "文字入力に失敗しました（{inserted}/{expected} イベント）。一部だけ入力された可能性があります"
        ))
    }
}

/// Deliver text without modifying any clipboard format or racing another clipboard owner.
/// A control that accepts only Ctrl+V and not Unicode keyboard input will not support this path.
pub fn paste_text(text: &str, target: PasteTarget) -> Result<(), String> {
    let inputs = unicode_inputs(text)?;
    if inputs.is_empty() {
        return Ok(());
    }
    if inputs.len() > u32::MAX as usize {
        return Err("入力文字列が長すぎます".into());
    }
    check_target(target)?;
    if !no_modifiers_pressed() {
        return Err("修飾キーが押されているため、文字入力を中止しました".into());
    }
    // A single SendInput inserts the sequence atomically into the keyboard input stream.
    // Partial delivery must be reported, never blindly retried (which duplicates text).
    let inserted = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
    if (inserted as usize) < inputs.len() && inserted % 2 != 0 {
        // If the last delivered event was key-down, try to release that VK_PACKET key.
        let _ = unsafe {
            SendInput(
                &inputs[inserted as usize..inserted as usize + 1],
                std::mem::size_of::<INPUT>() as i32,
            )
        };
    }
    validate_injected_count(inputs.len(), inserted)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> PasteTarget {
        PasteTarget {
            foreground: 100,
            focused: 101,
            thread_id: 102,
            process_id: 103,
        }
    }

    #[test]
    fn focus_and_window_changes_are_rejected() {
        let t = target();
        assert!(unchanged_target(t, t));
        for other in [
            PasteTarget {
                foreground: 200,
                ..t
            },
            PasteTarget { focused: 200, ..t },
            PasteTarget {
                thread_id: 200,
                ..t
            },
            PasteTarget {
                process_id: 200,
                ..t
            },
        ] {
            assert!(!unchanged_target(t, other));
        }
    }

    #[test]
    fn unicode_input_handles_japanese_and_surrogate_pairs() {
        let inputs = unicode_inputs("漢字😀").unwrap();
        assert_eq!(inputs.len(), 8);
        assert!(inputs.iter().all(|input| input.r#type == INPUT_KEYBOARD));
        let (pairs, remainder) = inputs.as_chunks::<2>();
        assert!(remainder.is_empty());
        let units: Vec<_> = pairs
            .iter()
            .map(|pair| unsafe { pair[0].Anonymous.ki.wScan })
            .collect();
        assert_eq!(units, "漢字😀".encode_utf16().collect::<Vec<_>>());
        for pair in pairs {
            assert_eq!(unsafe { pair[0].Anonymous.ki.dwFlags }, KEYEVENTF_UNICODE);
            assert_eq!(
                unsafe { pair[1].Anonymous.ki.dwFlags },
                KEYEVENTF_UNICODE | KEYEVENTF_KEYUP
            );
        }
    }

    #[test]
    fn empty_and_nul_are_handled_without_input() {
        assert!(unicode_inputs("").unwrap().is_empty());
        assert!(unicode_inputs("a\0b").is_err());
    }

    #[test]
    fn sendinput_partial_or_zero_is_not_mistaken_for_success() {
        assert!(validate_injected_count(8, 8).is_ok());
        assert!(validate_injected_count(8, 4).is_err());
        assert!(validate_injected_count(8, 0).is_err());
    }

    #[test]
    fn explicit_recovery_copy_prepares_exact_utf16_without_touching_clipboard() {
        assert_eq!(
            clipboard_utf16("復旧😀"),
            vec![0x5FA9, 0x65E7, 0xD83D, 0xDE00, 0]
        );
        assert_eq!(clipboard_utf16(""), vec![0]);
    }
}
