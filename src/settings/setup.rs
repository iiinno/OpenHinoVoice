use eframe::egui;
use std::io::Read;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{CloseHandle, FILETIME, HANDLE};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows::Win32::System::Pipes::PeekNamedPipe;
use windows::Win32::System::Threading::{
    GetProcessIdOfThread, GetThreadTimes, OpenThread, ResumeThread, CREATE_SUSPENDED,
    THREAD_QUERY_LIMITED_INFORMATION, THREAD_SUSPEND_RESUME,
};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const OUTPUT_LIMIT: usize = 512 * 1024;
const PROGRESS_LIMIT: usize = 512;

#[derive(Clone, Debug, PartialEq)]
pub enum ModelStatus {
    Running {
        message: String,
        fraction: Option<f32>,
    },
    Ready,
    Missing,
    Cancelled,
    Failed(String),
}

pub struct ModelJob {
    status: Arc<Mutex<ModelStatus>>,
    cancel: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    fetch: bool,
}

impl ModelJob {
    pub fn start(ctx: &egui::Context, exe: String, model: String, fetch: bool) -> Self {
        let status = Arc::new(Mutex::new(ModelStatus::Running {
            message: if fetch {
                "モデルを取得しています"
            } else {
                "モデルを確認しています"
            }
            .into(),
            fraction: None,
        }));
        let cancel = Arc::new(AtomicBool::new(false));
        let (out, flag, ctx) = (status.clone(), cancel.clone(), ctx.clone());
        let thread = std::thread::spawn(move || {
            let result = if fetch {
                run_command(&exe, &["pull", &model], &flag, &out, &ctx, None)
                    .and_then(|_| check_model(&exe, &model, &flag, &out, &ctx))
            } else {
                check_model(&exe, &model, &flag, &out, &ctx)
            };
            let final_status = completion_status(result, fetch, flag.load(Ordering::Acquire));
            *out.lock().unwrap_or_else(|p| p.into_inner()) = final_status;
            ctx.request_repaint();
        });
        Self {
            status,
            cancel,
            thread: Some(thread),
            fetch,
        }
    }

    pub fn status(&self) -> ModelStatus {
        self.status
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Release);
    }

    pub fn is_finished(&self) -> bool {
        self.thread.as_ref().is_none_or(JoinHandle::is_finished)
    }

    #[cfg(test)]
    pub(super) fn blocked_for_test(
        release: std::sync::mpsc::Receiver<()>,
    ) -> (Self, Arc<AtomicBool>) {
        let cancel = Arc::new(AtomicBool::new(false));
        let status = Arc::new(Mutex::new(ModelStatus::Running {
            message: "test worker".into(),
            fraction: None,
        }));
        let thread = std::thread::spawn(move || {
            let _ = release.recv_timeout(Duration::from_secs(5));
        });
        (
            Self {
                status,
                cancel: cancel.clone(),
                thread: Some(thread),
                fetch: false,
            },
            cancel,
        )
    }

    pub fn is_download(&self) -> bool {
        self.fetch
    }
}

fn completion_status(result: Result<bool, String>, fetch: bool, cancelled: bool) -> ModelStatus {
    if cancelled {
        return ModelStatus::Cancelled;
    }
    match result {
        Ok(true) => ModelStatus::Ready,
        Ok(false) if fetch => ModelStatus::Failed(
            "取得は終了しましたが、モデルを確認できません。もう一度確認してください".into(),
        ),
        Ok(false) => ModelStatus::Missing,
        Err(e) => ModelStatus::Failed(e),
    }
}

impl Drop for ModelJob {
    fn drop(&mut self) {
        self.cancel();
        // Settings retains cancelled jobs until is_finished; never join on its UI thread.
    }
}

fn check_model(
    exe: &str,
    model: &str,
    cancel: &AtomicBool,
    status: &Mutex<ModelStatus>,
    ctx: &egui::Context,
) -> Result<bool, String> {
    let path = std::path::Path::new(model);
    if path.is_file()
        || path.is_absolute()
        || model.to_ascii_lowercase().ends_with(".gguf")
        || model.contains('\\')
    {
        return Ok(path.is_file());
    }
    *status.lock().unwrap_or_else(|p| p.into_inner()) = ModelStatus::Running {
        message: "取得済みのモデルを確認しています".into(),
        fraction: None,
    };
    ctx.request_repaint();
    let json = run_command(
        exe,
        &["models", "--json"],
        cancel,
        status,
        ctx,
        Some(Duration::from_secs(15)),
    )?;
    catalog_contains(&json, model)
}

fn catalog_contains(json: &str, model: &str) -> Result<bool, String> {
    let catalog: serde_json::Value =
        serde_json::from_str(json).map_err(|e| format!("モデル一覧を読めません: {e}"))?;
    let entries = catalog
        .get("models")
        .and_then(|m| m.as_array())
        .ok_or_else(|| "モデル一覧に models がありません".to_owned())?;
    let (name, requested) = model.split_once(':').unwrap_or((model, ""));
    for entry in entries {
        if entry["name"].as_str() != Some(name) {
            continue;
        }
        let kind = if requested.is_empty() {
            entry["type"].as_str().unwrap_or("")
        } else {
            requested
        };
        return Ok(entry["files"].as_array().into_iter().flatten().any(|file| {
            file["type"].as_str() == Some(kind) && file["fetched"].as_bool() == Some(true)
        }));
    }
    Ok(false)
}

struct ProcessJob(HANDLE);
impl ProcessJob {
    fn new() -> Result<Self, String> {
        let handle = unsafe { CreateJobObjectW(None, None) }
            .map_err(|e| format!("モデル処理の管理を開始できません: {e}"))?;
        let job = Self(handle);
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        unsafe {
            SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                &limits as *const _ as _,
                std::mem::size_of_val(&limits) as u32,
            )
        }
        .map_err(|e| format!("モデル処理の終了管理を設定できません: {e}"))?;
        Ok(job)
    }
    fn assign(&self, child: &std::process::Child) -> Result<(), String> {
        unsafe { AssignProcessToJobObject(self.0, HANDLE(child.as_raw_handle())) }
            .map_err(|e| format!("モデル処理を終了管理に登録できません: {e}"))
    }
    fn stop(&self) {
        let _ = unsafe { TerminateJobObject(self.0, 1) };
    }
}
impl Drop for ProcessJob {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

// Rust 1.99 keeps Child::main_thread_handle behind an unstable feature. Find
// the oldest thread of our still-suspended child using the stable Win32 APIs.
fn resume_initial_thread(child: &std::process::Child) -> Result<(), String> {
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) }
        .map_err(|e| format!("モデル処理の初期スレッドを確認できません: {e}"))?;
    let snapshot = unsafe { OwnedHandle::from_raw_handle(snapshot.0) };
    let mut entry = THREADENTRY32 {
        dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
        ..Default::default()
    };
    unsafe { Thread32First(HANDLE(snapshot.as_raw_handle()), &mut entry) }
        .map_err(|e| format!("モデル処理のスレッド一覧を読めません: {e}"))?;
    let mut first: Option<(u64, OwnedHandle)> = None;
    let mut ambiguous = false;
    loop {
        if entry.th32OwnerProcessID == child.id() {
            let handle = unsafe {
                OpenThread(
                    THREAD_QUERY_LIMITED_INFORMATION | THREAD_SUSPEND_RESUME,
                    false,
                    entry.th32ThreadID,
                )
            }
            .map_err(|e| format!("モデル処理のスレッドを開けません: {e}"))?;
            let handle = unsafe { OwnedHandle::from_raw_handle(handle.0) };
            if unsafe { GetProcessIdOfThread(HANDLE(handle.as_raw_handle())) } != child.id() {
                return Err("モデル処理のスレッド所有者が変わりました".into());
            }
            let (mut created, mut exited, mut kernel, mut user) = (
                FILETIME::default(),
                FILETIME::default(),
                FILETIME::default(),
                FILETIME::default(),
            );
            unsafe {
                GetThreadTimes(
                    HANDLE(handle.as_raw_handle()),
                    &mut created,
                    &mut exited,
                    &mut kernel,
                    &mut user,
                )
            }
            .map_err(|e| format!("モデル処理の初期スレッドを識別できません: {e}"))?;
            let created =
                (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime);
            if first.as_ref().is_none_or(|(oldest, _)| created < *oldest) {
                first = Some((created, handle));
                ambiguous = false;
            } else if first.as_ref().is_some_and(|(oldest, _)| created == *oldest) {
                ambiguous = true;
            }
        }
        entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
        match unsafe { Thread32Next(HANDLE(snapshot.as_raw_handle()), &mut entry) } {
            Ok(()) => {}
            Err(error)
                if error.code()
                    == windows::core::HRESULT::from_win32(
                        windows::Win32::Foundation::ERROR_NO_MORE_FILES.0,
                    ) =>
            {
                break
            }
            Err(error) => return Err(format!("モデル処理のスレッド一覧を読めません: {error}")),
        }
    }
    if ambiguous {
        return Err("モデル処理の初期スレッドを一意に識別できません".into());
    }
    let (_, thread) = first.ok_or("モデル処理の初期スレッドがありません")?;
    if unsafe { GetProcessIdOfThread(HANDLE(thread.as_raw_handle())) } != child.id() {
        return Err("モデル処理のスレッド所有者が変わりました".into());
    }
    let previous = unsafe { ResumeThread(HANDLE(thread.as_raw_handle())) };
    if previous != 1 {
        return Err(format!(
            "モデル処理を安全に開始できません（停止数 {previous}）"
        ));
    }
    Ok(())
}

#[derive(Default)]
struct Capture {
    bytes: Vec<u8>,
    overflow: bool,
    latest: String,
    line: Vec<u8>,
}
impl Capture {
    fn push(&mut self, bytes: &[u8], progress: bool) {
        let remaining = OUTPUT_LIMIT.saturating_sub(self.bytes.len());
        self.bytes
            .extend_from_slice(&bytes[..bytes.len().min(remaining)]);
        self.overflow |= bytes.len() > remaining;
        if progress {
            for byte in bytes {
                if *byte == b'\n' || *byte == b'\r' {
                    if !self.line.is_empty() {
                        self.latest = clean_progress(&String::from_utf8_lossy(&self.line));
                        self.line.clear();
                    }
                } else if self.line.len() < PROGRESS_LIMIT * 4 {
                    self.line.push(*byte);
                }
            }
            if !self.line.is_empty() {
                self.latest = clean_progress(&String::from_utf8_lossy(&self.line));
            }
        }
    }
}

// There is one reader per handle. Reading no more than the bytes PeekNamedPipe
// reports cannot wait for EOF, even if a descendant keeps its write end open.
fn drain_available(
    pipe: &mut (impl Read + AsRawHandle),
    output: &mut Capture,
    progress: bool,
) -> Result<(), String> {
    let mut budget = 64 * 1024;
    let mut buffer = [0_u8; 4096];
    while budget > 0 {
        let mut available = 0;
        if let Err(error) = unsafe {
            PeekNamedPipe(
                HANDLE(pipe.as_raw_handle()),
                None,
                0,
                None,
                Some(&mut available),
                None,
            )
        } {
            if error.code().0 as u32 == 0x8007006d {
                break;
            } // ERROR_BROKEN_PIPE is normal EOF.
            return Err(format!("モデル処理の出力を読めません: {error}"));
        }
        let count = (available as usize).min(buffer.len()).min(budget);
        if count == 0 {
            break;
        }
        let count = pipe
            .read(&mut buffer[..count])
            .map_err(|e| format!("モデル処理の出力を読めません: {e}"))?;
        if count == 0 {
            break;
        }
        output.push(&buffer[..count], progress);
        budget -= count;
    }
    Ok(())
}

fn run_command(
    exe: &str,
    args: &[&str],
    cancel: &AtomicBool,
    status: &Mutex<ModelStatus>,
    ctx: &egui::Context,
    timeout: Option<Duration>,
) -> Result<String, String> {
    if cancel.load(Ordering::Acquire) {
        return Err("中断しました".into());
    }
    let process_job = ProcessJob::new()?;
    let mut child = Command::new(exe)
        .args(args)
        .creation_flags(CREATE_NO_WINDOW | CREATE_SUSPENDED.0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("speech.cpp を起動できません: {e}"))?;
    if let Err(error) = process_job
        .assign(&child)
        .and_then(|_| resume_initial_thread(&child))
    {
        process_job.stop();
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    let mut stdout_pipe = child.stdout.take().expect("piped stdout");
    let mut stderr_pipe = child.stderr.take().expect("piped stderr");
    let mut stdout = Capture::default();
    let mut stderr = Capture::default();
    let started = Instant::now();
    let mut last = String::new();
    let outcome = loop {
        if cancel.load(Ordering::Acquire) || timeout.is_some_and(|limit| started.elapsed() >= limit)
        {
            process_job.stop();
            let _ = child.kill();
            let _ = child.wait();
            break Err(if cancel.load(Ordering::Acquire) {
                "中断しました"
            } else {
                "モデルの確認が時間切れになりました"
            }
            .to_owned());
        }
        if let Err(error) = drain_available(&mut stdout_pipe, &mut stdout, false)
            .and_then(|_| drain_available(&mut stderr_pipe, &mut stderr, true))
        {
            process_job.stop();
            let _ = child.kill();
            let _ = child.wait();
            break Err(error);
        }
        match child.try_wait() {
            Ok(Some(exit)) => {
                drain_available(&mut stdout_pipe, &mut stdout, false)?;
                drain_available(&mut stderr_pipe, &mut stderr, true)?;
                break Ok(exit);
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                break Err(format!("speech.cpp の終了を確認できません: {e}"));
            }
            Ok(None) => {}
        }
        let progress = stderr.latest.clone();
        if !progress.is_empty() && progress != last {
            last.clone_from(&progress);
            *status.lock().unwrap_or_else(|p| p.into_inner()) = ModelStatus::Running {
                fraction: progress_fraction(&progress).map(|f| f.min(0.99)),
                message: progress,
            };
            ctx.request_repaint();
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    process_job.stop();
    let exit = outcome?;
    if !exit.success() {
        let out = &stderr;
        let reason = if out.latest.is_empty() {
            format!("終了コード {}", exit)
        } else {
            out.latest.clone()
        };
        return Err(format!("モデルを処理できません: {reason}"));
    }
    let out = &stdout;
    if out.overflow {
        return Err("speech.cpp の応答が大きすぎます".into());
    }
    String::from_utf8(out.bytes.clone())
        .map_err(|_| "speech.cpp の応答を文字として読めません".into())
}

fn clean_progress(line: &str) -> String {
    let mut ansi = false;
    line.chars()
        .filter(|c| {
            if *c == '\u{1b}' {
                ansi = true;
                return false;
            }
            if ansi {
                if c.is_ascii_alphabetic() {
                    ansi = false;
                }
                return false;
            }
            !c.is_control()
        })
        .take(PROGRESS_LIMIT)
        .collect::<String>()
        .trim()
        .to_owned()
}

fn progress_fraction(line: &str) -> Option<f32> {
    let end = line.find('%')?;
    let number: String = line[..end]
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_digit() || matches!(*c, '.' | '-'))
        .collect();
    let percent: f32 = number.chars().rev().collect::<String>().parse().ok()?;
    (percent.is_finite() && (0.0..=100.0).contains(&percent)).then_some(percent / 100.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_only_uses_a_valid_reported_percentage() {
        assert_eq!(progress_fraction("download 42.5%"), Some(0.425));
        assert_eq!(progress_fraction("download started"), None);
        assert_eq!(progress_fraction("download 120%"), None);
        assert_eq!(progress_fraction("download -5%"), None);
        assert_eq!(progress_fraction("NaN%"), None);
        assert_eq!(clean_progress("\u{1b}[32m取得中\u{1b}[0m\r"), "取得中");
    }

    #[test]
    fn catalog_requires_the_selected_variant_to_be_fetched() {
        let json = r#"{"models":[{"name":"haas","type":"q8_0","files":[{"type":"q4_k","fetched":true},{"type":"q8_0","fetched":false}]}]}"#;
        assert!(catalog_contains(json, "haas:q4_k").unwrap());
        assert!(!catalog_contains(json, "haas").unwrap());
        assert!(!catalog_contains(json, "other:q4_k").unwrap());
        assert!(catalog_contains("{}", "haas").is_err());
    }

    #[test]
    fn pipe_capture_is_bounded_but_drains_all_input() {
        let bytes = vec![b'x'; OUTPUT_LIMIT + 9000];
        let mut out = Capture::default();
        for chunk in bytes.chunks(4096) {
            out.push(chunk, false);
        }
        assert_eq!(out.bytes.len(), OUTPUT_LIMIT);
        assert!(out.overflow);
    }

    #[test]
    fn completion_requires_verified_model_and_honors_cancellation() {
        assert_eq!(completion_status(Ok(true), true, false), ModelStatus::Ready);
        assert!(matches!(
            completion_status(Ok(false), true, false),
            ModelStatus::Failed(_)
        ));
        assert_eq!(
            completion_status(Ok(false), false, false),
            ModelStatus::Missing
        );
        assert_eq!(
            completion_status(Ok(true), true, true),
            ModelStatus::Cancelled
        );
        assert!(matches!(
            completion_status(Err("exit 1".into()), true, false),
            ModelStatus::Failed(_)
        ));
    }

    #[test]
    fn cancelling_a_running_command_reaps_the_child_promptly() {
        let flag = Arc::new(AtomicBool::new(false));
        let trigger = flag.clone();
        let cancel_thread = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            trigger.store(true, Ordering::Release);
        });
        let status = Mutex::new(ModelStatus::Missing);
        let exe = std::env::current_exe().unwrap();
        let started = Instant::now();
        let result = run_command(
            exe.to_str().unwrap(),
            &[
                "--ignored",
                "--exact",
                "settings::setup::tests::waiting_child_fixture",
                "--nocapture",
            ],
            &flag,
            &status,
            &egui::Context::default(),
            None,
        );
        cancel_thread.join().unwrap();
        assert_eq!(result.unwrap_err(), "中断しました");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn dropping_a_model_job_does_not_join_a_busy_worker_on_the_ui_thread() {
        let (release, wait) = std::sync::mpsc::channel();
        let (done, finished) = std::sync::mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let thread = std::thread::spawn(move || {
            let _ = wait.recv_timeout(Duration::from_secs(2));
            let _ = done.send(());
        });
        let job = ModelJob {
            status: Arc::new(Mutex::new(ModelStatus::Missing)),
            cancel: cancel.clone(),
            thread: Some(thread),
            fetch: false,
        };
        let started = Instant::now();
        drop(job);
        assert!(started.elapsed() < Duration::from_millis(250));
        assert!(cancel.load(Ordering::Acquire));
        release.send(()).unwrap();
        finished.recv_timeout(Duration::from_secs(2)).unwrap();
    }

    #[test]
    fn a_child_cannot_run_or_launch_descendants_before_job_assignment() {
        let job = ProcessJob::new().unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "settings::setup::tests::pipe_holder_parent_fixture",
                "--nocapture",
            ])
            .creation_flags(CREATE_NO_WINDOW | CREATE_SUSPENDED.0)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut pipe = child.stdout.take().unwrap();
        // Deliberately widen the old spawn-to-assign race. The test harness and
        // its descendant-producing fixture must both remain stopped here.
        std::thread::sleep(Duration::from_millis(150));
        let mut before = Capture::default();
        let drained = drain_available(&mut pipe, &mut before, false);
        if let Err(error) = drained
            .and_then(|_| job.assign(&child))
            .and_then(|_| resume_initial_thread(&child))
        {
            job.stop();
            let _ = child.kill();
            let _ = child.wait();
            panic!("{error}");
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(5))
                }
                other => {
                    job.stop();
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("resumed fixture did not finish: {other:?}");
                }
            }
        };
        job.stop();
        let mut after = Capture::default();
        drain_available(&mut pipe, &mut after, false).unwrap();
        assert!(
            before.bytes.is_empty(),
            "suspended child executed before assignment"
        );
        assert!(status.success());
        assert!(String::from_utf8_lossy(&after.bytes).contains("descendant_pid="));
    }

    #[test]
    fn a_descendant_holding_output_pipes_cannot_delay_command_completion() {
        use windows::Win32::System::Threading::{
            OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE,
        };
        let exe = std::env::current_exe().unwrap();
        let started = Instant::now();
        let output = run_command(
            exe.to_str().unwrap(),
            &[
                "--ignored",
                "--exact",
                "settings::setup::tests::pipe_holder_parent_fixture",
                "--nocapture",
            ],
            &AtomicBool::new(false),
            &Mutex::new(ModelStatus::Missing),
            &egui::Context::default(),
            Some(Duration::from_secs(5)),
        )
        .unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "EOF must not wait for the descendant"
        );
        let pid: u32 = output
            .split("descendant_pid=")
            .nth(1)
            .expect("the descendant was launched")
            .split_whitespace()
            .next()
            .unwrap()
            .parse()
            .unwrap();
        if let Ok(handle) = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) } {
            let stopped = unsafe { WaitForSingleObject(handle, 2000) };
            let _ = unsafe { CloseHandle(handle) };
            assert_eq!(stopped.0, 0, "the command's job must reap its descendant");
        }
    }

    #[test]
    #[ignore = "subprocess fixture with an inherited-pipe descendant"]
    #[allow(clippy::zombie_processes)] // The owning test reaps this child through its process job.
    fn pipe_holder_parent_fixture() {
        let child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "settings::setup::tests::waiting_child_fixture",
                "--nocapture",
            ])
            .creation_flags(CREATE_NO_WINDOW)
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        println!("descendant_pid={}", child.id());
        // Exiting leaves both writers inherited by the descendant. The owner job
        // must stop it without waiting for the descendant's thirty-second sleep.
    }

    #[test]
    #[ignore = "subprocess fixture for cancellation; invoked by its owning test"]
    fn waiting_child_fixture() {
        std::thread::sleep(Duration::from_secs(30));
    }
}
