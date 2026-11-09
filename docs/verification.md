# 開発とAIの検証手順

WindowsとPowerShell 7.2以降で、リポジトリ直下から実行する。初回だけRustの依存を `cargo fetch --locked` で取得する。検証中のCargoは `--offline --locked` を使う。モデル、マイク、APIキーは通常テストに不要。

```powershell
pwsh -NoProfile -File scripts/verify.ps1 -Mode Fast
pwsh -NoProfile -File scripts/verify.ps1 -Mode Fast -Filter recovery
pwsh -NoProfile -File scripts/verify.ps1 -Mode Full
```

| モード | 実行内容 | 用途 |
|---|---|---|
| Fast | fmtと全ターゲットの通常テスト | 小さい実装単位の確認 |
| Fast + Filter | fmtと名前が一致する通常テスト | 関連する回帰試験。0件なら失敗 |
| Full | fmt、clippy、通常テスト、releaseビルド | 統合後とCI。releaseビルドはこの段階にまとめる |
| Live | 指定した認識診断テスト1件 | 取得済みモデルと評価音声による測定 |

本体と統合テストは共通の `openhinovoice` ライブラリを使う。`#[path]` でソースを再取り込みしない。各モジュールの単体テストと `app.rs` の制御テストはライブラリで一度だけ実行される。`main.rs` はアプリの呼び出しだけを行う。

`build.rs` は自身と `Cargo.toml` の更新だけを監視する。レポートの追加でWindowsリソースを作り直さない。初回のreleaseコンパイルとLTOは重いため、小さい変更ではFastを使い、Fullは統合後にまとめる。

実行ごとに `target/qa/<日時とID>/` を作り、最後に `result.json` のパスを表示する。`APPDATA`、`LOCALAPPDATA`、検証用ミューテックス名を隔離し、終了時に元の環境変数を戻す。作成したディレクトリとログは証拠として残す。子プロセスはコンソールウィンドウを出さずに実行する。

AIは終了コードとJSONの両方を読む。`status` が `passed` でも、対象は `scope` に記録された範囲だけである。各チェックには成否、所要時間、終了コード、相対ログパスがあり、失敗後のチェックは `not_run` になる。`ignored` と `manualPrerequisites` は未実施であり、成功件数へ含めない。フィルターに一致するテストが0件の場合も失敗する。

`revision` はHEADで、`dirty` は未コミット差分の有無。`sourceSha256` は実行開始時のsrc・tests・examples・scriptsとCargo設定などの内容をまとめたSHA-256で、新規ファイルも含む。差分がある測定をコミットだけで再現できるとは扱わない。検証中は同じ作業ツリーを編集せず、並列担当は別worktreeと別の出力先を使う。Cargoの出力先を共有した場合、ビルドはCargo自身のロックで順番に進む。ロック待ちを障害としてビルドプロセスを停止しない。

非同期writerのテストは、送信を予約した時点と実際に書き込まれた時点を分けて確認する。固定のsleep後に成功を仮定せず、対象要求のtypeとidを期限付きで待つ。必要ならテスト用sinkをロックし、write前の状態を意図的に作る。

実認識の例は次のとおり。`ModelPath` は取得済みのGGUFファイルのフルパスを指定する。検証スクリプトはモデルを取得しない。

```powershell
pwsh -NoProfile -File scripts/verify.ps1 -Mode Live -LiveTest Segmented `
  -SpeechExe 'C:\path\speech.exe' -ModelPath 'C:\path\Haas.gguf' `
  -Corpus short -CorpusDir tests/corpus
```

Liveは既存ファイルを確認し、WAV、正解文、モデルのSHA-256を記録する。指定した音声か正解が欠ければ認識を始める前に失敗する。`-LiveTest Worker` は一区間のworker診断、`Segmented` は事前分割した全区間の逐次診断。どちらも録音、仮字幕、Enter時点の未処理量、AI、貼り付けを含む通し試験ではない。

`-LiveTest Realtime` は同じ明示引数で本番の認識セッションを使う。WAVを25msずつ実時間で渡し、本番の区切り、仮認識、確定区間の優先処理、Enterから認識完了までを通す。ポンプの待機時間も本番と共有する。音声供給が予定より遅れた最大時間を記録するため、高負荷で供給時刻がずれた測定を通常の測定と混ぜない。

RealtimeのJSONには最初の非空partial結果の時刻、未認識PCM量の最大値・Enter時点・1秒ごとの推移、Enterから認識完了まで、全文のCERを残す。partialが出ない場合は `null` であり、字幕表示の成功とは扱わない。マイク、Windowsのイベントループと描画、AI、履歴、貼り付けは含まない。アプリ全体の実機試験は別に行う。

実機の入力試験と外部AI試験は別ゲートにする。`cargo test -- --ignored` の一括実行は、マイク、前面アプリへの入力、実APIを同時に動かすため使わない。確認する試験名、前面アプリ、設定の保存先を決めて個別に実行する。実アカウントの操作は利用者が行う。手動確認は対象アプリ・条件・結果を記録し、自動テストの成功で代替しない。
