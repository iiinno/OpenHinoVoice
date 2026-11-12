# 性能の測り方

測定の入口は `scripts/verify.ps1 -Mode Live`。開発時の通常検証は [docs/verification.md](../../docs/verification.md) を参照する。過去の記録は当時のコードと手順の値であり、現行版の性能保証として扱わない。

```powershell
pwsh -NoProfile -File scripts/verify.ps1 -Mode Live -LiveTest Segmented `
  -SpeechExe 'C:\path\speech.exe' -ModelPath 'C:\path\Haas.gguf' `
  -Corpus short -CorpusDir tests/corpus
```

`Corpus` は `short`、`1min`、`5min`、`30min` から選ぶ。PowerShellから直接スクリプトを呼ぶ場合は `-Corpus short,1min` と複数指定できる。別worktreeに音声を複製する必要はなく、`CorpusDir` に既存コーパスの場所を指定できる。モデルも取得済みのローカルファイルを渡す。モデルやコーパスがなければ失敗し、欠けた条件を飛ばして成功扱いにしない。

結果は `target/qa/<実行ID>/result.json` と `perf/` のJSON・Markdownに残る。版、HEAD、dirty状態、入力とモデルのSHA-256、CPU情報、測定区間と除外範囲を記録する。長文の実テキストやAPIキーは測定結果へ書かない。

| 測定 | 含むもの | 含まないもの |
|---|---|---|
| 準備1回目 | workerの起動要求からreadyまで | 終了処理。OSキャッシュは消していない |
| 再起動準備 | 同じ実行内の次の起動からreadyまで | 終了処理。保持workerの再利用とは異なる |
| Workerの認識 | transcribe要求から終端応答まで | 事前のPCM送信 |
| Segmentedの認識 | 全音声を事前分割した後の逐次送信と認識 | 実時間の音声到着、仮認識、録音中の待ち行列 |
| 最後の区間 | 最後のtranscribe要求から終端応答まで | Enterから貼り付け完了までの全経路 |

文字誤り率（CER）は空白を除いた値と、さらに句読点を除いた値を併記する。未完了の認識があれば診断JSONを残してテストを失敗させる。CERの許容値はコーパスと用途に依存するため、診断の正常終了だけで精度合格とは扱わない。

実時間の認識経路は `-LiveTest Realtime` で測る。同じ明示モデルとcorpusを渡し、25msずつ音声を供給する。本番のSession、仮認識、確定区間の優先送信、Enter時の残り音声処理、AsrDoneの状態遷移を使う。独立したASR処理や事前の区間分割は使わない。

Realtimeは最初の非空partial結果、未認識PCM量（開いている区間と未完了の確定区間）、EnterからASR完了まで、全体時間、CERをJSONへ記録する。PCM量は音声の未処理量であり、プロセスの使用メモリではない。音声供給の最大遅延も記録する。起動待ちと音声供給は重ね、OSキャッシュは消去しない。partial結果の時刻に画面描画は含まれず、partialが出ない場合はnullになる。

プロセス資源は、測定対象のPIDを明示する。

```powershell
pwsh -NoProfile -File scripts/measure-process.ps1 -ProcessId 12345 `
  -DurationSeconds 60 -IntervalMs 1000
```

このスクリプトはプロセスを起動・終了しない。指定した一つのプロセスについて、CPU時間の差分、Private Bytes、Working Set、exeサイズをJSONへ保存する。途中終了は失敗で、PIDが再利用されても別プロセスへ測定を引き継がない。speech.exeを含む合計が必要な場合は、対象の子PIDも別々に記録して測定範囲を併記する。名前による `Stop-Process` は使わない。

変更前後は同じ端末、モデル、コーパス、電源設定、ビルド設定で交互に複数回測る。初回起動とモデル保持後、待機中・録音中・仮字幕・設定画面を混ぜない。中央値を基本にし、p95には十分な標本数を確保する。メモリの比較ではPrivate BytesとWorking Setを区別する。

録音から対象アプリの本文更新までの評価には、実時間の音声供給、Enter時点の未処理区間、字幕時刻、AI整形、貼り付け先の本文読み戻しが必要である。この診断試験だけで「長時間でも一定」「欠落なし」「実機の全経路合格」と結論しない。Plan.mdの予算達成と実機試験が未確認なら、未確認として残す。
