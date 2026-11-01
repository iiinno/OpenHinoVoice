//! Paste the final text at once, instead of simulating typing through the IME.
//! Retain the previous clipboard data object and restore it after Ctrl+V.

use std::ffi::c_void;
use std::thread;
use std::time::Duration;
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Ole::{
    OleGetClipboard, OleInitialize, OleSetClipboard, OleUninitialize,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP,
    VIRTUAL_KEY, VK_LCONTROL, VK_V,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetClassNameW, GetForegroundWindow, GetGUIThreadInfo, GetWindowLongW, GetWindowThreadProcessId,
    SendMessageTimeoutW, GUITHREADINFO, GWL_STYLE, SMTO_ABORTIFHUNG, SMTO_BLOCK, WM_PASTE,
};

const CF_UNICODETEXT: u32 = 13;
const GMEM_MOVEABLE: u32 = 0x0002;

#[link(name = "user32")]
extern "system" {
    fn OpenClipboard(hwnd: *mut c_void) -> i32;
    fn CloseClipboard() -> i32;
    fn EmptyClipboard() -> i32;
    fn SetClipboardData(format: u32, mem: *mut c_void) -> *mut c_void;
    fn GetClipboardSequenceNumber() -> u32;
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

impl PasteTarget {
    pub fn process_id(&self) -> u32 {
        self.process_id
    }
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

fn paste_shortcut() -> [INPUT; 4] {
    let key = |key: VIRTUAL_KEY, up| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: key,
                dwFlags: if up {
                    KEYEVENTF_KEYUP
                } else {
                    Default::default()
                },
                ..Default::default()
            },
        },
    };
    [
        // Use an explicit left Ctrl instead of the generic Ctrl virtual key.
        // Keyboard remappers can treat the two differently.
        key(VK_LCONTROL, false),
        key(VK_V, false),
        key(VK_V, true),
        key(VK_LCONTROL, true),
    ]
}

/// Native Windows text fields support a paste *command* rather than a keyboard shortcut.
/// Restrict this to known edit classes: arbitrary apps may ignore WM_PASTE or interpret it
/// differently. Never send both WM_PASTE and Ctrl+V to the same target (duplicate insertion).
fn supports_native_paste_class(class: &str) -> bool {
    matches!(
        class.to_ascii_lowercase().as_str(),
        "edit" | "richedit20a" | "richedit20w" | "richedit50w"
    )
}

fn is_native_edit_target(target: PasteTarget) -> bool {
    let window = HWND(target.focused as *mut c_void);
    let mut class = [0u16; 128];
    let len = unsafe { GetClassNameW(window, &mut class) };
    len > 0 && supports_native_paste_class(&String::from_utf16_lossy(&class[..len as usize]))
}

fn send_paste_command(target: PasteTarget) -> Result<(), String> {
    if is_native_edit_target(target) {
        let window = HWND(target.focused as *mut c_void);
        // A disabled/read-only native control silently ignores WM_PASTE, so don't
        // claim the text was inserted and never attempt a second shortcut.
        const ES_READONLY: u32 = 0x0800;
        const WS_DISABLED: u32 = 0x0800_0000;
        let style = unsafe { GetWindowLongW(window, GWL_STYLE) } as u32;
        if style & (ES_READONLY | WS_DISABLED) != 0 {
            return Err("入力先の編集欄は無効か読み取り専用です。復旧テキストを保持します".into());
        }
        // WM_PASTE is synchronous for Win32 edit controls and avoids layout/remap handling.
        // A timeout is an ambiguous outcome, so never retry by injecting Ctrl+V.
        let delivered = unsafe {
            SendMessageTimeoutW(
                window,
                WM_PASTE,
                None,
                None,
                SMTO_ABORTIFHUNG | SMTO_BLOCK,
                500,
                None,
            )
        };
        if delivered.0 == 0 {
            return Err("入力先の応答を確認できませんでした。貼り付いた可能性もあるため再送せず、復旧テキストを保持します".into());
        }
        return Ok(());
    }

    // Browsers, Electron apps and custom text fields do not have a universal WM_PASTE.
    // VK_V means the logical V key, independent of US/JIS physical key placement.
    let keys = paste_shortcut();
    let inserted = unsafe { SendInput(&keys, std::mem::size_of::<INPUT>() as i32) };
    // Release the modifier even if the OS accepted only part of the sequence.
    if inserted != keys.len() as u32 {
        let _ = unsafe { SendInput(&keys[3..], std::mem::size_of::<INPUT>() as i32) };
    }
    validate_injected_count(keys.len(), inserted)?;
    // SendInput confirms event injection, NOT that the destination actually pasted.
    thread::sleep(Duration::from_millis(250));
    Ok(())
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

/// A small, one-time clipboard transaction. Ctrl+V is a shortcut, not character injection.
/// Preserve all formats through IDataObject, and never overwrite a newer external clipboard.
pub fn paste_text(text: &str, target: PasteTarget, owner: HWND) -> Result<(), String> {
    if text.is_empty() {
        return Ok(());
    }
    if text.contains('\0') {
        return Err("入力文字列に NUL が含まれています".into());
    }
    check_target(target)?;
    if !no_modifiers_pressed() {
        return Err("修飾キーが押されているため、文字入力を中止しました".into());
    }

    // This function runs on the window's UI thread (STA). OleInitialize may
    // fail if the thread was initialized as MTA; in that case do not touch the clipboard.
    unsafe { OleInitialize(None) }
        .map_err(|e| format!("クリップボードの初期化に失敗しました: {e}"))?;
    struct OleScope;
    impl Drop for OleScope {
        fn drop(&mut self) {
            unsafe { OleUninitialize() }
        }
    }
    let _ole = OleScope;
    let previous = unsafe { OleGetClipboard() }
        .map_err(|e| format!("既存のクリップボードを保持できません: {e}"))?;

    // Bail out rather than racing with another copy operation.
    let before = unsafe { GetClipboardSequenceNumber() };
    check_target(target)?;
    if !no_modifiers_pressed() {
        return Err("修飾キーが押されているため、貼り付けを中止しました".into());
    }
    if before != unsafe { GetClipboardSequenceNumber() } {
        return Err("クリップボードが他のアプリで更新されました".into());
    }
    let set_result = copy_text_to_clipboard(text, owner);
    let our_sequence = unsafe { GetClipboardSequenceNumber() };
    let result = set_result.and_then(|_| {
        check_target(target)?;
        if !no_modifiers_pressed() {
            return Err("修飾キーが押されているため、貼り付けを中止しました".into());
        }
        if our_sequence != 0 && our_sequence != unsafe { GetClipboardSequenceNumber() } {
            return Err("貼り付け前にクリップボードが別のアプリで変更されました".into());
        }
        send_paste_command(target)
    });
    // External copy during paste takes precedence; never overwrite it.
    if our_sequence != 0
        && our_sequence != before
        && our_sequence == unsafe { GetClipboardSequenceNumber() }
    {
        if let Err(e) = unsafe { OleSetClipboard(&previous) } {
            // Pasting might already have succeeded. Do not report it as a paste
            // failure and invite a duplicate manual recovery.
            eprintln!("元のクリップボードを復元できませんでした: {e}");
        }
    }
    result
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
        assert_eq!(t.process_id(), 103);
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
    fn paste_sends_shortcut_not_text_events() {
        let keys = paste_shortcut();
        let vkeys: Vec<_> = keys
            .iter()
            .map(|key| unsafe { key.Anonymous.ki.wVk })
            .collect();
        assert_eq!(vkeys, vec![VK_LCONTROL, VK_V, VK_V, VK_LCONTROL]);
        assert_eq!(keys.len(), 4);
    }

    #[test]
    fn native_paste_only_for_known_edit_controls() {
        for name in ["Edit", "EDIT", "RichEdit20A", "RichEdit20W", "RICHEDIT50W"] {
            assert!(supports_native_paste_class(name), "{name}");
        }
        for name in [
            "",
            "Chrome_RenderWidgetHostHWND",
            "Notepad",
            "Button",
            "CustomEdit",
        ] {
            assert!(!supports_native_paste_class(name), "{name}");
        }
    }

    #[test]
    fn empty_and_nul_are_handled_without_input() {
        assert!(clipboard_utf16("").len() == 1);
        assert_eq!(clipboard_utf16("a\0b").len(), 4);
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
