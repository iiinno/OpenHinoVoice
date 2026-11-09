# 音セットの制作・確認

4セット・7イベントを assets/sounds/manifest.json に列挙した。Classicの4音は保持し、新規10音をローカルのearshotで合成した。録音・アップロード・スピーカー再生は行っていない。

| セット | Start | Stop | Done |
|---|---:|---:|---:|
| Classic | 163.424ms | 既存音 | 既存音 |
| Soft | 160ms | 260ms | 240ms |
| Minimal | 80ms | 180ms | 190ms |
| Rich | 160ms | 280ms | 450ms |

Warningは320msの共通音、Errorは既存の共通音。Startupは各セットのDone、Cancelは各セットのStop（ClassicのみSoft Stop）を再利用する。Startup/Cancelは既定でオフ、MinimalのStopもプリセットではオフ。Silentは全音を無効化し、専用の音源を増やさない。

音の重なりは制作時にPCMへ合成済み。実行時のDSPや追加の音響ライブラリはない。全14WAVは276,644 bytes、新規分205,064 bytes。追加埋込アイコン51,215 bytesと合わせ256,279 bytesで、提案予算512KiB以内。単一再生PCMの最大は39,734 bytesで、256KiB上限内。埋込原音とOS内の音声資源はこの再生バッファ値に含めない。

最終PCM10音をearshot.measure(light)で計測した。44.1kHz/mono/PCM16へ変換後のtrue peakは−18.5〜−15.4dBTP、clipped samplesは全て0。短音のためLUFSは不成立のものがあり、RMS（−33.5〜−29.0dBFS）も併記した。知覚音量が完全一致したという評価ではない。機械計測値は audio-measurements.json、hashと制作recipeはmanifestに保存した。

初稿の短いStop/Warningでtail cutが検出され、減衰時間を延ばした。Rich Startは最後60msを制作時にfadeして、開始音長を増やさず末尾を滑らかにした。最終Rich Startはearshot.describeでsuccess chime(z3.04)/notification ding(z2.93)、最終meterでpeak−17dBTP、attack24ms、decay99ms、clip0。合成UI音であり、実楽器や高級音源という評価はしていない。Soft Start初稿とRich Start木質初稿はerror buzzの候補が強く出たため不採用とした。Rich Done初稿はsuccess chime(z3.5)、Minimal Doneは短いswish/clickに近い判定だった。モデルの分類は参考情報で、利用者による聞き分け・好みの評価は未実施。

制作: earshot bank forge_v2r6+forge04r1+fit1+pluck1。FFmpegでmetadata除去、mono/44.1kHz/PCM16へ変換。Rich Startのみ afade=t=out:st=0.10:d=0.06。recipeと変換argvをmanifestに記録した。
