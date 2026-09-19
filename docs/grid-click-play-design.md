# サムネイルグリッドのクリック再生

Resultsモードでグリッド表示(`LayoutMode::Grid`)中に、サムネイルまたはタイトル部分をクリックすると、その項目を選択し即座に再生を開始する。

## 対象範囲

グリッド表示のみ(v1)。リスト表示(`LayoutMode::List`)はスクロール位置をratatui内部に任せておりクリック→index変換が別立てで必要になるため対象外とする。

## 当たり判定

`ui::grid_layout(app, cell_size())`が返す`layout.cells`(描画・選択移動(`move_selection`)と共通の割り付け)を使う。`tab_at_point`と同型の`result_at_point`関数を新設し、`cell.image`/`cell.title`/`cell.meta`いずれかにクリック座標が含まれるセルを探し、ヒットした添字`i`に`layout.offset`を足して結果indexを返す。

## マウス処理

`input::handle_mouse`の`Mode::Results`を専用ハンドラに分離する(現状`Mode::Input | Mode::Results`が同じ`handle_mouse_tabs`を共有しており、結果領域のクリックは扱われていない)。

- `MouseEventKind::Down(MouseButton::Left)`のみ処理
- 先にタブ行クリック(`ui::tab_at_point`)を試し、当たらなければ`result_at_point`を試す(タブクリックの既存動作を壊さない)
- `result_at_point`が当たれば`app.selected`を更新し、`actions::start_playback(app, tx, session).await`を呼ぶ
- `Mode::Input`のマウス処理は変更しない(従来どおりタブクリックのみ)

## 入れ替わった結果へのクリック

検索結果が入れ替わると`App::set_results`が`selected`/`scroll`を0に戻すため、入れ替わる前の画面を狙ったクリックが、同じ座標で別の動画を指す。`handle_batch`は1回の描画に対して最大64件のイベントをまとめて捌くので、検索完了イベントの後ろに並んだクリックがこれに当たる。

`App`に世代番号を2つ持たせて、描いた画面と今の結果が一致するときだけ再生する。

- `results_generation`: `sync_from_tab`(結果を入れ替える唯一の経路)で加算
- `drawn_generation`: `terminal.draw`の直後に`App::mark_drawn`で`results_generation`を写す
- `results_click`は`App::results_are_drawn`が偽の間、セルのクリックを捨てる

タブ行のクリックは結果の入れ替わりで位置が変わらないので、この判定の対象外とする。

## 対象外

- リスト表示でのクリック再生
- ダブルクリック等、Left単クリック以外の入力
- 入力モード(`Mode::Input`)でのクリック再生。同じグリッドを描いているがクリックはタブ行だけを見る
