# テスト不足箇所の洗い出しと追加(#10)

## 測り方

`cargo tarpaulin --engine llvm` で行カバレッジを測る(手元に入っていた 0.37.2 を使う。追加のインストールは無い)。作業用のビルド先を分け、通常の `target/` には触れない。

追加前は全体で 91.2%(6339 行中 5781 行)、追加後は 92.6%(5873 行)。

## テストを足さないところ

本物の mpv・curl・pbcopy・openssl・ブラウザ・TCP・端末を動かす部分。テストでは差し替え口(`FakeYtDlp`・`FakeBackend`・`FakeCurl`・`FakeClipboard`・`Recorder` など)の向こう側にあり、単体テストからは起動しない。

- mpv の起動と IPC(`mpv::launch`・`spawn_video_reader`・`connect_with_retry`・`kill_and_collect_stderr`・`MpvController` の送信と後始末)
- OAuth の実通信とコールバック待ち(`oauth` の `curl`・`bind`・`wait`・`accept_callback`・`openssl_sha256`・`open`・`random`)
- クリップボードへの書き込み(`clipboard::write_to_pbcopy`)
- 端末の初期化とイベントループ(`main::main`・`run`・`spawn_input_reader`・マウス捕捉の切り替え)
- 環境変数から実際のパスを決めて読み書きする入口(`settings::load`・`hidden::load`・`resume::load`・`engagement::load`/`save`・`mpv::socket_dir` など)。パスを受け取る `_from`/`_to` の側はテスト済み
- 本物の実行者を渡すだけの入口(`start_playback` の mpv 起動・`start_comments`・`apply_resize`・`handle_key_download`・`save_settings`)

次の分岐もテストでは通さない。

- `ui::help_text` の設定画面の分岐。`help_line` が先に設定画面の案内を返すので通らない。`Mode` の網羅のために残している
- `settings::expand_home` の `HOME` が無いときの分岐。プロセス全体の環境変数を書き換えないと通せず、並列に走る他のテストに響く
- `kitty::ApcParser` の、OSC を BEL で閉じる分岐。閉じた後も読み飛ばし中も ESC 以外を捨てるので、結果が変わらない

## テストを足すところ

いずれも既存の差し替え口で外部に出ずに書ける。

### 高(壊れても気づきにくい・実害がある)

- ダウンロード完了の反映(`screen::download::apply_download_done`)。今はテストが無い
  - 打ち切った後の古い nonce の結果は捨てる
  - 成功は期限つきの知らせで出す
  - 失敗は「ダウンロード中: …」の知らせを畳んでエラーを出す。関係の無い知らせは消さない
- 再生中のイベント(`main::handle_event`)
  - 前の再生の nonce の `MpvProperty`・`VideoFrame`・`VideoError`・`MpvExited` は今の再生に触れない
  - 今の nonce の `VideoError`・`MpvExited` は再生を終えて一覧へ戻る
- イベントの振り分け(`main::handle_event`)。`PlaylistsReady`・`DownloadDone`・`OauthDone`・`EngagementReady`・`ChannelLookup` がそれぞれの反映先に届く
- 選択の移動で表示範囲がずれたら、スクロール位置を更新してサムネイルを貼り直す(`move_selection_with`)
- 手で書いた非表示リストが改行で終わっていなくても、追記後の中身が TOML として読める(`hidden::append`)
- いいね/登録の状態確認(`start_engagement_with`)
  - 登録の問い合わせが失敗したら、そのチャンネルを未登録として控えない
  - 新しい問い合わせは前の問い合わせを打ち切る

### 中

- チャンネルのタブの押し直しは、読めていないタブだけ取り直す。読めているタブへ移ったときは取り直さない(`select_channel_tab_with`)
- リサイズで寸法が変わらなければ mpv へ送らない。送信に失敗したらエラーを出す(`apply_resize_with`)
- yt-dlp を起動できないときの文言(`search` の `launch_error`)
- cookie が一部効かなかった(`Degraded`)ことを知らせる(`main::apply_search_done`)
- mpv の出力の読み分け(`kitty::ApcParser`・`FrameAssembler`)。OSC・DCS などが混ざっても後ろの画像コマンドを取りこぼさない。問い合わせ(`a=q`)で組み立て中のフレームを捨てない
- list 表示ではサムネイルを取りに行かない(`start_thumbnails_with`)
- ダウンロード画面のファイル名の欄への入力と → キー。Download モードで `handle_key` を通したときの振り分け
- プレイリスト一覧・プレイリストの中身でのバックグラウンド操作(b キーで再生へ戻る、案内の「b:全画面へ」)
- 再生画面のマウスの移動とドラッグがシークバーの hover とドラッグに届く(`mouse_input`)

### 低(小さな関数の分岐)

- `display_label` の text 表示、`mpv::describe_status` のシグナル終了、`oauth::config_escape` の `\t`・`\r`・`\v`、`video::option_arg` の true、`seekbar::clamp_column` のトラックが無い幅
- 状態行の代わりの表示(チャンネル・プレイリスト・プレイリスト一覧を開いていないとき)

## 進め方

1. 高 → 中 → 低の順に、対象のモジュールのテストに足す
2. 足したテストごとに、対象の行を一時的に壊すと落ちることを確かめてから戻す(テストが本当にその分岐を見ていることの確認)
3. 今の実装で落ちるテストが出たら不具合として直し、何を直したかを報告する
4. 最後に `cargo build`・`cargo test`・`cargo clippy --all-targets`・`cargo fmt --check` を通し、カバレッジを測り直して追加前と比べる
