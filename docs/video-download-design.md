# 動画/音声ファイルのダウンロード

再生とは別に、選んだ動画をディスクへ保存する。保存には yt-dlp をそのまま使う(mpv は経由しない)。1 本ずつの保存のみで、プレイリストや検索結果の一括保存は対象外。

## 開く・閉じる

- Results / Channel モード: 選択中の行で `d` キー
- Playing モード: 再生中の動画で `d` キー(`app.playback.title` / `app.playback.url` を使う)
- 新しい `Mode::Download` を開く。閉じたら開いた元のモードへ戻る(`Mode::Settings` の `settings_return` と同じ考え方で `download_return: Mode` を持つ)
- `Esc`: 何もせず元のモードへ戻る
- `Enter`: 入力中の内容でダウンロードを開始し、即座に元のモードへ戻る(ダウンロードは背景で進む)

## 画面構成

設定画面と同じ3段(タイトル・項目・ヘルプ)構成。項目は3行:

1. 保存先: 自由入力(`QueryEditor`を再利用。左右矢印でカーソル移動、文字入力で編集)
2. ファイル名: 自由入力(同上)。末尾に固定で `.%(ext)s` を表示する(編集不可。実際の拡張子は yt-dlp が決める)
3. 形式: `動画` / `音声のみ`(← → または Enter/Space で切替)

↑↓ で3行の間をフォーカス移動する(巻き戻る)。フォーカス中の行だけが文字入力/矢印キーを受け取る。

開いた時点の初期値:

- 保存先: `[download] dir`(設定。未指定なら `$HOME/Downloads`。`$HOME` も無ければ空欄で始まり、空欄のまま `Enter` すると保存を断る)
- ファイル名: 動画タイトルから `/` `\` を `_` に置き換えたもの(パス区切りをファイル名に混ぜてディレクトリを飛び出されないようにする)
- 形式: `動画`(前回の選択は覚えない)

## ダウンロード

`Enter` を押した時点の3項目から yt-dlp の引数を組み立てる。

- 保存先の先頭 `~/` は `$HOME` へ展開する(`settings::expand_home` と同じ規則。設定ファイルの他のパス項目と扱いを揃える)
- 出力テンプレートは `<保存先>/<ファイル名>.%(ext)s`。ディレクトリが無ければ yt-dlp 自身が作る(`-o` の既定動作)
- 動画: `yt-dlp -f "bestvideo+bestaudio/best" -o "<template>" --print after_move:filepath <url>`
- 音声のみ: `yt-dlp -x --audio-format mp3 -o "<template>" --print after_move:filepath <url>`
- `--print after_move:filepath` は保存完了後の実際のパスを標準出力へ1行出す。完了通知の文言に使う
- 映像+音声の結合(動画)や音声変換(音声のみ)には ffmpeg が要る。無ければ yt-dlp 自身がその旨のエラーを返すので、そのまま画面に出す(こちらで ffmpeg の有無を事前チェックしない)

保存先・ファイル名のどちらかが空欄なら `Enter` を断り、`set_temporary_error` で知らせる。ダウンロードは開始しない。

## 状態・進捗

進捗はパーセント表示せず、進行中かどうかだけを示す(既存のサムネイル取得の `fetching: bool` と同じ粒度)。

- 開始時: `app.set_notice("ダウンロード中: <タイトル>")`(期限なし。終わるまで消えない)
- 成功時: 標準出力の最後の行をパスとして `set_temporary_notice("保存しました: <path>")`。パスが取れなければ `保存しました: <保存先>` に落とす
- 失敗時: `set_error(Some("ダウンロードに失敗しました: <yt-dlpの標準エラー>"))`

同時に走らせるのは1件まで。新しいダウンロードを始めると前のものは打ち切る(`search_task`/`oauth_task`/`channel_lookup_task` と同じ nonce + `abort()` の形)。

## 状態管理

`App` に追加:

- `Mode::Download`
- `download_return: Mode`
- `download_dir: QueryEditor`
- `download_filename: QueryEditor`
- `download_audio_only: bool`
- `download_focus: DownloadField`(`Dir` / `Filename` / `Format` の3値。`next()` / `prev()` で巡回)
- `download_url: String`(開いた時点の対象。タイトルは初期ファイル名の計算にだけ使うので保持しない)

`Session` に追加:

- `download_task: Option<JoinHandle<()>>`
- `download_nonce: u64`

## 実装箇所

- `src/download.rs`(新規): `Downloader` トレイト(`search::YtDlp` とは独立。`oauth.rs` が独自の `Backend` トレイトを持つのと同じ考え方)、`RealYtDlp`、引数組み立て(`download_args`)、既定保存先(`default_dir`)、ファイル名のサニタイズ(`sanitize_filename`)、出力からのパス抽出(`extract_saved_path`)
- `src/settings.rs`: `[download]` セクション(`RawDownload { dir: Option<String> }` → `DownloadSettings { dir: Option<PathBuf> }`)。設定画面(v1)には出さない(`window.*` 等と同じく対象外)。`expand_home` を `pub(crate)` にして `download.rs` から再利用する
- `src/app.rs`: `Mode::Download`、上記フィールド、`DownloadField`、`open_download` 時の初期値計算
- `src/actions.rs`: `open_download` / `close_download` / `move_download_focus` / `start_download`(nonceの打ち切り込み)、`AppEvent::DownloadDone { nonce, notice: Result<String, String> }` の送出
- `src/main.rs`: `AppEvent::DownloadDone` のハンドラ(nonce一致時だけ通知を反映)
- `src/input.rs`: Results/Channel/Playing の `d` キー配線、`handle_key_download`(↑↓でフォーカス移動・文字入力とカーソル移動をフォーカス中の`QueryEditor`へ・← →は`Format`行ではtoggle/それ以外はカーソル移動・Enter/Esc)
- `src/ui.rs`: `draw_download`、`download_areas`、フッタヒント

## 対象外(v1)

- パーセント表示・残り時間などの詳細な進捗
- 複数本の同時ダウンロード・キュー
- プレイリスト/検索結果一覧のまとめ保存
- ファイル名欄での yt-dlp テンプレート構文(`%(uploader)s` 等)の展開。欄はプレーンな文字列として扱う
- ダウンロード履歴の一覧・管理
- 保存先ディレクトリを設定画面(`Mode::Settings`)から編集する導線(`config.toml` 手編集のみ。他の自由文字列項目と同じ扱い)
