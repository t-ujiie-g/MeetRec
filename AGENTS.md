# AGENTS.md

このリポジトリで作業する人と AI エージェント（Claude Code・Codex・Antigravity ほか）向けの手引き。
`CLAUDE.md` は `@AGENTS.md` の1行だけにして、中身はここに書く。

## これは何か

Zoom / Microsoft Teams のデスクトップアプリの会議を自動で検出し、会議ウィンドウだけをローカルに録画する
常駐アプリ（macOS 15 以降 / Windows 10 22H2 以降）。Rust 製で、GUI フレームワークも WebView も使わず、
ネットワーク通信もしない。

使い方・設定は [README.md](README.md)、設計判断は [DECISIONS.md](DECISIONS.md)、
進捗と実機未検証の仮定は [PROGRESS.md](PROGRESS.md)、実機での確認手順は [docs/manual-test.md](docs/manual-test.md)。

## コマンド

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings                    # 今いる OS 向け
cargo test --workspace                                                   # core のテスト（どの OS でも実行可）

# もう一方の OS 向けは型検査と lint まで（リンクと実行は CI か実機）
cargo clippy -p meetrec --target x86_64-pc-windows-msvc -- -D warnings
cargo clippy -p meetrec --target aarch64-apple-darwin -- -D warnings

cargo build --release -p meetrec                                         # 各 OS 上でのビルド
packaging/macos/bundle.sh target/release/meetrec dist                    # macOS: .app 化と ad-hoc 署名
```

- Rust の版は `rust-toolchain.toml` で固定している（CI もこのファイルを読む）。上げるときは `Cargo.toml` の
  `rust-version` も揃え、CI の 3 ジョブ（core / macos / windows）が緑になることを確かめる。
- 環境変数は使わない（保存先などは `HOME` / `APPDATA` / `USERPROFILE` から決める）。

## 構成

```
crates/core            OS 非依存のロジック（Linux でもテストできる）
  rules.rs / detect.rs   会議アプリの規則（型と照合）と、検出の状態機械
  rules/builtin.rs       組み込みの定数表（Zoom / Teams のプロセス名・ウィンドウ・ミュートの見分け方）
  controller.rs          全体の制御（確認ポップアップ・録画の開始と停止・ディスク監視・メニュー）
  controller/            録画中の追従（recording.rs）、診断ログ（diagnostics.rs）、テスト（tests.rs）
  session.rs             録画セッション（ミキサ → AAC → fragmented MP4）
  audio.rs / mp4.rs / h264.rs   音声ミキサ、fMP4 の muxer と修復、H.264 の変換
  output.rs              出力機器の消音を外して音量を 0 にする判断（Windows の「PC から出る音すべて」の録画。D37）
  storage.rs / config.rs / log.rs / metrics.rs / text.rs   保存、設定、ログ、自己計測、UI 文言
  platform.rs            OS 層との境界（`Platform` / `App` trait と受け渡す型）
crates/platform-mac    macOS 層（ScreenCaptureKit / VideoToolbox / AudioToolbox / CoreAudio / AppKit / アクセシビリティ API）
crates/platform-win    Windows 層（Windows.Graphics.Capture / Media Foundation / WASAPI / Win32 / UI オートメーション）
crates/app             エントリポイント（`main.rs`）と Windows のマニフェスト埋め込み（`build.rs`）
packaging/macos        .app の組み立てと署名
packaging/windows      インストーラ（Inno Setup の `MeetRec.iss`。CI の Windows ジョブが `MeetRec-setup.exe` を作る）
docs                   実機確認の手順、署名・公証の手順、Windows の Teams の音の調査の手引き
tools/lbtest           Windows の音の録り方を確かめる検証用ツール（ワークスペースの外。本体・CI には入らない）
```

## 守ること

- **core は `Platform` trait 経由でしか OS に触れない。** OS の API は `platform-mac` / `platform-win` に閉じ込める。
  判断を伴うロジックは core に置き、モックでテストできる形を保つ。
- **ネットワーク通信をしない**（自動更新・テレメトリ・クラウド連携なし）。
- **依存クレートは最小限**（MIT / Apache-2.0 系のみ。D9）。GUI フレームワーク・WebView・ffmpeg などの同梱は不可（D8）。
  足す前に利用者に確認する。
- **録画を失わない**ことを最優先にする（D1・D10・D17）。書き込みは fragmented MP4 の追記と fsync、
  異常終了は進行中マーカーと起動時の修復で救う。この流れを変えるときはテスト（`mp4`・`session`・`storage`）を先に直す。
- **外から来る値を信用しない。** 設定ファイル、ウィンドウタイトル（ファイル名にするときは `storage::sanitize_component` を通す）、
  進行中マーカーの中身、修復対象の MP4、他プロセスからも送れるウィンドウメッセージ（D25〜D27）。
- **ログ・診断ログに、会議アプリ以外のウィンドウタイトルを出さない**（D13）。
- **実機で分かった会議アプリの挙動は、テストにして残す。** ウィンドウの名前・階層・退出後に残るウィンドウ・ミュートの部品の
  名前などは `rules.rs` / `controller/tests.rs` のテストに「実機で確認」と書いて固定する（D30〜D36）。録画が止まらない・
  別の画面を録る、といった不具合はここから起きる。
- **会議アプリごとの違いは `rules/builtin.rs` の表に書く。** OS 層や controller に「Teams なら」のような分岐を書かない
  （例：共有中の目印 `sharing`、ミュートの見分け方 `mute`、PC から出る音すべてを録る `system_audio`）。
- **分からないときは録る側に倒す。** ミュート状態を読めない、共有しているディスプレイを決められない、といった場合に
  自分の発言や映像を捨てない。ただし、共有していない画面を録るおそれがあるときは切り替えない（D31・D35）。
- **実機で確かめていない仮定を断定でコードに埋め込まない。** `core::rules` の定数表（`config.toml` で上書きできる）に置き、
  PROGRESS.md の「未検証事項」に載せる。確かめられたら一覧から消す。
- `Platform` のメソッドと `App::on_event` はメインスレッドで呼ぶ。OS 層のイベントはキューで直列化し、再入させない（D24）。
- `unsafe` には、なぜ安全かを `// SAFETY:` で1行添える。
- **コメントは、コードから読み取れない「理由」だけを 1〜2 行で書く。** 何をしているかの言い換え、関数名やテスト名の繰り返し、
  経緯の説明は書かない。判断の経緯や引き換えは DECISIONS.md に書き、コードからは番号（`D39` など）で指す。
  変更のたびにコメントを足すのではなく、古くなったコメントを直す・消す。
- 設計判断を変えた・足したときは DECISIONS.md の末尾に追記する。

## ブランチとコミット

- `main` に直接コミット・push しない。`feature/<名前>_<内容>`（例: `feature/ujiie_update`。`fix/` `refactor/` `docs/` `chore/` でもよい）
  で枝を切り、PR にして取り込む。
- コミットメッセージは日本語で、先頭に範囲を付ける（`core:` `platform-mac:` `platform-win:` `ci:` `docs:` `chore:` `refactor:` `security:`）。
- push したら CI の 3 ジョブ（core / macos-latest / windows-latest）の結果を確かめる。

## リファクタリング時の補足

共通の観点は社内スキル `venect-refactor`（[venect/Skills](https://github.com/venect/Skills)）にある。
ここにはこのリポジトリ固有のことだけを書く。

- 定数の置き場所:
  - 両 OS で揃える値は core に置く。時間の単位（`NS_PER_SEC`）は `platform.rs`、音声の形式（`OUT_RATE`・`FRAME`・`FRAME_LEN`・`DEFAULT_ASC`）は
    `audio.rs`、エンコードの制限（`SOFTWARE_MAX_LONG_SIDE`・`VIDEO_TIMESCALE`）は `session.rs` / `mp4.rs`、UI 文言は `text.rs`、
    会議アプリの定数表（Teams のタブ名 `TEAMS_TABS` を含む）は `rules/builtin.rs`、目標値（メモリ・CPU）は `metrics.rs`、
    出力機器の音量の閾値（`ZERO_EPS`・`JUMP`）は `output.rs`、
    OS 層の動きを揃える間隔（`CONTROL_SEARCH_RETRY_NS`）は `platform.rs`。
  - 設定の既定値と範囲は `config.rs` が正。README の設定例はこれに揃える。
  - 1 ファイルの中でしか使わない閾値・待ち時間は、そのファイルの先頭の `const` にする。
- 意図的な重複:
  - `platform-mac` と `platform-win` は同じ役割のファイル（`app` / `tray` / `prompt` / `probe` / `capture` / `aac` / `sys`）を別々に持つ。
    OS の API が違うので共通化しない（共通にできるのは core に置ける「判断」だけ）。
    片方にしか無いもの：mac の `toast`（OS の通知が使えないときの代わり。D29）と `panel`（`prompt` と `toast` の共通部分）。
  - 会議アプリの部品を読む `ax`（mac）と `uia`（win）は、どちらも `ControlWatch`（見つけた部品を覚えて読み直す）を持つ。
    流れは同じだが API がまったく違うので、別々に持つ。名前の比べ方（`rules::starts_with_ci`）と探し直しの間隔は core で揃える。
  - トレイのメニュー ID（mac の `TAG_*`、win の `ID_*`）は各 OS の中で閉じた値で、揃える必要はない。
  - `windows` と `windows-core` は同じ系列の版に揃える（`#[implement]` マクロのため）。`windows-core` だけを上げない。
  - ソフトウェアエンコードに落ちたときの fps の下げ方は OS で違う（mac は半分で下限 5、win は上限 5）。
    実機で確かめるまで揃えない（D23）。
- 点検後に通すコマンド: 上の「コマンド」の `cargo fmt` / `cargo clippy`（今いる OS ともう一方の OS）/ `cargo test`。
  packaging を触ったら `bundle.sh` も実行する。Windows のインストーラは手元の Mac では作れないので、CI の成果物で確かめる。
- 手で確かめる手順: OS 層（`platform-*`）には自動テストが無い。変えた分岐に当たる [docs/manual-test.md](docs/manual-test.md) の項目を示し、
  今いる OS のぶんは実際に動かす。もう一方の OS は型検査までしかできないので、その旨を PR に書き、PROGRESS.md の「未検証事項」に足す。
- 依存の脆弱性: `cargo audit`（無ければ `Cargo.lock` の全クレートを OSV に照会）と `cargo update --dry-run --verbose`。
- バイナリサイズ: CI が上限（Windows 20MB、macOS universal 40MB）を検査している。依存を足すときは気にする。
