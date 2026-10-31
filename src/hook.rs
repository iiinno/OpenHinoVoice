use crate::config::{Hotkey, MOD_ALT, MOD_CTRL, MOD_SHIFT, MOD_WIN};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
use std::sync::mpsc::Sender;
use std::sync::OnceLock;
use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP,
    VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, SetWindowsHookExW, UnhookWindowsHookEx, HC_ACTION, HHOOK, KBDLLHOOKSTRUCT,
    WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN, WM_SYSKEYUP,
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
static MIRROR: AtomicU8 = AtomicU8::new(MIRROR_IDLE);
static HOTKEY_VK: AtomicU32 = AtomicU32::new(0x77);
static HOTKEY_MODS: AtomicU8 = AtomicU8::new(0);
// 押下を横取りした主キーは、離したときも横取りしてアプリに片側だけ届かないようにする
static HOTKEY_HELD: AtomicBool = AtomicBool::new(false);
// A held Enter produces repeated WM_KEYDOWN messages. Only its first physical
// press may advance Recording -> Recognizing or request raw text.
static ENTER_HELD: AtomicBool = AtomicBool::new(false);
static ENTER_CAPTURED: AtomicBool = AtomicBool::new(false);
static HOOK_HANDLE: OnceLock<isize> = OnceLock::new();

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

pub fn mirror() -> u8 {
    MIRROR.load(Ordering::SeqCst)
}

pub fn set_hotkey(hotkey: Hotkey) {
    HOTKEY_VK.store(hotkey.vk, Ordering::SeqCst);
    HOTKEY_MODS.store(hotkey.mods, Ordering::SeqCst);
}

pub fn install(sender: Sender<KeyEvent>, hotkey: Hotkey) -> Result<(), String> {
    let _ = SENDER.set(sender);
    set_hotkey(hotkey);
    unsafe {
        let hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), None, 0)
            .map_err(|e| format!("キーフックを設置できません: {e}"))?;
        let _ = HOOK_HANDLE.set(hook.0 as isize);
    }
    Ok(())
}

pub fn uninstall() {
    if let Some(raw) = HOOK_HANDLE.get() {
        unsafe {
            let _ = UnhookWindowsHookEx(HHOOK(*raw as *mut std::ffi::c_void));
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
            let hotkey = HOTKEY_VK.load(Ordering::SeqCst);
            let state = mirror();
            if vk == hotkey {
                let swallow = if is_down {
                    let mods = HOTKEY_MODS.load(Ordering::SeqCst);
                    let hit = current_mods() == mods;
                    if hit {
                        HOTKEY_HELD.store(true, Ordering::SeqCst);
                        if mods & (MOD_ALT | MOD_WIN) != 0 {
                            send_mask_key();
                        }
                        if let Some(sender) = SENDER.get() {
                            let _ = sender.send(KeyEvent::Hotkey);
                        }
                    }
                    hit
                } else {
                    HOTKEY_HELD.swap(false, Ordering::SeqCst)
                };
                if swallow {
                    return LRESULT(1);
                }
                return CallNextHookEx(None, code, wparam, lparam);
            }
            if vk == VIRTUAL_KEY(0x0D).0 as u32 {
                let (swallow, send) =
                    capture_press(&ENTER_HELD, &ENTER_CAPTURED, is_down, state != MIRROR_IDLE);
                if send {
                    if let Some(sender) = SENDER.get() {
                        let _ = sender.send(KeyEvent::Enter);
                    }
                }
                if swallow {
                    return LRESULT(1);
                }
                return CallNextHookEx(None, code, wparam, lparam);
            }
            let event: Option<KeyEvent> = if vk == VIRTUAL_KEY(0x1B).0 as u32 {
                if state != MIRROR_IDLE {
                    Some(KeyEvent::Esc)
                } else {
                    None
                }
            } else {
                None
            };
            if let Some(ev) = event {
                if is_down {
                    if let Some(sender) = SENDER.get() {
                        let _ = sender.send(ev);
                    }
                }
                return LRESULT(1);
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
