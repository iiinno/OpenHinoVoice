// Integration crates compile full production modules but use only a subset of their APIs.
#![allow(dead_code)]

#[path = "../src/audio.rs"]
mod audio;

#[test]
#[ignore = "needs a microphone; run explicitly"]
fn records_two_seconds() {
    let recorder = audio::Recorder::start().expect("recorder must start");
    let sample_rate = recorder.sample_rate;
    std::thread::sleep(std::time::Duration::from_millis(2100));
    let samples = recorder.drain();
    drop(recorder);
    assert!(sample_rate >= 8000, "unexpected sample rate {sample_rate}");
    let expected = sample_rate as f32 * 2.0;
    let count = samples.len() as f32;
    assert!(
        count > expected * 0.5 && count < expected * 1.6,
        "expected about {expected} samples, got {count}"
    );
    let rms = (samples.iter().map(|s| s * s).sum::<f32>() / samples.len().max(1) as f32).sqrt();
    println!(
        "sample_rate={sample_rate} samples={} rms={rms:.6}",
        samples.len()
    );

    let pcm = audio::samples_to_pcm_le(&samples);
    let mut wav = Vec::new();
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + pcm.len() as u32).to_le_bytes());
    wav.extend_from_slice(b"WAVE");
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&(sample_rate * 2).to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&(pcm.len() as u32).to_le_bytes());
    wav.extend_from_slice(&pcm);
    let out =
        std::env::var("JV_RECORD_OUT").unwrap_or_else(|_| "testdata/recorded.wav".to_string());
    std::fs::write(&out, &wav).expect("recorded wav must be written");
    println!("wrote {out}");
}
