# 設定画面

TUI 上で設定を一覧・編集できる画面。`Mode::Settings` として実装する。

## 起動・終了

- `Ctrl+S` で Input / Results モードから開く。Results モードでは `S`(大文字) でも開く。Playing モードからは開けない。
  - Input モードの `S` は検索語の文字として扱う。ここで設定画面へ取られると "SEKIRO" のような語を打てない。
  - Input モードでは Ctrl 付きの文字キーを検索語に入れない。
- `Esc` / `q` で閉じ、開いた元のモードへ戻る。このとき `app.settings` を開いた時点の値へ戻す(保存していれば最後に保存した値)。
  - `search.limit` / `search.layout` / `thumbnails.enabled` / `thumbnails.timeout` はセッション中に読み直されるので、編集を残すと保存していない値が実行中の表示に出る。
- `s`(小文字) で `app.settings` を `settings::save_to()` でファイルへ保存する。成功・失敗は `set_temporary_notice` / `set_error` で通知する。保存した値が以後の `Esc` の戻り先になる。

保存した値が効き始める時点は項目で違う。

- `display.mode`: 次回起動から。実行中の表示モードは `app.display` が持ち、再生中の `w` で切り替える。保存した値が `app.display` と違う間は、保存の知らせに「display.mode は次回起動から反映されます」を添える。
- `display.quality` / `fps_cap` / `subtitles.enabled`: 次の再生から(`LaunchPlan` が起動時に読む)。
- `search.limit` / `search.layout` / `search.timeout_secs` / `thumbnails.*`: 保存した時点から。

### 環境変数が上書きしている項目

`TUITUBE_FPS_LIMIT` / `TUITUBE_COOKIES_FROM_BROWSER` は一時的な指定なので、`s` を押しても設定ファイルへは書かない。

- `settings::validate()` は上書きした項目のファイル側の値を `EnvOverridden` に控え、`Loaded` 経由で `App::env_overridden` に持つ。
- 保存時は `EnvOverridden::restore()` でその項目だけファイル側の値へ戻してから `render()` に渡す。他の行の編集は保存される。
- 上書き中の行には画面で `(TUITUBE_FPS_LIMIT が指定中。保存しません)` を添え、保存の知らせにも書き換えなかったキー名を出す。

### 裏で終わった検索

検索は設定画面を開いている間も続く。結果が届いても画面は閉じず、`settings_return` (閉じたときの戻り先) だけを書き換える(`App::enter_search_mode`)。再生が終わったときの戻り先も同じ扱い。

## 画面構成

既存の `search_areas` と同様、`Layout::vertical` で4段:

1. タイトル行 (1行): `設定 (s で保存。Esc は編集を捨てて戻る)`
2. 項目一覧 (`List` + `ListState`、残り全部)
3. ステータス行 (1行、既存 `draw_footer` を共有)
4. ヘルプ行 (1行): `↑↓:選択 ←→:値変更 Enter/Space:切替 s:保存 Esc:破棄して戻る`

各行は `ラベル: 現在値` 形式。選択中の行は既存 `draw_list` と同じハイライトを適用する。

検索画面のヘルプ行も他のモードと同じ `fit_hints` に通し、設定を開くキー(Input は `Ctrl+S:設定`、Results は `S:設定`)を末尾に並べる。幅が足りない端末では末尾から落とす。

## 編集対象項目

| 項目 | 操作 | 値の範囲 |
|---|---|---|
| display.mode | → で次、← で前 | `DisplayMode::next()` / `prev()` |
| display.quality | → で次、← で前 | `Quality::next()` / `prev()`(Low→Medium→High→Native→Low) |
| fps_cap | ←→ で 5 刻み増減 | 0(無制限)〜`MAX_FPS_CAP` |
| subtitles.enabled | Enter/Space で toggle | bool |
| search.layout | → で次、← で前 | `LayoutMode::next()` / `prev()`(2 値なので同じ動き) |
| search.limit | ←→ で 1 刻み増減 | `MIN_SEARCH_LIMIT`〜`MAX_SEARCH_LIMIT` |
| search.timeout_secs | ←→ で 5 刻み増減 | `MIN_SEARCH_TIMEOUT_SECS`〜`MAX_SEARCH_TIMEOUT_SECS` |
| thumbnails.enabled | Enter/Space で toggle | bool |
| thumbnails.max_cached | ←→ で 50 刻み増減 | 0〜(上限なし、既存に準拠) |
| thumbnails.timeout_secs | ←→ で 5 刻み増減 | 1〜`MAX_THUMB_TIMEOUT_SECS` |

範囲外への変化はクランプする(境界で止まる。ラップしない)。

数値は刻みの目盛り(刻みの倍数)の上を動く。下限が刻みに乗っていない項目(`thumbnails.timeout_secs` の 1)でも目盛りの基準はずらさず、`10 → 5 → 1 → 5 → 10` と戻れるようにする。設定ファイルに手で書いた端数から始めたときは、隣の目盛りへ寄せる。

対象外(v1、config.toml 手編集のまま): `window.*`、`cookies.browser`、`mpv.extra_args`、`subtitles.lang`、`categories`。

## 状態

`App` に以下を追加:

- `Mode::Settings`
- `settings_selected: usize`(選択中の項目インデックス。↑↓ は端で巻き戻す。壊れた値は項目数の範囲へ丸めて使う)
- `settings_return: Mode`(閉じたときの戻り先)
- `settings_backup: Settings`(開いた時点・最後に保存した時点の設定。`Esc` はここへ戻す)
- `env_overridden: EnvOverridden`(環境変数が上書きしている項目)

## 関数配置

- `input.rs`: `handle_key_settings`、`is_settings_key`
- `ui.rs`: `draw_settings`、`settings_areas`
- `actions.rs`: `open_settings`、`close_settings`、`move_settings_selection`、`adjust_settings_value`、`save_settings`、`saved_notice`
- `app.rs`: `SettingsItem`、`enter_search_mode`、数値の刻み計算(`step` / `step_up` / `step_down`)
- `display.rs`: `Quality::next()` / `Quality::prev()` / `DisplayMode::prev()`
- `grid.rs`: `LayoutMode::next()` / `LayoutMode::prev()`
- `settings.rs`: `EnvOverridden`、`Validated`(既存の `validate` / `render` / `save_to` は再利用)
