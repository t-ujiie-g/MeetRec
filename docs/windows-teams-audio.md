# Windows の Teams の相手の音：調査の記録と手引き

Windows の実機で調べる人（と AI エージェント）向け。設計判断は [DECISIONS.md](../DECISIONS.md) の D36・D37、
残っている確認は [PROGRESS.md](../PROGRESS.md) の「未検証事項」。

## 今の仕様（2026-11-23）

- Windows の Teams は、相手の音を「PC から出る音すべて」（Teams が音を出している出力機器のループバック）として録る（D36）。
  Teams の音だけを録る方式（プロセスループバック）は、無音しか届かないので使えない。Zoom はプロセスループバックで録る。
- 出力機器が消音だと Teams 自身が音を出さなくなるので、録画中だけ、MeetRec が消音を外して音量を 0 にする。
  会議に入るときに上げられる音量は 0 に戻し、録画が終わったら消音と元の音量に戻す（D37。判断は `crates/core/src/output.rs`、
  出力機器の読み書きは `crates/platform-win/src/audio.rs` の `OutputVolume`）。

理想は、Zoom と同じく **Teams の音だけ** を、PC の音量や消音に関係なく録れること。その方法は見つかっていない。

## 分かったこと

測定は Windows 11 26200、スピーカー (Conexant SmartAudio HD)、Teams 26288.2004。道具は下の `lbtest`。

### 1. プロセスループバックは、Communications と GameChat のストリームを渡さない（合成音）

Teams を使わず、440Hz の音を出し、出す側のストリームのカテゴリ（`AudioClientProperties.eCategory`）だけを変えて、
各方式で届く音量（RMS）を測った。出した音の RMS は 0.0071。

| 出す側のカテゴリ | プロセスループバック（対象を含める） | 同（無関係のプロセスを除外） | 出力機器のループバック |
|---|---|---|---|
| Other / Alerts / SoundEffects / GameEffects / GameMedia / Speech / Movie / Media | 0.0070（録れる） | 0.0070（録れる。Other で確認） | 録れる |
| **Communications** | **0.000015（無音）** | **0.000015（無音）** | 録れる |
| **GameChat** | **0.000015（無音）** | （未測定） | 録れる |

- RAW（`AUDCLNT_STREAMOPTIONS_RAW`）を付けても、Communications は無音のまま。
- 無音のときも、エラーにはならず、データは届く（`AUDCLNT_BUFFERFLAGS_SILENT` も付かない）。
- 出力機器のループバックは、この機器では **出力機器を消音にしても、音量を 0% にしても録れた**（RMS は変わらない）。
  この機器は音量と消音を機器の側で持つ（`IAudioEndpointVolume::QueryHardwareSupport` が `0x3`）。
- 音を出しているアプリのセッションだけを消音にする（音量ミキサーでそのアプリを消すのに相当）と、
  出力機器のループバックもプロセスループバックも無音になる。

### 2. 実機の Teams も、プロセスループバックでは無音。消音にすると、Teams が音を出すのをやめる

通話中、相手が音を出し続けている状態で測った。音を出しているのは、ルート（`ms-teams.exe`）の子の `ms-teams.exe`
（内蔵スピーカーに動作中のセッションが 2 つ。`msedgewebview2.exe` のセッションは動作していない）。

| 出力機器の状態 | Teams のセッションのピーク | 出力機器のループバック | プロセスループバック |
|---|---|---|---|
| 消音なし・音量 2% | 0.49〜0.56 | 録れる（RMS 0.069） | 無音（ルート・子・無関係のプロセスを除外、どれも 0.000015） |
| **消音** | **0.0000** | **無音**（全パケットが `AUDCLNT_BUFFERFLAGS_SILENT`） | （未測定） |
| **消音なし・音量 0%** | 0.43〜0.52 | **録れる（RMS 0.077）** | （未測定） |

- プロセスループバックの値は、合成音の Communications と同じ。対象の選び方では変わらない。
- 出力機器を消音にすると、Teams のセッションのピークが 0 になる。合成音は消音中もループバックに届くので、
  無音になるのは Windows や機器のせいではなく、Teams の動き。Teams の「オーディオの設定」のスピーカーの絵も、消音の表示になる。
- 音量を 0%（消音ではない）にすれば、Teams は音を出し続け、スピーカーからは音が出ない。

### 3. 音量 0 のまま会議に入ると、入った瞬間に音量が 20% に上げられる

`lbtest watch` で出力機器の消音と音量の変化を記録しながら、消音にして会議に入った。

- MeetRec が消音を外して音量を 0 にしたあと（参加前の画面）、会議に入った瞬間に音量が 20% になった。
  MeetRec の操作ではなく、参加の時刻と一致する（Teams の動きとみられる。断定できる証拠は無い）。
- 0 に戻したあとは、会議を出るまで上げ直されなかった（22 秒間の記録）。
- 音量の変化の通知（`IAudioEndpointVolumeCallback`）で気づいて戻すと、0.05 秒おきの監視には上がった状態が写らなかった。

### 推測（確かめる手段が無い）

- Teams は相手の音を Communications カテゴリで出していて、Windows のプロセスループバックは Communications と GameChat の
  ストリームを意図的に混ぜない（ゲームの録画に通話の声を入れないための作りと思われる）。
  `AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS` には PID とモードしか無く、アプリの側からこれを変える手段は見当たらない。
  同じ結果の報告：[Windows-classic-samples #414](https://github.com/microsoft/Windows-classic-samples/issues/414)
  （Microsoft のサンプルでも Teams は無音。カテゴリを変えた実験も同じ結果。Microsoft の回答は無い）。
- 音量と消音を機器の側で持たない出力機器（`hardware_support` が `0x0`。Bluetooth や USB のイヤホンに多い）では、
  Windows がソフトウェアで音量を掛けてからループバックに渡すので、**音量 0% でも無音になる** おそれがある。

## 確かめたいこと

結果は、PROGRESS.md の「実機で確認できたこと」に書き、「未検証事項」から消す。

1. **内蔵スピーカー以外の出力機器で、音量 0% のときに録れるか**（通話中に、相手に音を出してもらって）。
   `lbtest sessions`（機器ごとの `hardware_support` と、Teams のセッションのピーク）と `lbtest endpoint 3000` を、
   音量 0%・消音のそれぞれで記録する。録れない機器があれば、その機器では消音を外さない、などの扱いを決める。
2. **会議の途中で、音量がもう一度上げられることがあるか**（長い会議、出力機器の切り替え、画面共有の前後）。
   `lbtest watch` を動かしたまま会議をし、MeetRec のログ（「出力機器の音量が 0 から一気に上げられたので、0 に戻しました」）と突き合わせる。
3. Teams の中でスピーカーをオフにする／出力先を、音が鳴らない別の出力機器（モニターの音声出力など）にしたときに、どう録れるか。

## 調べるための道具

- `tools/lbtest`：MeetRec と切り離して、方式ごとに届く音量を測り、出力機器の状態を見る（ワークスペースの外にあり、本体には入らない）。
  `cargo build --release --manifest-path tools/lbtest/Cargo.toml` でビルドし、`tools\lbtest\target\release\lbtest.exe` を使う。

  | コマンド | すること |
  |---|---|
  | `lbtest matrix [base\|cats\|mute\|all]` | 合成音で「分かったこと 1」の表を作る。`mute` は、既定の出力機器の消音と音量を一時的に変えて戻す |
  | `lbtest cap <include\|exclude> <PID> <ミリ秒>` | その PID を対象に（または除外して）プロセスループバックで録り、音量を出す |
  | `lbtest endpoint <ミリ秒>` | 既定の出力機器のループバックで録り、音量を出す |
  | `lbtest sessions` | 出力機器ごとの消音・音量・`hardware_support` と、オーディオセッション（PID・1 秒間のピーク） |
  | `lbtest watch <秒>` | 既定の出力機器の消音・音量が変わるたびに、時刻つきで出す |
  | `lbtest epmute <0\|1>` / `lbtest epvol <0〜1>` | 既定の出力機器の消音・音量を変える |
  | `lbtest sessmute <PID> <0\|1>` | その PID のオーディオセッションの消音を変える |

  音量（RMS）が 0.00002 前後なら無音。音は録音せず、音量だけを測る。
- `MeetRec.exe` の「診断ログを保存」：出力側のオーディオセッション（プロセス・親・動作中か）が出る（`probe.rs` の `diagnostics`）。
- `%APPDATA%\MeetRec\meetrec.log`：「相手音声のキャプチャ開始（…）」に、どの方式で始めたかが出る。消音を外した・音量を 0 に戻した・
  元に戻したことも出る。時刻は UTC。
- 方式の切り替え：`config.toml` の `[[app]]` で `id = "teams"` の定義を丸ごと書けば、`system_audio = false` にして
  プロセスループバックを試せる（組み込みの定義は `crates/core/src/rules/builtin.rs`。診断ログの「有効な会議アプリ定義」を
  写して書き換えるのが早い）。
- 実験用に PID や方式を変えたいときは、`audio.rs` を一時的に書き換えてビルドする（`cargo build --release -p meetrec`。
  できた `target\release\meetrec.exe` をそのまま起動できる。先にインストール済みの MeetRec を終了すること）。
- 録画に相手の音が入ったかは、自分を Teams でミュートにして録り、録画の音量で判定するのが確実（自分のマイクが止まるので、
  残る音は相手の音だけ）。`ffmpeg -i 録画.mp4 -vn -af volumedetect -f null -` の `mean_volume` が `-91 dB` 付近なら無音。

## 守ること

- 実験で分かった事実と、推測を分けて書く。録れた・録れないは、ログと音量で確かめてから書く。
- Zoom の経路（プロセスループバックで録れている）は変えない。会議アプリごとの違いは `rules/builtin.rs` の表に書く（AGENTS.md）。
- 仮想オーディオ デバイス（ドライバーの導入が要るもの）は、方針（軽量・導入が簡単）に合わないので、採る前に利用者に確認する。
- `lbtest` の `matrix mute`・`epmute`・`epvol`・`sessmute` は PC の設定を変える。通話中に使うときは、相手の音が聞こえなくなったり、
  スピーカーから音が出たりすることを、使う人に伝えてから実行する。
