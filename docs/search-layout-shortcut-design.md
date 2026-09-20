# 結果一覧の表示形式(grid/list)をショートカットキーで切替+自動保存(#74)

現状は `Ctrl+S` で設定画面を開き `search.layout` を選んで矢印キーで変えて `s` で保存する必要がある。既存の `w`(表示モード切替、#42)と同じ考え方で、結果一覧の画面から1キーで即時切替+自動保存できるようにする。

## 既存の流用

- `LayoutMode::next()`(`src/grid.rs`): Grid⇔List の反転が既にある。
- `w` キーの保存パターン(`src/actions.rs` の `cycle_display_mode`/`save_display_mode`、`src/settings.rs` の `save_display_mode_to`/`with_display_mode`)。今回は mpv へコマンドを送る必要が無いぶん `cycle_display_mode` より簡潔になる。

## 変更内容

`src/settings.rs`:

- `with_display_mode(text, mode)` の内部ロジック(セクション名・キー名・値を見て該当行を書き換え/挿入する処理)を `with_toml_string_value(text, section, key, value)` に切り出す(非 `pub`)。`with_display_mode` はこれを呼ぶ薄いラッパーに変える(既存のテストはそのまま通る)。
- `with_search_layout(text, layout)` を新設(`with_toml_string_value(text, "search", "layout", layout.key())` を呼ぶ)。
- `save_search_layout_to(path, layout) -> Result<(), String>` を `save_display_mode_to` と同じ形(ファイルが無ければテンプレート生成、無効な書き方なら書き換えを拒否)で新設。

`src/actions.rs`:

- `toggle_search_layout(app, config, now)` を新設。`app.settings.search.layout` を `next()` で反転 → `save_search_layout_to` で保存 → 成功したら `app.settings.search.layout`/`app.settings_backup.search.layout` を新しい値に、失敗したら `set_temporary_error`。mpv・再生には触れない。

`src/input.rs`:

- `Mode::Results`/`Mode::Channel`/`Mode::Playlist` で `v` キー(空きキー)を押すと `toggle_search_layout` を呼ぶ。

`src/ui.rs`:

- `results_hints`/`channel_hints`/`playlist_hints` に `v:表示切替` を追加。

## 対象外(v1)

- `Mode::Playlists`(プレイリスト一覧そのもの)への適用。あちらはサムネイルを持たないタイトルのみのリスト固定で、grid/list切替の対象ではない
- マウスでの切替
