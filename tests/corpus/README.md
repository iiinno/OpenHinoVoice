# 評価用音声 corpus

性能と精度の測定に使う音声と正解文章です。利用者の声を含めないよう、
speech.cpp の TTS(irodori-tts)で合成した音声だけを置きます。

## 構成

- `script.txt` — 読み上げの元になる文。1 行が 1 文
- `<名前>.wav` / `<名前>.ref.txt` — 生成物。WAV は Git の管理外(`.gitignore` の `*.wav`)

| 名前 | 長さの目安 | 用途 |
|---|---|---|
| short | 15 秒以上 | 短い発話の認識と、準備時間の測定 |
| 1min | 1 分 | 基準の精度(CER)と遅延 |
| 5min | 5 分 | 長めの録音での文の落ち方 |
| 30min | 30 分 | FastConformer の文の丸ごと落ちの再現(P1 の効果測定) |

## 作り直し方

irodori-tts-mf:q4_k が未取得なら、先に `speech pull irodori-tts-mf:q4_k` を行う
(speech.cpp の公式モデル。約 0.86GB)。

```powershell
cargo run --release --example gen_corpus
```

オプション: `--speech PATH`(speech.exe の場所)、`--tts MODEL`(TTS のモデル名)、
`--out DIR`(出力先。初期値 `tests/corpus`)。

合成には GPU がある環境で 30 分音声について 20〜60 分ほどかかる。1 回作れば、
`tests/perf_live.rs` と P0 以降の測定はこの WAV を使い回す。
