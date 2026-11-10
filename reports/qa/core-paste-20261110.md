# 核心の貼付回帰（2026-11-10）

## 問題

利用者から、録音開始→Enter→自動貼付が全アプリで失敗し、手動貼付時にもクリップボードと競合する報告。導入版460d466の実機usage-logでは直近6件とも「貼り付け前にクリップボードが別のアプリで変更されました」でCtrl+V送信前に拒否していた。音声・認識本文・認証情報は調査出力に含めない。

## 検証の欠落

旧paste_integrationはignoreされ、実入力先の本文を確認していなかった。既存ReplayはAction::Pasteとcapture_targetを省略している。294テスト成功は録音から貼付までの成功を証明していなかった。

## 実行計画

1. 実Windows clipboardへの書込み後、閉じた時点のsequenceが返却所有sequenceと一致することを確認する失敗先行テスト。
2. CloseClipboard後のWindowsの形式合成を自己競合にしない修正。第三者の更新を自分の更新として引き受けない。
3. 通常Appで録音開始→fixture PCM→Enter→fixture ASR→本番PasteTransactionを通し、専用Win32 EDIT本文の完全一致、1回のみ入力、再試行、元clipboard復元、新しい第三者copyの非上書きを確認する。
4. Actionsの通常CIとrelease artifact生成に必須ゲートとして組込み。テスト0件・環境不足は合格にしない。ローカルビルドなし。

並列の担当はpaste実装と実clipboard回帰、App/PCM/入力先を結ぶE2E、親のCI/実機診断。共有ファイルを分ける。失敗先行のCIを確認してから修正をpushし、成功後のみ導入版を更新する。実マイク/実モデルの品質と個々のアプリ固有入力方式は、この合成音声・ASR境界の試験とは区別する。

## 再現した2つの欠陥

03632f1の[Actions42790499734](https://github.com/iiinno/OpenHinoVoice/actions/runs/42790499734)で、更新番号が書込み直後7、CloseClipboard後10になり、自己更新を他アプリ更新と誤判定することを再現。19c2e88で当該回帰は成功した。

24697daの[Actions42792327208](https://github.com/iiinno/OpenHinoVoice/actions/runs/42792327208)では、別プロセスのEDIT本文へ正確に一度だけ入った後、クリップボード復元完了時のCF_UNICODETEXTが消失して失敗した。OleGetClipboardの参照保持とOleSetClipboard後の解放だけでは元データの維持を証明できない。native形式を保持し、復元判定と書戻しを同じロック内で実施する修正対象とした。

途中のa70d82dはテスト用初期化の追加漏れでcompile失敗、42d13e7はテスト用キャレット位置の誤りで失敗した。これらを製品不具合の再現成功としては数えない。
