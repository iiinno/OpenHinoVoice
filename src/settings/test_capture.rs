//! Explicit framebuffer capture for the isolated UI harness only.
use eframe::egui;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{IsWindow, RegisterWindowMessageW, WM_NCDESTROY};

const SUBCLASS: usize = 0x4F48_5643;
const MAX_CAPTURES: usize = 64;

struct State {
    ctx: egui::Context,
    directory: PathBuf,
    message: u32,
    last_token: AtomicUsize,
    installed: AtomicBool,
}

pub(super) struct Capture {
    window: HWND,
    state: Arc<State>,
}

fn authorized_directory() -> Option<PathBuf> {
    if !crate::config::is_test_sandbox() {
        return None;
    }
    let namespace = std::env::var("OHV_TEST_NAMESPACE").ok()?;
    let local = PathBuf::from(std::env::var_os("LOCALAPPDATA")?);
    let requested = PathBuf::from(std::env::var_os("OHV_TEST_SETTINGS_CAPTURE_DIR")?);
    capture_directory(&local, &namespace, &requested)
}

fn capture_directory(local: &Path, namespace: &str, requested: &Path) -> Option<PathBuf> {
    if !local.is_absolute() || !requested.is_absolute() {
        return None;
    }
    let expected = local
        .join("OpenHinoVoice")
        .join("ui-captures")
        .join(namespace);
    // The harness creates this exact directory first. An arbitrary path, parent,
    // or a symlink/junction escaping LOCALAPPDATA is never accepted.
    let local = local.canonicalize().ok()?;
    let expected = expected.canonicalize().ok()?;
    let requested = requested.canonicalize().ok()?;
    (requested == expected && expected.starts_with(&local)).then_some(expected)
}

impl Capture {
    pub(super) fn install(cc: &eframe::CreationContext<'_>) -> Option<Self> {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        let directory = authorized_directory()?;
        let RawWindowHandle::Win32(handle) = cc.window_handle().ok()?.as_raw() else {
            return None;
        };
        let window = HWND(handle.hwnd.get() as *mut std::ffi::c_void);
        let message = unsafe {
            RegisterWindowMessageW(windows::core::w!("OpenHinoVoice.Test.SettingsCapture.v1"))
        };
        if message == 0 {
            return None;
        }
        let state = Arc::new(State {
            ctx: cc.egui_ctx.clone(),
            directory,
            message,
            last_token: AtomicUsize::new(0),
            installed: AtomicBool::new(true),
        });
        let callback_state = Arc::into_raw(Arc::clone(&state));
        if !unsafe {
            SetWindowSubclass(
                window,
                Some(capture_proc),
                SUBCLASS,
                callback_state as usize,
            )
        }
        .as_bool()
        {
            unsafe {
                drop(Arc::from_raw(callback_state));
            }
            return None;
        }
        Some(Self { window, state })
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        if !self.state.installed.swap(false, Ordering::AcqRel) {
            return;
        }
        let removed = unsafe {
            RemoveWindowSubclass(self.window, Some(capture_proc), SUBCLASS).as_bool()
                || !IsWindow(self.window).as_bool()
        };
        if removed {
            unsafe {
                drop(Arc::from_raw(Arc::as_ptr(&self.state)));
            }
        } else {
            self.state.installed.store(true, Ordering::Release);
        }
    }
}

unsafe extern "system" fn capture_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    data: usize,
) -> LRESULT {
    let state = &*(data as *const State);
    if message == state.message {
        let token = wparam.0;
        if token == 0
            || token > MAX_CAPTURES
            || lparam.0 != 0
            || token <= state.last_token.load(Ordering::Acquire)
        {
            return LRESULT(0);
        }
        state.last_token.store(token, Ordering::Release);
        let directory = state.directory.clone();
        // Egui's official screenshot command paints even an occluded viewport
        // and reads the actual GL framebuffer after the normal Settings::ui pass.
        state.ctx.request_screenshot(move |image| {
            let file = directory.join(format!("capture-{token}.bmp"));
            let result = write_bmp(&file, &image);
            let status = serde_json::json!({
                "token": token, "renderer": "eframe glow framebuffer",
                "width": image.width(), "height": image.height(),
                "status": if result.is_ok() { "captured" } else { "failed" },
                "error": result.err().map(|error| error.to_string()),
            });
            let _ = std::fs::write(
                directory.join(format!("capture-{token}.json")),
                status.to_string(),
            );
        });
        return LRESULT(1);
    }
    let release = message == WM_NCDESTROY && state.installed.swap(false, Ordering::AcqRel);
    if release {
        let _ = RemoveWindowSubclass(window, Some(capture_proc), SUBCLASS);
    }
    let result = DefSubclassProc(window, message, wparam, lparam);
    if release {
        drop(Arc::from_raw(data as *const State));
    }
    result
}

fn write_bmp(path: &Path, image: &egui::ColorImage) -> std::io::Result<()> {
    let width = image.width();
    let height = image.height();
    if width == 0 || height == 0 || width > 4096 || height > 4096 {
        return Err(std::io::Error::other(
            "Framebuffer dimensions exceed the capture bound",
        ));
    }
    let stride = (width * 3 + 3) & !3;
    let size = 54 + stride * height;
    let mut bytes = vec![0u8; size];
    bytes[..2].copy_from_slice(b"BM");
    bytes[2..6].copy_from_slice(&(size as u32).to_le_bytes());
    bytes[10..14].copy_from_slice(&54u32.to_le_bytes());
    bytes[14..18].copy_from_slice(&40u32.to_le_bytes());
    bytes[18..22].copy_from_slice(&(width as i32).to_le_bytes());
    bytes[22..26].copy_from_slice(&(height as i32).to_le_bytes());
    bytes[26..28].copy_from_slice(&1u16.to_le_bytes());
    bytes[28..30].copy_from_slice(&24u16.to_le_bytes());
    for y in 0..height {
        for x in 0..width {
            let color = image.pixels[y * width + x];
            let offset = 54 + (height - 1 - y) * stride + x * 3;
            bytes[offset..offset + 3].copy_from_slice(&[color.b(), color.g(), color.r()]);
        }
    }
    let temporary = path.with_extension("bmp.tmp");
    std::fs::write(&temporary, bytes)?;
    std::fs::rename(temporary, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_path_requires_exact_sandbox_subdirectory() {
        let root = std::env::temp_dir().join(format!("ohv-capture-{}", std::process::id()));
        let expected = root.join("OpenHinoVoice/ui-captures/test");
        std::fs::create_dir_all(&expected).unwrap();
        assert_eq!(
            capture_directory(&root, "test", &expected),
            expected.canonicalize().ok()
        );
        assert!(capture_directory(&root, "test", &root).is_none());
        assert!(capture_directory(&root, "test", Path::new("relative")).is_none());
        // Do not recursively delete shared temp roots; remove only this test's
        // empty directories in leaf-to-root order.
        let _ = std::fs::remove_dir(&expected);
        let _ = std::fs::remove_dir(root.join("OpenHinoVoice/ui-captures"));
        let _ = std::fs::remove_dir(root.join("OpenHinoVoice"));
        let _ = std::fs::remove_dir(root);
    }
}
