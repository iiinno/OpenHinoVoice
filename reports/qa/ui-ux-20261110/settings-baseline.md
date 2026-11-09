# U01 設定ホームと入力配置

対象は基準コミット `205efa0` からの設定画面変更。実装日は2026-11-10。

## 変更

ホームに認識エンジンとモデルの準備状態を追加した。speech.cppがない場合、モデル未確認・未取得・確認/取得中・失敗・中断の場合を表示し、既存の「セットアップを開く」操作から準備を続けられる。取得済みのモデルはオフラインでも取得済みとして表示する。モデル取得済みという表示は、マイクや入力先まで動作確認済みという意味にはしない。

既存の `Settings::model_status` と `setup::ModelStatus` を入力に、保存しない `PreparationNotice` を導出する。画面ごとに独立した状態enumを増やす案は採らなかった。ホームの描画は新しいモデル確認・ダウンロード・マイク取得を起動しない。従来のセットアップと認識ページのジョブ管理をそのまま使う。

入力ページの「最大録音時間」と「認識エンジンを残す時間」の後に、2列Gridの行区切りを追加した。

## 検証

- `rustfmt --edition 2021 --config skip_children=true src/settings.rs` は成功した。
- 状態の意味を検証する単体テストを3件追加した。未確認/未取得を使用可能扱いしないこと、エンジン欠損が取得済み状態より優先されること、取得済みモデルはオフラインでも利用できること、取得進捗・失敗理由・取消が残ることを確認する内容。
- Cargoテスト・ビルドは親の統合検証へ引き渡す。この担当では未実行。
- 実画面の600×420配置、文字の折返し、ボタン操作は未確認。実画面の証拠はこの報告に含めない。
- `Plan.md` は追跡外の旧ローカル資料のため、このworktreeでは更新しない。親が該当記述の注記を更新する。

テスト名は `home_preparation_never_reports_ready_without_an_engine_or_verified_model`、`home_preparation_explains_offline_recovery_without_claiming_a_download`、`home_preparation_preserves_progress_errors_and_cancellation`。統合後は通常のFast検証で実行できる。

親で統合後、Fast -Filter home_preparationは3成功・0失敗。fmtも成功。結果は[u01/result.json](u01/result.json)。実画面の確認は後続の統合段階で行う。
