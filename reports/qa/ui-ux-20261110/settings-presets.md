# U06 / U06a 音と表示の設定・プレビュー

2026-11-10。担当作業ツリー: ui-settings-20261110。

## 実装

- 9ページ構成を保ち、「表示」を「音と表示」に変更。入力ページの旧音 controls を移動し重複を解消。
- 標準、ソフト、ミニマル、サイレント、リッチを選択状態付きの標準ボタンで選択。音セットと表示スタイル、Windowsに合わせる/動きを減らす、音量、7場面の音を独立に調整できる。プリセット選択・hoverでは試聴を開始しない。起動音は既定offを維持。
- 保存は既存TOML編集経路に FeedbackSnapshot.edits を追加。「音と表示を保存済みに戻す」はこのスナップショットの範囲のみを戻し、テーマ・字幕・他ページの編集中項目を保持。
- Ctrl+S、エラーのあるページへの導線、対象編集欄のfocus/scroll、extra_bodyの折りたたみ展開を追加。失敗時にdraftを破棄しない。
- 設定ウィンドウとナビゲーションに assets/branding/settings.png を接続。ナビゲーションの画像textureは再利用。
- eframe公式accesskit featureを有効化。名前・選択状態は標準Button/Checkbox/Radio/Slider/ComboBoxで表現し、独立ラベルのComboBoxはlabelled_byで関連付け。IPC用Windows DataExchange featureも追加。

## プレビュー境界

src/settings/preview.rs の純粋な Timeline とGUI wrapperを分離。Sequenceは3900ms、単体eventは900msで終了する。合成時計・合成level・合成文を使用し、録音/ASR/貼り付け/履歴/設定保存は呼ばない。失われたframeの音をまとめて後追い再生しない。

描画は overlay::preview_frame / motion_enabled を使用。RGBAはunmultipliedとして読み、1枚のtextureを再利用。動きが有効な再生中は最大60fps相当、Reduced/Windows動き無効時はphase境界の一回通知のみ。停止中はpreview自身が再描画を予約しない。scrollで見切れたプレビューは描画を省き、finite timelineの境界だけ進める。

音は preview_ipc::new_token / request_play / request_stop のみ。IPCの失敗は当該timelineの後続音を抑止し「表示だけを試しています」の理由を保持。明示的な再試行で新tokenを作る。録音優先で本体側に停止されたtokenは再使用しない。local sound::play 呼び出しは削除済み。

Esc、別ページ、閉じる操作、再Play、設定選択の変更で停止。最小化/occludedではeframe 0.37のApp::uiが呼ばれないためApp::logicでも停止する。Dropもtokenを停止する。

## 検証用入口

OHV_TEST_NAMESPACEが有効な場合のみ、OHV_TEST_SETTINGS_PAGEに start/home/input/display/asr/ai/edit/history/about の既知ページを指定可能。OHV_TEST_SETTINGS_SIZEは600x420または720x520のみを受理。通常起動のページ・サイズは従来どおり。音・録音・モデル取得を自動開始する入口は追加していない。

## 検証状況

- 実施済み: 所有Rustファイルのrustfmt、git diff --check、ローカルegui/eframe 0.37実ソースのAPI確認。
- 実施済み: 非表示 cargo update -p eframe --precise 0.37.0、および cargo fetch --locked --target x86_64-pc-windows-msvc。既存lockのpackage@version削除/変更は0件、AccessKit関連の追加90件。Windows対象で全追加packageがbuildされるという意味ではない。
- 追加済み/未実行: Timelineの5試験（状態順、cue一回、mute/volume0/個別off、停止、遅延frame、全event終了）と設定の3試験（Ctrl+S、validation復帰とdraft保持、テストページwhitelist）。既存共通libraryのunit testsとして追加。
- 未実施: Cargo build/test、Fast/Full検証、実Windows UIA/keyboard操作、600x420実画面、実音試聴、CPU/メモリ/起動時間の計測。親統合後に実施する。
- 統合依存: overlay::preview_frame / motion_enabled / Tone::Success,Info、preview_ipc moduleの本番実装。stubは残していない。

## 担当差分

src/settings.rs、src/settings/preview.rs、Cargo.toml、Cargo.lock、本ファイル。settings.rsには親から受け取ったU01変更を保持。その他の基盤変更・assetsは親からのコピーであり、この担当では編集していない。
