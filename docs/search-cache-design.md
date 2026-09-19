# 検索結果のキャッシュ

同じ検索(クエリ/フィード/チャンネルタブ)を毎回yt-dlpで取り直さず、一定時間はキャッシュした結果を使い回す。

## 設定

`[search]`セクションに追加する。

```toml
[search]
cache_enabled = false
cache_ttl_secs = 300
```

- `cache_enabled`: 既定`false`(既存挙動を変えない)。設定画面にも項目を追加する(toggle)
- `cache_ttl_secs`: 既定300秒。範囲10〜3600秒でクランプ(既存の`timeout_secs`系と同じパターン)。設定画面に数値項目として追加する

## キャッシュ層

`Session`にメモリ内キャッシュを持つ(ディスク永続化はしない。アプリ再起動で消える)。

```rust
struct CacheEntry {
    results: Vec<SearchResult>,
    fetched_at: Instant,
}
session.search_cache: HashMap<String, CacheEntry>
```

キーは`Target`(検索クエリ/フィード/チャンネルタブいずれも判別できる)+`limit`+cookieを使ったかどうかを文字列化したもの。cookie有無で結果が変わりうるため、同じクエリでもcookie状態が違えば別キーとして扱う。

失敗した検索(タイムアウト・エラー)はキャッシュしない。成功した結果のみ格納する。

## 参照・更新のタイミング

- 検索開始時(`spawn_search`/`start_tab_search_with`等): `cache_enabled`かつキャッシュに該当キーがあり`fetched_at`から`cache_ttl_secs`以内なら、yt-dlpを呼ばずキャッシュの結果をそのまま`set_results`する(cookie状態の観測(`CookieState::observe`)は行わない。新規取得ではないため)
- キャッシュが無い/期限切れの場合は、既存どおりyt-dlpを呼ぶ。成功したら結果をキャッシュへ格納(既存エントリは上書き)
- `cache_enabled`が`false`の間はキーを作らない。参照も格納もしないので、既定のままではキャッシュは空のまま
- `r`(再取得)キーは常にキャッシュを無視して取り直し、成功したらキャッシュを更新する(「明示的な再取得」の意味を保つ)

`r`の取り直しは`TabState::reload`に印として持つ。検索が完了する前にタブを移って打ち切られても印は残り、次にそのタブを開いた時もキャッシュを使わず取りに行く。印は結果を反映した時点(`set_results`)で下りる。

## 退去

`remember_search`で新しい結果を格納するたびに、期限切れのエントリを落とす。それでも`CACHE_CAPACITY`(64件)を超える場合は`fetched_at`が古いものから捨てる。キーは検索クエリを含むため使い捨ての検索語のぶんだけ鍵が増える。1エントリが最大`search.limit`(最大1000)件の結果を持つので、上限なしにはしない。

## 対象外(v1)

- ディスクへの永続化(アプリ再起動でキャッシュは消える)
- キャッシュの手動クリア操作
- 上限件数の設定項目化(固定値`CACHE_CAPACITY`)
