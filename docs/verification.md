# 実行場所: GitHub Actions

Rustのビルド・テスト・clippy・release生成はGitHub ActionsのWindows runnerで行う。利用者のPCでは実行しない。以下のCargo/verifyコマンド例もCI用である。verify.ps1はCI以外で既定停止する。ローカル実行には利用者の新たな明示許可が必要。ローカルでは編集、差分確認、リモートログと成果物の取得を行う。

# 開発とAIの検証手順

以下はWindows Actions runnerとPowerShell 7.2以降で、リポジトリ直下から実行する。初回だけRustの依存を `cargo fetch --locked` で取得する。検証中のCargoは `--offline --locked` を使う。モデル、マイク、APIキーは通常テストに不要。

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

`build.rs` は自身と `Cargo.toml`、埋込アイコン `assets/branding/app.ico` の更新だけを監視する。レポートの追加でWindowsリソースを作り直さない。初回のreleaseコンパイルとLTOは重いため、小さい変更ではFastを使い、Fullは統合後にまとめる。

実行ごとに `target/qa/<日時とID>/` を作り、最後に `result.json` のパスを表示する。`APPDATA`、`LOCALAPPDATA`、検証用ミューテックス名を隔離し、終了時に元の環境変数を戻す。作成したディレクトリとログは証拠として残す。子プロセスはコンソールウィンドウを出さずに実行する。

AIは終了コードとJSONの両方を読む。`status` が `passed` でも、対象は `scope` に記録された範囲だけである。各チェックには成否、所要時間、終了コード、相対ログパスがあり、失敗後のチェックは `not_run` になる。`ignored` と `manualPrerequisites` は未実施であり、成功件数へ含めない。フィルターに一致するテストが0件の場合も失敗する。

`revision` はHEADで、`dirty` は未コミット差分の有無。`sourceSha256` は実行開始時のsrc・tests・examples・scripts・assetsとCargo設定などの内容をまとめたSHA-256で、新規ファイルも含む。差分がある測定をコミットだけで再現できるとは扱わない。検証中は同じ作業ツリーを編集せず、並列担当は別worktreeと別の出力先を使う。Cargoの出力先を共有した場合、ビルドはCargo自身のロックで順番に進む。ロック待ちを障害としてビルドプロセスを停止しない。

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

## 前面に出さない設定画面の確認

`scripts/verify-ui.ps1` はActionsでビルド済みexeを、専用の非表示Windows desktopで検査する。CI以外ではコンパイル前に停止する。既存の利用者プロセスや画面設定は変更しない。

```powershell
pwsh -NoProfile -File scripts/verify-ui.ps1 -Executable target/release/openhinovoice.exe
# ハーネスのコンパイルだけ。UI成功とは扱わない
pwsh -NoProfile -File scripts/verify-ui.ps1 -Executable target/release/openhinovoice.exe -CompileOnly
```

内部のC#ワーカーと設定プロセスを同じ別desktopへ配置し、SwitchDesktopとグローバル入力は使わない。起動はCreateNoWindow付きで、APPDATA・LOCALAPPDATA・OHV_TEST_NAMESPACEを各ケースへ隔離する。無音・オフラインの専用設定を作成し、OHV_TEST_SETTINGS_PAGE=displayで「音と表示」を開く。600×420・720×520 logical pxへclient領域を変更して、次を出力する。

- `result.json`：exeとハーネスのSHA-256、コンパイルとUIの成否、未実施項目。CompileOnlyのstatusはcompiled_only、UIはnot_run。
- `controller-result.json`・`worker-result.json`：別desktop、処理phase、子PID、終了、タイムアウト。UIAが停止した場合も最後に完了したphaseを残す。
- 各サイズの`uia.json`：名前・role・フォーカス可能状態・現在のfocus・画面内外・矩形。可視の「試す」操作が取得できなければ失敗。
- 各サイズの`client.png`：egui公式screenshot callbackから取得した実GL framebuffer。専用namespaceと隔離された保存先でだけ有効になる。失敗または単色に近い画像は失敗。非単色だけでレイアウト・可読性の成功とは扱わず、画像は別途確認する。
- 7操作のUIA状態と画像：ソフト選択、保存済みへ戻す、試す、停止、再試行、ページ離脱、復帰。操作後の期待状態を待ってから撮影し、設定ファイルが保存されていないことも確認する。
- 各サイズのPrivate・Working Set・CPU・GDI/USER観測とstdout/stderr。UIA・キャプチャを挟む前後値なので、アイドル性能や描画p95/p99とは区別する。

ワーカーは90秒で期限切れとなり、作成したJobに所属する子だけを回収する。通常は設定へWM_CLOSEを送り実終了を待つ。期限切れ、実ウィンドウ欠損、UIA非対応、キャプチャ失敗を成功へ読み替えない。別desktopでOpenGLの出力が得られない環境もあり、その場合は画像確認を未達として残す。

ネイティブのDPIは現在のWindows設定を記録するだけで、125〜200%の実画面確認や高コントラスト切替の代わりにはならない。キーボード操作・音・マイク・ASR・貼付・外部APIもこの検証の対象外。

本番HUDと同じ描画だけを確認する場合は、Actionsのrelease成果物に含まれる`overlay_preview.exe <output-directory> --all-dpi`を実行する。4スタイル×9状態×2テーマ×2動作設定×5倍率のBMPとindex.jsonを出力し、ウィンドウ・音・マイク・本番状態を作らない。こちらは設定画面全体の実画面確認とは区別する。index.jsonのrender_usは単発のfixture生成時間であり、性能ベンチマークではない。

別desktopの指定とUIAの呼出し条件は[CreateDesktopW](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-createdesktopw)、[STARTUPINFOW.lpDesktop](https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/ns-processthreadsapi-startupinfow)、[UI Automationのスレッド](https://learn.microsoft.com/en-us/dotnet/framework/ui-automation/ui-automation-threading-issues)を参照。

同じexampleの `--benchmark` は標準・リッチの録音/認識/整形/成功を3反復、各120フレーム描画し、各状態の前に12フレームをwarmupする。`benchmark.json` にp50/p95/p99/max、プロセスCPU時間、Private/WS/GDI/USERを残す。さらに4スタイル・9状態の切替を100回ずつ2周し、2周目のGDI/USER非増加とPrivate増分1MiB以内を確認する。超過は終了コード1となる。

```powershell
./target/release/examples/overlay_preview.exe <output-directory> --benchmark
```

これは時間間隔を空けないoffscreenの処理時間測定で、実HUDのUpdateLayeredWindow、eguiのtexture upload、実際の60fps間隔、入力遅延は含まない。Windowsが動きを無効にしている場合はその条件をJSONへ記録する。100切替もCPU描画側の資源確認であり、設定画面の「試す」100回や停止後の周期repaintを確認した証拠にはしない。

## 同じ認識workerを再利用する測定

Realtimeはマイク・AI・貼付を使わず、既存WAVを実時間で本番セッションへ供給する。新規processもOS/model cacheは消去しない。

```powershell
./scripts/verify.ps1 -Mode Live -LiveTest Realtime -Corpus short -RealtimeWorkerMode Fresh -RealtimeRepetitions 3 -SpeechExe <既存speech.exe絶対パス> -ModelPath <取得済みgguf絶対パス>
./scripts/verify.ps1 -Mode Live -LiveTest Realtime -Corpus short -RealtimeWorkerMode Reuse -RealtimeRepetitions 4 -SpeechExe <同じspeech.exe> -ModelPath <同じgguf>
```

Reuseの先頭は初回起動。後続3sessionが同じPID・追加spawnなしの比較対象になる。各sessionにPID、spawn数、ready、仮認識、Enter後、backlog、CER、要求受付と終端の時刻を記録する。受付から終端まではqueue待ちと推論の両方を含む。取消終端欠損、未対応終端、未送信確定区間、未完了要求、trace上限超過を完了扱いしない。現プロトコルで同じIDを再使用した後の古い終端を厳密識別できない制約も結果へ記録する。UI/描画/通常buildとASRの測定は同時に実行しない。

長いverifyの実行中は各checkの `*.stdout.log` / `*.stderr.log` を読める。終了後の集約 `*.log` とresult.jsonでexit code・成功数を判断し、途中ログだけで成功としない。起動した子の完了まで待ち、途中で失われた検証は成功に含めない。

## CI上の反復性能測定

`scripts/measure-ui.ps1` はGitHub Actions専用。`-CompileOnly`はハーネスのコンパイルだけを確認し、測定成功とは区別する。測定では明示した2つのexeを別desktopで交互の順に3〜10回起動し、各条件30〜60秒観測する。`-Mode Settings`はUIA照会の前後を分け、UIAなし・ありは同じソースとrelease条件で比較する。exe hash/bytes、Private/WS、CPU時間、GDI/USER、応答するウィンドウまでの時間、正常終了をJSONへ記録する。

UIAなしは初回の空のtreeだけで判定せず5秒間観測する。子孫processがケース間に残れば後続の測定を止める。採取成功のstatus=measuredと予算合格は別で、privateExceeded/executableExceededを必ず読む。非対話desktopやOpenGL不足で起動できない場合は失敗のまま記録する。実HUD稼働・入力遅延・マイク・ASR・貼付はこの測定に含まない。

Actionsのrenderer専用ジョブは `verify-renderer.ps1` で一時的にclient-area animationを有効化し、子exe自身が有効状態を読んだことを確認する。720fixture/24測定の後、元の設定へfinallyで戻す。Windowsの動き設定を変更するこの処理はGitHub-hosted Windowsだけで動き、ローカル・self-hostedでは開始前に停止する。過去のmotion無効の数値は静止描画の比較として区別する。

GitHub-hosted Windowsの標準OpenGLではeguiが起動できないことを実際のダイアログで確認したため、UI/設定Private比較には固定版Mesa llvmpipeを使う。`prepare-ci-opengl.ps1`が公開SHA256を照合してからtarget/qa配下へDLLを展開し、検証済みexeの隔離コピーだけを同じフォルダーへ置く。配布artifactへDLLは追加しない。software描画条件・archive/DLL hashを保存し、実機GPUの性能値とは扱わない。

通常CIはRust作業の前にUI/性能ハーネスをcompile-onlyで検査し、C#の構文・参照エラーを先に止める。UIのcompile-onlyにはアプリexeは不要。選択式ボタンは公開されたToggle/SelectionItem、通常ボタンはInvokeで操作し、使用したpatternと状態変化を記録する。性能測定のケース間検査は単純なプロセス数ではなく、起動前に検証したworker/コンソール基盤のPID・実行ファイル・生成時刻を照合する。未知の残留があれば後続は実行しない。
