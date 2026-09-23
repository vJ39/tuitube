# 画面ごとのモジュール構成(#40)

1つの画面を直すときに読む・触るファイルを、その画面のモジュールと共通部品に絞る。現状は1つの画面の処理が `input.rs`(キー)・`ui.rs`(描画)・`app.rs`(状態)・`actions.rs`(操作)・`main.rs`(イベント)に分かれていて、画面の変更のたびに5ファイル前後を行き来している。

## 構成

`src/screen/` に画面ごとのモジュールを置く。各モジュールは、その画面の状態・キー処理・マウス処理・描画・ヘルプ文言・その画面だけで使う操作と、それらのテストを持つ。

- `screen::settings` — `Mode::Settings`
  - 状態 `SettingsScreen`(選択行・戻り先・開いた時点の設定の控え・打ち込み途中の数値)。今の `App` の `settings_selected`/`settings_return`/`settings_backup`/`settings_edit` をまとめたもの。`App` は `settings_screen: SettingsScreen` として持つ
  - `SettingsItem`・`SETTINGS_ITEMS`・行の文言・値の増減と数値の確定
  - キー処理(通常時・数値の打ち込み中)、描画、カーソル位置、ヘルプ
  - 開く・閉じる・選択移動・値の変更・保存(設定ファイルへの書き出し)
- `screen::download` — `Mode::Download`
  - 状態 `DownloadForm`(戻り先・保存先・ファイル名・音声のみ・フォーカス・対象URL)。今の `App` の `download_*` 6項目をまとめたもの。`App` は `download: DownloadForm` として持つ
  - `DownloadField`、キー処理、描画、カーソル位置、ヘルプ
  - 開く・閉じる・フォーカス移動・開始・完了通知の反映(`AppEvent::DownloadDone`)
  - yt-dlp の呼び出しそのもの(`download.rs`)は今のまま
- `screen::playlists` — `Mode::Playlists`
  - 状態 `PlaylistsView`、キー処理、描画、ヘルプ
  - 一覧を開く・届いた一覧の反映(`AppEvent::PlaylistsReady`)・閉じる・1つのプレイリストを開く/取り直す/戻る
  - 1つのプレイリストの中身を見る `Mode::Playlist` の描画とキー処理は、結果一覧と同じなので `screen::browse` 側に置く
- `screen::playing` — `Mode::Playing`
  - キー処理、マウス処理(シークバー・アクション行)、描画(映像・コメント・シークバー・アクション行)、ヘルプ
  - 画面の割り付け(`video_area`・`seek_bar_area`・`mini_video_area` など)は `main.rs`・`actions.rs` も使うので、このモジュールから公開する
- `screen::browse` — `Mode::Input`/`Results`/`Channel`/`Playlist`(検索欄と結果一覧を持つ画面)
  - キー処理、マウス処理、描画(検索欄・タブ行・グリッド・リスト)、当たり判定(タブ・結果・検索欄のクリック位置)、ヘルプ

残す側:

- `input.rs`: Ctrl+C と終了確認の横取りと、`Mode` から各画面の `handle_key`/`handle_mouse` への振り分けだけ
- `ui.rs`: `Mode` から各画面の `draw` への振り分けと、画面をまたいで使う部品(フッタ、`fit_hints`、`search_areas` など)
- `app.rs`: `App`・`Mode`・`AppEvent`、一覧の参照先を切り替える `view_*`、`ChannelView`/`PlaylistView`、状態行の振り分け
- `actions.rs`: `Session` と、複数の画面が共有する状態を触る操作(検索の実行とキャッシュ、再生の開始・終了・バックグラウンド、シーク・速度・字幕などプレイヤーへの送信、サムネイル、いいね/登録、リサイズ)

画面モジュールへ移すのは「その画面の状態だけを触る操作」まで。プレイヤーや検索結果のように複数の画面が共有する状態を触る操作は `actions.rs` に残し、画面モジュールから呼ぶ。

状態行の文言も各画面のモジュールに置く。`app.rs` に残すのは振り分けと、検索側の画面が共有する部分(検索中・エラー・cookie の表示、バックグラウンド中の目印)だけ。

## 進め方

1段階=1モジュールで、`settings` → `download` → `playlists` → `playing` → `browse` の順に切り出す。各段階で1コミットにする。

振る舞いは変えない。テストはコードと一緒に新しいモジュールへ移す。

1. 移す対象のテストを先に新しいモジュールへ移す(移し先に本体が無いのでコンパイルが通らない状態=Red)
2. 本体を移して参照を直す(Green)
3. `cargo build`・`cargo test`・`cargo clippy --all-targets`・`cargo fmt --check` を通す
4. テスト件数が移す前と同じであることを確認する(移動で落ちたテストが無いこと)

各モジュールのテストで使う小さなヘルパー(`key()`・`result()` など)は、今の各ファイルと同じくそのモジュールのテストの中に持たせる。

## 対象外

- 振る舞い・表示・キー割り当ての変更
- `actions.rs` の再生・検索まわりの操作の分割
- `settings.rs`(設定ファイルの読み書き)・`oauth.rs` など、画面ではないモジュールの分割
