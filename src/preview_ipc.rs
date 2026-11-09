use crate::feedback::{SoundEvent, SoundSet};
use crate::{config, sound};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    CloseHandle, BOOL, HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, WPARAM,
};
use windows::Win32::System::DataExchange::COPYDATASTRUCT;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::System::Threading::{
    GetCurrentProcessId, OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, EnumWindows, GetPropW,
    GetWindowThreadProcessId, RegisterClassW, RemovePropW, SendMessageTimeoutW, SetPropW,
    HWND_MESSAGE, SMTO_ABORTIFHUNG, SMTO_BLOCK, SMTO_ERRORONEXIT, WM_COPYDATA, WNDCLASSW,
};

const MAGIC: usize = 0x4F48_5650;
const VERSION: u8 = 1;
const MAX_MESSAGE: usize = 512;
const MAX_PENDING: usize = 8;
const REQUEST_MS: u64 = 100;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    version: u8,
    namespace: String,
    pid: u32,
    issued_ms: u64,
    expires_ms: u64,
    command: Command,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Command {
    Play {
        set: String,
        event: String,
        volume: u8,
        token: u64,
    },
    Stop {
        token: u64,
    },
}

impl Command {
    fn token(&self) -> u64 {
        match self {
            Self::Play { token, .. } | Self::Stop { token } => *token,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Play {
        set: SoundSet,
        event: SoundEvent,
        volume: u8,
        token: u64,
        pid: u32,
    },
    Stop(u64),
}

#[derive(Debug, Clone, Copy)]
struct Generation {
    pid: u32,
    token: u64,
    closed: bool,
}

struct Active {
    pid: u32,
    token: u64,
    expires: Instant,
}

#[derive(Default)]
struct Controller {
    available: bool,
    invalidated_ms: u64,
    generation: Option<Generation>,
    pending: VecDeque<Request>,
    active: Option<Active>,
}

impl Controller {
    fn set_available(&mut self, available: bool, now_ms: u64) -> Option<u64> {
        self.available = available;
        if available {
            return None;
        }
        self.invalidated_ms = now_ms;
        self.pending.clear();
        if let Some(generation) = &mut self.generation {
            generation.closed = true;
        }
        self.active.take().map(|active| active.token)
    }

    fn enqueue(&mut self, request: Request, now_ms: u64) -> bool {
        if !fresh(&request, now_ms) {
            return false;
        }
        let token = request.command.token();
        match &request.command {
            Command::Play { .. } => {
                if !self.available || request.issued_ms <= self.invalidated_ms {
                    return false;
                }
                if let Some(previous) = self.generation {
                    if token < previous.token
                        || (token == previous.token
                            && (previous.closed || previous.pid != request.pid))
                    {
                        return false;
                    }
                }
                if self.pending.len() >= MAX_PENDING {
                    return false;
                }
                self.generation = Some(Generation {
                    pid: request.pid,
                    token,
                    closed: false,
                });
            }
            Command::Stop { .. } => {
                let owner = self
                    .generation
                    .is_some_and(|g| g.pid == request.pid && g.token == token)
                    || self
                        .active
                        .as_ref()
                        .is_some_and(|a| a.pid == request.pid && a.token == token);
                if !owner {
                    return true;
                }
                self.pending
                    .retain(|queued| queued.command.token() != token);
                if let Some(generation) = &mut self.generation {
                    if generation.pid == request.pid && generation.token == token {
                        generation.closed = true;
                    }
                }
                // A stop is never dropped behind a full queue of obsolete previews.
                if self.pending.len() >= MAX_PENDING {
                    self.pending.pop_front();
                }
            }
        }
        self.pending.push_back(request);
        true
    }

    fn next_action(&mut self, now_ms: u64) -> Option<Action> {
        while let Some(request) = self.pending.pop_front() {
            match request.command {
                Command::Stop { token } => {
                    if self
                        .active
                        .as_ref()
                        .is_some_and(|active| active.token == token)
                    {
                        self.active = None;
                    }
                    return Some(Action::Stop(token));
                }
                Command::Play {
                    set,
                    event,
                    volume,
                    token,
                } => {
                    let current = self
                        .generation
                        .is_some_and(|g| g.pid == request.pid && g.token == token && !g.closed);
                    if !self.available
                        || !current
                        || request.issued_ms <= self.invalidated_ms
                        || now_ms > request.expires_ms
                    {
                        continue;
                    }
                    return Some(Action::Play {
                        set: SoundSet::from_id(&set)?,
                        event: event_from_id(&event)?,
                        volume,
                        token,
                        pid: request.pid,
                    });
                }
            }
        }
        None
    }

    fn expire(&mut self, now: Instant) -> Option<u64> {
        if self
            .active
            .as_ref()
            .is_some_and(|active| active.expires <= now)
        {
            self.active.take().map(|active| active.token)
        } else {
            None
        }
    }
}

fn controller() -> &'static Mutex<Controller> {
    static STATE: OnceLock<Mutex<Controller>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(Controller::default()))
}

fn namespace() -> String {
    config::mutex_name("OpenHinoVoice.Preview")
}

fn property(kind: &str) -> Vec<u16> {
    wide(&config::mutex_name(&format!(
        "OpenHinoVoice.Preview.{kind}"
    )))
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

fn now_ms() -> u64 {
    unsafe { GetTickCount64() }
}

fn event_from_id(value: &str) -> Option<SoundEvent> {
    SoundEvent::ALL
        .into_iter()
        .find(|event| event.id() == value)
}

fn fresh(request: &Request, now: u64) -> bool {
    request.command.token() != 0
        && request.issued_ms <= now
        && request.expires_ms > request.issued_ms
        && request.expires_ms - request.issued_ms <= REQUEST_MS
        && now <= request.expires_ms
}

fn decode(bytes: &[u8], expected_namespace: &str, pid: u32, now: u64) -> Option<Request> {
    if bytes.is_empty() || bytes.len() > MAX_MESSAGE {
        return None;
    }
    let request: Request = serde_json::from_slice(bytes).ok()?;
    if request.version != VERSION
        || request.namespace != expected_namespace
        || request.pid != pid
        || !fresh(&request, now)
    {
        return None;
    }
    if let Command::Play {
        set, event, volume, ..
    } = &request.command
    {
        if SoundSet::from_id(set).is_none() || event_from_id(event).is_none() || *volume > 100 {
            return None;
        }
    }
    Some(request)
}

pub fn new_token() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let time = now_ms().saturating_mul(1 << 16);
    NEXT.fetch_max(time, Ordering::Relaxed);
    NEXT.fetch_add(1, Ordering::Relaxed).saturating_add(1)
}

pub fn set_available(available: bool) {
    let token = controller()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .set_available(available, now_ms());
    if let Some(token) = token {
        sound::stop_preview(token);
    }
}

pub fn invalidate() {
    set_available(false);
}

pub fn tick(now: Instant) {
    if crate::hook::preview_blocked() {
        invalidate();
        return;
    }
    let expired = controller()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .expire(now);
    if let Some(token) = expired {
        sound::stop_preview(token);
    }
    for _ in 0..MAX_PENDING {
        let action = controller()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .next_action(now_ms());
        match action {
            Some(Action::Play {
                set,
                event,
                volume,
                token,
                pid,
            }) => {
                if crate::hook::preview_blocked() {
                    invalidate();
                    return;
                }
                let played = sound::play_preview(set, event, volume, token);
                if played {
                    let lifetime = sound::duration_for(set, event) + Duration::from_millis(100);
                    controller()
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .active = Some(Active {
                        pid,
                        token,
                        expires: now + lifetime.min(Duration::from_secs(2)),
                    });
                }
            }
            Some(Action::Stop(token)) => {
                sound::stop_preview(token);
            }
            None => break,
        }
    }
}

pub fn deadline() -> Option<Instant> {
    let state = controller().lock().unwrap_or_else(|p| p.into_inner());
    if !state.pending.is_empty() {
        Some(Instant::now())
    } else {
        state.active.as_ref().map(|active| active.expires)
    }
}

pub fn register_endpoint(window: HWND) -> Result<(), String> {
    let property = property("Resident");
    unsafe {
        SetPropW(
            window,
            PCWSTR(property.as_ptr()),
            HANDLE(GetCurrentProcessId() as usize as *mut _),
        )
        .map_err(|error| format!("試聴の受付を作れません: {error}"))
    }
}

pub fn unregister_endpoint(window: HWND) {
    invalidate();
    let property = property("Resident");
    unsafe {
        let _ = RemovePropW(window, PCWSTR(property.as_ptr()));
    }
}

fn window_pid(window: HWND) -> u32 {
    let mut pid = 0;
    unsafe {
        GetWindowThreadProcessId(window, Some(&mut pid));
    }
    pid
}

fn same_executable(pid: u32) -> bool {
    unsafe {
        let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return false;
        };
        let mut path = [0u16; 32768];
        let mut count = path.len() as u32;
        let result = QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            PWSTR(path.as_mut_ptr()),
            &mut count,
        );
        let _ = CloseHandle(process);
        let Ok(current) = std::env::current_exe() else {
            return false;
        };
        result.is_ok()
            && String::from_utf16_lossy(&path[..count as usize])
                .trim_start_matches(r"\\?\")
                .to_lowercase()
                == current
                    .to_string_lossy()
                    .trim_start_matches(r"\\?\")
                    .to_lowercase()
    }
}

/// COPYDATA memory belongs to the sender/OS and is copied before this callback returns.
///
/// # Safety
/// For WM_COPYDATA, `data` must be the valid COPYDATASTRUCT supplied to this window
/// by Windows, including its readable buffer for the duration of the callback.
pub unsafe fn handle_message(
    window: HWND,
    message: u32,
    sender: WPARAM,
    data: LPARAM,
) -> Option<LRESULT> {
    if message != WM_COPYDATA {
        return None;
    }
    if data.0 == 0 {
        return Some(LRESULT(0));
    }
    let packet = &*(data.0 as *const COPYDATASTRUCT);
    if packet.dwData != MAGIC {
        return None;
    }
    let sender = HWND(sender.0 as *mut _);
    let pid = window_pid(sender);
    let own_pid = GetCurrentProcessId();
    if pid == 0
        || pid == own_pid
        || window_pid(window) != own_pid
        || GetPropW(window, PCWSTR(property("Resident").as_ptr())).0 as usize != own_pid as usize
        || GetPropW(sender, PCWSTR(property("Client").as_ptr())).0 as usize != pid as usize
        || packet.cbData as usize > MAX_MESSAGE
        || packet.cbData == 0
        || packet.lpData.is_null()
        || !same_executable(pid)
    {
        return Some(LRESULT(0));
    }
    let bytes =
        std::slice::from_raw_parts(packet.lpData.cast::<u8>(), packet.cbData as usize).to_vec();
    let now = now_ms();
    let accepted = decode(&bytes, &namespace(), pid, now).is_some_and(|request| {
        if matches!(request.command, Command::Play { .. }) && crate::hook::preview_blocked() {
            return false;
        }
        controller()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .enqueue(request, now)
    });
    Some(LRESULT(isize::from(accepted)))
}

struct ClientWindow(HWND);

impl Drop for ClientWindow {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.0);
        }
    }
}

thread_local! {
    static CLIENT: RefCell<Option<(String, ClientWindow)>> = const { RefCell::new(None) };
}

unsafe extern "system" fn client_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    DefWindowProcW(window, message, wparam, lparam)
}

fn client_window() -> Result<HWND, String> {
    CLIENT.with(|slot| {
        let mut slot = slot.borrow_mut();
        let current = namespace();
        if let Some((name, window)) = slot.as_ref() {
            if *name == current {
                return Ok(window.0);
            }
        }
        let class = property("ClientWindow");
        unsafe {
            let instance = GetModuleHandleW(None).map_err(|error| error.to_string())?;
            let wc = WNDCLASSW {
                hInstance: instance.into(),
                lpfnWndProc: Some(client_proc),
                lpszClassName: PCWSTR(class.as_ptr()),
                ..Default::default()
            };
            RegisterClassW(&wc);
            let window = CreateWindowExW(
                Default::default(),
                PCWSTR(class.as_ptr()),
                PCWSTR::null(),
                Default::default(),
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                None,
                HINSTANCE(instance.0),
                None,
            )
            .map_err(|error| format!("試聴の送信元を作れません: {error}"))?;
            let client = ClientWindow(window);
            SetPropW(
                window,
                PCWSTR(property("Client").as_ptr()),
                HANDLE(GetCurrentProcessId() as usize as *mut _),
            )
            .map_err(|error| format!("試聴の送信元を識別できません: {error}"))?;
            *slot = Some((current, client));
            Ok(window)
        }
    })
}

struct Search {
    property: Vec<u16>,
    window: HWND,
}

unsafe extern "system" fn find_endpoint(window: HWND, data: LPARAM) -> BOOL {
    let search = &mut *(data.0 as *mut Search);
    let pid = window_pid(window);
    if pid != 0
        && pid != GetCurrentProcessId()
        && GetPropW(window, PCWSTR(search.property.as_ptr())).0 as usize == pid as usize
        && same_executable(pid)
    {
        search.window = window;
        BOOL(0)
    } else {
        BOOL(1)
    }
}

fn send(command: Command) -> Result<(), String> {
    let sender = client_window()?;
    let mut search = Search {
        property: property("Resident"),
        window: HWND::default(),
    };
    unsafe {
        let _ = EnumWindows(
            Some(find_endpoint),
            LPARAM((&mut search as *mut Search) as isize),
        );
    }
    if search.window.0.is_null() {
        return Err("本体に接続できないため、音なしで表示を試します。".into());
    }
    let issued_ms = now_ms();
    let request = Request {
        version: VERSION,
        namespace: namespace(),
        pid: unsafe { GetCurrentProcessId() },
        issued_ms,
        expires_ms: issued_ms + REQUEST_MS,
        command,
    };
    let bytes = serde_json::to_vec(&request).map_err(|error| error.to_string())?;
    if bytes.len() > MAX_MESSAGE {
        return Err("試聴要求が上限を超えています。".into());
    }
    let packet = COPYDATASTRUCT {
        dwData: MAGIC,
        cbData: bytes.len() as u32,
        lpData: bytes.as_ptr().cast_mut().cast(),
    };
    let mut result = 0;
    let sent = unsafe {
        SendMessageTimeoutW(
            search.window,
            WM_COPYDATA,
            WPARAM(sender.0 as usize),
            LPARAM((&packet as *const COPYDATASTRUCT) as isize),
            SMTO_ABORTIFHUNG | SMTO_BLOCK | SMTO_ERRORONEXIT,
            REQUEST_MS as u32,
            Some(&mut result),
        )
    };
    if sent.0 == 0 || result != 1 {
        Err("録音・処理中、または本体が応答しないため、音なしで表示を試します。".into())
    } else {
        Ok(())
    }
}

/// Ok means accepted for processing, not that audio has already started.
pub fn request_play(
    set: SoundSet,
    event: SoundEvent,
    volume: u8,
    token: u64,
) -> Result<(), String> {
    send(Command::Play {
        set: set.id().into(),
        event: event.id().into(),
        volume,
        token,
    })
}

pub fn request_stop(token: u64) -> Result<(), String> {
    send(Command::Stop { token })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn play(token: u64, issued: u64) -> Request {
        Request {
            version: VERSION,
            namespace: "test".into(),
            pid: 42,
            issued_ms: issued,
            expires_ms: issued + REQUEST_MS,
            command: Command::Play {
                set: "rich".into(),
                event: "done".into(),
                volume: 70,
                token,
            },
        }
    }

    #[test]
    fn parser_rejects_invalid_namespace_sender_ids_volume_and_deadline() {
        let valid = play(10, 100);
        let bytes = serde_json::to_vec(&valid).unwrap();
        assert!(decode(&bytes, "test", 42, 101).is_some());
        assert!(decode(&bytes, "other", 42, 101).is_none());
        assert!(decode(&bytes, "test", 43, 101).is_none());
        assert!(decode(&bytes, "test", 42, 201).is_none());
        for command in [
            Command::Play {
                set: "unknown".into(),
                event: "done".into(),
                volume: 70,
                token: 10,
            },
            Command::Play {
                set: "rich".into(),
                event: "unknown".into(),
                volume: 70,
                token: 10,
            },
            Command::Play {
                set: "rich".into(),
                event: "done".into(),
                volume: 101,
                token: 10,
            },
            Command::Stop { token: 0 },
        ] {
            let bytes = serde_json::to_vec(&Request {
                command,
                ..valid.clone()
            })
            .unwrap();
            assert!(decode(&bytes, "test", 42, 101).is_none());
        }
        assert!(decode(&vec![b' '; MAX_MESSAGE + 1], "test", 42, 101).is_none());
        let future = serde_json::to_vec(&play(11, 200)).unwrap();
        assert!(decode(&future, "test", 42, 100).is_none());
    }

    #[test]
    fn recording_invalidates_queued_and_later_cues_from_the_same_preview() {
        let mut state = Controller::default();
        state.set_available(true, 100);
        assert!(state.enqueue(play(10, 101), 101));
        state.set_available(false, 102);
        assert!(state.next_action(102).is_none());
        assert!(!state.enqueue(play(10, 103), 103));
        state.set_available(true, 104);
        assert!(!state.enqueue(play(10, 105), 105));
        assert!(state.enqueue(play(11, 105), 105));
        assert!(matches!(
            state.next_action(105),
            Some(Action::Play { token: 11, .. })
        ));
    }

    #[test]
    fn stale_stop_cannot_cancel_new_generation_and_stopped_play_cannot_restart() {
        let mut state = Controller::default();
        state.set_available(true, 100);
        assert!(state.enqueue(play(10, 101), 101));
        assert!(state.enqueue(play(11, 102), 102));
        assert!(matches!(
            state.next_action(102),
            Some(Action::Play { token: 11, .. })
        ));
        let mut stop = play(10, 103);
        stop.command = Command::Stop { token: 10 };
        assert!(state.enqueue(stop, 103));
        assert_eq!(state.next_action(103), None);
        assert!(state.enqueue(play(11, 104), 104));
        let mut stop = play(11, 105);
        stop.command = Command::Stop { token: 11 };
        assert!(state.enqueue(stop, 105));
        assert_eq!(state.next_action(105), Some(Action::Stop(11)));
        assert!(!state.enqueue(play(11, 106), 106));
    }

    #[test]
    fn timeout_before_main_loop_processing_discards_the_sound() {
        let mut state = Controller::default();
        state.set_available(true, 100);
        assert!(state.enqueue(play(10, 101), 101));
        assert!(state.next_action(202).is_none());
    }

    #[test]
    fn queue_is_bounded_and_expiry_only_returns_the_preview_token() {
        let mut state = Controller::default();
        state.set_available(true, 100);
        for token in 1..=MAX_PENDING as u64 {
            assert!(state.enqueue(play(token, 101), 101));
        }
        assert!(!state.enqueue(play(99, 101), 101));
        assert_eq!(state.pending.len(), MAX_PENDING);
        let now = Instant::now();
        state.active = Some(Active {
            pid: 42,
            token: 8,
            expires: now + Duration::from_millis(200),
        });
        assert_eq!(state.expire(now), None);
        assert_eq!(state.expire(now + Duration::from_millis(200)), Some(8));
        assert!(state.active.is_none());
    }

    static TRANSPORT_ACCEPTED: AtomicU64 = AtomicU64::new(0);

    unsafe extern "system" fn transport_proc(
        window: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if let Some(result) = handle_message(window, message, wparam, lparam) {
            if result.0 == 1 {
                TRANSPORT_ACCEPTED.fetch_add(1, Ordering::Relaxed);
            }
            return result;
        }
        DefWindowProcW(window, message, wparam, lparam)
    }

    // This helper only submits Stop; neither process calls the audio backend.
    #[test]
    fn native_transport_child() {
        if std::env::var_os("OHV_PREVIEW_TRANSPORT_CHILD").is_none() {
            return;
        }
        request_stop(new_token()).expect("hidden-window preview transport");
    }

    #[test]
    fn hidden_window_transport_accepts_a_bounded_request_from_the_same_executable() {
        use std::os::windows::io::AsRawHandle;
        use std::os::windows::process::CommandExt;
        use std::process::{Command as ProcessCommand, Stdio};
        use windows::Win32::UI::WindowsAndMessaging::{
            DispatchMessageW, MsgWaitForMultipleObjectsEx, PeekMessageW, MSG, MWMO_INPUTAVAILABLE,
            PM_REMOVE, QS_ALLINPUT,
        };

        let class = wide(&format!(
            "OpenHinoVoice.Preview.TransportTest.{}",
            std::process::id()
        ));
        let endpoint = unsafe {
            let instance = GetModuleHandleW(None).unwrap();
            let wc = WNDCLASSW {
                hInstance: instance.into(),
                lpfnWndProc: Some(transport_proc),
                lpszClassName: PCWSTR(class.as_ptr()),
                ..Default::default()
            };
            assert_ne!(RegisterClassW(&wc), 0);
            ClientWindow(
                CreateWindowExW(
                    Default::default(),
                    PCWSTR(class.as_ptr()),
                    PCWSTR::null(),
                    Default::default(),
                    0,
                    0,
                    0,
                    0,
                    None,
                    None,
                    HINSTANCE(instance.0),
                    None,
                )
                .unwrap(),
            )
        };
        register_endpoint(endpoint.0).unwrap();
        TRANSPORT_ACCEPTED.store(0, Ordering::Relaxed);
        let mut child = ProcessCommand::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "preview_ipc::tests::native_transport_child",
                "--nocapture",
            ])
            .env("OHV_PREVIEW_TRANSPORT_CHILD", "1")
            .creation_flags(0x0800_0000)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let timeout = Instant::now() + Duration::from_secs(10);
        loop {
            unsafe {
                let mut message = MSG::default();
                while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                    DispatchMessageW(&message);
                }
            }
            if child.try_wait().unwrap().is_some() {
                break;
            }
            let remaining = timeout.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                let _ = child.kill();
                let _ = child.wait();
                panic!("hidden-window preview transport timed out");
            }
            unsafe {
                MsgWaitForMultipleObjectsEx(
                    Some(&[HANDLE(child.as_raw_handle())]),
                    remaining.as_millis().min(u32::MAX as u128) as u32,
                    QS_ALLINPUT,
                    MWMO_INPUTAVAILABLE,
                );
            }
        }
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(TRANSPORT_ACCEPTED.load(Ordering::Relaxed), 1);
    }
}
