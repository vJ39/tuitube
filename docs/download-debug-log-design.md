# ダウンロードのDEBUGログ

ダウンロード機能(#43)がうまく動かない場合の調査用に、yt-dlpへの実引数・終了コード・標準出力/エラーをファイルへ記録できるようにする。既定は無効(既存の挙動を変えない)。

## 設定

`[download]`セクションに`debug`(bool)を追加する。

```toml
[download]
debug = false
```

既定`false`。設定画面(`Mode::Settings`)にも項目を追加する(`SubtitlesEnabled`等と同じbool toggle)。`download.dir`は自由文字列のため設定画面では編集できない(#43の設計どおり対象外)が、`debug`はboolなので既存のtoggle項目と同じ形でそのまま追加できる。

設定画面から保存できるようにする以上、`render()`は`[download]`セクション全体(`dir`と`debug`の両方)を出力するようにする(`dir`のtoggle項目が無いことと、`render()`がそのキーを書くこととは別の話。書かないと、`debug`を保存するたびに既存の`dir`設定が消える)。`dir`は`[window]`の`vo`/`autofit`等と同じ扱い(値があればそのまま、無ければコメントアウトした例を出す)。

## ログの内容・出力先

`$XDG_CONFIG_HOME/tuitube/download-debug.log`(`hidden.toml`/`resume.toml`と同じ置き場、`settings::app_config_dir`)。

`debug`が有効な間、`download::run`を呼ぶたびに1エントリを追記する(成功・失敗のどちらでも書く。yt-dlp自体が起動できなかった場合も書く)。

```
##### <unixタイムスタンプ> #####
args: -f bestvideo+bestaudio/best -o /path/title.%(ext)s --print after_move:filepath https://...
result: success (exit 0) | failed (exit <code>) | launch error: <理由>
--- stdout ---
<標準出力そのまま>
--- stderr ---
<標準エラーそのまま>

```

書き込みは追記のみ(既存の内容を読み直したりはしない。`hidden.toml`/`resume.toml`と違い、正しさが要る内部状態ではなく調べ物用のログのため、失敗しても無視してよい=書けなくてもダウンロード自体は続行する)。

失敗時、画面のエラー文言にログの場所を添える: `ダウンロードに失敗しました: <理由> (詳細: <ログのパス>)`。`debug`が無効な間は今までと同じ文言のまま。

## 実装箇所

- `src/download.rs`:
  - `debug_log_path(xdg_config_home, home) -> Option<PathBuf>`(`hidden::hidden_path`と同じ形)
  - `format_debug_entry(now: SystemTime, args: &[String], output: &std::io::Result<std::process::Output>) -> String`
  - `append_debug_log(path: &Path, entry: &str) -> Result<(), String>`(追記のみ。親ディレクトリが無ければ作る)
  - `run`の引数に`debug_log_path: Option<&Path>`を追加。`Some`の間だけ`format_debug_entry`→`append_debug_log`を行い、エラー文言にログの場所を添える
- `src/settings.rs`: `RawDownload::debug: Option<bool>`、`DownloadSettings::debug: bool`、`validate_download`、`render()`の`[download]`セクション追加
- `src/app.rs`: `SettingsItem::DownloadDebug`(`SETTINGS_ITEMS`の末尾に追加)
- `src/actions.rs`: `start_download`で`app.settings.download.debug`から`debug_log_path`を求めて`download::run`へ渡す

## 対象外(v1)

- ログファイルの上限・ローテーション(有効にしている間は増え続ける。使い終わったら無効化するか手で消す)
- ダウンロード以外の機能(検索・再生等)のログ
- ログの一覧表示・アプリ内での閲覧UI(手でファイルを開く)
