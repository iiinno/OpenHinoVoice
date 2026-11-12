# Actions検証記録（460d466）

対象は460d4667f4101966cd1161d2d872354d45fb8131。実行は[GitHub Actions run 42665854698](https://github.com/iiinno/OpenHinoVoice/actions/runs/42665854698)。ローカルのビルド・アプリ起動は行っていない。ビルド済みexeはActions artifactに保存し、この記録には実行しない証拠ファイルだけを残す。

## Fullと画面操作

Fullはfmt、clippy、294テスト、release buildが成功。失敗0、ignore 11。ignoreには実マイク・モデル・外部API試験のほか、親テストから起動するsubprocess fixtureも含む。検証ソースのhashはverification/result.json、配布候補exeのhashはrelease/SHA256SUMS.txtに記録した。

設定画面はGitHub-hosted Windows、96 DPI、Mesa llvmpipe 26.2.6、非アクティブdesktopで実行した。600×420と720×520の両方でUIA 54要素、フォーカス可能30要素を観測。ソフト選択、保存済みへ戻す、プレビュー開始、停止、再開始、入力ページへ移動、音と表示へ戻る各操作が成功した。選択にはTogglePattern、通常ボタンにはInvokePatternを使い、期待状態の変化を確認している。操作中の設定ファイルは保存されず、両ケースとも正常終了した。

実GL framebuffer画像を目視した。選択中の「音と表示」は赤背景に白文字となり、600×420でも下部の保存・閉じるは表示される。本文はスクロール領域に収まる。音・マイク・ASR・貼付・外部APIはこの画面試験の対象外。実キーボードのTab/Esc/Ctrl+S、読み上げ、実モニターのDPI変更・高コントラストは未確認。

## HUD描画

Windowsのclient-area animationをこのrunner上で一時的に有効にし、終了後に元の無効状態へ復元した。共通描画経路で720画像を生成し、標準・リッチ×4状態×3回、各120フレームの24測定が成立した。p95は測定ごとに259〜1071µs。これはCPU/GDIの描画とRGBA変換だけで、ネイティブ合成・設定texture upload・入力遅延・ASRを含まない。高速化の比較結果や実機60fpsの保証としては使わない。

資源確認は動きを減らした状態で100切替を2周。2周目のPrivate増分は0 bytes、GDIとUSERは増加なし。初回のcache確保を含む全アプリの常駐増分ではなく、同じ遷移を温めた後の有界性を確認した結果である。

## 測定の読み方

UIA増分の比較は同一SHA・rustc・release・locked dependencies、同じMesaと設定内容、720×520で行う。feature-offとdefault(accessibility)をAB/BA/ABの順で各3回。各ケースはwarmup 2秒、UIA前15秒、UIA確認5秒、UIA後15秒。1秒間隔の観測で、Private予算は「UIA後のサンプル最大値の3回中央値」のcandidate−baselineを使う。

startupToResponsiveWindowMsは専用設定ファイルの準備時間も含み、終点は応答するHWND。純粋なアプリ起動時間や初回描画時間ではない。CPU値はUIA確認5秒間を含まない。背景負荷と電源状態を制御しておらず、今回の値から実機の速度改善や入力遅延改善を断定しない。

## UIA追加コストの結果

6ケースすべて測定成立し、予算評価も合格した。UIA後Privateの中央値は機能なし74,473,472 bytes（範囲73,072,640〜75,624,448）、あり74,756,096 bytes（74,059,776〜75,558,912）。中央値の差282,624 bytes（276KiB）は4MiB予算内。差は反復間のばらつきより小さく、一般的なメモリ増分を276KiBと確定するものではない。exe増分は272,896 bytes（266.5KiB）で1MiB予算内。今回の条件で予算に収まったことを採用判断の根拠とする。

起動前の2プロセスは、専用workerとC:\Windows\System32\conhost.exeであると実測で確認した。各ケース間にPID・生成時刻・実行ファイル・Job所属を照合し、終了時のJob残留は0件だった。以前の単純な「1プロセスのみ」という前提が誤っていたため、正体を確認する方式へ修正した。

## 残る受入範囲

実マイク・貼付、実キーボード、常駐とネイティブHUD全体のCPU/Private/入力遅延、実モニターのDPI/配色変更、モデルを使うASR反復測定は未実施。U12の診断入口は実装済みだが、U13のASR性能改善は実測がまだないため未実装。UIとv0.5全体の受入完了は、このActions合格だけでは宣言しない。
