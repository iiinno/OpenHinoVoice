# OpenHinoVoice

[日本語](README.md)

Japanese voice input for Windows. Press a key, speak, press Enter, and what you said is typed into the app you are using.

Speech recognition runs entirely on your PC; audio is never sent anywhere. Optionally, an AI can clean up the transcript (remove fillers such as "えーと", fix punctuation).

## Features

- One key starts recording; Enter transcribes and pastes
- Press Enter once to paste cleaned-up text. Press it again without waiting to paste the raw transcript immediately
- While recording, a small Hinomaru-style pill appears near the top of the screen. Its bars follow your voice, so you can see it is listening
- Short sound effects on start and paste (can be turned off)

## Install

About five minutes. No administrator rights needed.

### 1. Install speech.cpp

OpenHinoVoice uses [speech.cpp](https://github.com/nyosegawa/speech.cpp) for speech recognition. Open PowerShell and run:

```powershell
irm https://raw.githubusercontent.com/nyosegawa/speech.cpp/main/install.ps1 | iex
```

### 2. Download OpenHinoVoice

Download `openhinovoice.exe` from the [latest release](https://github.com/iiinno/OpenHinoVoice/releases/latest). Put it anywhere, for example `%LOCALAPPDATA%\Programs\OpenHinoVoice`, and double-click it. If Windows SmartScreen says "Windows protected your PC", click "More info", then "Run anyway". New unsigned apps can show this warning.

**The published executable is not code signed.** The product name and version shown in Windows file properties identify the build, but they are not a digital signature proving who published it. Check the download source before running it.

Each release also includes `LICENSE`, `THIRD_PARTY_LICENSES.txt`, and `SHA256SUMS.txt`. To check the downloaded EXE's integrity, run `Get-FileHash .\openhinovoice.exe -Algorithm SHA256` in PowerShell and compare its result with `SHA256SUMS.txt`. A matching hash alone does not establish publisher identity.

An icon appears in the notification area at the bottom right. If you don't see it, click the "^" next to the clock.

### 3. Configure

Right-click the icon and choose 設定 (Settings).

1. On 一般 (General), choose the hotkey (F8 by default). Tick the start-at-sign-in box if you want it to start automatically
2. To clean up text with AI, choose a provider on 整形(AI). With ChatGPT Plus or Pro, pick the ChatGPT plan and press "Continue with ChatGPT"
3. Press 接続テスト (connection test) to check, then 保存 (Save)

### 4. Try it

Open Notepad, press the hotkey, speak, and press Enter. The first time, the recognition model (about 0.35 GB) downloads, which takes about a minute.

## Let an AI install it

If you use a coding AI such as Claude Code or Codex, [AGENTS.md](AGENTS.md) has step-by-step instructions for it. Give it this message:

```text
Follow AGENTS.md in https://github.com/iiinno/OpenHinoVoice to install OpenHinoVoice on this Windows PC.
```

The AI only installs and starts the app. Enter API keys and sign in to ChatGPT yourself, in the settings window.

## Usage

| Key | Action |
|---|---|
| Hotkey (F8 by default) | Start recording |
| Enter | Stop, transcribe, and paste the cleaned-up text |
| Enter again | Paste the raw transcript without waiting |
| Esc | Cancel. Nothing is pasted |

Text is pasted through the clipboard, and the previous clipboard text is restored. Enter is not sent after pasting.

To start recording from a mouse button, assign a key combination such as Ctrl+Alt+J to the button in your mouse software (such as Logi Options+), and set the same combination as the hotkey.

## AI providers

| Provider | For |
|---|---|
| ChatGPT plan | ChatGPT Plus or Pro subscribers. No extra cost; uses your plan |
| OpenCode Go | OpenCode Go subscribers. Uses a low-cost model (`deepseek-v4.1-flash` by default) |
| OpenAI API key | OpenAI API users. Uses `gpt-6-luna` by default |
| Other | Any OpenAI-compatible API |

Only the transcribed text is sent. Check each provider's terms for how they handle data. ChatGPT sign-in tokens are encrypted with Windows DPAPI and stored on this PC.

OpenCode Go's terms describe it as designed for OpenCode and other coding agents. Cleaning up dictation is outside that intended use, so use it at your own discretion.

## Troubleshooting

| Problem | What to check |
|---|---|
| No icon | Look under "^" next to the clock, or start `openhinovoice.exe` again |
| Hotkey does nothing | Another app may use the same key. Try a combination such as Ctrl+Alt+J |
| "speech.exe" not found | Redo step 1, or set the speech.exe path on the 認識 (Recognition) page |
| "生テキストを貼りました" (pasted raw text) | The AI cleanup timed out or failed. The connection test shows why |

The settings file is `%APPDATA%\OpenHinoVoice\config.toml`.

## Build from source

```
cargo build --release
```

The executable is `target\release\openhinovoice.exe`. Run the tests with `cargo test`; tests that need a real microphone or API are `#[ignore]`d.

Windows CI runs `cargo fmt --all -- --check`, `cargo check --all-targets --locked`, `cargo clippy --all-targets --locked -- -D warnings`, and `cargo test --all-targets --locked`. A release tag such as `v0.1.0` must match `Cargo.toml`'s `version`. The Windows executable embeds `FileVersion` and `ProductVersion` from that Cargo version and uses `OpenHinoVoice` as its `ProductName`. Existing releases are never overwritten.

## Special thanks

OpenHinoVoice is built on work shared by these people. Thank you.

- [speech.cpp](https://github.com/nyosegawa/speech.cpp) by nyosegawa (MIT): fast, fully local speech recognition
- [Haas-ja-v1.0](https://huggingface.co/third-intelligence/Haas-ja-v1.0) by Third Intelligence (Apache-2.0): a fast and accurate Japanese speech recognition model
- [Haas-ja-v1.0 GGUF](https://huggingface.co/sakasegawa/Haas-ja-v1.0-GGUF) by sakasegawa (Apache-2.0): the model files for speech.cpp
- [earshot](https://github.com/Leonxlnx/earshot) by Leon Lin (MIT): used to make the sound effects
- [egui](https://github.com/emilk/egui) by Emil Ernerfeldt and contributors (MIT or Apache-2.0): the settings window
- All bundled libraries and their licenses are listed in [THIRD_PARTY_LICENSES.txt](THIRD_PARTY_LICENSES.txt)

## Rights and terms

- OpenHinoVoice is a personal project. It is not affiliated with or endorsed by OpenAI, OpenCode, Third Intelligence or any other company or project named here. ChatGPT is a trademark of OpenAI
- The recognition model and speech.cpp are not included in this repository; they are downloaded from their official sources during setup
- The sound effects (`assets/sounds`) were synthesized entirely in code by earshot; no recorded material was used
- Sign in with ChatGPT follows OpenAI's published flow for open-source apps

## License

[MIT](LICENSE)
