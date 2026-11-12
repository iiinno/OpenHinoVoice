use crate::config::{Hotkey, MOD_ALT, MOD_CTRL, MOD_SHIFT, MOD_WIN};
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Mutex, OnceLock};
use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP,
    VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetMessageW, PeekMessageW, PostThreadMessageW,
    SetWindowsHookExW, UnhookWindowsHookEx, HC_ACTION, KBDLLHOOKSTRUCT, MSG, PM_NOREMOVE,
    WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP, WM_QUIT, WM_SYSKEYDOWN, WM_SYSKEYUP,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyEvent {
    Hotkey,
    Enter,
    Esc,
}

pub const MIRROR_IDLE: u8 = 0;
pub const MIRROR_RECORDING: u8 = 1;
pub const MIRROR_RECOGNIZING: u8 = 2;
pub const MIRROR_FORMATTING: u8 = 3;

static SENDER: OnceLock<Sender<KeyEvent>> = OnceLock::new();
static RESIDENT_WINDOW: AtomicIsize = AtomicIsize::new(0);
static MIRROR: AtomicU8 = AtomicU8::new(MIRROR_IDLE);
static PENDING_KEYS: AtomicU32 = AtomicU32::new(0);
// 主キーと修飾キーは一つの値にして、設定更新の途中の組合せを読まない。
static HOTKEY: AtomicU32 = AtomicU32::new(0x77);
// 押下を横取りした主キーは、離したときも横取りしてアプリに片側だけ届かないようにする
static CAPTURED_HOTKEYS: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];
// A held Enter produces repeated WM_KEYDOWN messages. Only its first physical
// press may advance Recording -> Recognizing or request raw text.
static ENTER_HELD: AtomicBool = AtomicBool::new(false);
static ENTER_CAPTURED: AtomicBool = AtomicBool::new(false);
static ESC_HELD: AtomicBool = AtomicBool::new(false);
static ESC_CAPTURED: AtomicBool = AtomicBool::new(false);
static PUMP: Mutex<Option<(u32, std::thread::JoinHandle<()>)>> = Mutex::new(None);

/// キーの通知を待ちループに知らせる。待機中はスリープでなくメッセージ待ちで
/// 起きるため、通知にはウィンドウメッセージを使う
pub fn wake_resident() {
    use windows::Win32::UI::WindowsAndMessaging::PostMessageW;
    let hwnd = windows::Win32::Foundation::HWND(
        RESIDENT_WINDOW.load(Ordering::Acquire) as *mut std::ffi::c_void
    );
    if !hwnd.0.is_null() {
        unsafe {
            let _ = PostMessageW(hwnd, crate::overlay::WM_WAKE, WPARAM(0), LPARAM(0));
        }
    }
}

/// Returns (swallow, send_event). Also captures the matching key-up even if
/// the app state changes while the key is held.
fn capture_press(
    held: &AtomicBool,
    captured: &AtomicBool,
    is_down: bool,
    active: bool,
) -> (bool, bool) {
    if is_down {
        if held.swap(true, Ordering::SeqCst) {
            return (captured.load(Ordering::SeqCst), false);
        }
        captured.store(active, Ordering::SeqCst);
        (active, active)
    } else {
        held.store(false, Ordering::SeqCst);
        (captured.swap(false, Ordering::SeqCst), false)
    }
}

pub fn set_mirror(state: u8) {
    MIRROR.store(state, Ordering::SeqCst);
}

pub fn acknowledge_hotkey() {
    let _ = PENDING_KEYS.try_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
        Some(n.saturating_sub(1))
    });
}

pub(crate) fn preview_blocked() -> bool {
    mirror() != MIRROR_IDLE || PENDING_KEYS.load(Ordering::SeqCst) > 0
}

pub fn mirror() -> u8 {
    MIRROR.load(Ordering::SeqCst)
}

pub fn set_hotkey(hotkey: Hotkey) {
    HOTKEY.store(hotkey.vk | (u32::from(hotkey.mods) << 16), Ordering::SeqCst);
}

pub fn install(sender: Sender<KeyEvent>, hotkey: Hotkey) -> Result<(), String> {
    SENDER
        .set(sender)
        .map_err(|_| "キーフックは既に初期化されています".to_string())?;
    set_hotkey(hotkey);
    RESIDENT_WINDOW.store(crate::overlay::hwnd().0 as isize, Ordering::Release);
    let (ready, result) = channel();
    let thread = std::thread::Builder::new()
        .name("keyboard-hook".into())
        .spawn(move || unsafe {
            // 最初にメッセージキューを作り、終了通知を受け取れる状態で知らせる。
            let mut message = MSG::default();
            let _ = PeekMessageW(&mut message, None, 0, 0, PM_NOREMOVE);
            let id = windows::Win32::System::Threading::GetCurrentThreadId();
            match SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), None, 0) {
                Ok(hook) => {
                    if ready.send(Ok(id)).is_ok() {
                        while GetMessageW(&mut message, None, 0, 0).0 > 0 {
                            DispatchMessageW(&message);
                        }
                    }
                    let _ = UnhookWindowsHookEx(hook);
                }
                Err(error) => {
                    let _ = ready.send(Err(format!("キーフックを設置できません: {error}")));
                }
            }
        })
        .map_err(|e| format!("キー入力の担当を起動できません: {e}"))?;
    let id = result
        .recv()
        .map_err(|_| "キー入力の担当が起動中に終了しました".to_string())??;
    *PUMP.lock().unwrap_or_else(|p| p.into_inner()) = Some((id, thread));
    Ok(())
}

pub fn uninstall() {
    if let Some((id, thread)) = PUMP.lock().unwrap_or_else(|p| p.into_inner()).take() {
        let sent = unsafe { PostThreadMessageW(id, WM_QUIT, WPARAM(0), LPARAM(0)) };
        if sent.is_ok() {
            let _ = thread.join();
        }
    }
}

unsafe fn current_mods() -> u8 {
    let down = |vk: i32| GetAsyncKeyState(vk) as u16 & 0x8000 != 0;
    let mut mods = 0;
    if down(0x11) {
        mods |= MOD_CTRL;
    }
    if down(0x12) {
        mods |= MOD_ALT;
    }
    if down(0x10) {
        mods |= MOD_SHIFT;
    }
    if down(0x5B) || down(0x5C) {
        mods |= MOD_WIN;
    }
    mods
}

/// Alt や Win を単独で離したとみなされるとメニューバーやスタートメニューが開くので、割り当てのないキーを挟んで打ち消す
unsafe fn send_mask_key() {
    let key = |flags| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(0xE8),
                dwFlags: flags,
                ..Default::default()
            },
        },
    };
    let inputs = [key(Default::default()), key(KEYEVENTF_KEYUP)];
    SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
}

unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code != HC_ACTION as i32 {
        return CallNextHookEx(None, code, wparam, lparam);
    }
    let kb = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
    {
        let vk = kb.vkCode;
        let msg = wparam.0 as u32;
        let is_down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
        let is_up = msg == WM_KEYUP || msg == WM_SYSKEYUP;
        if is_down || is_up {
            let captured = &CAPTURED_HOTKEYS[(vk / 64).min(3) as usize];
            let bit = 1_u64 << (vk % 64);
            if captured.load(Ordering::SeqCst) & bit != 0 {
                if is_up {
                    captured.fetch_and(!bit, Ordering::SeqCst);
                }
                return LRESULT(1);
            }
            let packed = HOTKEY.load(Ordering::SeqCst);
            let hotkey = packed & 0xffff;
            if vk == hotkey && is_down {
                let mods = (packed >> 16) as u8;
                if current_mods() == mods {
                    // 本体が開始を処理する前の Enter/Esc も同じ録音の操作として捕捉する。
                    PENDING_KEYS.fetch_add(1, Ordering::SeqCst);
                    if SENDER
                        .get()
                        .is_some_and(|sender| sender.send(KeyEvent::Hotkey).is_ok())
                    {
                        captured.fetch_or(bit, Ordering::SeqCst);
                        if mods & (MOD_ALT | MOD_WIN) != 0 {
                            send_mask_key();
                        }
                        wake_resident();
                        return LRESULT(1);
                    }
                    acknowledge_hotkey();
                }
                return CallNextHookEx(None, code, wparam, lparam);
            }
            let active = mirror() != MIRROR_IDLE || PENDING_KEYS.load(Ordering::SeqCst) > 0;
            if vk == VIRTUAL_KEY(0x0D).0 as u32 {
                let (swallow, send) = capture_press(&ENTER_HELD, &ENTER_CAPTURED, is_down, active);
                if send {
                    if let Some(sender) = SENDER.get() {
                        let _ = sender.send(KeyEvent::Enter);
                    }
                    wake_resident();
                }
                if swallow {
                    return LRESULT(1);
                }
                return CallNextHookEx(None, code, wparam, lparam);
            }
            if vk == VIRTUAL_KEY(0x1B).0 as u32 {
                let (swallow, send) = capture_press(&ESC_HELD, &ESC_CAPTURED, is_down, active);
                if send {
                    if let Some(sender) = SENDER.get() {
                        let _ = sender.send(KeyEvent::Esc);
                    }
                    wake_resident();
                }
                if swallow {
                    return LRESULT(1);
                }
            }
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enter_repeat_never_issues_second_event_after_state_transition() {
        let held = AtomicBool::new(false);
        let captured = AtomicBool::new(false);
        assert_eq!(capture_press(&held, &captured, true, true), (true, true));
        assert_eq!(capture_press(&held, &captured, true, true), (true, false));
        assert_eq!(capture_press(&held, &captured, true, false), (true, false));
        assert_eq!(capture_press(&held, &captured, false, false), (true, false));
        assert_eq!(capture_press(&held, &captured, true, true), (true, true));
    }

    #[test]
    fn enter_pressed_in_idle_does_not_become_capture_on_repeat() {
        let held = AtomicBool::new(false);
        let captured = AtomicBool::new(false);
        assert_eq!(capture_press(&held, &captured, true, false), (false, false));
        assert_eq!(capture_press(&held, &captured, true, true), (false, false));
        assert_eq!(capture_press(&held, &captured, false, true), (false, false));
        assert_eq!(capture_press(&held, &captured, true, true), (true, true));
        assert_eq!(capture_press(&held, &captured, false, false), (true, false));
    }
}
