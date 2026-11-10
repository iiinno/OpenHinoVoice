# AGENTS.md: OpenHinoVoice を導入するコーディング AI への手順

## 開発・検証する場合

- Rustのビルド・テスト・clippy・release生成はGitHub Actionsで実行する。利用者のPCでは実行しない。ローカル実行は利用者が改めて明示した場合だけにする。非表示であっても禁止は変わらない。
- ローカルでは編集、差分確認、CIログ・成果物の取得を行う。CIへ検証用ブランチを送って結果を確認し、失敗を修正する。

- 検証できる変更単位でこまめにコミットする。各単位の関連検証を通し、壊れた中間状態を残さない。
- UI改善中も性能の計測と改善を継続する。演出で実際の遅延を解消した扱いにしない。UIとASRの性能測定は同時に実行しない。
- 音・表示のプリセット、プレビュー、アイコンの実装は [UI・UX計画](docs/ui-ux-plan-20261109.md) の所有範囲と受入条件に従う。

- GitHub Actions上の通常検証は `pwsh -NoProfile -File scripts/verify.ps1 -Mode Fast`、統合後は `-Mode Full` を使う。詳細と実機試験は [docs/verification.md](docs/verification.md) を参照する。
- 検証は専用の APPDATA・LOCALAPPDATA と `OHV_TEST_NAMESPACE` を使う。利用者の設定、API キー、自動起動、稼働中のプロセスを検証用に変更しない。
- コマンドは非表示で実行する。Windows では `CreateNoWindow` / `windowsHide: true` と直接の実行ファイル・引数を使い、検証のたびにターミナルを開かない。起動した子プロセスだけを回収する。
- 担当ファイルを分け、既存の未コミット変更と他担当の変更を保持する。単体テストは共通ライブラリを使い、`#[path]` で実装を再取り込みしない。
- 非同期処理の完了を固定のsleepで推定しない。送信された要求や完了通知を期限付きで待ち、失敗時は観測内容を残す。
- 録音→Enter→実際の入力先本文への貼付は必須のコア検証とする。ReplayやSendInputの戻り値だけを成功根拠にしない。通常CIで実Windowsクリップボードと専用EDIT本文の回帰を通し、未実施・0件・失敗なら配布候補を作らない。
- 導入更新には、同じSHAのActions artifactと必須コア検証の成功を確認する。build.jsonのcoreClipboardVerifiedとcoreTextInsertionVerifiedがtrueであることを確認し、設定と旧exeを保持する。
- `result.json` の成功・失敗・未実施を確認する。テスト0件、欠損コーパス、モデル未指定を成功扱いしない。実マイク、対象アプリへの貼り付け、外部 API の検証は通常テストと区別して記録する。
- 性能の数値には対象経路と条件を併記する。逐次区間の認識時間を、実際の Enter から入力までの時間として扱わない。

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
2. 「入力」で録音を始めるキーを決め、自動起動を使うならチェックを入れる
3. AI で文を整えたいなら、「AI」で接続先を選ぶ。ChatGPT の Plus か Pro を契約していれば「Continue with ChatGPT」でサインインする。API キーを使う場合は、自分で入力する
4. 「接続テスト」で確かめてから「保存」を押す
5. メモ帳などで、録音を始めるキーを押して話し、Enter を押して試す。最初の 1 回は認識モデルのダウンロードに 1 分ほどかかる

## うまくいかないとき

- `Invoke-WebRequest` が失敗する: リリースの有無とGitHub ActionsのWindowsビルド結果を確認する。未公開の場合は検証済みartifactを確認し、利用者のPC上でのビルドへ切り替えない。
- 録音しても何も貼られない: `speech.exe` が見つかっていない。`%APPDATA%\OpenHinoVoice\config.toml` の `speech_exe` に、speech.exe のフルパスを書く
- 録音が始まらない: 録音開始キーがほかのアプリと重なっている。設定の画面で Ctrl+Alt+J のような組み合わせに変える
- オフラインモードで「認識モデルが未取得です」と出る: インターネットにつながった状態で 1 回録音すると、speech.cpp がモデルを取得する。そのあとオフラインモードに戻す
