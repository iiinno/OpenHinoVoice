use base64::Engine;
use std::os::windows::process::CommandExt;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{channel, Receiver};
use std::time::{Duration, Instant};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Debug)]
pub enum AsrError {
    Spawn(String),
    Io(String),
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

pub struct Worker {
    child: Child,
    stdin: ChildStdin,
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
            stdin,
            lines: rx,
            seq: 0,
            ready: false,
        })
    }

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
                    let value: serde_json::Value = serde_json::from_str(&line)
                        .map_err(|e| AsrError::Protocol(e.to_string()))?;
                    match value.get("type").and_then(|t| t.as_str()) {
                        Some("ready") => {
                            self.ready = true;
                            return Ok(());
                        }
                        Some("error") => {
                            return Err(AsrError::Worker(line));
                        }
                        _ => continue,
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
                let value: serde_json::Value =
                    serde_json::from_str(&line).map_err(|e| AsrError::Protocol(e.to_string()))?;
                match value.get("type").and_then(|t| t.as_str()) {
                    Some("ready") => {
                        self.ready = true;
                        Ok(true)
                    }
                    Some("error") => Err(AsrError::Worker(line)),
                    _ => Ok(false),
                }
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => Ok(false),
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                Err(AsrError::Worker("worker が終了しました".into()))
            }
        }
    }

    pub fn send_chunk(&mut self, pcm_le: &[u8]) -> Result<(), AsrError> {
        let msg = chunk_message(self.seq, pcm_le);
        self.seq += 1;
        writeln!(self.stdin, "{msg}").map_err(|e| AsrError::Io(e.to_string()))?;
        self.stdin
            .flush()
            .map_err(|e| AsrError::Io(e.to_string()))?;
        Ok(())
    }

    pub fn transcribe(&mut self, sample_rate: u32, words: &str) -> Result<(), AsrError> {
        let msg = transcribe_message(sample_rate, words);
        writeln!(self.stdin, "{msg}").map_err(|e| AsrError::Io(e.to_string()))?;
        self.stdin
            .flush()
            .map_err(|e| AsrError::Io(e.to_string()))?;
        Ok(())
    }

    pub fn wait_end(&mut self, timeout: Duration) -> Result<String, AsrError> {
        let deadline = Instant::now() + timeout;
        loop {
            let remain = deadline.saturating_duration_since(Instant::now());
            if remain.is_zero() {
                return Err(AsrError::Timeout("end".into()));
            }
            match self.lines.recv_timeout(remain) {
                Ok(line) => {
                    let value: serde_json::Value = serde_json::from_str(&line)
                        .map_err(|e| AsrError::Protocol(e.to_string()))?;
                    match value.get("type").and_then(|t| t.as_str()) {
                        Some("end") => {
                            let text = value
                                .get("text")
                                .and_then(|t| t.as_str())
                                .unwrap_or("")
                                .to_string();
                            return Ok(text);
                        }
                        Some("error") => return Err(AsrError::Worker(line)),
                        _ => continue,
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
            Ok(line) => {
                let parsed = serde_json::from_str::<serde_json::Value>(&line);
                match parsed {
                    Ok(value) => match value.get("type").and_then(|t| t.as_str()) {
                        Some("end") => {
                            let text = value
                                .get("text")
                                .and_then(|t| t.as_str())
                                .unwrap_or("")
                                .to_string();
                            Some(Ok(text))
                        }
                        Some("error") => Some(Err(AsrError::Worker(line))),
                        _ => None,
                    },
                    Err(e) => Some(Err(AsrError::Protocol(e.to_string()))),
                }
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => None,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                Some(Err(AsrError::Worker("worker が終了しました".into())))
            }
        }
    }

    pub fn shutdown(mut self) {
        drop(self.stdin);
        let deadline = Instant::now() + Duration::from_millis(800);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => {
                    if Instant::now() > deadline {
                        let _ = self.child.kill();
                        let _ = self.child.wait();
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(_) => {
                    let _ = self.child.kill();
                    return;
                }
            }
        }
    }
}

pub struct WavData {
    pub sample_rate: u32,
    pub pcm_le: Vec<u8>,
}

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
        let size = u32::from_le_bytes([bytes[pos + 4], bytes[pos + 5], bytes[pos + 6], bytes[pos + 7]]) as usize;
        let body_start = pos + 8;
        let body_end = (body_start + size).min(bytes.len());
        if id == b"fmt " {
            if body_end - body_start >= 16 {
                channels = u16::from_le_bytes([bytes[body_start + 2], bytes[body_start + 3]]);
                sample_rate =
                    u32::from_le_bytes([bytes[body_start + 4], bytes[body_start + 5], bytes[body_start + 6], bytes[body_start + 7]]);
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
