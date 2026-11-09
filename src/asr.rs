use base64::Engine;
use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{channel, sync_channel, Receiver, SyncSender, TryRecvError, TrySendError};
use std::time::{Duration, Instant};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

// Initial model setup can take longer than an ordinary transcription. Both
// deadlines are checked by the UI loop so a silent worker can be torn down.
const READY_TIMEOUT: Duration = Duration::from_secs(180);
/// 区間 1 つの認識に許す時間。区間は 20 秒までなので、遅い PC でも十分
pub(crate) const TRANSCRIPTION_TIMEOUT: Duration = Duration::from_secs(90);
// Each chunk is queued independently, so neither a non-reading child nor a
// slow pipe may hold the UI loop in a synchronous stdin write.
const WRITE_QUEUE_CAPACITY: usize = 64;
const WRITE_TIMEOUT: Duration = Duration::from_secs(8);
const MAX_FILE_BYTES: u64 = 128 * 1024 * 1024;
// 区間ごとに認識するので、長いファイルでも 1 リクエストの長さは発話 1 つのままである。
// 128 MiB は 16kHz モノラル PCM16 でおよそ 66 分
const MAX_FILE_SECONDS: u64 = 60 * 60;
const FILE_FEED_TIMEOUT: Duration = Duration::from_secs(90);
const FILE_PCM_CHUNK_BYTES: usize = 128 * 1024;

/// worker を起動してから ready を返さないまま READY_TIMEOUT を超えたか。
/// ready のあとの時間切れは区間ごとに別に数える
pub fn ready_timed_out(spawned_at: Instant, now: Instant) -> bool {
    now.saturating_duration_since(spawned_at) >= READY_TIMEOUT
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

pub fn chunk_message(id: &str, seq: u64, pcm_le: &[u8]) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(pcm_le);
    serde_json::json!({
        "type": "chunk",
        "id": id,
        "seq": seq,
        "pcm": encoded,
    })
    .to_string()
}

pub fn transcribe_message(id: &str, sample_rate: u32, words: &str, protocol: u64) -> String {
    let mut message = serde_json::json!({
        "type": "transcribe",
        "id": id,
        "sample_rate": sample_rate,
    });
    // speech.cpp 0.10.0 renamed Haas's "words" option to "keywords".
    // Omit the option entirely when empty; newer workers reject unknown fields
    // even if their values are empty.
    if !words.is_empty() {
        let name = if protocol == 4 { "keywords" } else { "words" };
        message[name] = serde_json::Value::String(words.to_string());
    }
    message.to_string()
}

/// cancel 要求
pub fn cancel_message(id: &str) -> String {
    serde_json::json!({ "type": "cancel", "id": id }).to_string()
}

// 0.9.4 installed on Windows uses protocol 3; current speech.cpp uses 4.
fn parse_ready(line: &str) -> Result<Option<u64>, AsrError> {
    let value: serde_json::Value =
        serde_json::from_str(line).map_err(|e| AsrError::Protocol(e.to_string()))?;
    match value["type"].as_str() {
        Some("ready") => match value["protocol"].as_u64() {
            Some(protocol @ (3 | 4)) => Ok(Some(protocol)),
            _ => Err(AsrError::Protocol(
                "未対応の worker protocol です（3 または 4 が必要）".into(),
            )),
        },
        Some("fatal" | "error") => Err(worker_error(&value)),
        _ => Ok(None),
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

/// 1 つの要求に対する worker の終端メッセージ。要求は id で見分ける
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Terminal {
    pub id: String,
    /// end の stop の値。認識の診断に使う
    pub stop: String,
    pub outcome: TerminalOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalOutcome {
    /// 認識できた。空の結果(無音の区間など)は None
    Text(Option<String>),
    /// worker がエラーを返した
    Error(String),
    /// cancel したか、worker 自身が止めた
    Cancelled,
}

/// 終端メッセージ(end, error, cancelled)を読む。それ以外の行は None。
/// fatal は id を持たないので、id の確認より先に扱う
fn parse_terminal(line: &str) -> Result<Option<Terminal>, AsrError> {
    let value: serde_json::Value =
        serde_json::from_str(line).map_err(|e| AsrError::Protocol(e.to_string()))?;
    if value["type"].as_str() == Some("fatal") {
        return Err(worker_error(&value));
    }
    let Some(id) = value["id"].as_str().map(str::to_string) else {
        if matches!(value["type"].as_str(), Some("end" | "error" | "cancelled")) {
            return Err(AsrError::Protocol(
                "終端メッセージに request id がありません".into(),
            ));
        }
        return Ok(None);
    };
    match value["type"].as_str() {
        Some("end") => {
            if value["stop"].as_str() == Some("model_limit") {
                return Ok(Some(Terminal {
                    id,
                    stop: "model_limit".into(),
                    outcome: TerminalOutcome::Error(
                        "認識結果がモデルの上限で途中で切れました".into(),
                    ),
                }));
            }
            let text = value["text"].as_str().map(str::trim).unwrap_or("");
            let outcome = if text.is_empty() {
                TerminalOutcome::Text(None)
            } else {
                TerminalOutcome::Text(Some(text.to_string()))
            };
            Ok(Some(Terminal {
                id,
                stop: value["stop"].as_str().unwrap_or("").to_string(),
                outcome,
            }))
        }
        Some("error") => Ok(Some(Terminal {
            id,
            stop: String::new(),
            outcome: TerminalOutcome::Error(
                value["error"]["message"]
                    .as_str()
                    .unwrap_or("worker がエラーを返しました")
                    .to_string(),
            ),
        })),
        Some("cancelled") => Ok(Some(Terminal {
            id,
            stop: String::new(),
            outcome: TerminalOutcome::Cancelled,
        })),
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
    /// 要求 id ごとの chunk の seq。1 つの id を 1 つの要求にしか使わないので、
    /// この map は起動し直すと新しくなる
    seqs: HashMap<String, u64>,
    ready: bool,
    protocol: u64,
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
            seqs: HashMap::new(),
            ready: false,
            protocol: 0,
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
                    if let Some(protocol) = parse_ready(&line)? {
                        self.protocol = protocol;
                        self.ready = true;
                        return Ok(());
                    }
                }
                Err(_) => return Err(AsrError::Timeout("ready".into())),
            }
        }
    }

    /// 診断用の子プロセス識別子。プロセスを操作しない。
    pub fn process_id(&self) -> u32 {
        self.child.id()
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
                if let Some(protocol) = parse_ready(&line)? {
                    self.protocol = protocol;
                    self.ready = true;
                }
                Ok(self.ready)
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => Ok(false),
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                Err(AsrError::Worker("worker が終了しました".into()))
            }
        }
    }

    /// 区間や仮の認識など、要求 id ごとに音を送る。戻り値 false は送信列が
    /// 満杯の一時的な押し戻しで、同じ chunk を後の tick で送り直す
    pub fn send_chunk(&mut self, id: &str, pcm_le: &[u8]) -> Result<bool, AsrError> {
        let seq = self.seqs.entry(id.to_string()).or_insert(0);
        let msg = chunk_message(id, *seq, pcm_le);
        if self.writer.enqueue(msg)? {
            *seq += 1;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub fn transcribe(
        &mut self,
        id: &str,
        sample_rate: u32,
        words: &str,
    ) -> Result<bool, AsrError> {
        let msg = transcribe_message(id, sample_rate, words, self.protocol);
        self.writer.enqueue(msg)
    }

    /// 要求 id の採番を忘れる。録音のあとに worker を残して次の録音で
    /// 使い回すとき、同じ区間 id(s0, s1…)を seq 0 から使い直すため
    pub fn forget_ids(&mut self) {
        self.seqs.clear();
    }

    /// 仮の認識を止める。実行中の要求は次の止められる場所で
    /// cancelled で終わる。飛行中でない id への cancel は無視される
    pub fn cancel(&mut self, id: &str) -> Result<bool, AsrError> {
        self.writer.enqueue(cancel_message(id))
    }

    /// Nonblocking acknowledgement + age check; called every UI tick even
    /// when the recognition process is no longer responding on stdout.
    pub fn poll_writer(&mut self) -> Result<(), AsrError> {
        self.writer.poll_at(Instant::now())
    }

    #[allow(dead_code)] // Used by the explicit, ignored ASR integration test.
    pub fn wait_end(&mut self, id: &str, timeout: Duration) -> Result<String, AsrError> {
        let deadline = Instant::now() + timeout;
        loop {
            let remain = deadline.saturating_duration_since(Instant::now());
            if remain.is_zero() {
                return Err(AsrError::Timeout("end".into()));
            }
            match self.lines.recv_timeout(remain) {
                Ok(line) => match parse_terminal(&line)? {
                    Some(Terminal {
                        id: terminal_id,
                        outcome: TerminalOutcome::Text(text),
                        ..
                    }) if terminal_id == id => {
                        return text.ok_or_else(|| {
                            AsrError::Worker("音声を認識できませんでした（結果が空です）".into())
                        });
                    }
                    Some(Terminal {
                        outcome: TerminalOutcome::Error(message),
                        ..
                    }) => return Err(AsrError::Worker(message)),
                    Some(Terminal {
                        outcome: TerminalOutcome::Cancelled,
                        ..
                    }) => return Err(AsrError::Worker("認識がキャンセルされました".into())),
                    _ => {}
                },
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    return Err(AsrError::Timeout("end".into()))
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(AsrError::Worker("worker が終了しました".into()))
                }
            }
        }
    }

    /// 終端メッセージを 1 行読む。どの要求のものかは Terminal.id で見分ける
    pub fn poll_terminal(&mut self) -> Option<Result<Terminal, AsrError>> {
        match self.lines.try_recv() {
            Ok(line) => parse_terminal(&line).transpose(),
            Err(std::sync::mpsc::TryRecvError::Empty) => None,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                Some(Err(AsrError::Worker("worker が終了しました".into())))
            }
        }
    }

    /// 終了要求だけを直ちに送る。待機は別途kill/shutdownで行う。
    pub fn request_stop(&mut self) {
        // Terminate first; a child stuck reading neither stdin nor stdout
        // cannot be asked to exit gracefully. Never join the writer thread.
        let _ = self.child.kill();
    }

    /// 消費せずに子プロセスを止める。ファイル文字起こしの送り直しの前に使う
    pub fn kill(&mut self) {
        self.request_stop();
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

    pub fn shutdown(mut self) {
        self.kill();
    }
}

pub struct WavData {
    pub sample_rate: u32,
    pub pcm_le: Vec<u8>,
}

/// オフラインモードの起動時の確認。認識モデルが取得済みかを
/// `speech models --json` の一覧で調べる。ここでは取得(ダウンロード)はしない
pub fn model_fetched(speech_exe: &str, model: &str) -> Result<bool, String> {
    let path = Path::new(model);
    if path.is_file() {
        return Ok(true);
    }
    if path.is_absolute() || model.to_ascii_lowercase().ends_with(".gguf") || model.contains('\\') {
        return Ok(false);
    }
    let output = Command::new(speech_exe)
        .arg("models")
        .arg("--json")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("speech models を実行できません: {e}"))?;
    if !output.status.success() {
        return Err("認識モデルの取得状況を確認できません".into());
    }
    let text = String::from_utf8_lossy(&output.stdout);
    model_fetched_from_catalog(&text, model)
}

pub fn check_offline_model(speech_exe: &str, model: &str, offline: bool) -> Result<(), String> {
    if offline && !model_fetched(speech_exe, model)? {
        return Err("認識モデルが未取得です。オンラインのときに 1 回取得してください".into());
    }
    Ok(())
}

/// `speech models --json` の出力から、指定のモデル(名前[:型])の取得済みを返す
fn model_fetched_from_catalog(json: &str, model: &str) -> Result<bool, String> {
    let value: serde_json::Value =
        serde_json::from_str(json).map_err(|e| format!("モデル一覧を読めません: {e}"))?;
    let (name, wanted_type) = model.split_once(':').unwrap_or((model, ""));
    for entry in value["models"].as_array().into_iter().flatten() {
        if entry["name"].as_str() != Some(name) {
            continue;
        }
        // 型の指定がなければ、カタログのこのモデルの既定の型を見る
        let wanted_type = if wanted_type.is_empty() {
            entry["type"].as_str().unwrap_or("")
        } else {
            wanted_type
        };
        let mut any_fetched = false;
        for file in entry["files"].as_array().into_iter().flatten() {
            if file["type"].as_str() == Some(wanted_type) {
                return Ok(file["fetched"].as_bool() == Some(true));
            }
            any_fetched |= file["fetched"].as_bool() == Some(true);
        }
        return Ok(wanted_type.is_empty() && any_fetched);
    }
    Ok(false)
}

/// 区間認識の単体テスト用。偽の標準入力と、返す行を決められる worker を作る。
/// broken_stdin を真にすると、書き込みが必ず失敗する(worker の死を模す)
#[cfg(test)]
pub fn test_worker(lines: std::sync::mpsc::Receiver<String>, broken_stdin: bool) -> Worker {
    struct Broken;
    impl Write for Broken {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let child = Command::new("cmd.exe")
        .args(["/c", "exit"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .expect("test child must spawn");
    let writer = if broken_stdin {
        WriterQueue::spawn(Broken)
    } else {
        WriterQueue::spawn(Vec::<u8>::new())
    };
    Worker {
        child,
        writer,
        lines,
        seqs: HashMap::new(),
        ready: true,
        protocol: 4,
    }
}

#[cfg(test)]
pub fn test_starting_worker(lines: std::sync::mpsc::Receiver<String>) -> Worker {
    let mut worker = test_worker(lines, false);
    worker.ready = false;
    worker.protocol = 0;
    worker
}

/// 送られた JSON 行を後から読める test_worker。区間と仮の認識の送り方のテストに使う
#[cfg(test)]
pub fn test_worker_with_recorder(
    lines: std::sync::mpsc::Receiver<String>,
) -> (Worker, std::sync::Arc<std::sync::Mutex<String>>) {
    struct Recording {
        buffer: std::sync::Arc<std::sync::Mutex<String>>,
    }
    impl Write for Recording {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.buffer
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .push_str(&String::from_utf8_lossy(buf));
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let buffer = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let child = Command::new("cmd.exe")
        .args(["/c", "exit"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .expect("test child must spawn");
    let worker = Worker {
        child,
        writer: WriterQueue::spawn(Recording {
            buffer: buffer.clone(),
        }),
        lines,
        seqs: HashMap::new(),
        ready: true,
        protocol: 4,
    };
    (worker, buffer)
}

/// Decodes uncompressed RIFF/WAVE PCM16 or IEEE float32 into mono PCM16.
/// Unsupported/compressed formats are rejected instead of fed to speech.cpp.
pub fn parse_wav(bytes: &[u8]) -> Result<WavData, String> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err("WAV ヘッダが不正です".into());
    }
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err("音声ファイルは128 MiB以下にしてください".into());
    }
    let riff_size = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
    let riff_end = riff_size.checked_add(8).ok_or("WAV サイズが不正です")?;
    if riff_size < 4 || riff_end > bytes.len() {
        return Err("WAV のデータが途中で切れています".into());
    }
    let mut pos = 12usize;
    let mut format: Option<(u16, u16, u32, usize)> = None;
    let mut data: Option<&[u8]> = None;
    while pos + 8 <= riff_end {
        let id = &bytes[pos..pos + 4];
        let size = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let body_start = pos + 8;
        let body_end = body_start
            .checked_add(size)
            .ok_or("WAV チャンクの長さが不正です")?;
        if body_end > riff_end {
            return Err("WAV チャンクが途中で切れています".into());
        }
        if id == b"fmt " {
            if size < 16 {
                return Err("WAV fmt チャンクが短すぎます".into());
            }
            let fmt = &bytes[body_start..body_end];
            let mut kind = u16::from_le_bytes(fmt[0..2].try_into().unwrap());
            let channels = u16::from_le_bytes(fmt[2..4].try_into().unwrap());
            let sample_rate = u32::from_le_bytes(fmt[4..8].try_into().unwrap());
            let block_align = u16::from_le_bytes(fmt[12..14].try_into().unwrap()) as usize;
            let bits = u16::from_le_bytes(fmt[14..16].try_into().unwrap());
            if kind == 0xfffe {
                // WAVE_FORMAT_EXTENSIBLE: only standard PCM/IEEE float GUIDs.
                if fmt.len() < 40 || u16::from_le_bytes(fmt[16..18].try_into().unwrap()) < 22 {
                    return Err("WAV 拡張フォーマットが不正です".into());
                }
                let valid_bits = u16::from_le_bytes(fmt[18..20].try_into().unwrap());
                if valid_bits != 0 && valid_bits != bits {
                    return Err("WAV の有効ビット数に対応していません".into());
                }
                let guid = &fmt[24..40];
                if guid[2..] != [0, 0, 0, 0, 0x10, 0, 0x80, 0, 0, 0xaa, 0, 0x38, 0x9b, 0x71] {
                    return Err("WAV 拡張フォーマットの種類に対応していません".into());
                }
                kind = u16::from_le_bytes(guid[0..2].try_into().unwrap());
            }
            if !matches!((kind, bits), (1, 16) | (3, 32)) {
                return Err("WAV は PCM16 または float32 のみ対応です".into());
            }
            if !(1..=8).contains(&channels) || !(8000..=192000).contains(&sample_rate) {
                return Err("WAV のチャンネル数またはサンプルレートが不正です".into());
            }
            let expected_align = channels as usize * (bits as usize / 8);
            if block_align != expected_align {
                return Err("WAV のブロックサイズが不正です".into());
            }
            format = Some((kind, channels, sample_rate, block_align));
        } else if id == b"data" && data.is_none() {
            data = Some(&bytes[body_start..body_end]);
        }
        pos = body_end
            .checked_add(size % 2)
            .ok_or("WAV チャンクの長さが不正です")?;
        if pos > riff_end {
            return Err("WAV パディングが不足しています".into());
        }
    }
    if pos != riff_end {
        return Err("WAV チャンクヘッダが不完全です".into());
    }
    let (kind, channels, sample_rate, block_align) = format.ok_or("fmt チャンクがありません")?;
    let raw = data.ok_or("data チャンクがありません")?;
    if raw.is_empty() || raw.len() % block_align != 0 {
        return Err("WAV の音声データ長が不正です".into());
    }
    let frames = raw.len() / block_align;
    if (frames as u128) > (sample_rate as u128 * MAX_FILE_SECONDS as u128) {
        return Err(format!(
            "音声ファイルは{}分以内にしてください",
            MAX_FILE_SECONDS / 60
        ));
    }
    let pcm = if kind == 1 && channels == 1 {
        raw.to_vec()
    } else {
        let mut mono = Vec::with_capacity(frames * 2);
        for frame in raw.chunks_exact(block_align) {
            let sample = if kind == 1 {
                let mut sum = 0i64;
                for sample in frame.as_chunks::<2>().0 {
                    sum += i16::from_le_bytes([sample[0], sample[1]]) as i64;
                }
                (sum / channels as i64) as i16
            } else {
                let mut sum = 0.0f64;
                for sample in frame.as_chunks::<4>().0 {
                    let value = f32::from_le_bytes(*sample);
                    if value.is_finite() {
                        sum += value as f64;
                    }
                }
                let mono = (sum / channels as f64) as f32;
                (mono.clamp(-1.0, 1.0) * i16::MAX as f32) as i16
            };
            mono.extend_from_slice(&sample.to_le_bytes());
        }
        mono
    };
    Ok(WavData {
        sample_rate,
        pcm_le: pcm,
    })
}

/// Starts file transcription on a dedicated thread; receiving the result is
/// nonblocking for the settings window. Only the recognized raw text is sent.
pub fn transcribe_file_async(
    path: PathBuf,
    speech_exe: String,
    model: String,
    words: String,
    offline: bool,
) -> Receiver<Result<String, String>> {
    let (sender, receiver) = channel();
    std::thread::spawn(move || {
        let result = check_offline_model(&speech_exe, &model, offline)
            .and_then(|()| transcribe_file(&path, &speech_exe, &model, &words));
        let _ = sender.send(result);
    });
    receiver
}

/// Synchronous engine for background jobs and headless callers. Do not call
/// this from the UI thread: reading WAV and waiting for speech.cpp take time.
/// FastConformer に長い音声を 1 リクエストで渡さないよう、区間ごとに認識してつなぐ。
pub fn transcribe_file(
    path: &Path,
    speech_exe: &str,
    model: &str,
    words: &str,
) -> Result<String, String> {
    let file = std::fs::File::open(path).map_err(|e| format!("音声ファイルを開けません: {e}"))?;
    if file.metadata().map_err(|e| e.to_string())?.len() > MAX_FILE_BYTES {
        return Err("音声ファイルは128 MiB以下にしてください".into());
    }
    // Also enforce the bound if the file grows between metadata and read.
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("音声ファイルを読めません: {e}"))?;
    let wav = parse_wav(&bytes)?;
    drop(bytes);

    let mut intervals = FileIntervals::new(&wav.pcm_le, wav.sample_rate).peekable();
    if intervals.peek().is_none() {
        return Err("話している部分が見つかりませんでした".into());
    }

    let mut worker = Worker::spawn(speech_exe, model).map_err(|e| e.to_string())?;
    let outcome = transcribe_intervals_with_retry(
        &mut worker,
        intervals,
        wav.sample_rate,
        words,
        speech_exe,
        model,
    );
    worker.shutdown();
    let texts = outcome.map_err(|e| e.to_string())?;
    Ok(crate::segment::join_intervals(&texts))
}

/// ファイル全体を f32 と区間の一覧に展開せず、認識する一区間ずつ取り出す。
struct FileIntervals<'a> {
    pcm: std::slice::Chunks<'a, u8>,
    segmenter: crate::segment::Segmenter,
    samples: Vec<f32>,
    ready: VecDeque<Vec<f32>>,
    finished: bool,
}

impl<'a> FileIntervals<'a> {
    fn new(pcm: &'a [u8], sample_rate: u32) -> Self {
        let frame_samples = (sample_rate as usize / 5).max(1);
        Self {
            pcm: pcm.chunks(frame_samples * 2),
            segmenter: crate::segment::Segmenter::new(sample_rate),
            samples: Vec::with_capacity(frame_samples),
            ready: VecDeque::new(),
            finished: false,
        }
    }
}

impl Iterator for FileIntervals<'_> {
    type Item = Vec<f32>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(interval) = self.ready.pop_front() {
                return Some(interval);
            }
            if self.finished {
                return None;
            }
            if let Some(pcm) = self.pcm.next() {
                self.samples.clear();
                self.samples.extend(
                    pcm.as_chunks::<2>()
                        .0
                        .iter()
                        .map(|b| i16::from_le_bytes(*b) as f32 / 32768.0),
                );
                self.ready.extend(self.segmenter.push(&self.samples));
            } else {
                self.finished = true;
                return self
                    .segmenter
                    .finish()
                    .filter(|samples| !samples.is_empty());
            }
        }
    }
}

/// 区間を順に認識させる。worker が死んだら起動し直して、その区間だけを送り直す。
/// 終わった区間の文章はすでに texts に積んである
fn transcribe_intervals_with_retry<I>(
    worker: &mut Worker,
    intervals: I,
    sample_rate: u32,
    words: &str,
    speech_exe: &str,
    model: &str,
) -> Result<Vec<String>, AsrError>
where
    I: IntoIterator,
    I::Item: AsRef<[f32]>,
{
    worker.wait_ready(READY_TIMEOUT)?;
    let mut texts = Vec::new();
    for (index, interval) in intervals.into_iter().enumerate() {
        let id = format!("f{index}");
        let pcm = crate::audio::samples_to_pcm_le(interval.as_ref());
        let text = match send_and_await(worker, &id, &pcm, sample_rate, words) {
            Ok(text) => text,
            Err(error) => {
                worker.kill();
                *worker = Worker::spawn(speech_exe, model)?;
                worker.wait_ready(READY_TIMEOUT)?;
                send_and_await(worker, &id, &pcm, sample_rate, words)
                    .map_err(|_| AsrError::Worker(format!("送り直しでも失敗しました: {error}")))?
            }
        };
        texts.push(text);
    }
    Ok(texts)
}

/// 1 つの区間を chunk + transcribe で送り、終端まで待つ。結果の空は空文字にして返す
fn send_and_await(
    worker: &mut Worker,
    id: &str,
    pcm: &[u8],
    sample_rate: u32,
    words: &str,
) -> Result<String, AsrError> {
    let feed_deadline = Instant::now() + FILE_FEED_TIMEOUT;
    for chunk in pcm.chunks(FILE_PCM_CHUNK_BYTES) {
        loop {
            worker.poll_writer()?;
            if Instant::now() >= feed_deadline {
                return Err(AsrError::Timeout("音声送信".into()));
            }
            if worker.send_chunk(id, chunk)? {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    loop {
        worker.poll_writer()?;
        if Instant::now() >= feed_deadline {
            return Err(AsrError::Timeout("音声送信".into()));
        }
        if worker.transcribe(id, sample_rate, words)? {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let end_deadline = Instant::now() + TRANSCRIPTION_TIMEOUT;
    loop {
        worker.poll_writer()?;
        match worker.poll_terminal() {
            Some(Ok(terminal)) if terminal.id == id => {
                return Ok(match terminal.outcome {
                    TerminalOutcome::Text(text) => text.unwrap_or_default(),
                    TerminalOutcome::Error(message) => return Err(AsrError::Worker(message)),
                    TerminalOutcome::Cancelled => {
                        return Err(AsrError::Worker("認識がキャンセルされました".into()))
                    }
                });
            }
            Some(Ok(_)) => {} // 別の要求の終端。ファイル経路では起こらないはず
            Some(Err(e)) => return Err(e),
            None => {
                if Instant::now() >= end_deadline {
                    return Err(AsrError::Timeout("end".into()));
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::Sender;

    #[test]
    fn a_model_limit_is_not_a_complete_transcript() {
        let terminal =
            parse_terminal(r#"{"type":"end","id":"s0","stop":"model_limit","text":"途中だけ"}"#)
                .unwrap()
                .unwrap();
        assert!(matches!(terminal.outcome, TerminalOutcome::Error(_)));
    }

    #[test]
    fn file_intervals_match_batch_segmentation_and_keep_the_final_fraction() {
        let rate = 8000;
        let mut samples = vec![0.25; rate as usize];
        samples.extend(std::iter::repeat_n(0.0, rate as usize));
        samples.extend(std::iter::repeat_n(0.5, rate as usize * 22 + 37));
        let pcm: Vec<u8> = samples
            .iter()
            .flat_map(|sample| ((*sample * 32768.0) as i16).to_le_bytes())
            .collect();
        let mut batch = crate::segment::Segmenter::new(rate);
        let mut expected = batch.push(&samples);
        expected.extend(batch.finish());
        let mut streaming = FileIntervals::new(&pcm, rate);
        let first = streaming.next().unwrap();
        assert!(
            streaming.pcm.len() > 0,
            "最初の区間で残りの音声を展開しない"
        );
        let actual: Vec<_> = std::iter::once(first).chain(streaming).collect();
        assert_eq!(actual, expected);
        assert_eq!(*actual.last().unwrap().last().unwrap(), 0.5);
    }

    #[test]
    fn silent_file_intervals_are_empty() {
        assert!(FileIntervals::new(&vec![0; 16000], 8000).next().is_none());
    }

    #[test]
    fn file_interval_errors_are_not_successful_empty_results() {
        for line in [
            r#"{"type":"error","id":"f0","error":{"code":"worker","message":"認識失敗"}}"#,
            r#"{"type":"cancelled","id":"f0"}"#,
        ] {
            let (tx, rx) = channel();
            tx.send(line.into()).unwrap();
            let mut worker = test_worker(rx, false);
            assert!(send_and_await(&mut worker, "f0", &[0; 32], 16000, "").is_err());
            worker.shutdown();
        }
    }

    #[test]
    fn ready_checks_installed_and_current_protocols_and_startup_failure() {
        assert_eq!(
            parse_ready(r#"{"type":"ready","protocol":3,"version":"0.9.4"}"#).unwrap(),
            Some(3)
        );
        assert_eq!(
            parse_ready(r#"{"type":"ready","protocol":4,"version":"0.10.0"}"#).unwrap(),
            Some(4)
        );
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
        assert_eq!(
            parse_ready(r#"{"type":"progress","done":0.5}"#).unwrap(),
            None
        );
        assert!(matches!(
            parse_ready("not json"),
            Err(AsrError::Protocol(_))
        ));
    }

    #[test]
    fn recognition_terminal_carries_id_and_distinguishes_empty_text() {
        let terminal =
            parse_terminal(r#"{"type":"end","id":"s0","text":"おはよう。","stop":"complete"}"#)
                .unwrap()
                .expect("end は終端");
        assert_eq!(terminal.id, "s0");
        assert_eq!(terminal.stop, "complete");
        assert_eq!(
            terminal.outcome,
            TerminalOutcome::Text(Some("おはよう。".into()))
        );
        // 途中までの文を完成した結果として貼り付けない
        let limited =
            parse_terminal(r#"{"type":"end","id":"s1","text":"ここまで","stop":"model_limit"}"#)
                .unwrap()
                .unwrap();
        assert_eq!(limited.stop, "model_limit");
        assert!(matches!(limited.outcome, TerminalOutcome::Error(_)));
        // 空の結果は区間としては正常(無音の区間がありうる)
        for line in [
            r#"{"type":"end","id":"s2","text":"","stop":"complete"}"#,
            r#"{"type":"end","id":"s3","text":"   ","stop":"complete"}"#,
            r#"{"type":"end","id":"s4","stop":"complete"}"#,
        ] {
            let terminal = parse_terminal(line).unwrap().unwrap();
            assert_eq!(terminal.outcome, TerminalOutcome::Text(None));
        }
        let other = parse_terminal(r#"{"type":"end","id":"other","text":"x","stop":"complete"}"#)
            .unwrap()
            .unwrap();
        assert_eq!(other.id, "other");
        assert!(matches!(
            parse_terminal(r#"{"type":"error","id":"r","error":{"message":"failed"}}"#)
                .unwrap()
                .unwrap()
                .outcome,
            TerminalOutcome::Error(message) if message == "failed"
        ));
        assert_eq!(
            parse_terminal(r#"{"type":"cancelled","id":"p0"}"#)
                .unwrap()
                .unwrap()
                .outcome,
            TerminalOutcome::Cancelled
        );
        assert_eq!(
            parse_terminal(r#"{"type":"progress","id":"r","done":0.5}"#).unwrap(),
            None
        );
        // id のない終端は worker 側の欠陥として失敗にする
        assert!(matches!(
            parse_terminal(r#"{"type":"end","text":"x","stop":"complete"}"#),
            Err(AsrError::Protocol(_))
        ));
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
            seqs: HashMap::new(),
            ready: true,
            protocol: 3,
        };
        assert!(worker.send_chunk("r", &vec![1; 256 * 1024]).unwrap());
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
    fn silent_worker_times_out_and_a_new_worker_gets_a_fresh_deadline() {
        let base = Instant::now();
        assert!(!ready_timed_out(
            base,
            base + READY_TIMEOUT - Duration::from_millis(1)
        ));
        assert!(ready_timed_out(base, base + READY_TIMEOUT));
        // 起動し直した worker は新しい期限を持つ
        let respawned = base + READY_TIMEOUT + Duration::from_secs(5);
        assert!(!ready_timed_out(
            respawned,
            respawned + Duration::from_secs(1)
        ));
    }

    #[test]
    fn chunk_message_shape() {
        let msg = chunk_message("s2", 3, &[1, 2, 3, 4]);
        let v: serde_json::Value = serde_json::from_str(&msg).unwrap();
        assert_eq!(v["type"], "chunk");
        assert_eq!(v["id"], "s2");
        assert_eq!(v["seq"], 3);
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(v["pcm"].as_str().unwrap())
            .unwrap();
        assert_eq!(decoded, vec![1, 2, 3, 4]);
    }

    #[test]
    fn transcribe_message_shape() {
        for (protocol, option, absent) in [(3, "words", "keywords"), (4, "keywords", "words")] {
            let msg = transcribe_message("s0", 48000, "OpenHinoVoice", protocol);
            let v: serde_json::Value = serde_json::from_str(&msg).unwrap();
            assert_eq!(v["type"], "transcribe");
            assert_eq!(v["id"], "s0");
            assert_eq!(v["sample_rate"], 48000);
            assert_eq!(v[option], "OpenHinoVoice");
            assert!(v.get(absent).is_none());

            // Empty hints must not send a stale field to the newer protocol.
            let empty: serde_json::Value =
                serde_json::from_str(&transcribe_message("s0", 48000, "", protocol)).unwrap();
            assert!(empty.get("words").is_none());
            assert!(empty.get("keywords").is_none());
        }
    }

    #[test]
    fn forget_ids_restart_sequence_numbers_per_request() {
        // 録音のあとに worker を残して使い回すとき、同じ区間 id を seq 0 から
        // 使い直せる。worker は「chunk 0 で新しい要求が始まる」規則だから
        let buffer = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        struct Recording {
            buffer: std::sync::Arc<std::sync::Mutex<String>>,
        }
        impl Write for Recording {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.buffer
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .push_str(&String::from_utf8_lossy(buf));
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut worker = Worker {
            child: dead_child(),
            writer: WriterQueue::spawn(Recording {
                buffer: buffer.clone(),
            }),
            lines: channel().1,
            seqs: HashMap::new(),
            ready: true,
            protocol: 4,
        };
        assert!(worker.send_chunk("s0", &[1, 2]).unwrap());
        assert!(worker.send_chunk("s0", &[3, 4]).unwrap());
        worker.forget_ids();
        assert!(worker.send_chunk("s0", &[5, 6]).unwrap());
        let deadline = Instant::now() + Duration::from_secs(2);
        while worker.writer.pending_len() > 0 && Instant::now() < deadline {
            worker.poll_writer().unwrap();
            std::thread::sleep(Duration::from_millis(1));
        }
        let sent = buffer.lock().unwrap_or_else(|p| p.into_inner()).clone();
        let seqs: Vec<u64> = sent
            .lines()
            .filter(|l| !l.is_empty())
            .map(|line| {
                let v: serde_json::Value = serde_json::from_str(line).unwrap();
                v["seq"].as_u64().unwrap()
            })
            .collect();
        assert_eq!(seqs, vec![0, 1, 0], "使い回した id は seq 0 から始まる");
    }

    #[test]
    fn cancel_message_names_the_request() {
        let v: serde_json::Value = serde_json::from_str(&cancel_message("p3")).unwrap();
        assert_eq!(v["type"], "cancel");
        assert_eq!(v["id"], "p3");
    }

    #[test]
    fn model_catalog_check_finds_fetched_files() {
        let catalog = r#"{"models":[
            {"name":"haas-ja-v1.0","type":"q8_0","files":[
                {"type":"q8_0","fetched":true},
                {"type":"q4_k","fetched":false}]},
            {"name":"qwen3-tts-0.6b","type":"q8_0","files":[
                {"type":"q8_0","fetched":false}]}
        ]}"#;
        assert!(model_fetched_from_catalog(catalog, "haas-ja-v1.0").unwrap());
        assert!(!model_fetched_from_catalog(catalog, "haas-ja-v1.0:q4_k").unwrap());
        assert!(!model_fetched_from_catalog(catalog, "qwen3-tts-0.6b").unwrap());
        assert!(!model_fetched_from_catalog(catalog, "unknown-model").unwrap());
        assert!(!model_fetched_from_catalog(catalog, "haas-ja-v1.0:missing-type").unwrap());
        assert!(!model_fetched("存在しないspeech.exe", r"C:\存在しない場所\mine.gguf").unwrap());
        assert!(
            check_offline_model("存在しないspeech.exe", r"C:\存在しない場所\mine.gguf", true)
                .is_err()
        );
    }

    #[test]
    fn worker_counts_chunk_sequence_per_request_id() {
        // 送られる JSON 行を書き留める標準入力の代わり。1 行は複数の write に
        // 分かれて届くので、1 つの文字列に積んでから行に分ける
        struct Recording {
            buffer: std::sync::Arc<std::sync::Mutex<String>>,
        }
        impl Write for Recording {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.buffer
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .push_str(&String::from_utf8_lossy(buf));
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let buffer = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let mut worker = Worker {
            child: dead_child(),
            writer: WriterQueue::spawn(Recording {
                buffer: buffer.clone(),
            }),
            lines: channel().1,
            seqs: HashMap::new(),
            ready: true,
            protocol: 4,
        };
        let sent_lines = || -> Vec<String> {
            buffer
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .lines()
                .filter(|l| !l.is_empty())
                .map(str::to_string)
                .collect()
        };
        assert!(worker.send_chunk("s0", &[1, 2]).unwrap());
        assert!(worker.send_chunk("s0", &[3, 4]).unwrap());
        assert!(worker.send_chunk("s1", &[5, 6]).unwrap());
        let deadline = Instant::now() + Duration::from_secs(2);
        while worker.writer.pending_len() > 0 && Instant::now() < deadline {
            worker.poll_writer().unwrap();
            std::thread::sleep(Duration::from_millis(1));
        }
        let ids: Vec<(String, u64)> = sent_lines()
            .iter()
            .map(|line| {
                let v: serde_json::Value = serde_json::from_str(line).unwrap();
                (
                    v["id"].as_str().unwrap().to_string(),
                    v["seq"].as_u64().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            ids,
            vec![("s0".into(), 0), ("s0".into(), 1), ("s1".into(), 0)]
        );
        assert!(worker.transcribe("s1", 16000, "").unwrap());
        let deadline = Instant::now() + Duration::from_secs(2);
        while worker.writer.pending_len() > 0 && Instant::now() < deadline {
            worker.poll_writer().unwrap();
            std::thread::sleep(Duration::from_millis(1));
        }
        let last: serde_json::Value =
            serde_json::from_str(sent_lines().last().unwrap().as_str()).unwrap();
        assert_eq!(last["type"], "transcribe");
        assert_eq!(last["id"], "s1");
    }

    #[test]
    fn a_stop_request_terminates_before_cleanup_is_scheduled() {
        let mut child = Command::new("cmd.exe")
            .args(["/D", "/C", "set /p wait="])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .unwrap();
        let writer = WriterQueue::spawn(child.stdin.take().unwrap());
        let (_tx, lines) = channel();
        let mut worker = Worker {
            child,
            writer,
            lines,
            seqs: HashMap::new(),
            ready: false,
            protocol: 0,
        };
        assert!(worker.child.try_wait().unwrap().is_none());
        worker.request_stop();
        // 回収用スレッドを作らなくても、この呼び出しだけで子が終了する。
        let deadline = Instant::now() + Duration::from_secs(2);
        let exited = loop {
            if worker.child.try_wait().unwrap().is_some() {
                break true;
            }
            if Instant::now() >= deadline {
                break false;
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        worker.shutdown();
        assert!(exited, "終了要求を回収用スレッドへ先送りしない");
    }

    /// shutdown と構造のテスト用に使う、すぐ終わる子プロセス
    fn dead_child() -> Child {
        Command::new("cmd.exe")
            .args(["/c", "exit"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .expect("test child must spawn")
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
        let parsed = parse_wav(&wav).unwrap();
        assert_eq!(parsed.sample_rate, 16000);
        assert_eq!(parsed.pcm_le.len(), 8);
    }

    fn wav_with_format(kind: u16, bits: u16, channels: u16, samples: &[u8]) -> Vec<u8> {
        let mut wav = Vec::new();
        let aligned = channels * (bits / 8);
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + samples.len() as u32).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&kind.to_le_bytes());
        wav.extend_from_slice(&channels.to_le_bytes());
        wav.extend_from_slice(&16000u32.to_le_bytes());
        wav.extend_from_slice(&(16000 * aligned as u32).to_le_bytes());
        wav.extend_from_slice(&aligned.to_le_bytes());
        wav.extend_from_slice(&bits.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&(samples.len() as u32).to_le_bytes());
        wav.extend_from_slice(samples);
        wav
    }

    #[test]
    fn wav_float32_stereo_is_converted_to_mono_pcm16() {
        let mut samples = Vec::new();
        for value in [1.0f32, -0.5, -1.0, -1.0] {
            samples.extend_from_slice(&value.to_le_bytes());
        }
        let wav = wav_with_format(3, 32, 2, &samples);
        let result = parse_wav(&wav).unwrap();
        assert_eq!(result.sample_rate, 16000);
        let mono: Vec<_> = result
            .pcm_le
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| i16::from_le_bytes(*b))
            .collect();
        assert_eq!(mono, [8191, -32767]);
    }

    #[test]
    fn wav_extensible_pcm16_is_accepted() {
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&62u32.to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&40u32.to_le_bytes());
        wav.extend_from_slice(&0xfffeu16.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&16000u32.to_le_bytes());
        wav.extend_from_slice(&32000u32.to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(&22u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(&0u32.to_le_bytes());
        wav.extend_from_slice(&[
            1, 0, 0, 0, 0, 0, 0x10, 0, 0x80, 0, 0, 0xaa, 0, 0x38, 0x9b, 0x71,
        ]);
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&2u32.to_le_bytes());
        wav.extend_from_slice(&123i16.to_le_bytes());
        assert_eq!(
            parse_wav(&wav).unwrap().pcm_le,
            123i16.to_le_bytes().to_vec()
        );
    }

    #[test]
    fn wav_rejects_truncation_compression_and_broken_frames() {
        let samples = [0, 0, 1, 0];
        let valid = wav_with_format(1, 16, 1, &samples);
        let mut truncated = valid.clone();
        truncated.pop();
        assert!(parse_wav(&truncated).is_err());
        assert!(parse_wav(&wav_with_format(6, 16, 1, &samples)).is_err());
        assert!(parse_wav(&wav_with_format(1, 16, 2, &[0, 0])).is_err());
        let mut corrupted = valid;
        // A claimed chunk larger than its RIFF boundary must not be clamped.
        corrupted[40..44].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(parse_wav(&corrupted).is_err());
    }
}
