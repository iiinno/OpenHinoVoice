# AGENTS.md: OpenHinoVoice を導入するコーディング AI への手順

この文書は、利用者から「OpenHinoVoice を導入して」と頼まれた AI(Claude Code、Codex など)向けの手順です。Windows 11 の PC に、管理者の権限なしで導入します。

## 守ること

- API キー、パスワード、ChatGPT のサインインは扱わない。利用者に、設定の画面で自分で入れてもらう。チャットに API キーを貼るよう頼まない
- インターネットからスクリプトを実行する手順(1 の speech.cpp の導入)は、実行する前に利用者に確認を取る
- 既存の `%APPDATA%\OpenHinoVoice\config.toml` があれば、上書きしない

## 手順

PowerShell で実行する。

### 1. speech.cpp があるか確かめ、なければ入れる

```powershell
Get-Command speech -ErrorAction SilentlyContinue
Test-Path "$env:LOCALAPPDATA\Programs\speech.cpp"
```

どちらも見つからなければ、利用者の了解を得てから公式のインストーラーを実行する。

```powershell
irm https://raw.githubusercontent.com/nyosegawa/speech.cpp/main/install.ps1 | iex
```

### 2. 最新版の exe を置く

```powershell
$dir = "$env:LOCALAPPDATA\Programs\OpenHinoVoice"
New-Item -ItemType Directory -Force $dir | Out-Null
Invoke-WebRequest https://github.com/iiinno/OpenHinoVoice/releases/latest/download/openhinovoice.exe -OutFile "$dir\openhinovoice.exe"
```

### 3. 起動する

```powershell
Start-Process "$env:LOCALAPPDATA\Programs\OpenHinoVoice\openhinovoice.exe"
```

二重に起動しても、2 つ目はすぐに終わるので害はない。

### 4. 動いていることを確かめる

```powershell
Get-Process openhinovoice
Test-Path "$env:APPDATA\OpenHinoVoice\config.toml"
```

プロセスがあり、設定ファイルができていれば導入は完了。

### 5. 利用者に伝えること

導入が終わったら、次をそのまま伝える。

1. タスクバー右下の OpenHinoVoice のアイコン(見当たらなければ時計の左の「^」の中)を右クリックし、「設定」を開く
2. 「一般」で録音を始めるキーを決め、自動起動を使うならチェックを入れる
3. AI で文を整えたいなら、「整形(AI)」で接続先を選ぶ。ChatGPT の Plus か Pro を契約していれば「Continue with ChatGPT」でサインインする。API キーを使う場合は、自分で入力する
4. 「接続テスト」で確かめてから「保存」を押す
5. メモ帳などで、録音を始めるキーを押して話し、Enter を押して試す。最初の 1 回は認識モデルのダウンロードに 1 分ほどかかる

## うまくいかないとき

- `Invoke-WebRequest` が失敗する: リリースがまだない可能性がある。Rust がある PC なら、リポジトリを取得して `cargo build --release` し、`target\release\openhinovoice.exe` を 2 の場所に置く
- 録音しても何も貼られない: `speech.exe` が見つかっていない。`%APPDATA%\OpenHinoVoice\config.toml` の `speech_exe` に、speech.exe のフルパスを書く
- 録音が始まらない: 録音開始キーがほかのアプリと重なっている。設定の画面で Ctrl+Alt+J のような組み合わせに変える
