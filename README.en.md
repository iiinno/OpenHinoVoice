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

## Credits

- Speech recognition: [speech.cpp](https://github.com/nyosegawa/speech.cpp) (MIT), [Haas-ja-v1.0](https://huggingface.co/third-intelligence/Haas-ja-v1.0) (Third Intelligence, Apache-2.0)
- Sound effects: made with [earshot](https://github.com/Leonxlnx/earshot) (MIT)
- Settings window: [egui](https://github.com/emilk/egui) (MIT or Apache-2.0)

## License

[MIT](LICENSE)
