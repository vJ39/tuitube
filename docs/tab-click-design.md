# カテゴリタブのクリック選択

Input/Resultsモードのタブ行をクリックすると、そのタブを選択する。

## 対象範囲

タブ行のRect(`search_areas(area)[1]`)は両モードで同一。クリック処理も両モード共通の1つの関数で扱う。

## 当たり判定

`ui.rs`の`tab_spans`が使っている幅消費ロジック(マーカー`TAB_MORE_LEFT`/`TAB_MORE_RIGHT`、区切り`TAB_GAP`、`grid::truncate`)を再利用し、列位置→タブindexを返す新関数を追加する(`tab_spans`とロジックが重複しないよう、共通のヘルパーに寄せる)。

- 描画に使う`visible_tabs()`の窓(`Range<usize>`)と同じ範囲だけを判定対象にする(窓の外のタブはクリックしようがない)
- 左右のマーカー(`< `/` >`)自体をクリックしても選択は変えない(スクロールは既存どおりTab/BackTabキーで行う。クリックでのスクロールはv1のスコープ外)

## Tabsへの変更

`category::Tabs`に`select(&mut self, index: usize) -> bool`を追加する(範囲外indexは無視して`false`を返す)。範囲の判定はここだけに置き、呼び出し側では持たない。

## actionsへの変更

`actions::switch_tab_with`と同じ骨格(cancel_search→store_to_tab→選択変更→sync_from_tab→未読み込みなら検索起動)で、`forward: bool`の代わりに`index: usize`を受け取る`select_tab_with`を追加する。

選択中のタブを押し直したときは、そのタブがまだ読み込めていない(cookieが無くて断られた等)かつ検索が走っていないときだけ検索を投げ直す。Inputモードでは`r`が検索語になるため、クリック以外に取り直す手段が無いため。読み込み済みのタブや検索中のタブでは何もしない。

## マウス処理

`input::handle_mouse`の先頭にある早期return(`if app.mode != Mode::Playing { return; }`)を、Input/Resultsモードでもタブクリックを処理するよう変更する。

- `MouseEventKind::Down(MouseButton::Left)`のみ処理する(Drag/Upは無視)
- クリック行(row)がタブ行と一致し、かつ列(column)が当たり判定に該当するタブがあれば`select_tab_with`を呼ぶ
- 既存の`mouse_is_ignored_outside_playing_mode`テストは、Input/Resultsでもタブクリックだけは処理されるよう前提を見直す

## 当たり判定と描画のずれ

当たり判定は直前に描いた寸法(`app.screen`)と窓(`Tabs::window`)で行う。`main.rs`のイベントループは1回の描画につき`EVENT_DRAIN_LIMIT`件までまとめて捌くので、Resizeを受けたらそこで束を区切り、描き直してから続きのイベントを捌く。

## サムネイル再取得

`main.rs`の`handle_key_event`がキー起因のタブ切替後にサムネイル再取得(`start_thumbnails_with`)をトリガーしているのと同様に、マウス起因のタブ切替後も同じトリガーが必要か確認し、必要なら`handle_event`のマウス分岐にも同じラップを追加する。
