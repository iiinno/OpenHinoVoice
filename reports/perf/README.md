# 性能の測り方

版ごとに同じ手順で測り、`reports/perf/` の下に結果を残す。数値が予算
(Plan.md 4.5 の表)を超えた版は出さない。

## 測るものと予算

| 項目 | 測り方 | 予算 |
|---|---|---|
| 準備時間(worker 起動から ready まで) | `tests/perf_live.rs` | 記録して比較する |
| Enter から認識結果まで | `tests/perf_live.rs` の「認識」 | P1 で録音の長さによらずほぼ一定にする |
| 文字誤り率(CER) | `tests/perf_live.rs` | 1 分の音声を基準にする |
| 常駐メモリ(待機中) | この文書の手順 A | 0.1.2 の基準値から増やさない |
| 待機中の CPU 時間 | この文書の手順 B | 記録して増やさない |
| exe のサイズ | この文書の手順 C | 記録して増やさない |

## 音声認識の測定(perf_live)

```powershell
cargo run --release --example gen_corpus   # 初回だけ。30 分音声の合成に時間がかかる
cargo test --release --test perf_live -- --ignored --nocapture
```

結果は `reports/perf/perf-live-<エポック秒>.md` に書かれる。CER は厳密
(空白を除く)と緩和(句読点も除く)の 2 種類を出す。緩和の値は、Haas が
句読点を出力するかどうかを確かめるためのものである。

## 手順 A: 常駐プロセスのメモリ

利用者の config.toml を触らないよう、`APPDATA` を一時フォルダに差し替えて
測定用の exe を起動する。利用者の openhinovoice.exe が動いているときは、
単一インスタンスの仕組みで 2 つ目がすぐ終わるため、測定の前に終了してもらう。

```powershell
# 1. 利用者のインスタンスが動いていれば終了する(測定後にもう一度起動して戻す)
Get-Process openhinovoice -ErrorAction SilentlyContinue

# 2. 測定用の起動。設定は一時フォルダに作られ、本物の config.toml は変わらない
cargo build --release
$measure = "$env:TEMP\ohv-perf-measure"
New-Item -ItemType Directory -Force $measure | Out-Null
$realAppData = $env:APPDATA
$env:APPDATA = $measure
Start-Process "$PWD\target\release\openhinovoice.exe"
Start-Sleep -Seconds 20   # トレイ常駐が落ち着くまで待つ

# 3. メモリの 3 回測定(Private Bytes と Working Set)
1..3 | ForEach-Object {
    $p = Get-Process openhinovoice
    "{0:N0} KB private, {1:N0} KB workingset" -f ($p.PrivateMemorySize64/1KB), ($p.WorkingSet64/1KB)
    Start-Sleep -Seconds 5
}

# 4. 後片付け。終了メニューはトレイから行うか、プロセスを止める
Stop-Process -Name openhinovoice -ErrorAction SilentlyContinue
$env:APPDATA = $realAppData
Remove-Item -Recurse -Force $measure
```

録音中の増加(speech.exe を含む)はマイクの実機確認が必要なので、手順としては
`Get-Process openhinovoice, speech` を録音中に実行して記録する。数字は
reports/qa/manual-*.md の確認項目に載せる。

## 手順 B: 待機中 1 分あたりの CPU 時間

手順 A の 2 までを実行した状態で、何も操作せず 60 秒置いたときの
TotalProcessorTime の増分を測る。

```powershell
$p = Get-Process openhinovoice
$t0 = $p.TotalProcessorTime
Start-Sleep -Seconds 60
$p.Refresh()
$t1 = $p.TotalProcessorTime
"CPU 時間: {0:N2} 秒/分" -f ($t1 - $t0).TotalSeconds
```

## 手順 C: exe のサイズ

```powershell
"{0:N1} MB" -f ((Get-Item "$PWD\target\release\openhinovoice.exe").Length/1MB)
```

## 結果の残し方

- 版の基準値は `reports/perf/baseline-<版>.md` に、測定日時・手順・数値をそえて書く
- 版ごとの継続測定は `reports/perf/` に `perf-live-<エポック秒>.md` として残る
- 比較は必ず同じ corpus、同じモデル、同じ手順で行う
