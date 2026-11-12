# UI・UX実装記録

承認済み計画は docs/ui-ux-plan-20261109.md。開始基準は205efa0。ユーザーの3つの追跡外ファイルと既存worktreeを保持する。ターミナルは非表示。導入済みアプリ・利用者設定を変更しない。

## 手順と進捗

- [x] how: ConfigをSettingsのDraftで編集し、保存後は本体が再読込する。設定は別プロセス。音はWinMM、表示はWin32/GDI。overlay::presentが描画と表示を兼ね、プレビューへ分離が必要。
- [x] architect Ground: 録音と試聴が音を取り合う境界、非同期PCM寿命、静止描画の停止を確認。
- [x] architect Sketch: 音は単一active PCM所有案とLRU+pin案を比較。上限管理と寿命を小さく保てる単一active所有案を採用。ホームは既存状態の直接分岐と純粋な表示summaryを比較し、保存状態を増やさないsummaryを採用。
- [x] architect Agree: ユーザーが計画の実装を承認済み。通常の実装判断で停止しない。
- [x] Implement: U01〜U09のUI機能と資産、U12の測定入口を統合。受入検証の完了とは区別する。
- [ ] Verify: 関連Fast、統合Full、非表示の描画出力、音源解析と聴覚確認、同条件性能計測。
- [x] Commit: 変更単位ごとにコミット。Actions実行のためcodex/v050-finishへpush済み。リリース・mainへの統合は行っていない。
- [ ] Scrap: 選んだ構造に反復する無理が出た場合だけ再設計する。未実施の構造変更は成功扱いにしない。

## 並行実行の区切り

- Blocking first steps: U02の音再生所有を先に確定してからプリセットと試聴を接続する。
- Independent workstreams: 設定U01、音U02、アイコンU08を専用worktreeで同時に進める。
- Shared mutable state: 担当をファイルごとに割り当てる。Cargoと性能測定はGitHub Actions上でSHAを固定し、UI・ASR測定を同時実行しない。
- Smallest safe decomposition: settings.rs、sound.rs/app.rs、brandingを独立させる。feedback型、IPC、描画adapterは契約を確定し、同じファイルの担当を順に渡す。

## 次の契約

- プリセットは保存する個別値から導出し、プリセット名を二重保存しない。音セット、イベント有効、HUD style、motionを型で表す。既存キーは維持する。
- 試聴は本体プロセスが所有し、世代付き要求で古い停止を無視する。録音開始時に試聴を無効化する。別プロセスで音を同時再生しない。
- 本番とプレビューの描画は同じフレーム生成を使う。現在のGDI Canvasを使う案と共有幾何モデルを2rendererへ渡す案では、見た目の一致を保ちやすい前者を優先して小試作する。
- 初期設定の未知値警告、出力結果の音が一度だけ鳴ること、非表示とreduced motionで描画を停止することを受入条件に含める。

## 検証結果

初期基準は既存236テスト成功のソース。以下は検証ごとの対象と結果であり、後続差分まで合格したとは扱わない。

## UI統合（2026-11-10）

5プリセット、音/表示の独立選択、合成プレビュー、世代付き試聴IPC、結果通知の一本化、4スタイルHUD、Windows配色/動き設定、トレイ・exe・設定アイコンを接続した。表示全項目の戻す、取消後に注意表示が残る回帰、配色変更で静止プレビューが古いままになる回帰、トレイ再追加失敗時の有限再試行も修正した。

Fast: 282成功・0失敗・11明示実機試験を除外。fmt成功。sourceSha256 D1E7BDA9FCA808C25B91A71AAE2141C1E08AB5E966661A889925EE24947E370D。この時点のhashはassetsを除く旧方式。CI切替後のhashにはassetsも含むため、異なる方式のhashを同一対象として比較しない。生ログは integration/。初回はegui RawInputのAPI差分でcompile失敗し、修正後に上記を実行した。途中終了した最初のbackground検証は成功に含めない。

独立レビューはsettings/preview/HUDとapp/sound/IPCを相互担当が確認し、3件のUI欠陥と古い出力通知を修正。subclass参照寿命、トレイ再試行、描画cacheは別担当も静的確認した。現時点のFast成功は実画面・実音・実入力先・性能予算の達成を示さない。Fullと実画面検証は続行中。

## 追加検証と実行場所の変更

U12の再利用診断、起動記号・波紋、隔離画面のframebuffer captureを追加。Fastは294成功・0失敗・11明示除外、sourceSha256 2BAA19CE5AB1CEA008E269586638955B35BCB467DE6CC7DE72D6BEB93FB45A27。完了時刻2026-11-09T16:22:16Zで、ローカル停止指示より前に終了した結果。以後の変更の合格を意味しない。

Windows 125%DPIで600x420/720x520の実settingsを別desktopで起動し、UIA52要素、実GL framebuffer画像、正常終了を確認。入力desktopは前後ともDefault。PrintWindowは単色となったため失敗を記録し、egui公式screenshot callbackへ切り替えた。screenshots/はこの実描画。後続の視認性変更とUIA操作追加は別の未検証差分。

利用者の指示によりローカルのRustビルド・テストを停止し、GitHub Actionsへ切替。停止確認時にcargo/rustc/clippy-driver/linkerは0件。以後のFull・release・描画計測はCIへ委譲する。UIA/描画・ASRのCI runner上の制約は別に記録し、個人PCの過去の数値と同条件扱いしない。

## Actionsでの検証移行

- Rustのbuild/test/clippyとC#ハーネスのcompile/実行はGitHub-hosted Windowsへ移した。ローカル入口は開始前に停止する。
- CIはFull、release exe、SHA256SUMS、build.json、各検証ログを保存する。明示した[ui-check]コミットだけで別のUI診断ジョブも実行する。
- 7ba25e8の初回CIはClippy 3件で停止。Win32整数resource IDを維持して局所対応し、pixel検証をas_chunksへ変更。1055da9はfmt差分で停止し、CIの提示した書式を適用した。失敗後のテスト・releaseを成功には含めない。
- 最新のActions結果と成果物は確認後に別項へ記録する。

未確認は実キーボードのTab/Esc/Ctrl+S、実マイクと他アプリへの貼付、実モニターのDPI/配色変更、常駐・UIA増分と入力遅延の性能予算、モデルを使うU12反復測定。U13はその測定結果で原因を選ぶため未実装。

### Actions Full 合格（08ceaee）

https://github.com/iiinno/OpenHinoVoice/actions/runs/42657871038 の build-and-test jobでfmt・clippy・294テスト・release buildが成功。失敗0、実資源を必要とする11試験はignore。sourceSha256 E9D85EED736EFD8C228FF18EA961F14DD3B6EBD40F1ADEC7A4A745579889C43B。生ログとresult.jsonは actions-08ceaee/。UI診断jobは別判定。

Actions生成openhinovoice.exeは8,438,784 bytes、SHA256 2368dcf4df4d20365a36bfbdcd53801eb1ead3ceae555c01804cd255d63d1305。overlay_preview.exeは261,120 bytes。このPCでexeを起動・導入していない。

同runのUI jobは720 fixture生成・renderer 24測定が成功し、設定windowのサイズ検証で失敗。renderer計測時はWindowsの動き設定が無効だったため、これは静止描画の結果で、リッチの稼働時予算の合格ではない。CPU描画を100切替×2周後、2周目Private増分0、GDI/USER非増加。ネイティブ表示・入力遅延は対象外。ui-result.json/visual-status.jsonに失敗も保持。

### 最新のActions統合検証（460d466）

[run 42665854698](https://github.com/iiinno/OpenHinoVoice/actions/runs/42665854698)はFull・UI・UIA追加コストの全ジョブが成功。294テスト成功、失敗0、ignore 11。設定の2サイズ×7操作、720 HUD画像、動き有効の24描画測定が成立した。UIAのexe増分266.5KiB、Privateの中央値差276KiBは今回の設定画面比較条件で予算内。条件・範囲・生ログとスクリーンショットは[actions-460d466/README.md](actions-460d466/README.md)へ保存した。実機入出力・常駐性能・ASR反復とU13は引き続き未確認または未実装であり、計画全体の完了とは区別する。
