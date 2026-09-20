# 検索結果を日付降順にする(#72)

検索結果・チャンネルの動画一覧・プレイリストの動画一覧など、動画が並ぶ画面すべてを公開日の新しい順に並べる。

## 前提: 技術検証(実機で確認済み)

- 既存の検索方式(`ytsearchN:<query>`、`--flat-playlist`)では、返る行に日付情報(`timestamp`)が一切乗らない(常に `null`)。並び替えの元データが無い。
- YouTube検索結果ページを直接叩く方式(`https://www.youtube.com/results?search_query=<query>`)に `--extractor-args youtubetab:approximate_date` を付けると、`timestamp` に妥当な値が入る。
- ただしこの方式は、返る行の約1/3が動画でなくチャンネル(`ie_key: "YoutubeTab"`)になる。既存の `ytsearchN:` はチャンネル行を返さないため、新たに除外フィルタが必要。
- `sp=`(YouTube検索のソート指定パラメータ)で「アップロード日順」を試したが、グループごとに新しい順という不完全な並びだった(完全な降順ではない)。使わず、取得後にこちら側で `timestamp` を見て降順ソートする。
- ページネーション(`--playlist-start`/`--playlist-end`)は新方式でも問題なく動く。

## 対象範囲と実機確認済みの対応可否

`--extractor-args youtubetab:approximate_date` を付けたときに `timestamp` が実際に入るかを種別ごとに確認した。

| 対象 | 対応可否 |
| --- | --- |
| 通常検索(`Target::Search`) | 可(検索方式の変更が必要) |
| チャンネル Videos タブ | 可(既存URLのまま) |
| プレイリスト内動画(`Target::Playlist`) | 可(既存URLのまま) |
| `:ytwatchlater`(後で見る) | 可(既存URLのまま) |
| `:ytrec`(おすすめ) | 可(既存URLのまま) |
| チャンネル Shorts/Streams タブ | 不可(`timestamp` が常に空) |
| `:ythis`(履歴) | 不可 |
| `:ytsubs`(登録チャンネル) | 不可 |

対応不可な種別は「ソートしても何も変わらない」ため、コード上で対応可否を条件分岐で書き分ける必要はない。全種別に同じ処理(`approximate_date` を付けて取得し、結果を `timestamp` 降順でソート)を適用すれば、対応可能な種別は正しく並び替わり、対応不可な種別は元の順序のまま(実害なし)になる。

## 変更内容

`src/cookies.rs` / `src/search.rs`:

- `Target::Search` の検索URLを `ytsearchN:<query>` から `https://www.youtube.com/results?search_query=<urlencoded query>` に変える(この種別だけ既存URLでは日付情報が一切取れないため)。件数は既存どおり `--playlist-end` で渡す。
- yt-dlp の実行引数に `--extractor-args youtubetab:approximate_date` を常に追加する(全 `Target` 共通)。
- `Target::Search` 用のパース関数で、`ie_key` が `"Youtube"` でない行(チャンネル等、新方式で約1/3混在する)を読み飛ばす。他の `Target` は既存のパースのまま。
- 全ての `Target` で、パースした `Vec<SearchResult>` を返す直前に `timestamp` の降順でソートする(安定ソート。`timestamp` が無い行同士は元の順序を保つ)。`SearchResult` 自体に `timestamp` フィールドは持たせない(並べ替え専用でパース時にしか使わないため)。

## 対象外(v1)

- 並び順の切り替えUI(関連度順に戻す設定等)。常に日付降順のみ
- `approximate_date` の精度以上の厳密な日時表示
- Shorts/Streamsタブ・履歴・登録チャンネルフィードでの日付ソート(技術的に不可能なため、これらは既存の並び順のまま)
