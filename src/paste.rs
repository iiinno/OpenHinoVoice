//! Paste the final text at once, instead of simulating typing through the IME.
//! Retain the previous clipboard data object and restore it after Ctrl+V.

use std::ffi::c_void;
use std::marker::PhantomData;
use std::rc::Rc;
use std::thread;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::HWND;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Com::IDataObject;
use windows::Win32::System::Ole::{
    OleGetClipboard, OleInitialize, OleSetClipboard, OleUninitialize,
};
use windows::Win32::System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP,
    VIRTUAL_KEY, VK_CONTROL, VK_V,
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
    fn GetClipboardSequenceNumber() -> u32;
    fn GetClipboardOwner() -> *mut c_void;
    fn GetClipboardData(format: u32) -> *mut c_void;
}

#[link(name = "kernel32")]
extern "system" {
    fn GlobalAlloc(flags: u32, bytes: usize) -> *mut c_void;
    fn GlobalLock(mem: *mut c_void) -> *mut c_void;
    fn GlobalUnlock(mem: *mut c_void) -> i32;
    fn GlobalFree(mem: *mut c_void) -> *mut c_void;
    fn GlobalSize(mem: *mut c_void) -> usize;
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

impl OpenedClipboard {
    fn open(owner: HWND) -> Result<Self, String> {
        for _ in 0..5 {
            if unsafe { OpenClipboard(owner.0) } != 0 {
                return Ok(Self);
            }
            thread::sleep(Duration::from_millis(30));
        }
        Err("クリップボードが使用中のためコピーできません".into())
    }
}

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
    copy_text_to_clipboard_checked(text, owner, None, &mut 0)
}

fn copy_text_to_clipboard_checked(
    text: &str,
    owner: HWND,
    expected_sequence: Option<u32>,
    owned_sequence: &mut u32,
) -> Result<(), String> {
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

        let _clipboard = OpenedClipboard::open(owner)?;
        if expected_sequence.is_some_and(|sequence| sequence != GetClipboardSequenceNumber()) {
            return Err("クリップボードが他のアプリで更新されました".into());
        }
        if EmptyClipboard() == 0 {
            return Err("クリップボードを空にできません".into());
        }
        // Record only changes made while we own the clipboard lock. An error
        // before EmptyClipboard must never authorize restoring somebody else's update.
        *owned_sequence = GetClipboardSequenceNumber();
        if SetClipboardData(CF_UNICODETEXT, mem.0).is_null() {
            // Even when SetClipboardData fails, retain the failed text in App for another attempt.
            return Err("クリップボードへテキストを書き込めません".into());
        }
        *owned_sequence = GetClipboardSequenceNumber();
        // Windows now owns the moveable memory handle; never free it in this process.
        mem.0 = std::ptr::null_mut();
    }
    // Closing a text write can publish Windows-synthesized formats and advance
    // the sequence. Reopen read-only and prove ownership before accepting it.
    *owned_sequence = 0;
    *owned_sequence = published_clipboard_sequence(owner, &utf16)?;
    Ok(())
}

fn published_clipboard_sequence(owner: HWND, expected: &[u16]) -> Result<u32, String> {
    let _clipboard = OpenedClipboard::open(owner)?;
    unsafe {
        if GetClipboardOwner() != owner.0 {
            return Err("クリップボードが他のアプリで更新されました".into());
        }
        let mem = GetClipboardData(CF_UNICODETEXT);
        if mem.is_null() || GlobalSize(mem) / std::mem::size_of::<u16>() < expected.len() {
            return Err("クリップボードのテキストが変更されました".into());
        }
        let ptr = GlobalLock(mem) as *const u16;
        if ptr.is_null() {
            return Err("クリップボードのテキストを確認できません".into());
        }
        let matches = std::slice::from_raw_parts(ptr, expected.len()) == expected;
        GlobalUnlock(mem);
        if !matches {
            return Err("クリップボードのテキストが変更されました".into());
        }
        Ok(GetClipboardSequenceNumber())
    }
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
        key(VK_CONTROL, false),
        key(VK_V, false),
        key(VK_V, true),
        key(VK_CONTROL, true),
    ]
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

struct OleScope;
impl Drop for OleScope {
    fn drop(&mut self) {
        unsafe { OleUninitialize() }
    }
}

static PASTE_ACTIVE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
struct ProcessLease;
impl ProcessLease {
    fn acquire() -> Result<Self, String> {
        if PASTE_ACTIVE.swap(true, std::sync::atomic::Ordering::SeqCst) {
            Err("別の貼り付けが進行中です".into())
        } else {
            Ok(Self)
        }
    }
}
impl Drop for ProcessLease {
    fn drop(&mut self) {
        PASTE_ACTIVE.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}
struct PasteLease {
    handle: HANDLE,
    _process: ProcessLease,
}
impl PasteLease {
    fn acquire() -> Result<Self, String> {
        let process = ProcessLease::acquire()?;
        let name =
            windows::core::HSTRING::from(crate::config::mutex_name("Local\\OpenHinoVoice.Paste"));
        let handle = unsafe { CreateMutexW(None, false, &name) }
            .map_err(|e| format!("貼り付けの排他を作れません: {e}"))?;
        let result = unsafe { WaitForSingleObject(handle, 0) };
        if result.0 == 0 || result.0 == 0x80 {
            return Ok(Self {
                handle,
                _process: process,
            });
        }
        let _ = unsafe { CloseHandle(handle) };
        Err("別の貼り付けが進行中です。少し待ってから再入力してください".into())
    }
}
impl Drop for PasteLease {
    fn drop(&mut self) {
        unsafe {
            let _ = ReleaseMutex(self.handle);
            let _ = CloseHandle(self.handle);
        }
    }
}

fn should_restore(before: u32, ours: u32, current: u32) -> bool {
    ours != 0 && ours != before && ours == current
}

/// 同じ STA が旧データを保持し、期限後に復元する。別スレッドへは移せない。
pub struct PasteTransaction {
    previous: Option<IDataObject>,
    before: u32,
    our_sequence: u32,
    deadline: Instant,
    _ole: OleScope,
    _lease: PasteLease,
    _same_thread: PhantomData<Rc<()>>,
}

impl PasteTransaction {
    pub fn begin(
        text: &str,
        target: PasteTarget,
        owner: HWND,
        now: Instant,
    ) -> Result<Self, String> {
        if text.is_empty() {
            return Err("入力テキストが空です".into());
        }
        if text.contains('\0') {
            return Err("入力文字列に NUL が含まれています".into());
        }
        check_target(target)?;
        if !no_modifiers_pressed() {
            return Err("修飾キーが押されているため、文字入力を中止しました".into());
        }
        let lease = PasteLease::acquire()?;
        unsafe { OleInitialize(None) }
            .map_err(|e| format!("クリップボードの初期化に失敗しました: {e}"))?;
        let ole = OleScope;
        let before = unsafe { GetClipboardSequenceNumber() };
        let previous = unsafe { OleGetClipboard() }
            .map_err(|e| format!("既存のクリップボードを保持できません: {e}"))?;
        if before != unsafe { GetClipboardSequenceNumber() } {
            return Err("既存のクリップボードの取得中に内容が更新されました".into());
        }
        check_target(target)?;
        if !no_modifiers_pressed() {
            return Err("修飾キーが押されているため、貼り付けを中止しました".into());
        }
        if before != unsafe { GetClipboardSequenceNumber() } {
            return Err("クリップボードが他のアプリで更新されました".into());
        }
        let mut our_sequence = 0;
        let result = copy_text_to_clipboard_checked(text, owner, Some(before), &mut our_sequence);
        let mut transaction = Self {
            previous: Some(previous),
            before,
            our_sequence,
            deadline: now,
            _ole: ole,
            _lease: lease,
            _same_thread: PhantomData,
        };
        result?;
        check_target(target)?;
        if !no_modifiers_pressed() {
            return Err("修飾キーが押されているため、貼り付けを中止しました".into());
        }
        if our_sequence != 0 && our_sequence != unsafe { GetClipboardSequenceNumber() } {
            return Err("貼り付け前にクリップボードが別のアプリで変更されました".into());
        }
        let keys = paste_shortcut();
        let inserted = unsafe { SendInput(&keys, std::mem::size_of::<INPUT>() as i32) };
        if inserted != keys.len() as u32 {
            let _ = unsafe { SendInput(&keys[3..], std::mem::size_of::<INPUT>() as i32) };
        }
        validate_injected_count(keys.len(), inserted)?;
        // 転送前の COM 待ちが長くても、送信後の猶予は必ず確保する。
        transaction.deadline = Instant::now() + Duration::from_millis(250);
        Ok(transaction)
    }

    pub fn deadline(&self) -> Instant {
        self.deadline
    }

    pub fn restore(mut self) -> Result<(), String> {
        self.restore_inner()
    }

    fn restore_inner(&mut self) -> Result<(), String> {
        let Some(previous) = self.previous.take() else {
            return Ok(());
        };
        if should_restore(self.before, self.our_sequence, unsafe {
            GetClipboardSequenceNumber()
        }) {
            unsafe { OleSetClipboard(&previous) }.map_err(|e| {
                format!("入力は送信済みですが、元のクリップボードを復元できませんでした: {e}")
            })?;
        }
        Ok(())
    }
}

impl Drop for PasteTransaction {
    fn drop(&mut self) {
        if let Err(error) = self.restore_inner() {
            eprintln!("{error}");
        }
    }
}

/// 実機試験など、同期で呼ぶ既存の入口。本番 UI は期限を待たずにループへ戻る。
pub fn paste_text(text: &str, target: PasteTarget, owner: HWND) -> Result<(), String> {
    if text.is_empty() {
        return Ok(());
    }
    let transaction = PasteTransaction::begin(text, target, owner, Instant::now())?;
    thread::sleep(
        transaction
            .deadline()
            .saturating_duration_since(Instant::now()),
    );
    if let Err(error) = transaction.restore() {
        eprintln!("{error}");
    }
    Ok(())
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
    #[ignore = "uses the real clipboard; run explicitly on a GitHub-hosted Windows runner"]
    fn clipboard_write_tracks_the_published_sequence() {
        use windows::Win32::Foundation::HINSTANCE;
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DestroyWindow, WS_EX_TOOLWINDOW, WS_POPUP,
        };

        assert_eq!(std::env::var("GITHUB_ACTIONS").as_deref(), Ok("true"));
        assert_eq!(
            std::env::var("RUNNER_ENVIRONMENT").as_deref(),
            Ok("github-hosted")
        );
        struct Owner(HWND);
        impl Drop for Owner {
            fn drop(&mut self) {
                unsafe {
                    let _ = DestroyWindow(self.0);
                }
            }
        }
        fn make_owner() -> Owner {
            Owner(unsafe {
                CreateWindowExW(
                    WS_EX_TOOLWINDOW,
                    windows::core::w!("STATIC"),
                    windows::core::w!("OpenHinoVoice clipboard regression"),
                    WS_POPUP,
                    0,
                    0,
                    1,
                    1,
                    None,
                    None,
                    HINSTANCE::default(),
                    None,
                )
                .expect("create hidden clipboard owner")
            })
        }
        let owner = make_owner();
        copy_text_to_clipboard("CI previous clipboard", owner.0).unwrap();
        let before = unsafe { GetClipboardSequenceNumber() };
        let mut owned_sequence = 0;
        copy_text_to_clipboard_checked(
            "録音開始から Enter で貼り付け 😀",
            owner.0,
            Some(before),
            &mut owned_sequence,
        )
        .expect("publish dictation text");
        let published_sequence = unsafe { GetClipboardSequenceNumber() };
        eprintln!(
            "clipboard sequence: before={before}, recorded={owned_sequence}, after_close={published_sequence}"
        );
        assert_ne!(published_sequence, before);
        assert_eq!(
            owned_sequence, published_sequence,
            "closing our own clipboard write must not look like another application's change"
        );

        let expected = clipboard_utf16("録音開始から Enter で貼り付け 😀");
        let foreign_owner = make_owner();
        copy_text_to_clipboard("録音開始から Enter で貼り付け 😀", foreign_owner.0).unwrap();
        let foreign_sequence = unsafe { GetClipboardSequenceNumber() };
        assert!(published_clipboard_sequence(owner.0, &expected).is_err());
        assert_eq!(foreign_sequence, unsafe { GetClipboardSequenceNumber() });

        copy_text_to_clipboard("newer copy from our owner", owner.0).unwrap();
        let newer_sequence = unsafe { GetClipboardSequenceNumber() };
        assert!(published_clipboard_sequence(owner.0, &expected).is_err());
        assert_eq!(newer_sequence, unsafe { GetClipboardSequenceNumber() });
    }

    #[test]
    fn a_second_transaction_is_rejected_even_on_the_same_thread() {
        let first = ProcessLease::acquire().unwrap();
        assert!(ProcessLease::acquire().is_err());
        drop(first);
        assert!(ProcessLease::acquire().is_ok());
    }

    #[test]
    fn restore_never_replaces_a_newer_clipboard_change() {
        assert!(should_restore(10, 11, 11));
        assert!(!should_restore(10, 11, 12));
        assert!(!should_restore(10, 10, 10));
        assert!(!should_restore(0, 0, 0));
        assert!(
            !should_restore(10, 0, 11),
            "書込み前の拒否では他アプリの内容を戻さない"
        );
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
        assert_eq!(vkeys, vec![VK_CONTROL, VK_V, VK_V, VK_CONTROL]);
        assert_eq!(keys.len(), 4);
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
