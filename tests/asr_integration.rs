// Integration crates compile full production modules but use only a subset of their APIs.
#![allow(dead_code)]

use std::time::Duration;

#[path = "../src/asr.rs"]
mod asr;
#[path = "../src/chatgpt.rs"]
mod chatgpt;
#[path = "../src/config.rs"]
mod config;
#[path = "../src/format.rs"]
mod format;

#[test]
#[ignore = "needs the speech.cpp worker, a Haas model and a test WAV; run explicitly"]
fn wav_is_transcribed() {
    let wav_path = std::env::var("JV_ASR_WAV").expect("JV_ASR_WAV must point to a test WAV");
    let expect = std::env::var("JV_ASR_EXPECT_B64")
        .ok()
        .and_then(|b64| {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD.decode(b64).ok()
        })
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .unwrap_or_else(|| std::env::var("JV_ASR_EXPECT").unwrap_or_default());
    let bytes = std::fs::read(&wav_path).expect("test WAV must be readable");
    let wav = asr::parse_wav_pcm16(&bytes).expect("WAV must be 16bit PCM");

    let cfg = config::load().expect("config must load");
    let exe = std::env::var("JV_SPEECH_EXE").unwrap_or(cfg.speech_exe.clone());
    let model = std::env::var("JV_MODEL").unwrap_or(cfg.model.clone());

    let mut worker = asr::Worker::spawn(&exe, &model).expect("worker must spawn");
    worker
        .wait_ready(Duration::from_secs(60))
        .expect("worker must become ready");

    let chunk_bytes = (wav.sample_rate as usize / 10) * 2;
    for chunk in wav.pcm_le.chunks(chunk_bytes.max(2)) {
        while !worker.send_chunk(chunk).expect("chunk must be queued") {
            worker.poll_writer().expect("writer must make progress");
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    while !worker
        .transcribe(wav.sample_rate, "")
        .expect("transcribe must be queued")
    {
        worker.poll_writer().expect("writer must make progress");
        std::thread::sleep(Duration::from_millis(1));
    }
    let text = worker
        .wait_end(Duration::from_secs(120))
        .expect("transcription must finish");
    worker.shutdown();
    println!("transcription: {text}");

    assert!(!text.trim().is_empty(), "transcription was empty");
    if !expect.is_empty() {
        assert!(
            text.contains(&expect),
            "expected transcription to contain {expect:?}, got {text:?}"
        );
    }
}
