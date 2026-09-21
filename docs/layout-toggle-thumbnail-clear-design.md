# vキーでgrid→list切替時のサムネイル残存(#75)

`v`キー(#74)でgridからlistへ切り替えると、Kitty画像(サムネイル)が消えずにリストのテキストと重なって表示される不具合。

## 原因

Kitty画像はratatuiの`Buffer`とは別レイヤーで、端末に一度貼ったら明示的な`a=d`クリア命令(`video::encode_clear`)を送らない限り残り続ける(#61/#66と同じ構造)。`toggle_search_layout`(#74)は`app.thumbs.mark_dirty()`を呼んでいなかったため、`present_thumbs`が起動されず、古い画像がそのまま残っていた。

## 修正

`toggle_search_layout`の保存成功時に`app.thumbs.mark_dirty()`を呼ぶ。次フレームの`present_thumbs`が`take_dirty()`で拾い、`encode_clear`で`a=d`を送ってから、`grid_layout_in`(list設定では`None`を返す)を見て貼り直すかどうかを決める。

- grid → list: `grid_layout_in`が`None`になるため、クリアだけが送られ画像は残らない。
- list → grid: クリア後に通常のサムネイル貼り付けが再度走り、見た目は変わらない(副作用なし)。
