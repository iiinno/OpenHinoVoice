# 核心の貼付回帰（2026-11-10）

## 問題

利用者から、録音開始→Enter→自動貼付が全アプリで失敗し、手動貼付時にもクリップボードと競合する報告。導入版460d466の実機usage-logでは直近6件とも「貼り付け前にクリップボードが別のアプリで変更されました」でCtrl+V送信前に拒否していた。音声・認識本文・認証情報は調査出力に含めない。

## 検証の欠落

旧paste_integrationはignoreされ、実入力先の本文を確認していなかった。既存ReplayはAction::Pasteとcapture_targetを省略している。294テスト成功は録音から貼付までの成功を証明していなかった。

## 実装と検証の順序

1. 実Windows clipboardへの書込み後、閉じた時点のsequenceが返却所有sequenceと一致することを確認する失敗先行テスト。
2. CloseClipboard後のWindowsの形式合成を自己競合にしない修正。第三者の更新を自分の更新として引き受けない。
3. 通常Appで録音開始→fixture PCM→Enter→fixture ASR→本番PasteTransactionを通し、専用Win32 EDIT本文の完全一致、1回のみ入力、再試行、元clipboard復元、新しい第三者copyの非上書きを確認する。
4. Actionsの通常CIとrelease artifact生成に必須ゲートとして組込み。テスト0件・環境不足は合格にしない。ローカルビルドなし。

並列の担当はpaste実装と実clipboard回帰、App/PCM/入力先を結ぶE2E、親のCI/実機診断。共有ファイルを分ける。失敗先行のCIを確認してから修正をpushし、成功後のみ導入版を更新する。実マイク/実モデルの品質と個々のアプリ固有入力方式は、この合成音声・ASR境界の試験とは区別する。

## 再現した2つの欠陥

03632f1の[Actions42790499734](https://github.com/iiinno/OpenHinoVoice/actions/runs/42790499734)で、更新番号が書込み直後7、CloseClipboard後10になり、自己更新を他アプリ更新と誤判定することを再現。19c2e88で当該回帰は成功した。

24697daの[Actions42792327208](https://github.com/iiinno/OpenHinoVoice/actions/runs/42792327208)では、別プロセスのEDIT本文へ正確に一度だけ入った後、クリップボード復元完了時のCF_UNICODETEXTが消失して失敗した。OleGetClipboardの参照保持とOleSetClipboard後の解放だけでは元データの維持を証明できない。native形式を保持し、復元判定と書戻しを同じロック内で実施する修正対象とした。

途中のa70d82dはテスト用初期化の追加漏れでcompile失敗、42d13e7はテスト用キャレット位置の誤りで失敗した。これらを製品不具合の再現成功としては数えない。

## 修正内容

書込みを閉じてWindows側の形式合成が完了した後、clipboardを再度開き、所有者とUTF-16本文の一致を確認して更新番号を確定する。第三者の同文コピーを自己更新と認めない。

元clipboardはOLE参照ではなく、書換前に各形式の実データを複製して保持する。復元時の更新番号・所有者の確認と書戻しは1回のOpenClipboard内で行い、その間の競合を防ぐ。新しい第三者コピーは上書きしない。256形式・64MiBを上限に、未対応形式や複製失敗は元データを消す前に停止する。OLE配信者を指す内部形式は復元しない。

## 必須ゲート

通常CIとタグ公開用Releaseの双方で、実clipboard回帰と別プロセスへの入力E2Eを各1件必須とした。0件実行・失敗・実行環境不足なら配布へ進めない。E2EはF8とEnterを実際に送信し、通常のhook・App・貼付を通し、入力先PIDが別であることと本文の完全一致を確認する。2回連続の入力で、余計な改行・二重入力がないこと、元clipboardの復元、新しいコピーの保持を確認する。

## 検証の境界

PCM供給とASR応答のみfixtureを使う。実マイク、実ASRモデル、外部API、管理者権限の異なるアプリ、個々のアプリ固有の入力方式は未検証。Win32 EDITでの成功を全アプリでの実測成功とは扱わない。GDI各形式の複製と解放は静的レビュー済みだが、今回の実行回帰はUnicode・登録形式・空clipboardを中心とする。

復元待機の既存250msは今回変更しておらず、任意に遅いアプリのclipboard読取り完了まで保証する仕組みではない。OS書込APIが途中失敗した場合の完全rollbackも保証しない。今回の2つの再現済み欠陥を防ぐゲートを継続実行し、未検証領域を成功に含めない。

## 合格した実行

a66c410の[Actions 42796362130](https://github.com/iiinno/OpenHinoVoice/actions/runs/42796362130)で核心回帰2件を各1件ずつ実行して成功した。Fullは通常テスト294件成功・0件失敗、fmt、clippy、release buildが成功。通常suiteのignore14件には別ゲートで実行済みの核心2件と子fixture、実環境前提の試験を含む。ignoreを実行済みとは数えない。

[green-a66c410.log](core-paste-20261110/green-a66c410.log)に入力先PID、2サイクルの本文一致、clipboard復元・新規copy保護とFullの成功を保存した。artifactのbuild.jsonで同SHA、release/default、核心2項目trueを確認した。ローカルでRustをビルド・テストしていない。タグ公開ワークフローは通常CIと同じ検証コマンドを使用し、独立した静的レビューを実施した。タグ作成・公開そのものは未実行。
