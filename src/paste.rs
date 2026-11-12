//! Paste the final text at once, instead of simulating typing through the IME.
//! Snapshot native clipboard formats and restore them after Ctrl+V.

use std::ffi::c_void;
use std::marker::PhantomData;
use std::rc::Rc;
use std::thread;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::HWND;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Graphics::Gdi::BITMAP;
use windows::Win32::System::DataExchange::METAFILEPICT;
use windows::Win32::System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject};
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
    fn GetClipboardOwner() -> *mut c_void;
    fn GetClipboardData(format: u32) -> *mut c_void;
    fn EnumClipboardFormats(format: u32) -> u32;
    fn GetClipboardFormatNameW(format: u32, name: *mut u16, capacity: i32) -> i32;
}

#[link(name = "kernel32")]
extern "system" {
    fn GlobalAlloc(flags: u32, bytes: usize) -> *mut c_void;
    fn GlobalLock(mem: *mut c_void) -> *mut c_void;
    fn GlobalUnlock(mem: *mut c_void) -> i32;
    fn GlobalFree(mem: *mut c_void) -> *mut c_void;
    fn GlobalSize(mem: *mut c_void) -> usize;
    fn SetLastError(error: u32);
    fn GetLastError() -> u32;
}

#[link(name = "ole32")]
extern "system" {
    fn OleDuplicateData(source: *mut c_void, format: u16, flags: u32) -> *mut c_void;
}

#[link(name = "gdi32")]
extern "system" {
    fn GetObjectW(object: *mut c_void, bytes: i32, output: *mut c_void) -> i32;
    fn GetPaletteEntries(palette: *mut c_void, start: u32, count: u32, entries: *mut c_void)
        -> u32;
    fn GetMetaFileBitsEx(metafile: *mut c_void, bytes: u32, output: *mut c_void) -> u32;
    fn GetEnhMetaFileBits(metafile: *mut c_void, bytes: u32, output: *mut c_void) -> u32;
    fn CopyEnhMetaFileW(metafile: *mut c_void, file: *const u16) -> *mut c_void;
    fn DeleteObject(object: *mut c_void) -> i32;
    fn DeleteMetaFile(metafile: *mut c_void) -> i32;
    fn DeleteEnhMetaFile(metafile: *mut c_void) -> i32;
}

const SNAPSHOT_MAX_BYTES: usize = 64 * 1024 * 1024;
const SNAPSHOT_MAX_FORMATS: usize = 256;

#[derive(Clone, Copy)]
enum ClipboardFormatKind {
    Global,
    Bitmap,
    Palette,
    MetafilePicture,
    EnhancedMetafile,
}

impl ClipboardFormatKind {
    fn from_format(format: u32) -> Result<Self, String> {
        match format {
            2 | 0x82 => Ok(Self::Bitmap),
            9 => Ok(Self::Palette),
            3 | 0x83 => Ok(Self::MetafilePicture),
            14 | 0x8e => Ok(Self::EnhancedMetafile),
            1 | 4..=8 | 10..=13 | 15..=17 | 0x81 | 0xc000..=0xffff => Ok(Self::Global),
            _ => Err(format!(
                "元のクリップボード形式 {format} を安全に保持できないため、貼り付けを中止しました"
            )),
        }
    }
}

struct OwnedClipboardFormat {
    format: u32,
    handle: *mut c_void,
    kind: ClipboardFormatKind,
    nested_metafile: *mut c_void,
}

impl Drop for OwnedClipboardFormat {
    fn drop(&mut self) {
        if self.handle.is_null() {
            return;
        }
        unsafe {
            match self.kind {
                ClipboardFormatKind::Global => {
                    GlobalFree(self.handle);
                }
                ClipboardFormatKind::Bitmap | ClipboardFormatKind::Palette => {
                    DeleteObject(self.handle);
                }
                ClipboardFormatKind::MetafilePicture => {
                    DeleteMetaFile(self.nested_metafile);
                    GlobalFree(self.handle);
                }
                ClipboardFormatKind::EnhancedMetafile => {
                    DeleteEnhMetaFile(self.handle);
                }
            }
        }
    }
}

fn metafile_picture(handle: *mut c_void) -> Result<METAFILEPICT, String> {
    unsafe {
        if GlobalSize(handle) < std::mem::size_of::<METAFILEPICT>() {
            return Err("元のクリップボードのメタファイルが不正です".into());
        }
        let pointer = GlobalLock(handle) as *const METAFILEPICT;
        if pointer.is_null() {
            return Err("元のクリップボードのメタファイルを読み取れません".into());
        }
        let picture = pointer.read();
        GlobalUnlock(handle);
        Ok(picture)
    }
}

impl OwnedClipboardFormat {
    fn duplicate(
        format: u32,
        source: *mut c_void,
        remaining: usize,
    ) -> Result<(Self, usize), String> {
        let kind = ClipboardFormatKind::from_format(format)?;
        let bytes = unsafe {
            match kind {
                ClipboardFormatKind::Global => GlobalSize(source),
                ClipboardFormatKind::Bitmap => {
                    let mut bitmap = BITMAP::default();
                    if GetObjectW(
                        source,
                        std::mem::size_of_val(&bitmap) as i32,
                        &mut bitmap as *mut _ as *mut c_void,
                    ) != std::mem::size_of_val(&bitmap) as i32
                    {
                        return Err("元のクリップボードの画像を確認できません".into());
                    }
                    (bitmap.bmWidthBytes.unsigned_abs() as usize)
                        .checked_mul(bitmap.bmHeight.unsigned_abs() as usize)
                        .and_then(|bytes| bytes.checked_mul(bitmap.bmPlanes as usize))
                        .ok_or("元のクリップボードの画像が大きすぎます")?
                }
                ClipboardFormatKind::Palette => {
                    GetPaletteEntries(source, 0, 0, std::ptr::null_mut()) as usize * 4
                }
                ClipboardFormatKind::MetafilePicture => {
                    let picture = metafile_picture(source)?;
                    let size = GetMetaFileBitsEx(picture.hMF.0, 0, std::ptr::null_mut()) as usize;
                    if size == 0 {
                        return Err("元のクリップボードのメタファイルを確認できません".into());
                    }
                    size.checked_add(std::mem::size_of_val(&picture))
                        .ok_or("元のクリップボードのメタファイルが大きすぎます")?
                }
                ClipboardFormatKind::EnhancedMetafile => {
                    GetEnhMetaFileBits(source, 0, std::ptr::null_mut()) as usize
                }
            }
        };
        if bytes == 0 || bytes > remaining {
            return Err(
                "元のクリップボードを保持できません（無効な形式、または64 MiBを超過）".into(),
            );
        }
        let handle = unsafe {
            match kind {
                ClipboardFormatKind::EnhancedMetafile => CopyEnhMetaFileW(source, std::ptr::null()),
                ClipboardFormatKind::Bitmap => OleDuplicateData(source, 2, GMEM_MOVEABLE),
                ClipboardFormatKind::Palette => OleDuplicateData(source, 9, GMEM_MOVEABLE),
                ClipboardFormatKind::MetafilePicture => OleDuplicateData(source, 3, GMEM_MOVEABLE),
                ClipboardFormatKind::Global => {
                    OleDuplicateData(source, format as u16, GMEM_MOVEABLE)
                }
            }
        };
        if handle.is_null() {
            return Err("元のクリップボードを複製できません".into());
        }
        let mut owned = Self {
            format,
            handle,
            kind,
            nested_metafile: std::ptr::null_mut(),
        };
        if matches!(kind, ClipboardFormatKind::MetafilePicture) {
            owned.nested_metafile = metafile_picture(handle)?.hMF.0;
        }
        Ok((owned, bytes))
    }
}

fn is_transient_clipboard_format(format: u32) -> Result<bool, String> {
    if format < 0xc000 {
        return Ok(false);
    }
    let mut name = [0u16; 256];
    let length = unsafe { GetClipboardFormatNameW(format, name.as_mut_ptr(), name.len() as i32) };
    if length == 0 {
        return Err("元のクリップボードの形式名を取得できません".into());
    }
    let name = String::from_utf16_lossy(&name[..length as usize]);
    // 転送元と一時的な所有権の印は、文書データではないため復元しない。
    Ok([
        "DataObject",
        "Ole Private Data",
        "Wine Marshalled DataObject",
        "OpenHinoVoice.PasteOwnership",
    ]
    .iter()
    .any(|transport| name.eq_ignore_ascii_case(transport)))
}

#[derive(Clone, Copy)]
struct ClipboardMarker {
    format: u32,
    nonce: [u8; 16],
}

impl ClipboardMarker {
    fn allocate() -> Result<(Self, AllocatedClipboardMemory), String> {
        let format = unsafe {
            windows::Win32::System::DataExchange::RegisterClipboardFormatW(windows::core::w!(
                "OpenHinoVoice.PasteOwnership"
            ))
        };
        if format == 0 {
            return Err("クリップボードの所有権を確認する形式を登録できません".into());
        }
        let guid = unsafe { windows::Win32::System::Com::CoCreateGuid() }
            .map_err(|e| format!("クリップボードの所有権を確認する印を作れません: {e}"))?;
        let mut nonce = [0; 16];
        nonce[..4].copy_from_slice(&guid.data1.to_le_bytes());
        nonce[4..6].copy_from_slice(&guid.data2.to_le_bytes());
        nonce[6..8].copy_from_slice(&guid.data3.to_le_bytes());
        nonce[8..].copy_from_slice(&guid.data4);
        let memory = AllocatedClipboardMemory::from_bytes(&nonce)?;
        Ok((Self { format, nonce }, memory))
    }
}

#[derive(Clone, Copy, Default)]
enum ClipboardOwnership {
    #[default]
    Unchanged,
    Sequence(u32),
    Marker(ClipboardMarker),
}

impl ClipboardOwnership {
    fn matches_locked(self, owner: HWND, before: u32) -> Result<bool, String> {
        if unsafe { GetClipboardOwner() } != owner.0 {
            return Ok(false);
        }
        match self {
            Self::Unchanged => Ok(false),
            Self::Sequence(sequence) => Ok(should_restore(before, sequence, unsafe {
                GetClipboardSequenceNumber()
            })),
            Self::Marker(marker) => unsafe {
                if windows::Win32::System::DataExchange::IsClipboardFormatAvailable(marker.format)
                    .is_err()
                {
                    return Ok(false);
                }
                let handle = GetClipboardData(marker.format);
                if handle.is_null() {
                    return Err("クリップボードの所有権を読み取れません".into());
                }
                if GlobalSize(handle) < marker.nonce.len() {
                    return Ok(false);
                }
                let pointer = GlobalLock(handle) as *const u8;
                if pointer.is_null() {
                    return Err("クリップボードの所有権を確認できません".into());
                }
                let matches =
                    std::slice::from_raw_parts(pointer, marker.nonce.len()) == marker.nonce;
                GlobalUnlock(handle);
                Ok(matches)
            },
        }
    }
}

struct ClipboardSnapshot {
    sequence: u32,
    formats: Vec<OwnedClipboardFormat>,
}

impl ClipboardSnapshot {
    fn capture_locked(_clipboard: &OpenedClipboard) -> Result<Self, String> {
        let mut formats = Vec::new();
        let mut used_bytes = 0;
        let mut format = 0;
        loop {
            unsafe { SetLastError(0) };
            format = unsafe { EnumClipboardFormats(format) };
            if format == 0 {
                if unsafe { GetLastError() } != 0 {
                    return Err("元のクリップボードの形式一覧を取得できません".into());
                }
                break;
            }
            if is_transient_clipboard_format(format)? {
                continue;
            }
            if formats.len() == SNAPSHOT_MAX_FORMATS {
                return Err("元のクリップボードの形式が256件を超えています".into());
            }
            ClipboardFormatKind::from_format(format)?;
            let source = unsafe { GetClipboardData(format) };
            if source.is_null() {
                return Err(format!("元のクリップボード形式 {format} を読み取れません"));
            }
            let (duplicate, bytes) =
                OwnedClipboardFormat::duplicate(format, source, SNAPSHOT_MAX_BYTES - used_bytes)?;
            used_bytes += bytes;
            formats.push(duplicate);
        }
        Ok(Self {
            sequence: unsafe { GetClipboardSequenceNumber() },
            formats,
        })
    }

    fn restore_locked(mut self, _clipboard: &OpenedClipboard) -> Result<(), String> {
        if unsafe { EmptyClipboard() } == 0 {
            return Err("元のクリップボードを復元する準備ができません".into());
        }
        let mut failed = false;
        for format in &mut self.formats {
            if unsafe { SetClipboardData(format.format, format.handle) }.is_null() {
                failed = true;
            } else {
                // SetClipboardData transfers ownership, including a METAFILEPICT's nested handle.
                format.handle = std::ptr::null_mut();
                format.nested_metafile = std::ptr::null_mut();
            }
        }
        if failed {
            Err("入力は送信済みですが、元のクリップボードの一部を復元できませんでした".into())
        } else {
            Ok(())
        }
    }
}

struct AllocatedClipboardMemory(*mut c_void);

impl AllocatedClipboardMemory {
    fn from_bytes(bytes: &[u8]) -> Result<Self, String> {
        let memory = Self(unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes.len()) });
        if memory.0.is_null() {
            return Err("クリップボード用メモリを確保できません".into());
        }
        unsafe {
            let pointer = GlobalLock(memory.0) as *mut u8;
            if pointer.is_null() {
                return Err("クリップボード用メモリをロックできません".into());
            }
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer, bytes.len());
            GlobalUnlock(memory.0);
        }
        Ok(memory)
    }
}

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
    fn try_open(owner: HWND) -> Option<Self> {
        if unsafe { OpenClipboard(owner.0) } != 0 {
            Some(Self)
        } else {
            None
        }
    }

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
    let mut ownership = ClipboardOwnership::Unchanged;
    let result = replace_clipboard_text(text, owner, expected_sequence, &mut ownership, None);
    *owned_sequence = match (result.is_ok(), ownership) {
        (true, ClipboardOwnership::Sequence(sequence)) => sequence,
        _ => 0,
    };
    result
}

fn replace_clipboard_text(
    text: &str,
    owner: HWND,
    expected_sequence: Option<u32>,
    ownership: &mut ClipboardOwnership,
    snapshot: Option<&mut Option<ClipboardSnapshot>>,
) -> Result<(), String> {
    if owner.0.is_null() {
        return Err("クリップボードの所有者ウィンドウがありません".into());
    }
    let retained = snapshot.is_some();
    let mut marker = if retained {
        Some(ClipboardMarker::allocate()?)
    } else {
        None
    };
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

        let clipboard = if retained {
            OpenedClipboard::try_open(owner)
                .ok_or("クリップボードが使用中のため入力を中止しました")?
        } else {
            OpenedClipboard::open(owner)?
        };
        if expected_sequence.is_some_and(|sequence| sequence != GetClipboardSequenceNumber()) {
            return Err("クリップボードが他のアプリで更新されました".into());
        }
        if let Some(snapshot) = snapshot {
            *snapshot = Some(ClipboardSnapshot::capture_locked(&clipboard)?);
        }
        if EmptyClipboard() == 0 {
            return Err("クリップボードを空にできません".into());
        }
        // Record only changes made while we own the clipboard lock. An error
        // before EmptyClipboard must never authorize restoring somebody else's update.
        *ownership = ClipboardOwnership::Sequence(GetClipboardSequenceNumber());
        if let Some((identity, memory)) = &mut marker {
            if SetClipboardData(identity.format, memory.0).is_null() {
                return Err("クリップボードの所有権を記録できません".into());
            }
            memory.0 = std::ptr::null_mut();
            *ownership = ClipboardOwnership::Marker(*identity);
        }
        if SetClipboardData(CF_UNICODETEXT, mem.0).is_null() {
            // Even when SetClipboardData fails, retain the failed text in App for another attempt.
            return Err("クリップボードへテキストを書き込めません".into());
        }
        if matches!(ownership, ClipboardOwnership::Sequence(_)) {
            *ownership = ClipboardOwnership::Sequence(GetClipboardSequenceNumber());
        }
        // Windows now owns the moveable memory handle; never free it in this process.
        mem.0 = std::ptr::null_mut();
    }
    #[cfg(test)]
    AFTER_CLIPBOARD_WRITE.with(|hook| {
        if let Some(hook) = hook.borrow_mut().take() {
            hook();
        }
    });
    // Closing a text write can publish Windows-synthesized formats and advance
    // the sequence. Reopen read-only and prove ownership before accepting it.
    let sequence = if retained {
        let clipboard = OpenedClipboard::try_open(owner)
            .ok_or("クリップボードが使用中のため入力を中止しました")?;
        published_sequence_locked(owner, &utf16, &clipboard)?
    } else {
        published_clipboard_sequence(owner, &utf16)?
    };
    if matches!(ownership, ClipboardOwnership::Sequence(_)) {
        *ownership = ClipboardOwnership::Sequence(sequence);
    }
    Ok(())
}

fn published_clipboard_sequence(owner: HWND, expected: &[u16]) -> Result<u32, String> {
    let clipboard = OpenedClipboard::open(owner)?;
    published_sequence_locked(owner, expected, &clipboard)
}

fn published_sequence_locked(
    owner: HWND,
    expected: &[u16],
    _clipboard: &OpenedClipboard,
) -> Result<u32, String> {
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

fn send_paste_command(target: PasteTarget) -> Result<Duration, String> {
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
        return Ok(Duration::ZERO);
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
    // SendInput confirms injection only; the destination still needs time to read the clipboard.
    Ok(Duration::from_millis(250))
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

/// 復元完了まで所有者スレッドで保持する。排他mutexも同じスレッドで解放する。
#[must_use]
pub struct PasteTransaction {
    previous: Option<ClipboardSnapshot>,
    owner: HWND,
    ownership: ClipboardOwnership,
    deadline: Instant,
    waiting_since: Option<Instant>,
    warned: bool,
    delivery_error: Option<String>,
    _lease: PasteLease,
    _same_thread: PhantomData<Rc<()>>,
}

#[derive(Debug)]
pub enum RestoreProgress {
    Pending,
    Waiting(String),
    Finished(Result<(), String>),
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
        let mut previous = None;
        let mut ownership = ClipboardOwnership::Unchanged;
        let written =
            replace_clipboard_text(text, owner, None, &mut ownership, Some(&mut previous));
        let mut transaction = Self {
            previous,
            owner,
            ownership,
            deadline: now,
            waiting_since: None,
            warned: false,
            delivery_error: None,
            _lease: lease,
            _same_thread: PhantomData,
        };
        let delivered = written.and_then(|()| {
            check_target(target)?;
            if !no_modifiers_pressed() {
                return Err("修飾キーが押されているため、貼り付けを中止しました".into());
            }
            let clipboard = OpenedClipboard::try_open(owner)
                .ok_or("クリップボードが使用中のため入力を中止しました")?;
            let before = transaction
                .previous
                .as_ref()
                .map_or(0, |snapshot| snapshot.sequence);
            if !ownership.matches_locked(owner, before)? {
                return Err("貼り付け前にクリップボードが別のアプリで変更されました".into());
            }
            drop(clipboard);
            send_paste_command(target)
        });
        match delivered {
            Ok(hold) => transaction.deadline = Instant::now() + hold,
            Err(error) => {
                if matches!(ownership, ClipboardOwnership::Unchanged) {
                    return Err(error);
                }
                transaction.delivery_error = Some(error);
                transaction.deadline = Instant::now();
            }
        }
        Ok(transaction)
    }

    pub fn delivery_error(&self) -> Option<&str> {
        self.delivery_error.as_deref()
    }

    pub fn deadline(&self) -> Instant {
        self.deadline
    }

    pub fn poll_restore(&mut self, now: Instant) -> RestoreProgress {
        if now < self.deadline {
            return RestoreProgress::Pending;
        }
        match self.restore_inner() {
            Ok(true) => RestoreProgress::Finished(Ok(())),
            Err(error) if self.previous.is_none() => RestoreProgress::Finished(Err(error)),
            result => {
                self.deadline = now + Duration::from_millis(50);
                let started = *self.waiting_since.get_or_insert(now);
                if !self.warned && now.saturating_duration_since(started) >= Duration::from_secs(2)
                {
                    self.warned = true;
                    RestoreProgress::Waiting(result.err().unwrap_or_else(|| {
                        "クリップボードが使用中です。元の内容を保持して復元を待っています".into()
                    }))
                } else {
                    RestoreProgress::Pending
                }
            }
        }
    }

    fn restore_inner(&mut self) -> Result<bool, String> {
        let Some(previous) = self.previous.as_ref() else {
            return Ok(true);
        };
        if matches!(self.ownership, ClipboardOwnership::Unchanged) {
            self.previous = None;
            return Ok(true);
        }
        let Some(clipboard) = OpenedClipboard::try_open(self.owner) else {
            return Ok(false);
        };
        if !self
            .ownership
            .matches_locked(self.owner, previous.sequence)?
        {
            self.previous = None;
            return Ok(true);
        }
        self.previous.take().unwrap().restore_locked(&clipboard)?;
        Ok(true)
    }
}

impl Drop for PasteTransaction {
    fn drop(&mut self) {
        match self.restore_inner() {
            Ok(true) => {}
            Ok(false) => eprintln!("クリップボードの復元待ちが完了する前に処理が終了しました"),
            Err(error) => eprintln!("{error}"),
        }
    }
}

#[cfg(test)]
pub(crate) struct ClipboardReadLockForTest {
    release: std::sync::mpsc::Sender<()>,
    reader: Option<std::thread::JoinHandle<()>>,
}
#[cfg(test)]
impl ClipboardReadLockForTest {
    pub(crate) fn acquire() -> Self {
        assert_eq!(std::env::var("GITHUB_ACTIONS").as_deref(), Ok("true"));
        assert_eq!(std::env::var("RUNNER_ENVIRONMENT").as_deref(), Ok("github-hosted"));
        let (ready, acquired) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            let clipboard = OpenedClipboard::open(HWND::default()).unwrap();
            ready.send(()).unwrap();
            let result = released.recv_timeout(Duration::from_secs(5));
            drop(clipboard);
            result.expect("test must release its clipboard reader");
        });
        acquired.recv_timeout(Duration::from_secs(5)).unwrap();
        Self {
            release,
            reader: Some(reader),
        }
    }
}
#[cfg(test)]
impl Drop for ClipboardReadLockForTest {
    fn drop(&mut self) {
        let _ = self.release.send(());
        if let Some(reader) = self.reader.take() {
            reader.join().unwrap();
        }
    }
}

#[cfg(test)]
pub(crate) fn after_next_clipboard_write_for_test(hook: impl FnOnce() + 'static) {
    AFTER_CLIPBOARD_WRITE.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
}

#[cfg(test)]
thread_local! {
    static AFTER_CLIPBOARD_WRITE: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transaction_for_test(
        previous: Option<ClipboardSnapshot>,
        owner: HWND,
        ownership: ClipboardOwnership,
    ) -> PasteTransaction {
        PasteTransaction {
            previous,
            owner,
            ownership,
            deadline: Instant::now(),
            waiting_since: None,
            warned: false,
            delivery_error: None,
            _lease: PasteLease::acquire().unwrap(),
            _same_thread: PhantomData,
        }
    }

    fn finish_restore(transaction: &mut PasteTransaction) {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match transaction.poll_restore(Instant::now()) {
                RestoreProgress::Finished(result) => {
                    result.unwrap();
                    return;
                }
                RestoreProgress::Pending | RestoreProgress::Waiting(_) => {
                    assert!(
                        Instant::now() < deadline,
                        "復元が期限内に完了しませんでした"
                    );
                    thread::sleep(
                        transaction
                            .deadline()
                            .saturating_duration_since(Instant::now()),
                    );
                }
            }
        }
    }

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

        {
            let _clipboard = OpenedClipboard::open(owner.0).unwrap();
            assert!(
                !unsafe { GetClipboardData(1) }.is_null(),
                "a consumer can request Windows-synthesized CF_TEXT"
            );
        }
        let after_consumer_read = unsafe { GetClipboardSequenceNumber() };
        eprintln!("clipboard sequence after CF_TEXT read: {after_consumer_read}");
        assert_eq!(
            owned_sequence, after_consumer_read,
            "reading a synthesized format must not invalidate our publication"
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

        #[link(name = "user32")]
        extern "system" {
            fn RegisterClipboardFormatW(name: *const u16) -> u32;
        }
        let format_name = windows::core::w!("OpenHinoVoice.NativeSnapshotRegression");
        let registered = unsafe { RegisterClipboardFormatW(format_name.as_ptr()) };
        assert!(registered >= 0xc000);
        let transports = [
            windows::core::w!("DataObject"),
            windows::core::w!("Ole Private Data"),
            windows::core::w!("Wine Marshalled DataObject"),
        ]
        .map(|name| unsafe { RegisterClipboardFormatW(name.as_ptr()) });
        assert!(transports.iter().all(|format| *format >= 0xc000));
        let original = "元のクリップボード 😀";
        copy_text_to_clipboard(original, owner.0).unwrap();
        let payload = b"registered clipboard payload 123";
        unsafe {
            let _clipboard = OpenedClipboard::open(owner.0).unwrap();
            for format in std::iter::once(registered).chain(transports) {
                let mut memory =
                    AllocatedClipboardMemory(GlobalAlloc(GMEM_MOVEABLE, payload.len()));
                assert!(!memory.0.is_null());
                let pointer = GlobalLock(memory.0) as *mut u8;
                assert!(!pointer.is_null());
                std::ptr::copy_nonoverlapping(payload.as_ptr(), pointer, payload.len());
                GlobalUnlock(memory.0);
                assert!(!SetClipboardData(format, memory.0).is_null());
                memory.0 = std::ptr::null_mut();
            }
        }
        let mut snapshot = None;
        let mut ours = ClipboardOwnership::Unchanged;
        replace_clipboard_text(
            "transient dictation",
            owner.0,
            None,
            &mut ours,
            Some(&mut snapshot),
        )
        .unwrap();
        finish_restore(&mut transaction_for_test(snapshot, owner.0, ours));
        let original_utf16 = clipboard_utf16(original);
        assert!(published_clipboard_sequence(owner.0, &original_utf16).is_ok());
        unsafe {
            let _clipboard = OpenedClipboard::open(owner.0).unwrap();
            let handle = GetClipboardData(registered);
            assert!(!handle.is_null(), "registered clipboard data must survive");
            assert!(GlobalSize(handle) >= payload.len());
            let pointer = GlobalLock(handle) as *const u8;
            assert!(!pointer.is_null());
            let restored = std::slice::from_raw_parts(pointer, payload.len()).to_vec();
            GlobalUnlock(handle);
            assert_eq!(restored, payload);
            for transport in transports {
                assert!(
                    GetClipboardData(transport).is_null(),
                    "old OLE transport references must not be republished"
                );
            }
        }

        let mut snapshot = None;
        replace_clipboard_text(
            "second dictation",
            owner.0,
            None,
            &mut ours,
            Some(&mut snapshot),
        )
        .unwrap();
        copy_text_to_clipboard("new user copy", foreign_owner.0).unwrap();
        let foreign_sequence = unsafe { GetClipboardSequenceNumber() };
        finish_restore(&mut transaction_for_test(snapshot, owner.0, ours));
        assert_eq!(foreign_sequence, unsafe { GetClipboardSequenceNumber() });
        assert!(
            published_clipboard_sequence(foreign_owner.0, &clipboard_utf16("new user copy"))
                .is_ok()
        );

        {
            let _clipboard = OpenedClipboard::open(owner.0).unwrap();
            assert_ne!(unsafe { EmptyClipboard() }, 0);
        }
        let mut snapshot = None;
        replace_clipboard_text(
            "dictation over an empty clipboard",
            owner.0,
            None,
            &mut ours,
            Some(&mut snapshot),
        )
        .unwrap();
        finish_restore(&mut transaction_for_test(snapshot, owner.0, ours));
        {
            let _clipboard = OpenedClipboard::open(owner.0).unwrap();
            assert_eq!(unsafe { EnumClipboardFormats(0) }, 0);
        }

        let mut failures = Vec::new();
        for phase in ["publication", "restoration"] {
            copy_text_to_clipboard(original, owner.0).unwrap();
            let reader = Rc::new(std::cell::RefCell::new(None));
            if phase == "publication" {
                let held = reader.clone();
                AFTER_CLIPBOARD_WRITE.with(|hook| {
                    *hook.borrow_mut() = Some(Box::new(move || {
                        *held.borrow_mut() = Some(ClipboardReadLockForTest::acquire());
                    }));
                });
            }
            let mut snapshot = None;
            let mut sequence = ClipboardOwnership::Unchanged;
            let replaced = replace_clipboard_text(
                "temporary dictation under contention",
                owner.0,
                None,
                &mut sequence,
                Some(&mut snapshot),
            );
            let mut transaction = transaction_for_test(snapshot, owner.0, sequence);
            if phase == "publication" {
                assert!(
                    replaced.is_err(),
                    "publication must observe the reader lock"
                );
            } else {
                replaced.unwrap();
                *reader.borrow_mut() = Some(ClipboardReadLockForTest::acquire());
            }
            let now = Instant::now();
            assert!(matches!(
                transaction.poll_restore(now),
                RestoreProgress::Pending
            ));
            assert!(
                transaction.previous.is_some(),
                "読み取りロック中も原内容を保持する"
            );
            assert!(
                transaction.deadline() > now,
                "再試行で待機ループを空回りさせない"
            );
            drop(reader.borrow_mut().take());
            finish_restore(&mut transaction);
            if published_clipboard_sequence(owner.0, &clipboard_utf16(original)).is_err() {
                failures.push(phase);
            }
        }
        assert!(
            failures.is_empty(),
            "read-only clipboard contention discarded the original during {failures:?}"
        );

        copy_text_to_clipboard(original, owner.0).unwrap();
        let mut snapshot = None;
        let mut ownership = ClipboardOwnership::Unchanged;
        replace_clipboard_text(
            "same text",
            owner.0,
            None,
            &mut ownership,
            Some(&mut snapshot),
        )
        .unwrap();
        copy_text_to_clipboard("same text", owner.0).unwrap();
        let newer_sequence = unsafe { GetClipboardSequenceNumber() };
        finish_restore(&mut transaction_for_test(snapshot, owner.0, ownership));
        assert_eq!(unsafe { GetClipboardSequenceNumber() }, newer_sequence);
        assert!(published_clipboard_sequence(owner.0, &clipboard_utf16("same text")).is_ok());
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
