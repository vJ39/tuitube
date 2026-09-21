# Feed系タブの並び順をYouTube返却順に変更(#76)

後で見る・履歴・登録チャンネル・おすすめ(`Target::Feed`)で、#72で入れた公開日ソートをやめ、YouTube側の返却順のまま表示する。

## 問題

`parse_target_lines`は全`Target`(検索・Feed・チャンネル・プレイリスト)に対して一律で`timestamp`降順ソートをかけていた(#72)。

yt-dlp本体(`_tab.py`)を確認すると、この`timestamp`(`--extractor-args youtubetab:approximate_date`)は`publishedTimeText`由来、つまり**動画自体の公開日**であり、プレイリストへの追加日ではない。

「後で見る」に動画を追加すると、YouTube側が`--flat-playlist`で返す生の順序(ソート前)は複数回の実行で安定しており、追加順(新しい順が先頭)に近い。tuitube側で公開日ソートをかけ直すことで、この意味のある順序を壊していた。実際に、後で見るへ追加したばかりの動画が、公開日が古いという理由で一覧の下位に埋もれる不具合が発生した。

## 対応

`parse_target_lines`で`Target::Feed(_)`の場合はソートをスキップし、YouTube側の返却順をそのまま使う。

対象外(現状維持、公開日ソートを続ける):

- `Target::Search`: YouTube検索結果ページの並び順は関連度順で、公開日順への変換自体が#72の目的
- `Target::Channel`: 元々アップロード日順寄りのタブで実害が小さい
- `Target::Playlist`: プレイリスト内の並び順(作成者が手動で並べた順)を保つ

## 対象外(今回のスコープ外)

- 動画が一覧に出てこない件(`~/.config/tuitube/hidden.toml`にIDが登録されていたことが原因。ローカル非表示機能(#51)の仕様通りの動作で、実装バグではない)
