# アプリ終了時の確認(Y/N)

`q` でアプリを終了する経路に、誤操作防止の確認を挟む。`Ctrl+C` は今まで通り即座に終了する(強制終了の逃げ道として残す)。

## 対象

現状 `app.should_quit = true` にしている経路のうち、以下の3つを確認ありに変える:

- Input モード: `Esc`(結果が0件のときだけ終了する経路)
- Results モード: `q`
- Channel モード: `q`

対象外(即終了のまま): `Ctrl+C`(グローバル)。確認ダイアログが固まった/操作を忘れた場合の逃げ道を必ず残す。

Playing モードは #45 の修正で `q` が再生を止めるだけになり、アプリを直接終了する経路を持たない(いったん Results へ戻ってから `q` を押す形になるので、そちらで確認が挟まる)。Settings / Download 画面の `q`/`Esc` はどちらも画面を閉じるだけでアプリは終了しないため対象外。

## 状態

新しい `Mode` は増やさない。`App` に `confirm_quit: bool` を追加するだけ(既定 `false`)。画面はどのモードでもそのまま出し続け、下段2行(`draw_footer` が描く status/help 行)だけを差し替える。この2行は Playing モードでも映像の矩形とは重ならないので、映像を消さずに出せる。

## キー処理

`input::handle_key` の先頭、`Ctrl+C` の判定の直後に分岐を足す。`confirm_quit` が立っている間は他のキーを一切無視し、以下だけを見る:

- `y` / `Y` / `Enter`: `app.should_quit = true`、`app.confirm_quit = false`
- `n` / `N` / `Esc`: `app.confirm_quit = false`(取り消し。他には何もしない)
- それ以外: 何もしない(状態はそのまま)

マウスも `handle_mouse` の先頭で `confirm_quit` を見て、立っている間は何もしない。

## 描画

`draw_footer` の先頭で `app.confirm_quit` を見る。立っていれば、モード別の通知・エラー・ヒントより優先して:

- status 行: `終了しますか？ (y/N)`(エラーと同じ赤で目立たせる)
- help 行: `y/Enter:終了  n/Esc:キャンセル`

## 対象外(v1)

- マウスクリックでの Y/N 選択
- 枠線付きの中央ダイアログ等、見た目の作り込み(status/help 行への文字表示のみ)
