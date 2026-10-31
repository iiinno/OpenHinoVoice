use base64::Engine;
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::os::windows::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{channel, sync_channel, Receiver, SyncSender, TryRecvError, TrySendError};
use std::time::{Duration, Instant};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

// Initial model setup can take longer than an ordinary transcription. Both
// deadlines are checked by the UI loop so a silent worker can be torn down.
const READY_TIMEOUT: Duration = Duration::from_secs(180);
const TRANSCRIPTION_TIMEOUT: Duration = Duration::from_secs(90);
// Each chunk is queued independently, so neither a non-reading child nor a
// slow pipe may hold the UI loop in a synchronous stdin write.
const WRITE_QUEUE_CAPACITY: usize = 64;
const WRITE_TIMEOUT: Duration = Duration::from_secs(8);

pub fn timeout_phase(
    session_started: Instant,
    ready: bool,
    transcribe_started: Option<Instant>,
    now: Instant,
) -> Option<&'static str> {
    if !ready && now.saturating_duration_since(session_started) >= READY_TIMEOUT {
        Some("ready")
    } else if let Some(started) = transcribe_started {
        if now.saturating_duration_since(started) >= TRANSCRIPTION_TIMEOUT {
            Some("end")
        } else {
            None
        }
    } else {
        None
    }
}

#[derive(Debug)]
pub enum AsrError {
    Spawn(String),
    Io(String),
    #[allow(dead_code)] // Blocking integration tests can report a timeout.
    Timeout(String),
    Worker(String),
    Protocol(String),
}

impl std::fmt::Display for AsrError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AsrError::Spawn(e) => write!(f, "認識エンジンを起動できません: {e}"),
            AsrError::Io(e) => write!(f, "認識エンジンとの通信に失敗しました: {e}"),
            AsrError::Timeout(what) => write!(f, "認識が時間切れです: {what}"),
            AsrError::Worker(e) => write!(f, "認識エラー: {e}"),
            AsrError::Protocol(e) => write!(f, "認識エンジンの応答が不正です: {e}"),
        }
    }
}

pub fn chunk_message(seq: u64, pcm_le: &[u8]) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(pcm_le);
    serde_json::json!({
        "type": "chunk",
        "id": "r",
        "seq": seq,
        "pcm": encoded,
    })
    .to_string()
}

pub fn transcribe_message(sample_rate: u32, words: &str) -> String {
    serde_json::json!({
        "type": "transcribe",
        "id": "r",
        "sample_rate": sample_rate,
        "words": words,
    })
    .to_string()
}

// 0.9.4 installed on Windows uses protocol 3; current speech.cpp uses 4.
fn parse_ready(line: &str) -> Result<bool, AsrError> {
    let value: serde_json::Value =
        serde_json::from_str(line).map_err(|e| AsrError::Protocol(e.to_string()))?;
    match value["type"].as_str() {
        Some("ready") => match value["protocol"].as_u64() {
            Some(3 | 4) => Ok(true),
            _ => Err(AsrError::Protocol(
                "未対応の worker protocol です（3 または 4 が必要）".into(),
            )),
        },
        Some("fatal" | "error") => Err(worker_error(&value)),
        _ => Ok(false),
    }
}

fn worker_error(value: &serde_json::Value) -> AsrError {
    AsrError::Worker(
        value["error"]["message"]
            .as_str()
            .unwrap_or("worker がエラーを返しました")
            .to_string(),
    )
}

fn parse_end(line: &str) -> Result<Option<String>, AsrError> {
    let value: serde_json::Value =
        serde_json::from_str(line).map_err(|e| AsrError::Protocol(e.to_string()))?;
    match value["type"].as_str() {
        Some("end") => {
            if value["id"].as_str() != Some("r") {
                return Err(AsrError::Protocol(
                    "認識結果の request id が一致しません".into(),
                ));
            }
            if value["stop"].as_str() != Some("complete") {
                return Err(AsrError::Worker("認識結果が途中で終了しました".into()));
            }
            match value["text"].as_str() {
                Some(text) if !text.trim().is_empty() => Ok(Some(text.to_string())),
                _ => Err(AsrError::Worker(
                    "音声を認識できませんでした（結果が空です）".into(),
                )),
            }
        }
        Some("fatal" | "error") => Err(worker_error(&value)),
        Some("cancelled") => Err(AsrError::Worker("認識がキャンセルされました".into())),
        _ => Ok(None),
    }
}

struct WriteCommand {
    id: u64,
    line: String,
}

struct WriteAck {
    id: u64,
    result: Result<(), String>,
}

struct WriterQueue {
    sender: SyncSender<WriteCommand>,
    acknowledgements: Receiver<WriteAck>,
    pending: VecDeque<(u64, Instant)>,
    next_id: u64,
}

impl WriterQueue {
    fn spawn<W: Write + Send + 'static>(mut stdin: W) -> Self {
        let (sender, commands) = sync_channel::<WriteCommand>(WRITE_QUEUE_CAPACITY);
        let (ack_sender, acknowledgements) = channel::<WriteAck>();
        // Detached on purpose. A blocked Windows pipe must never be joined
        // from the UI thread. Killing the child closes its read end.
        std::thread::spawn(move || {
            while let Ok(command) = commands.recv() {
                let result = (|| {
                    stdin.write_all(command.line.as_bytes())?;
                    stdin.write_all(b"\n")?;
                    stdin.flush()
                })()
                .map_err(|e| e.to_string());
                let failed = result.is_err();
                if ack_sender
                    .send(WriteAck {
                        id: command.id,
                        result,
                    })
                    .is_err()
                    || failed
                {
                    break;
                }
            }
        });
        Self {
            sender,
            acknowledgements,
            pending: VecDeque::new(),
            next_id: 0,
        }
    }

    /// False means temporary backpressure: caller must retain and retry the
    /// same message on a later tick. Order and sequence numbers are preserved.
    fn enqueue(&mut self, line: String) -> Result<bool, AsrError> {
        let id = self.next_id;
        match self.sender.try_send(WriteCommand { id, line }) {
            Ok(()) => {
                self.next_id = self.next_id.checked_add(1).expect("ASR write id exhausted");
                self.pending.push_back((id, Instant::now()));
                Ok(true)
            }
            Err(TrySendError::Full(_)) => Ok(false),
            Err(TrySendError::Disconnected(_)) => Err(AsrError::Io(
                "認識エンジンの送信スレッドが終了しました".into(),
            )),
        }
    }

    fn poll_at(&mut self, now: Instant) -> Result<(), AsrError> {
        loop {
            match self.acknowledgements.try_recv() {
                Ok(ack) => {
                    let Some((expected, _)) = self.pending.pop_front() else {
                        return Err(AsrError::Protocol("想定外の stdin 送信完了通知".into()));
                    };
                    if expected != ack.id {
                        return Err(AsrError::Protocol(
                            "stdin 送信完了通知の順序が不正です".into(),
                        ));
                    }
                    ack.result.map_err(AsrError::Io)?;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    return Err(AsrError::Io(
                        "認識エンジンの送信スレッドが終了しました".into(),
                    ));
                }
            }
        }
        if self
            .pending
            .front()
            .is_some_and(|(_, queued)| now.saturating_duration_since(*queued) >= WRITE_TIMEOUT)
        {
            return Err(AsrError::Timeout("stdin 書き込み".into()));
        }
        Ok(())
    }

    #[cfg(test)]
    fn pending_len(&self) -> usize {
        self.pending.len()
    }
}

pub struct Worker {
    child: Child,
    writer: WriterQueue,
    lines: Receiver<String>,
    seq: u64,
    ready: bool,
}

impl Worker {
    pub fn spawn(exe: &str, model: &str) -> Result<Worker, AsrError> {
        let mut child = Command::new(exe)
            .arg("worker")
            .arg(model)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            // speech.exe はコンソールアプリなので、指定しないと録音のたびに黒い窓が開く
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .map_err(|e| AsrError::Spawn(e.to_string()))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| AsrError::Spawn("stdin を取得できません".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| AsrError::Spawn("stdout を取得できません".into()))?;
        let (tx, rx) = channel::<String>();
        let writer = WriterQueue::spawn(stdin);
        std::thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                match line {
                    Ok(l) => {
                        if tx.send(l).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        Ok(Worker {
            child,
            writer,
            lines: rx,
            seq: 0,
            ready: false,
        })
    }

    #[allow(dead_code)] // Used by the explicit, ignored ASR integration test.
    pub fn wait_ready(&mut self, timeout: Duration) -> Result<(), AsrError> {
        if self.ready {
            return Ok(());
        }
        let deadline = Instant::now() + timeout;
        loop {
            let remain = deadline.saturating_duration_since(Instant::now());
            if remain.is_zero() {
                return Err(AsrError::Timeout("ready".into()));
            }
            match self.lines.recv_timeout(remain) {
                Ok(line) => {
                    if parse_ready(&line)? {
                        self.ready = true;
                        return Ok(());
                    }
                }
                Err(_) => return Err(AsrError::Timeout("ready".into())),
            }
        }
    }

    pub fn is_ready(&self) -> bool {
        self.ready
    }

    pub fn try_ready(&mut self) -> Result<bool, AsrError> {
        if self.ready {
            return Ok(true);
        }
        match self.lines.try_recv() {
            Ok(line) => {
                self.ready = parse_ready(&line)?;
                Ok(self.ready)
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => Ok(false),
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                Err(AsrError::Worker("worker が終了しました".into()))
            }
        }
    }

    pub fn send_chunk(&mut self, pcm_le: &[u8]) -> Result<bool, AsrError> {
        let msg = chunk_message(self.seq, pcm_le);
        if self.writer.enqueue(msg)? {
            self.seq += 1;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub fn transcribe(&mut self, sample_rate: u32, words: &str) -> Result<bool, AsrError> {
        let msg = transcribe_message(sample_rate, words);
        self.writer.enqueue(msg)
    }

    /// Nonblocking acknowledgement + age check; called every UI tick even
    /// when the recognition process is no longer responding on stdout.
    pub fn poll_writer(&mut self) -> Result<(), AsrError> {
        self.writer.poll_at(Instant::now())
    }

    #[allow(dead_code)] // Used by the explicit, ignored ASR integration test.
    pub fn wait_end(&mut self, timeout: Duration) -> Result<String, AsrError> {
        let deadline = Instant::now() + timeout;
        loop {
            let remain = deadline.saturating_duration_since(Instant::now());
            if remain.is_zero() {
                return Err(AsrError::Timeout("end".into()));
            }
            match self.lines.recv_timeout(remain) {
                Ok(line) => {
                    if let Some(text) = parse_end(&line)? {
                        return Ok(text);
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    return Err(AsrError::Timeout("end".into()))
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(AsrError::Worker("worker が終了しました".into()))
                }
            }
        }
    }

    pub fn poll_end(&mut self) -> Option<Result<String, AsrError>> {
        match self.lines.try_recv() {
            Ok(line) => parse_end(&line).transpose(),
            Err(std::sync::mpsc::TryRecvError::Empty) => None,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                Some(Err(AsrError::Worker("worker が終了しました".into())))
            }
        }
    }

    pub fn shutdown(mut self) {
        drop(self.writer);
        // Terminate first; a child stuck reading neither stdin nor stdout
        // cannot be asked to exit gracefully. Never join the writer thread.
        let _ = self.child.kill();
        let deadline = Instant::now() + Duration::from_millis(250);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => {
                    if Instant::now() >= deadline {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(_) => return,
            }
        }
    }
}

#[allow(dead_code)] // Used by the explicit ASR WAV integration test.
pub struct WavData {
    pub sample_rate: u32,
    pub pcm_le: Vec<u8>,
}

#[allow(dead_code)] // Used by the explicit ASR WAV integration test.
pub fn parse_wav_pcm16(bytes: &[u8]) -> Result<WavData, String> {
    if bytes.len() < 44 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err("WAV ヘッダが不正です".into());
    }
    let mut pos = 12usize;
    let mut sample_rate = 0u32;
    let mut channels = 1u16;
    let mut bits = 0u16;
    let mut data: Option<&[u8]> = None;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let size = u32::from_le_bytes([
            bytes[pos + 4],
            bytes[pos + 5],
            bytes[pos + 6],
            bytes[pos + 7],
        ]) as usize;
        let body_start = pos + 8;
        let body_end = (body_start + size).min(bytes.len());
        if id == b"fmt " {
            if body_end - body_start >= 16 {
                channels = u16::from_le_bytes([bytes[body_start + 2], bytes[body_start + 3]]);
                sample_rate = u32::from_le_bytes([
                    bytes[body_start + 4],
                    bytes[body_start + 5],
                    bytes[body_start + 6],
                    bytes[body_start + 7],
                ]);
                bits = u16::from_le_bytes([bytes[body_start + 14], bytes[body_start + 15]]);
            }
        } else if id == b"data" {
            data = Some(&bytes[body_start..body_end]);
        }
        pos = body_start + size + (size % 2);
    }
    if bits != 16 {
        return Err(format!("16bit PCM のみ対応です (bits={bits})"));
    }
    let raw = data.ok_or("data チャンクがありません")?;
    let pcm = if channels <= 1 {
        raw.to_vec()
    } else {
        let mut mono = Vec::with_capacity(raw.len() / channels as usize);
        for frame in raw.chunks_exact(2 * channels as usize) {
            let mut sum: i32 = 0;
            for ch in 0..channels as usize {
                let s = i16::from_le_bytes([frame[ch * 2], frame[ch * 2 + 1]]);
                sum += s as i32;
            }
            let avg = (sum / channels as i32) as i16;
            mono.extend_from_slice(&avg.to_le_bytes());
        }
        mono
    };
    Ok(WavData {
        sample_rate,
        pcm_le: pcm,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::Sender;

    #[test]
    fn ready_checks_installed_and_current_protocols_and_startup_failure() {
        assert!(parse_ready(r#"{"type":"ready","protocol":3,"version":"0.9.4"}"#).unwrap());
        assert!(parse_ready(r#"{"type":"ready","protocol":4,"version":"0.10.0"}"#).unwrap());
        for line in [
            r#"{"type":"ready","protocol":2}"#,
            r#"{"type":"ready","protocol":5}"#,
            r#"{"type":"ready"}"#,
        ] {
            assert!(matches!(parse_ready(line), Err(AsrError::Protocol(_))));
        }
        assert!(matches!(
            parse_ready(r#"{"type":"fatal","error":{"code":"model_file","message":"missing model"}}"#),
            Err(AsrError::Worker(message)) if message == "missing model"
        ));
        assert!(!parse_ready(r#"{"type":"progress","done":0.5}"#).unwrap());
        assert!(matches!(
            parse_ready("not json"),
            Err(AsrError::Protocol(_))
        ));
    }

    #[test]
    fn recognition_end_requires_complete_nonempty_text_and_matching_id() {
        assert_eq!(
            parse_end(r#"{"type":"end","id":"r","text":"おはよう。","stop":"complete"}"#).unwrap(),
            Some("おはよう。".into())
        );
        for line in [
            r#"{"type":"end","id":"r","text":"partial","stop":"model_limit"}"#,
            r#"{"type":"end","id":"r","text":"","stop":"complete"}"#,
            r#"{"type":"end","id":"r","text":"   ","stop":"complete"}"#,
            r#"{"type":"end","id":"r","stop":"complete"}"#,
            r#"{"type":"end","id":"r","text":"result"}"#,
        ] {
            assert!(parse_end(line).unwrap_err().to_string().contains("認識"));
        }
        assert!(matches!(
            parse_end(r#"{"type":"end","id":"other","text":"x","stop":"complete"}"#),
            Err(AsrError::Protocol(_))
        ));
        assert!(matches!(
            parse_end(r#"{"type":"error","id":"r","error":{"message":"failed"}}"#),
            Err(AsrError::Worker(message)) if message == "failed"
        ));
        assert!(matches!(
            parse_end(r#"{"type":"cancelled","id":"r"}"#),
            Err(AsrError::Worker(_))
        ));
        assert_eq!(
            parse_end(r#"{"type":"progress","id":"r","done":0.5}"#).unwrap(),
            None
        );
    }

    // Mimics a speech.exe process that stays alive but never drains stdin.
    // The writer blocks inside write(), while its caller must remain responsive.
    struct FrozenStdin {
        entered: Sender<()>,
        release: Receiver<()>,
        exited: Sender<()>,
        blocked: bool,
    }

    impl Write for FrozenStdin {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if !self.blocked {
                self.blocked = true;
                let _ = self.entered.send(());
                let _ = self.release.recv();
            }
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Drop for FrozenStdin {
        fn drop(&mut self) {
            let _ = self.exited.send(());
        }
    }

    #[test]
    fn frozen_worker_stdin_cannot_block_ui_or_shutdown() {
        let (entered_tx, entered_rx) = channel();
        let (release_tx, release_rx) = channel();
        let (exited_tx, exited_rx) = channel();
        let mut queue = WriterQueue::spawn(FrozenStdin {
            entered: entered_tx,
            release: release_rx,
            exited: exited_tx,
            blocked: false,
        });
        let first_send = Instant::now();
        assert!(queue.enqueue("first chunk".into()).unwrap());
        assert!(first_send.elapsed() < Duration::from_millis(200));
        entered_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("mock stdin entered write");
        // The bounded queue must reject further work instead of waiting for
        // the child to read its pipe.
        let start = Instant::now();
        for _ in 0..WRITE_QUEUE_CAPACITY {
            assert!(queue.enqueue("more audio".into()).unwrap());
        }
        assert!(!queue.enqueue("overflow".into()).unwrap());
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "UI enqueue blocked"
        );
        assert_eq!(queue.pending_len(), WRITE_QUEUE_CAPACITY + 1);
        assert!(matches!(
            queue.poll_at(Instant::now() + WRITE_TIMEOUT + Duration::from_millis(1)),
            Err(AsrError::Timeout(_))
        ));

        // Dropping the producer (the same operation used during shutdown)
        // must not join the writer while its pipe is frozen.
        let before_drop = Instant::now();
        drop(queue);
        assert!(before_drop.elapsed() < Duration::from_millis(200));
        release_tx.send(()).unwrap();
        exited_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("writer exits after release");
    }

    #[test]
    fn writer_reports_acknowledgements_and_io_errors() {
        let mut queue = WriterQueue::spawn(Vec::<u8>::new());
        assert!(queue.enqueue("chunk".into()).unwrap());
        assert!(queue.enqueue("transcribe".into()).unwrap());
        let deadline = Instant::now() + Duration::from_secs(2);
        while queue.pending_len() > 0 && Instant::now() < deadline {
            queue.poll_at(Instant::now()).unwrap();
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(queue.pending_len(), 0);

        struct BrokenStdin;
        impl Write for BrokenStdin {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut queue = WriterQueue::spawn(BrokenStdin);
        assert!(queue.enqueue("chunk".into()).unwrap());
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match queue.poll_at(Instant::now()) {
                Err(AsrError::Io(error)) => {
                    assert!(
                        error.contains("broken pipe") || error.contains("パイプ"),
                        "{error}"
                    );
                    break;
                }
                Err(other) => panic!("unexpected write error: {other}"),
                Ok(()) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(1)),
                Ok(()) => panic!("writer failed to report broken pipe"),
            }
        }
    }

    #[test]
    fn shutdown_kills_a_child_that_never_reads_its_pipe() {
        // One process, no model or microphone. It deliberately keeps stdin
        // open without reading for much longer than our allowed shutdown.
        let mut child = Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "Start-Sleep -Seconds 30",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .expect("PowerShell test worker must launch on Windows");
        let stdin = child.stdin.take().unwrap();
        let (_, lines) = channel();
        let mut worker = Worker {
            child,
            writer: WriterQueue::spawn(stdin),
            lines,
            seq: 0,
            ready: true,
        };
        assert!(worker.send_chunk(&vec![1; 256 * 1024]).unwrap());
        std::thread::sleep(Duration::from_millis(100));
        worker
            .poll_writer()
            .expect("non-reading worker should not have failed yet");
        assert_eq!(
            worker.writer.pending_len(),
            1,
            "the pipe write must still be pending"
        );
        let start = Instant::now();
        worker.shutdown();
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "shutdown blocked on frozen worker"
        );
    }

    #[test]
    fn silent_worker_times_out_and_new_session_gets_fresh_deadlines() {
        let base = Instant::now();
        assert_eq!(
            timeout_phase(
                base,
                false,
                None,
                base + READY_TIMEOUT - Duration::from_millis(1)
            ),
            None
        );
        assert_eq!(
            timeout_phase(base, false, None, base + READY_TIMEOUT),
            Some("ready")
        );
        // Once ready, the model may stay recording longer than the startup deadline.
        assert_eq!(
            timeout_phase(
                base,
                true,
                None,
                base + READY_TIMEOUT + Duration::from_secs(5)
            ),
            None
        );
        let start_transcribe = base + READY_TIMEOUT + Duration::from_secs(10);
        assert_eq!(
            timeout_phase(
                base,
                true,
                Some(start_transcribe),
                start_transcribe + TRANSCRIPTION_TIMEOUT - Duration::from_millis(1)
            ),
            None
        );
        assert_eq!(
            timeout_phase(
                base,
                true,
                Some(start_transcribe),
                start_transcribe + TRANSCRIPTION_TIMEOUT
            ),
            Some("end")
        );
        let fresh = start_transcribe + TRANSCRIPTION_TIMEOUT;
        assert_eq!(
            timeout_phase(fresh, false, None, fresh + Duration::from_secs(1)),
            None
        );
    }

    #[test]
    fn chunk_message_shape() {
        let msg = chunk_message(3, &[1, 2, 3, 4]);
        let v: serde_json::Value = serde_json::from_str(&msg).unwrap();
        assert_eq!(v["type"], "chunk");
        assert_eq!(v["id"], "r");
        assert_eq!(v["seq"], 3);
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(v["pcm"].as_str().unwrap())
            .unwrap();
        assert_eq!(decoded, vec![1, 2, 3, 4]);
    }

    #[test]
    fn transcribe_message_shape() {
        let msg = transcribe_message(48000, "OpenHinoVoice");
        let v: serde_json::Value = serde_json::from_str(&msg).unwrap();
        assert_eq!(v["type"], "transcribe");
        assert_eq!(v["sample_rate"], 48000);
        assert_eq!(v["words"], "OpenHinoVoice");
    }

    #[test]
    fn parse_synthetic_wav() {
        let mut wav = Vec::new();
        let samples: [i16; 4] = [100, -200, 300, -400];
        let data_len = (samples.len() * 2) as u32;
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data_len).to_le_bytes());
        wav.extend_from_slice(b"WAVE");
        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&16000u32.to_le_bytes());
        wav.extend_from_slice(&32000u32.to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_len.to_le_bytes());
        for s in samples {
            wav.extend_from_slice(&s.to_le_bytes());
        }
        let parsed = parse_wav_pcm16(&wav).unwrap();
        assert_eq!(parsed.sample_rate, 16000);
        assert_eq!(parsed.pcm_le.len(), 8);
    }
}
