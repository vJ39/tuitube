# 検索結果の段階的な追加読み込み(擬似lazy load)

yt-dlpのytsearch抽出はストリーミング出力ではなく全件集めてから一括出力するため、真のページング(段階取得)は不可(実測済み: 1000件検索は最初の1行が出るまで11秒かかり、その後全363件がほぼ同時に届いた)。件数を変えると順序も入れ替わるため、差分だけ追記することもできない。

実現案: 初期表示は小さい件数(速い)で取得し、「もっと見る」操作で件数を増やして一覧を丸ごと再取得・全置換する。差分追記ではない。

## 対象範囲

`Target::Search`のタブ(「すべて」タブの検索語、固定キーワードのカテゴリタブ)だけが対象。

`Target::Feed`(`:ytrec`等のcookie連携フィード)と`Target::Channel`(チャンネルモードの動画/ショート/ライブ配信タブ)は、`--playlist-end`で`FEED_LIMIT`/`CHANNEL_LIMIT`という固定件数を使っており、`search.limit`とは無関係([`yt_dlp_args`](../src/search.rs)参照)。これらは既に固定件数を毎回取得済みで、増やす余地が無いため対象外。

## 増分

新しい設定項目は追加しない。既存の`search.limit`(既定10、設定画面で変更可、範囲1〜`MAX_SEARCH_LIMIT`=1000)を、初回件数と1回あたりの増分の両方に使う。

- 初回検索: 従来通り`search.limit`件で検索する
- 「もっと見る」: 今表示中の件数 + `search.limit`を新しい要求件数とし、`MAX_SEARCH_LIMIT`で丸めて検索し直す。結果は一覧を丸ごと置き換える(差分追記ではない)
- 要求した件数より実際の結果が少なければ、それ以上無いと分かる(yt-dlpは要求件数まで可能な限り集めるため)。以降そのタブでは「もっと見る」を出さない
- 要求件数が`MAX_SEARCH_LIMIT`に達した後も出さない

## トリガー

明示キーのみ。スクロール末尾での自動読み込みは行わない(既存の取り直し(`r`)等も同じく明示キー方式)。

- Resultsモードで`m`キー: 「もっと見る」ができる状態でだけ動く
- フッタに、もっと見られる間だけ`m:もっと見る`のヒントを出す

## 状態管理

`TabState`(`src/category.rs`)に`requested_limit: usize`を追加する。そのタブの一覧を取得したときにyt-dlpへ実際に要求した件数(既定0=未取得)。

- 「もっと見られる」の判定 = `requested_limit > 0` かつ `results.len() >= requested_limit` かつ `requested_limit < MAX_SEARCH_LIMIT`
- `Target::Feed`/`Target::Channel`のタブは`requested_limit`を立てないままにする(0のまま)。上の判定式にそのまま乗るので、個別の場合分けが要らない
- 実際に要求した件数は`AppEvent::SearchDone`に`requested_limit: usize`として持たせ、`main.rs`の`apply_search_done`が結果の反映と一緒に`TabState`へ書き込む

## 選択位置の保持

丸ごと置き換え後、選んでいた動画IDが新しい一覧にまだあればそこへ、無ければ同じ添字を新しい件数で丸める。`hide_selected`等が使っている既存の[`retain_visible`](../src/app.rs)と同じ考え方を、`set_results`の置き換え時にも適用する。通常の新規検索(まったく違う一覧になる)ではID一致がほぼ起きないため、これまでの「常に先頭へ」という見た目は変わらない。

## 実装箇所

- `src/category.rs`: `TabState::requested_limit: usize`を追加
- `src/app.rs`:
  - `AppEvent::SearchDone`に`requested_limit: usize`を追加
  - `set_results`の選択位置決定をID優先(見つからなければ0)に変える
  - `App::can_load_more() -> bool`を追加(上記の判定式)
- `src/actions.rs`:
  - `spawn_search`が要求した`limit`を`AppEvent::SearchDone`へ含めて送るようにする
  - `load_more_with()`を追加。今の`view_results().len() + settings.search.limit`を`MAX_SEARCH_LIMIT`で丸めた値を要求件数として、`Target::Search`のタブでだけ`spawn_search`を呼ぶ
- `src/input.rs`: Resultsモードの`m`キーを`load_more_with`に配線(条件を満たさない間は何もしない)
- `src/ui.rs`: フッタに`m:もっと見る`ヒントを追加(条件を満たす時だけ)

キャッシュ(`search.cache_enabled`)との関係: 「もっと見る」も通常の検索と同じ経路(`spawn_search`→`run_search`→`apply_search_done`→`remember_search`)を通る。`cache_key`は`limit`を含むため、増えた件数は別のキャッシュキーとして書き込まれる。件数の少ない古いキャッシュエントリは上書きされず残るが、既存のTTL失効・`CACHE_CAPACITY`退去でいずれ片付くため追加対応は不要。

## 対象外(v1)

- スクロール末尾での自動読み込み
- `Target::Feed`/`Target::Channel`での「もっと見る」(既に固定件数を取得済みのため対象外)
- 「もっと見る」の多重発行を防ぐ専用の仕組み(`spawn_search`が先行検索を`cancel_search`で打ち切る既存の仕組みに委ねる)
