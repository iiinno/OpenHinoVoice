use super::*;
use std::ffi::c_void;
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use windows::core::w;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, SetFocus, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, DispatchMessageW, GetForegroundWindow, GetMessageW,
    GetWindowThreadProcessId, PeekMessageW, PostThreadMessageW, SendMessageTimeoutW, SendMessageW,
    SetForegroundWindow, TranslateMessage, MSG, PM_REMOVE, SMTO_ABORTIFHUNG, WINDOW_EX_STYLE,
    WINDOW_STYLE, WM_GETTEXT, WM_QUIT, WS_CHILD, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_OVERLAPPEDWINDOW, WS_POPUP, WS_VISIBLE,
};

#[link(name = "user32")]
extern "system" {
    fn OpenInputDesktop(flags: u32, inherit: i32, access: u32) -> *mut c_void;
    fn GetThreadDesktop(thread: u32) -> *mut c_void;
    fn GetProcessWindowStation() -> *mut c_void;
    fn GetUserObjectInformationW(
        object: *mut c_void,
        index: i32,
        value: *mut c_void,
        bytes: u32,
        needed: *mut u32,
    ) -> i32;
    fn CloseDesktop(desktop: *mut c_void) -> i32;
    fn OpenClipboard(owner: *mut c_void) -> i32;
    fn CloseClipboard() -> i32;
    fn GetClipboardData(format: u32) -> *mut c_void;
}

#[link(name = "kernel32")]
extern "system" {
    fn GlobalLock(memory: *mut c_void) -> *mut c_void;
    fn GlobalUnlock(memory: *mut c_void) -> i32;
    fn GlobalSize(memory: *mut c_void) -> usize;
}

fn object_name(object: *mut c_void) -> String {
    let mut value = [0u16; 256];
    let mut needed = 0;
    assert_ne!(
        unsafe {
            GetUserObjectInformationW(
                object,
                2,
                value.as_mut_ptr().cast(),
                std::mem::size_of_val(&value) as u32,
                &mut needed,
            )
        },
        0
    );
    String::from_utf16_lossy(&value[..value.iter().position(|&ch| ch == 0).unwrap()])
}

fn require_hosted_input_desktop() {
    assert_eq!(std::env::var("GITHUB_ACTIONS").as_deref(), Ok("true"));
    assert_eq!(
        std::env::var("RUNNER_ENVIRONMENT").as_deref(),
        Ok("github-hosted")
    );
    assert_eq!(std::env::var("RUNNER_OS").as_deref(), Ok("Windows"));
    assert!(config::is_test_sandbox());
    unsafe {
        assert_eq!(object_name(GetProcessWindowStation()), "WinSta0");
        let input = OpenInputDesktop(0, 0, 1);
        assert!(!input.is_null(), "The CI input desktop must be available");
        let input_name = object_name(input);
        assert_ne!(CloseDesktop(input), 0);
        assert_eq!(input_name, "Default");
        assert_eq!(
            object_name(GetThreadDesktop(GetCurrentThreadId())),
            input_name
        );
    }
}

struct OwnerWindow(HWND);
impl OwnerWindow {
    fn new() -> Self {
        unsafe {
            Self(
                CreateWindowExW(
                    WS_EX_LAYERED
                        | WS_EX_TOPMOST
                        | WS_EX_NOACTIVATE
                        | WS_EX_TOOLWINDOW
                        | WS_EX_TRANSPARENT,
                    w!("STATIC"),
                    w!("OpenHinoVoice core test owner"),
                    WS_POPUP,
                    0,
                    0,
                    1,
                    1,
                    None,
                    None,
                    HINSTANCE(GetModuleHandleW(None).unwrap().0),
                    None,
                )
                .unwrap(),
            )
        }
    }
}
impl Drop for OwnerWindow {
    fn drop(&mut self) {
        let _ = unsafe { DestroyWindow(self.0) };
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct EditReady {
    pid: u32,
    thread_id: u32,
    top: isize,
    edit: isize,
}

struct EditTarget {
    top: HWND,
    edit: HWND,
    thread_id: u32,
    child: Child,
}
impl EditTarget {
    fn new() -> Self {
        let directory = PathBuf::from(std::env::var_os("APPDATA").unwrap())
            .join(format!("core-edit-target-{}", std::process::id()));
        std::fs::create_dir(&directory).unwrap();
        let ready_path = directory.join("ready.json");
        let log = std::fs::File::create(directory.join("child.log")).unwrap();
        let child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "app::core_e2e::edit_target_fixture",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("OHV_CORE_EDIT_READY", &ready_path)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log))
            .creation_flags(0x0800_0000)
            .spawn()
            .unwrap();
        let mut target = Self {
            top: HWND::default(),
            edit: HWND::default(),
            thread_id: 0,
            child,
        };
        assert_ne!(target.child.id(), std::process::id());
        let deadline = Instant::now() + Duration::from_secs(10);
        let ready: EditReady = loop {
            if let Some(status) = target.child.try_wait().unwrap() {
                panic!(
                    "EDIT fixture exited before readiness: {status}; {}",
                    std::fs::read_to_string(directory.join("child.log")).unwrap_or_default()
                );
            }
            match std::fs::read(&ready_path) {
                Ok(bytes) => break serde_json::from_slice(&bytes).unwrap(),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => panic!("Cannot read EDIT readiness: {error}"),
            }
            assert!(
                Instant::now() < deadline,
                "EDIT fixture readiness timed out"
            );
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(ready.pid, target.child.id());
        target.top = HWND(ready.top as *mut c_void);
        target.edit = HWND(ready.edit as *mut c_void);
        target.thread_id = ready.thread_id;
        for window in [target.top, target.edit] {
            let mut actual_pid = 0;
            let actual_thread = unsafe { GetWindowThreadProcessId(window, Some(&mut actual_pid)) };
            assert_eq!(actual_pid, target.child.id());
            assert_eq!(actual_thread, ready.thread_id);
        }
        assert_eq!(unsafe { GetForegroundWindow() }, target.top);
        println!(
            "core_e2e: target PID {} differs from app PID {}",
            target.child.id(),
            std::process::id()
        );
        target
    }

    fn text(&self) -> String {
        let mut text = [0u16; 4096];
        let mut length = 0usize;
        let result = unsafe {
            SendMessageTimeoutW(
                self.edit,
                WM_GETTEXT,
                WPARAM(text.len()),
                LPARAM(text.as_mut_ptr() as isize),
                SMTO_ABORTIFHUNG,
                1000,
                Some(&mut length),
            )
        };
        assert_ne!(result.0, 0, "Owned EDIT must answer WM_GETTEXT");
        assert!(length < text.len());
        String::from_utf16(&text[..length]).unwrap()
    }

    fn close(&mut self) -> Result<(), String> {
        if let Some(status) = self.child.try_wait().map_err(|error| error.to_string())? {
            return if status.success() {
                Ok(())
            } else {
                Err(format!("EDIT fixture exited: {status}"))
            };
        }
        if self.thread_id == 0 {
            return Err("EDIT fixture did not provide a message-loop thread".into());
        }
        let mut actual_pid = 0;
        let actual_thread = unsafe { GetWindowThreadProcessId(self.top, Some(&mut actual_pid)) };
        if actual_pid != self.child.id() || actual_thread != self.thread_id {
            return Err("EDIT fixture window identity changed before shutdown".into());
        }
        unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) }
            .map_err(|error| error.to_string())?;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = self.child.try_wait().map_err(|error| error.to_string())? {
                return if status.success() {
                    Ok(())
                } else {
                    Err(format!("EDIT fixture exited: {status}"))
                };
            }
            if Instant::now() >= deadline {
                return Err("EDIT fixture failed to exit before its deadline".into());
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}
impl Drop for EditTarget {
    fn drop(&mut self) {
        if let Err(error) = self.close() {
            eprintln!("{error}; cleaning up only the owned fixture process");
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[test]
#[ignore = "child fixture launched only by the mandatory core E2E test"]
fn edit_target_fixture() {
    require_hosted_input_desktop();
    let ready_path = PathBuf::from(std::env::var_os("OHV_CORE_EDIT_READY").unwrap());
    let app_data = PathBuf::from(std::env::var_os("APPDATA").unwrap());
    assert!(ready_path.is_absolute());
    assert!(ready_path
        .parent()
        .unwrap()
        .canonicalize()
        .unwrap()
        .starts_with(app_data.canonicalize().unwrap()));
    assert!(!ready_path.exists());
    unsafe {
        let instance = HINSTANCE(GetModuleHandleW(None).unwrap().0);
        let top = OwnerWindow(
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("STATIC"),
                w!("OpenHinoVoice CI input target"),
                WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                100,
                100,
                640,
                240,
                None,
                None,
                instance,
                None,
            )
            .unwrap(),
        );
        let edit = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            w!("EDIT"),
            w!("前置き:"),
            WS_CHILD | WS_VISIBLE | WINDOW_STYLE(0x0004 | 0x0040 | 0x0080),
            8,
            8,
            600,
            180,
            top.0,
            None,
            instance,
            None,
        )
        .unwrap();
        assert!(SetForegroundWindow(top.0).as_bool());
        let _ = SetFocus(edit);
        let caret = "前置き:".encode_utf16().count();
        SendMessageW(edit, 0x00B1, WPARAM(caret), LPARAM(caret as isize));
        let ready = EditReady {
            pid: std::process::id(),
            thread_id: GetCurrentThreadId(),
            top: top.0 .0 as isize,
            edit: edit.0 as isize,
        };
        let pending = ready_path.with_extension("tmp");
        std::fs::write(&pending, serde_json::to_vec(&ready).unwrap()).unwrap();
        std::fs::rename(pending, ready_path).unwrap();
        let mut message = MSG::default();
        loop {
            let result = GetMessageW(&mut message, None, 0, 0).0;
            assert_ne!(result, -1, "EDIT fixture message loop failed");
            if result == 0 {
                break;
            }
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

struct InstalledHook;
impl Drop for InstalledHook {
    fn drop(&mut self) {
        hook::uninstall();
    }
}

struct RunningApp(App);
impl Drop for RunningApp {
    fn drop(&mut self) {
        self.0.paste_transaction.take();
        self.0.teardown_session();
        if let Some(kept) = self.0.kept_worker.take() {
            kept.worker.shutdown();
        }
    }
}

fn key_press(vk: u16) {
    let make = |up| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk),
                dwFlags: if up {
                    KEYEVENTF_KEYUP
                } else {
                    Default::default()
                },
                ..Default::default()
            },
        },
    };
    let keys = [make(false), make(true)];
    assert_eq!(
        unsafe { SendInput(&keys, std::mem::size_of::<INPUT>() as i32) },
        2
    );
}

fn pump(app: &mut App, keys: &Receiver<hook::KeyEvent>) {
    unsafe {
        let mut message = MSG::default();
        while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
            assert_ne!(message.message, WM_QUIT);
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
    while let Ok(key) = keys.try_recv() {
        match key {
            hook::KeyEvent::Hotkey => {
                app.dispatch(Event::F8);
                hook::acknowledge_hotkey();
            }
            hook::KeyEvent::Enter => app.dispatch(Event::Enter),
            hook::KeyEvent::Esc => app.dispatch(Event::Esc),
        }
    }
    app.tick_session();
    app.tick_output(Instant::now());
    overlay::tick();
}

fn wait_for(
    app: &mut App,
    keys: &Receiver<hook::KeyEvent>,
    label: &str,
    ready: impl Fn(&App) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        pump(app, keys);
        if ready(app) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "Timed out waiting for {label}; state={:?}, recovery={}",
            app.state,
            app.recoveries.count()
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn clipboard_text(owner: HWND) -> String {
    unsafe {
        assert_ne!(OpenClipboard(owner.0), 0);
        let memory = GetClipboardData(13);
        if memory.is_null() {
            CloseClipboard();
            panic!("Clipboard must contain the seeded Unicode text");
        }
        let pointer = GlobalLock(memory) as *const u16;
        if pointer.is_null() {
            CloseClipboard();
            panic!("Clipboard Unicode data must be readable");
        }
        let bytes = GlobalSize(memory);
        let text = std::slice::from_raw_parts(pointer, bytes / 2);
        let length = text.iter().position(|&ch| ch == 0).unwrap();
        let value = String::from_utf16(&text[..length]).unwrap();
        GlobalUnlock(memory);
        CloseClipboard();
        value
    }
}

#[test]
#[ignore = "requires the dedicated GitHub-hosted input desktop; mandatory core CI step"]
fn record_enter_pastes_once_and_restores_clipboard() {
    require_hosted_input_desktop();
    let owner = OwnerWindow::new();
    let _owner_binding = overlay::test_owner(owner.0);
    let mut target = EditTarget::new();
    let (key_tx, key_rx) = channel();
    hook::install(key_tx, config::Hotkey { mods: 0, vk: 0x77 }).unwrap();
    let _hook = InstalledHook;
    let mut cfg = config::parse(config::EXAMPLE_TOML).unwrap();
    cfg.sounds = false;
    cfg.live_caption = false;
    cfg.partial_caption = false;
    cfg.edit_mode = false;
    cfg.recovery_history = false;
    cfg.keep_worker_minutes = 1;
    cfg.format.provider = "disabled".into();
    cfg.format.offline = false;
    cfg.format.save_history = false;
    cfg.format.usage_log = false;
    cfg.format.api_key.clear();
    cfg.format.openai_api_key.clear();
    let (terminals, lines) = channel();
    let (worker, sent) = asr::test_worker_with_recorder(lines);
    let key = WorkerKey::for_config(&cfg);
    let mut running = RunningApp(App::new(cfg));
    let app = &mut running.0;
    assert!(app.replay.is_none());
    app.kept_worker = Some(KeptWorker {
        worker,
        key,
        expires: Instant::now() + Duration::from_secs(60),
    });
    app.sync_mirror();
    let mut expected = String::from("前置き:");
    for (index, text) in ["音声入力テスト😀", "次の文章"].into_iter().enumerate() {
        let seeded_clipboard = format!("previous clipboard {index}");
        paste::copy_text_to_clipboard(&seeded_clipboard, owner.0).unwrap();
        assert_eq!(unsafe { GetForegroundWindow() }, target.top);
        let captured = paste::capture_target().unwrap();
        assert_eq!(captured.process_id(), target.child.id());
        assert_ne!(captured.process_id(), std::process::id());
        let samples = [vec![0.0; 3200], vec![0.3; 6400]].concat();
        app.fixture_recorder = Some(Recorder::fixture(16000, samples));
        key_press(0x77);
        wait_for(app, &key_rx, "F8 to start recording", |app| {
            app.state == State::Recording
        });
        assert_eq!(app.session.paste_target, Some(captured));
        assert!(app.session.recorder.is_some());
        key_press(0x0D);
        wait_for(app, &key_rx, "Enter to request recognition", |_| {
            sent.lock()
                .unwrap()
                .lines()
                .filter(|line| {
                    serde_json::from_str::<serde_json::Value>(line)
                        .is_ok_and(|v| v["type"] == "transcribe" && v["id"] == "s0")
                })
                .count()
                == index + 1
        });
        terminals
            .send(
                serde_json::json!({"type":"end", "id":"s0", "text":text, "stop":"complete"})
                    .to_string(),
            )
            .unwrap();
        wait_for(
            app,
            &key_rx,
            "recognized text to enter the paste transaction",
            |app| app.paste_transaction.is_some(),
        );
        expected.push_str(text);
        let deadline = Instant::now() + Duration::from_millis(200);
        while target.text() != expected {
            assert!(
                Instant::now() < deadline,
                "Native EDIT did not receive exactly one transcript: {:?}",
                target.text()
            );
            std::thread::yield_now();
        }
        if index == 1 {
            paste::copy_text_to_clipboard("newer independent copy", target.top).unwrap();
        }
        wait_for(app, &key_rx, "clipboard transaction completion", |app| {
            app.paste_transaction.is_none() && app.pending_paste.is_none()
        });
        assert_eq!(app.state, State::Idle);
        assert_eq!(app.recoveries.count(), 0);
        assert_eq!(
            target.text(),
            expected,
            "Enter must be swallowed and the transcript inserted once"
        );
        assert_eq!(
            clipboard_text(owner.0),
            if index == 0 {
                seeded_clipboard.as_str()
            } else {
                "newer independent copy"
            }
        );
        println!(
            "core_e2e: cycle {} passed; actual EDIT text matched; clipboard {}",
            index + 1,
            if index == 0 {
                "restored"
            } else {
                "preserved newer copy"
            }
        );
    }
    target
        .close()
        .expect("The owned EDIT process must exit cleanly");
    println!("core_e2e: separate target process exited successfully");
}
