# OpenHinoVoice

[English](README.en.md)

Windows で使える、日本語の音声入力アプリです。キーを押して話し、Enter を押すと、話した内容がいま開いているアプリに文字で入ります。

音声を文字にする処理は、すべてこの PC の中で行います。音声がインターネットに送られることはありません。文字にしたあと、AI で言い淀みを消して句読点を整えることもできます(使わなくてもかまいません)。

## できること

- キー 1 つで録音を始め、Enter で文字にして貼り付けます
- Enter を 1 回押すと、「えーと」「あのー」などを消して整えた文を貼ります。待たずにもう 1 回押すと、整える前の文をすぐに貼ります
- 録音中は画面の上のほうに、日の丸のような小さな表示が出ます。声に合わせて棒が動くので、ちゃんと録れているかが分かります
- 録音の開始や貼り付けのときに、短い効果音が鳴ります(切ることもできます)

## 導入のしかた

全部で 5 分ほどです。管理者の権限は要りません。

### 1. 音声認識のソフト(speech.cpp)を入れる

OpenHinoVoice は、音声を文字にするのに [speech.cpp](https://github.com/nyosegawa/speech.cpp) を使います。スタートメニューから「PowerShell」を開き、次の 1 行を貼り付けて Enter を押してください。

```powershell
irm https://raw.githubusercontent.com/nyosegawa/speech.cpp/main/install.ps1 | iex
```

### 2. OpenHinoVoice をダウンロードする

[最新版のページ](https://github.com/iiinno/OpenHinoVoice/releases/latest)から `openhinovoice.exe` をダウンロードします。置き場所はどこでもかまいませんが、迷ったら `C:\Users\(あなたの名前)\AppData\Local\Programs\OpenHinoVoice` のようなフォルダを作って入れておくと、あとで消してしまう心配がありません。

置いたら、ダブルクリックで起動します。初めて起動するときに「Windows によって PC が保護されました」と出た場合は、「詳細情報」を押してから「実行」を押してください。署名のない新しいアプリには、この表示が出ることがあります。

**公開している exe はコード署名されていません。** Windows のファイルの「プロパティ」に表示される製品名やバージョンは、ビルド時に付与する識別情報であり、発行元の身元を証明するデジタル署名ではありません。ダウンロード元を確かめてから実行してください。

リリースには `LICENSE`、`THIRD_PARTY_LICENSES.txt`、`SHA256SUMS.txt` も添付しています。ダウンロードした exe の内容を確認したい場合は、PowerShell で `Get-FileHash .\openhinovoice.exe -Algorithm SHA256` を実行し、表示されたハッシュ値を `SHA256SUMS.txt` の値と比較してください。ハッシュの一致だけでは発行元の身元までは保証されません。

起動すると、タスクバーの右下(時計の近く)にアイコンが出ます。見当たらないときは、時計の左の「^」を押すと出てきます。

### 3. 設定する

アイコンを右クリックして「設定」を選ぶと、設定の画面が開きます。

1. 「一般」で、録音を始めるキーを決めます。初めは F8 です。「Windows にサインインしたときに起動する」にチェックを入れておくと、次からは自動で立ち上がります
2. 文を AI で整えたいときは、「整形(AI)」で接続先を選びます。ChatGPT の Plus か Pro を契約していれば、「ChatGPT のプラン」を選んで「Continue with ChatGPT」を押すのがいちばん手軽です。ブラウザでサインインすれば、それで終わりです
3. 「接続テスト」を押して、例文がきれいに整えば準備は完了です
4. 最後に「保存」を押します

### 4. 試してみる

メモ帳を開いて、録音を始めるキーを押してから話してみてください。話し終わったら Enter を押します。最初の 1 回だけは、認識モデル(約 0.35GB)のダウンロードに 1 分ほどかかります。

## AI に導入を任せる

Claude Code や Codex などのコーディング AI を使っているなら、導入をそのまま任せられます。このリポジトリの [AGENTS.md](AGENTS.md) に、AI 向けの手順を書いてあります。次の文をそのまま AI に渡してください。

```text
https://github.com/iiinno/OpenHinoVoice の AGENTS.md の手順に従って、
OpenHinoVoice をこの Windows の PC に導入してください。
```

AI がやるのは、ソフトの導入と起動までです。API キーの入力や ChatGPT へのサインインは、AI に伝えずに、あなた自身が設定の画面で行ってください。

## 使い方

| 操作 | 動作 |
|---|---|
| 録音を始めるキー(初めは F8) | 録音を始めます |
| Enter を 1 回 | 録音を止めて文字にし、整えた文を貼ります |
| 続けてもう 1 回 Enter | 整えるのを待たずに、そのままの文を貼ります |
| Esc | 録音をやめます。何も貼りません |

貼り付けにはクリップボードを使いますが、貼ったあとは元の内容に戻します。貼るだけで、Enter は送りません。チャットで送信されてしまう心配はありません。

マウスのボタンで録音を始めたいときは、マウスの設定アプリ(Logi Options+ など)で、ボタンに Ctrl+Alt+J のようなキーの組み合わせを割り当てます。設定の画面の「録音開始キー」にも、同じ組み合わせを入れてください。

## 設定の画面でできること

| ページ | 内容 |
|---|---|
| 一般 | 録音を始めるキー、表示の色(日の丸か、Windows のライト、ダークに合わせるか)、自動起動 |
| 整形(AI) | 接続先、API キー、モデル、Fast モード、整形を待つ時間、整え方の指示、接続テスト |
| 認識 | 認識モデルの大きさ、聞き間違えやすい言葉のヒント |
| サウンド | 効果音のオンとオフ、音量、試し聞き |

保存した内容は、アプリを起動し直さなくても 1 秒ほどで反映されます。

## AI の接続先

| 接続先 | こんな人に |
|---|---|
| ChatGPT のプラン | ChatGPT の Plus か Pro を契約している人。追加の料金はかからず、プランの範囲で使えます |
| OpenCode Go | OpenCode Go を契約している人。安いモデル(初めは `deepseek-v4.1-flash`)を使います |
| OpenAI の API キー | OpenAI の API を使っている人。初めは `gpt-6-luna` を使います |
| その他 | OpenAI と同じ形式の API を持つサービスを使いたい人 |

AI に送るのは、文字にしたあとの文だけです。各サービスが送った文をどう扱うかは、それぞれの規約を確かめてください。ChatGPT のサインインの情報は、Windows の機能(DPAPI)で暗号化して、この PC に保存します。

OpenCode Go は、規約で「OpenCode などのコーディングエージェント向け」とされています。音声入力の整形はその想定から外れる使い方なので、使う場合はご自身の判断でお願いします。

## うまく動かないとき

| 困っていること | 確かめること |
|---|---|
| アイコンが見当たらない | 時計の左の「^」の中を見てください。それでもなければ、`openhinovoice.exe` をもう一度起動します |
| キーを押しても録音が始まらない | 同じキーをほかのアプリが使っているかもしれません。設定の画面で、Ctrl+Alt+J のような組み合わせに変えてみてください |
| 「speech.exe」が見つからないと出る | 導入のしかたの 1 をやり直すか、設定の画面の「認識」で speech.exe の場所を指定します |
| 「生テキストを貼りました」と出る | AI の整形が時間内に終わらなかったか、うまくいかなかったときの表示です。設定の画面の「接続テスト」で原因が分かります |

設定のファイルは `%APPDATA%\OpenHinoVoice\config.toml` にあります。

## ソースからビルドする

Rust のツールチェインがあれば、自分でビルドできます。

```
cargo build --release
```

できあがった exe は `target\release\openhinovoice.exe` にあります。

テストは `cargo test` で動きます。本物のマイクや API を使うテストは `#[ignore]` にしてあるので、名前を指定したときだけ動きます。

CI では `cargo fmt --all -- --check`、`cargo check --all-targets --locked`、`cargo clippy --all-targets --locked -- -D warnings`、`cargo test --all-targets --locked` を Windows 上で実行します。リリース用の `v0.1.0` のようなタグは `Cargo.toml` の `version` と一致する必要があります。Windows の exe の `FileVersion`・`ProductVersion` は同じ Cargo バージョンから生成され、`ProductName` は `OpenHinoVoice` です。既存リリースは上書きしません。

| テスト | 内容 |
|---|---|
| `tests/asr_integration.rs` | WAV ファイルを speech.cpp で文字にします |
| `tests/audio_integration.rs` | マイクから 2 秒録音します |
| `tests/format_live.rs` | 設定した接続先で、本物の AI に整形させます |
| `tests/overlay_preview.rs` | 録音中の表示を、全部の状態について順に出します |
| `tests/paste_integration.rs` | 選んでいる入力欄に文字を貼ります |

## スペシャルサンクス

OpenHinoVoice は、次のみなさんが公開しているものの上に成り立っています。ありがとうございます。

- [speech.cpp](https://github.com/nyosegawa/speech.cpp)(nyosegawa さん、MIT): 音声認識を PC の中だけで速く動かせるのは、このソフトのおかげです
- [Haas-ja-v1.0](https://huggingface.co/third-intelligence/Haas-ja-v1.0)(Third Intelligence、Apache-2.0): 速くて正確な、日本語の音声認識モデルです
- [Haas-ja-v1.0 の GGUF 版](https://huggingface.co/sakasegawa/Haas-ja-v1.0-GGUF)(sakasegawa さん、Apache-2.0): speech.cpp で動かすためのモデルファイルです
- [earshot](https://github.com/Leonxlnx/earshot)(Leon Lin さん、MIT): 録音開始や貼り付けの効果音を、この道具で作りました
- [egui](https://github.com/emilk/egui)(Emil Ernerfeldt さんほか、MIT または Apache-2.0): 設定の画面に使っています
- そのほか、組み込んでいるライブラリの一覧とライセンスは [THIRD_PARTY_LICENSES.txt](THIRD_PARTY_LICENSES.txt) にあります

## 権利と規約について

- OpenHinoVoice は個人のプロジェクトです。OpenAI、OpenCode、Third Intelligence をはじめ、名前を挙げた会社やプロジェクトとは関係がなく、公認も受けていません。ChatGPT は OpenAI の商標です
- 認識モデルと speech.cpp はこのリポジトリに含めていません。導入のときに、それぞれの公式の配布元から取得します
- 効果音(`assets/sounds`)は earshot がコードだけで作った音で、録音を元にした素材は使っていません
- ChatGPT でのサインインは、OpenAI が公開している「Sign in with ChatGPT」の、オープンソースのアプリ向けの手順に従っています

## ライセンス

[MIT](LICENSE)
