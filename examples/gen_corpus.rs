//! 評価用の音声 corpus を、speech.cpp の TTS(irodori-tts)で合成して作る。
//! 実行例:
//!   cargo run --release --example gen_corpus
//! 生成物は tests/corpus/ に置かれ、WAV は Git の管理外(*.wav を無視)なので、
//! この例を再実行すれば何度でも同じ手順で作り直せる。
//!
//! 使い方:
//!   cargo run --release --example gen_corpus -- [--speech PATH] [--tts MODEL] [--out DIR]

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::os::windows::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{channel, Receiver};
use std::time::Duration;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const TARGETS: &[(&str, f64)] = &[
    ("short", 15.0),
    ("1min", 60.0),
    ("5min", 300.0),
    ("30min", 1800.0),
];
/// 文のあいだに入れる無音。発話の区切りをはっきりさせる
const GAP_SECONDS: f64 = 0.5;
const OUT_RATE: u32 = 16000;

fn main() {
    let mut speech_exe = String::from("speech");
    let mut tts_model = String::from("irodori-tts-mf:q4_k");
    let mut out_dir = std::path::PathBuf::from("tests/corpus");
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--speech" => speech_exe = args.next().expect("--speech には値が必要です"),
            "--tts" => tts_model = args.next().expect("--tts には値が必要です"),
            "--out" => out_dir = args.next().expect("--out には値が必要です").into(),
            other => {
                eprintln!("不明な引数: {other}");
                std::process::exit(2);
            }
        }
    }

    let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let script = std::fs::read_to_string(manifest.join("tests/corpus/script.txt"))
        .expect("tests/corpus/script.txt を読めません");
    let sentences: Vec<&str> = script
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    println!("文の数: {}", sentences.len());

    std::fs::create_dir_all(&out_dir).expect("出力フォルダを作れません");
    let mut tts = TtsWorker::spawn(&speech_exe, &tts_model);
    tts.wait_ready(Duration::from_secs(180));
    println!("TTS の準備ができました(sample_rate={})", tts.sample_rate);

    let mut sentence_index = 0usize;
    for (name, target_seconds) in TARGETS {
        let mut pcm: Vec<i16> = Vec::new();
        let mut ref_lines: Vec<String> = Vec::new();
        let started = std::time::Instant::now();
        while (pcm.len() as f64 / OUT_RATE as f64) < *target_seconds {
            let sentence = sentences[sentence_index % sentences.len()];
            sentence_index += 1;
            print!(
                "\r{}: {:.1}s / {}s(経路 {:.0}s)",
                name,
                pcm.len() as f64 / OUT_RATE as f64,
                target_seconds,
                started.elapsed().as_secs_f64()
            );
            let _ = std::io::stdout().flush();
            let synthesized = tts.synthesize(sentence);
            pcm.extend_from_slice(&synthesized);
            pcm.extend(std::iter::repeat_n(
                0i16,
                (GAP_SECONDS * OUT_RATE as f64) as usize,
            ));
            ref_lines.push(sentence.to_string());
        }
        println!();
        write_wav(&out_dir.join(format!("{name}.wav")), OUT_RATE, &pcm);
        std::fs::write(
            out_dir.join(format!("{name}.ref.txt")),
            ref_lines.join("\n") + "\n",
        )
        .expect("正解文章を書けません");
    }
    tts.shutdown();
    println!("完了");
}

/// speech worker を 1 つ起動し、文ごとに synthesize 要求を送って PCM を受け取る
struct TtsWorker {
    child: Child,
    stdin: std::process::ChildStdin,
    lines: Receiver<String>,
    sample_rate: u32,
    default_voice: Option<String>,
    next_id: u64,
    pending: VecDeque<String>,
}

impl TtsWorker {
    fn spawn(speech_exe: &str, model: &str) -> TtsWorker {
        let mut child = Command::new(speech_exe)
            .arg("worker")
            .arg(model)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .expect("speech worker を起動できません");
        let stdout = child.stdout.take().expect("stdout を取得できません");
        let stdin = child.stdin.take().expect("stdin を取得できません");
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
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
        // worker は 1 行ずつ読む前提なので、まとめて書いても順序は保たれる
        TtsWorker {
            child,
            stdin,
            lines: rx,
            sample_rate: 48000,
            default_voice: None,
            next_id: 1,
            pending: VecDeque::new(),
        }
    }

    fn wait_ready(&mut self, timeout: Duration) {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            assert!(
                std::time::Instant::now() < deadline,
                "worker が ready を返しません"
            );
            let line = self
                .lines
                .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
                .expect("worker が終了しました");
            let value: serde_json::Value =
                serde_json::from_str(&line).expect("worker の応答が JSON ではありません");
            match value["type"].as_str() {
                Some("ready") => {
                    self.sample_rate =
                        value["model"]["sample_rate"].as_u64().unwrap_or(48000) as u32;
                    // voice が必須のモデルは、用意されている最初の voice を既定として使う
                    self.default_voice = value["model"]["voices"][0]["name"]
                        .as_str()
                        .map(str::to_string);
                    return;
                }
                Some("fatal") => panic!("worker を起動できません: {value}"),
                _ => {}
            }
        }
    }

    /// 1 文を合成し、OUT_RATE に間引いた PCM を返す
    fn synthesize(&mut self, text: &str) -> Vec<i16> {
        let id = self.next_id.to_string();
        self.next_id += 1;
        let mut request = serde_json::json!({
            "type": "synthesize",
            "id": id,
            "text": text,
        });
        if let Some(voice) = &self.default_voice {
            request["voice"] = serde_json::Value::String(voice.clone());
        }
        let request = request.to_string();
        writeln!(self.stdin, "{request}").expect("worker への書き込みに失敗しました");
        let mut pcm: Vec<u8> = Vec::new();
        loop {
            let line = match self.pending.pop_front() {
                Some(pending) => pending,
                None => self
                    .lines
                    .recv_timeout(Duration::from_secs(300))
                    .expect("worker の応答が止まりました"),
            };
            let value: serde_json::Value =
                serde_json::from_str(&line).unwrap_or(serde_json::Value::Null);
            match value["type"].as_str() {
                Some("chunk") if value["id"].as_str() == Some(id.as_str()) => {
                    use base64::Engine;
                    let encoded = value["pcm"].as_str().expect("chunk に pcm がありません");
                    pcm.extend(
                        base64::engine::general_purpose::STANDARD
                            .decode(encoded)
                            .expect("pcm の base64 が不正です"),
                    );
                }
                Some("end") if value["id"].as_str() == Some(id.as_str()) => {
                    if value["stop"].as_str() != Some("complete") {
                        eprintln!("警告: 文が途中で切れました: {text}");
                    }
                    let samples: Vec<i16> = pcm
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .map(|b| i16::from_le_bytes(*b))
                        .collect();
                    return resample_to_16k(&samples, self.sample_rate);
                }
                Some("error" | "cancelled") if value["id"].as_str() == Some(id.as_str()) => {
                    panic!("合成に失敗しました: {value}");
                }
                _ => {}
            }
        }
    }

    fn shutdown(mut self) {
        drop(self.stdin);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            match self.child.try_wait() {
                Ok(Some(_)) | Err(_) => return,
                Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// 3 の倍数のサンプルレート(48kHz など)から 16kHz へ、低域通過フィルタを挟んで間引く
fn resample_to_16k(samples: &[i16], source_rate: u32) -> Vec<i16> {
    if source_rate == OUT_RATE {
        return samples.to_vec();
    }
    let factor = source_rate as usize / OUT_RATE as usize;
    assert_eq!(
        source_rate as usize % OUT_RATE as usize,
        0,
        "サンプルレートが {OUT_RATE} の整数倍ではありません: {source_rate}"
    );
    let taps = lowpass_taps(source_rate as f32, 6800.0, 33);
    let half = taps.len() / 2;
    let mut out = Vec::with_capacity(samples.len() / factor + 1);
    for out_index in 0..samples.len() / factor {
        let center = out_index * factor;
        let mut sum = 0.0f32;
        for (t, weight) in taps.iter().enumerate() {
            let src = center as isize + t as isize - half as isize;
            if src < 0 || src as usize >= samples.len() {
                continue;
            }
            sum += samples[src as usize] as f32 * weight;
        }
        out.push(sum.clamp(-32767.0, 32767.0).round() as i16);
    }
    out
}

/// 窓関数つき sinc の低域通過フィルタ。DC 利得が 1 になるよう正規化する
fn lowpass_taps(sample_rate: f32, cutoff_hz: f32, length: usize) -> Vec<f32> {
    let fc = cutoff_hz / sample_rate;
    let middle = (length - 1) as f32 / 2.0;
    let mut taps: Vec<f32> = (0..length)
        .map(|i| {
            let x = i as f32 - middle;
            let sinc = if x.abs() < 1e-9 {
                2.0 * fc
            } else {
                (2.0 * std::f32::consts::PI * fc * x).sin() / (std::f32::consts::PI * x)
            };
            let window =
                0.54 - 0.46 * (2.0 * std::f32::consts::PI * i as f32 / (length - 1) as f32).cos();
            sinc * window
        })
        .collect();
    let sum: f32 = taps.iter().sum();
    for tap in &mut taps {
        *tap /= sum;
    }
    taps
}

fn write_wav(path: &std::path::Path, sample_rate: u32, samples: &[i16]) {
    let data_len = (samples.len() * 2) as u32;
    let mut wav = Vec::with_capacity(44 + samples.len() * 2);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVE");
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&1u16.to_le_bytes()); // mono
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&(sample_rate * 2).to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        wav.extend_from_slice(&s.to_le_bytes());
    }
    std::fs::write(path, wav).expect("WAV を書けません");
}
