use std::ffi::c_void;
use std::thread;
use std::time::Duration;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, VIRTUAL_KEY,
};

const CF_UNICODETEXT: u32 = 13;
const GMEM_MOVEABLE: u32 = 0x0002;

extern "system" {
    fn OpenClipboard(hwnd: *mut c_void) -> i32;
    fn CloseClipboard() -> i32;
    fn EmptyClipboard() -> i32;
    fn GetClipboardData(format: u32) -> *mut c_void;
    fn SetClipboardData(format: u32, mem: *mut c_void) -> *mut c_void;
    fn GlobalAlloc(flags: u32, bytes: usize) -> *mut c_void;
    fn GlobalLock(mem: *mut c_void) -> *mut c_void;
    fn GlobalUnlock(mem: *mut c_void) -> i32;
    fn GlobalSize(mem: *mut c_void) -> usize;
}

fn get_clipboard_text() -> Option<String> {
    unsafe {
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return None;
        }
        let handle = GetClipboardData(CF_UNICODETEXT);
        let result = if handle.is_null() {
            None
        } else {
            let ptr = GlobalLock(handle) as *const u16;
            if ptr.is_null() {
                None
            } else {
                let size = GlobalSize(handle) / 2;
                let slice = std::slice::from_raw_parts(ptr, size);
                let text = String::from_utf16_lossy(slice)
                    .trim_end_matches('\0')
                    .to_string();
                GlobalUnlock(handle);
                Some(text)
            }
        };
        CloseClipboard();
        result
    }
}

fn set_clipboard_text(text: &str) -> Result<(), String> {
    unsafe {
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return Err("クリップボードを開けません".into());
        }
        let result = (|| {
            if EmptyClipboard() == 0 {
                return Err("クリップボードを空にできません".into());
            }
            let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
            let bytes = wide.len() * 2;
            let mem = GlobalAlloc(GMEM_MOVEABLE, bytes);
            if mem.is_null() {
                return Err("クリップボード用メモリを確保できません".into());
            }
            let ptr = GlobalLock(mem) as *mut u16;
            if ptr.is_null() {
                return Err("クリップボード用メモリをロックできません".into());
            }
            std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr, wide.len());
            GlobalUnlock(mem);
            if SetClipboardData(CF_UNICODETEXT, mem).is_null() {
                return Err("クリップボードへ書けません".into());
            }
            Ok(())
        })();
        CloseClipboard();
        result
    }
}

fn clear_clipboard() {
    unsafe {
        if OpenClipboard(std::ptr::null_mut()) != 0 {
            let _ = EmptyClipboard();
            CloseClipboard();
        }
    }
}

fn send_ctrl_v() {
    let inputs = [
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(0x11),
                    wScan: 0,
                    dwFlags: Default::default(),
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        },
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(0x56),
                    wScan: 0,
                    dwFlags: Default::default(),
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        },
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(0x56),
                    wScan: 0,
                    dwFlags: KEYEVENTF_KEYUP,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        },
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(0x11),
                    wScan: 0,
                    dwFlags: KEYEVENTF_KEYUP,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        },
    ];
    unsafe {
        SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
    }
}

pub fn paste_text(text: &str) -> Result<(), String> {
    let saved = get_clipboard_text();
    let mut last_err = String::new();
    let mut written = false;
    for _ in 0..5 {
        match set_clipboard_text(text) {
            Ok(()) => {
                written = true;
                break;
            }
            Err(e) => {
                last_err = e;
                thread::sleep(Duration::from_millis(30));
            }
        }
    }
    if !written {
        return Err(last_err);
    }
    send_ctrl_v();
    thread::sleep(Duration::from_millis(150));
    match saved {
        Some(original) => {
            let _ = set_clipboard_text(&original);
        }
        None => clear_clipboard(),
    }
    Ok(())
}
