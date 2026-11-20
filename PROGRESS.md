# PROGRESS — 進捗と再開メモ

新しいセッションはこのファイルと DECISIONS.md を読めば再開できる。

## 現在地
- ブランチ：`claude/relaxed-allen-gw2kq3`
- M0・M1 完了。M2 以降（OS 層）を実装中。

## 完了
- M0：ワークスペース骨格（`crates/core` `crates/platform-mac` `crates/platform-win` `crates/app`）、
  CI（`.github/workflows/ci.yml`：ubuntu で core テスト、macos/windows で fmt・clippy・test・release ビルド・成果物）。
- M1：core（Linux でユニットテスト 81 件が通る）
  - `rules` 会議アプリ定数表と照合、`detect` 検出状態機械、`controller` 全体制御（`Platform` をモックしてテスト）
  - `mp4` fragmented MP4 muxer と修復（ffmpeg で生成した実 H.264/AAC を mux し ffprobe/ffmpeg で再生確認済み）
  - `audio` ミキサ（リサンプル・ドリフト吸収・デバイス切替）、`session` 録画セッション
  - `storage` 命名・マーカー・修復・自動削除、`config`、`log`、`metrics`、`text`

## 次にやること
- M2〜M4：`platform-mac` / `platform-win` の実装（`core::platform::Platform` を実装し `run(app)` を提供）
- M5〜M7：異常系の仕上げ、計測、README、docs/manual-test.md

## 未検証事項
（OS 層の実装後に列挙する）

## 開発コマンド
```sh
cargo test --workspace                                   # Linux で core のテスト
cargo clippy --workspace --all-targets -- -D warnings
cargo check -p meetrec --target aarch64-apple-darwin     # macOS 向け型検査（Linux 上で可）
cargo check -p meetrec --target x86_64-pc-windows-msvc   # Windows 向け型検査（Linux 上で可）
```
