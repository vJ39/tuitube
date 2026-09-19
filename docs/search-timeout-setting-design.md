# 検索タイムアウトの設定可能化

検索(yt-dlp呼び出し)のタイムアウトが`src/search.rs`の`YT_DLP_TIMEOUT`(30秒固定)になっており、`[search] limit`を大きくすると容易に超過して検索が失敗する。設定可能にする。

## 設定ファイル

`[search]`セクションに`timeout_secs`を追加する(`thumbnails.timeout_secs`とは別物、混同しないようキー名はそのまま`[search]`配下に置く)。

```toml
[search]
limit = 1000
timeout_secs = 30
```

- 既定値は30秒のまま(既存挙動を変えない)
- 範囲: 5〜300秒。範囲外はクランプしnoticeを出す(既存の`thumbnails.timeout_secs`と同じパターン)

## コード変更

- `settings.rs`: `RawSearch`に`timeout_secs: Option<i64>`追加、`SearchSettings`に`timeout: Duration`追加、`validate_search`でクランプ、`render`に出力
- `search.rs`: `YT_DLP_TIMEOUT`定数は「未設定時の既定値」として残すが、`run_search`/`attempt`に`timeout: Duration`引数を追加し、呼び出し元(`actions.rs`)が`app.settings.search.timeout`を渡す
- `cookies.rs`: タイムアウト時の案内文(`describe`のTimedOutケース、現在`YT_DLP_TIMEOUT.as_secs()`をハードコード参照)を、実際に使われた秒数を受け取る形に変更する
- `search.rs`: `SearchReport`に使った`timeout`を持たせる。結果を受け取る`main.rs`の`apply_search_done`はその値で文言を作る(検索中に設定画面で秒数を変えられても、打ち切った側の秒数と食い違わない)

## 設定画面

`SETTINGS_ITEMS`(app.rs)に`search.timeout_secs`を追加する(既存の数値項目と同じ扱い、5刻み増減+数字キー直接入力)。
