# 再生のバックグラウンド化+隅っこミニプレイヤー

`Mode::Playing` を抜けても `session.player`(mpv本体)を止めず、検索/結果一覧の操作を続けられるようにする。`embedded`/`text` 表示中は、抜けている間だけ画面の隅に小さく映像を出す。

## 前提の確認

- `session.player`/`app.video`/`app.playback` は `app.mode` と無関係に生きている。`poll_player`/`apply_property`(ポーリング・位置更新)は `app.mode` を見ておらず、モードを移ってもそのまま動き続ける
- `video::encode` は毎フレーム、渡された `area` へカーソル移動してから mpv のフレームを描き直す(位置は tuitube 側が決めている。mpv は解像度だけを見る)。そのため映像の置き場所は「今回どの `area` を渡すか」だけで決まり、mpv 自体に位置は伝えていない
- 解像度(置き場所のセル数)を変えるのは既存の `mpv::resize_video`/`resize_text_video`(embedded/text 用。端末リサイズで既に使っている)。`window` は mpv が別ウィンドウを持つので対象外
- 置き場所を動かす・消す・小さくする操作は、古い位置の残像を消すために `session.owe_clear = true` + `app.thumbs.mark_dirty()` が要る(`end_playback` が既にこの組で行っている。`a=d` は置いてある画像を全部消す命令で、サムネイルも巻き込むため)

## 対象

- `Mode::Playing` から検索側へ戻る操作: 新規
- 戻った後の検索/結果一覧の操作: 既存のハンドラがそのまま動く(`app.mode` だけで振り分けているので、モードを検索側へ戻せば自動的に使えるようになる)
- `window` 表示中は元から別ウィンドウなので、戻ってもそのまま流れる。追加の映像描画は不要
- `embedded`/`text` 表示中は、戻っている間だけ結果一覧の右上に小さく映像を出す

## モード遷移

新しいキー:

- `Mode::Playing` で `b`: バックグラウンドへ。`app.channel.is_some()` なら `Mode::Channel`、`app.results.is_empty()` なら `Mode::Input`、それ以外は `Mode::Results`(`end_playback` の戻り先選びと同じ基準)。`session.player`/`app.video`/`app.playback` はそのまま
- `Mode::Results` / `Mode::Channel` で `b`: 前面へ。バックグラウンド中(`app.background`)でなければ何もしない
- `Mode::Input` で `Ctrl+B`: 前面へ(同上)。素の `b` は検索語の文字なので使えない(`Ctrl+S` が設定を開くのと同じ理由)

`App` に `background: bool` を追加する(既定 `false`)。バックグラウンドへ移ると `true`、前面へ戻ると `false`、`end_playback`(自然終了・エラー)でも `false` に戻す。

どちら向きの遷移でも:

1. `app.mode` を書き換える
2. 新しい置き場所(`ui::video_target_area(app, app.screen)`。下記)を求め、`app.video` があれば `video.resize(geometry)` を呼んでから、`embedded` なら `mpv::resize_video`、`text` なら `mpv::resize_text_video` を送る(`window` は何もしない。既存の `apply_resize_with` と同じ分岐)
3. `session.owe_clear = true`、`app.thumbs.mark_dirty()`

## 映像の置き場所

`ui.rs` に追加:

```rust
/// 結果一覧の右上に切り出す、隅のミニプレイヤー用の矩形。
pub fn mini_video_area(screen: Rect) -> Rect {
    let results = search_areas(screen)[2];
    let cols = MINI_VIDEO_COLS.min(results.width / 2).max(1);
    let rows = MINI_VIDEO_ROWS.min(results.height).max(1);
    Rect::new(results.right().saturating_sub(cols), results.y, cols, rows)
}

/// 今のフレームで映像をどこに描くか。無ければ None (window 中・映像が無い)。
pub fn video_target_area(app: &App, screen: Rect) -> Option<Rect> {
    app.video.as_ref()?;
    if app.mode == Mode::Playing {
        return Some(video_area(screen));
    }
    match app.display {
        DisplayMode::Window => None,
        DisplayMode::Embedded | DisplayMode::Text => Some(mini_video_area(screen)),
    }
}
```

`MINI_VIDEO_COLS` / `MINI_VIDEO_ROWS` は固定値(例: 32 / 10)とし、設定項目にはしない。

`main.rs` の `run()` ループは、今 `ui::video_area(app.screen)` を固定で渡している箇所を `ui::video_target_area(app, app.screen)` に差し替える。

`present_video` は貼ってあるサムネイルを剥がす `session.owe_clear` の処理も兼ねており、映像が無い間(設定画面など)もこの一手だけがそれを処理できる。そのため `video_target_area` が `None` でも `present_video` の呼び出し自体は毎フレーム続ける。`area` は `video_present_area` ヘルパー(`video_target_area` が `None` のとき `ui::video_area(app.screen)` にフォールバックする)を介して渡す。呼び出しごと省くと、映像が無い状態で `Mode::Settings`/`Mode::Download` を開いたときにサムネイルが剥がれず前面に残る。

`draw_search`(Input/Results/Channel共通の画面)は、`ui::video_target_area(app, frame.area())` が `Some` を返す間(=このモードに来る時点で `Mode::Playing` ではないので、必ずミニの方)、結果一覧の矩形(`search_areas` の3番目)からその幅ぶんを右側に切り取ってから `grid::layout`/`draw_list` へ渡す。`grid::layout` は渡された矩形の幅で列数を決め直すだけなので、ここ以外の変更は要らない(端末リサイズで列数が変わるのと同じ仕組み)。

## 状態表示

- Results/Channel の状態行(`results_status`/`channel_status`): `app.background` の間、先頭に `▶ <再生中のタイトル>` を足す(`app.playback.title` を使う。`end_playback` まで消えないのでそのまま読める)
- Results/Channel のヘルプ行: `app.background` の間だけ `b:全画面へ` を足す。バックグラウンドでない間は出さない
- Input のヘルプ行: 同様に `Ctrl+B:全画面へ` を `app.background` の間だけ足す
- Playing のヘルプ行: `b:検索へ`(常時)

## 他機能との整合

- `start_playback`: 既に呼び出し先頭で `stop_playback(session)` している。バックグラウンド中に別の動画を選んで Enter を押すと、今の再生を止めて新しい方に差し替わる(想定どおり)
- `end_playback`: `app.background = false` を追加する以外は変更なし。`enter_search_mode` はバックグラウンド中でも既にいるモードへ書き戻すだけなので、既存の分岐(Settings/Download 中は戻り先だけ更新)もそのまま安全
- 終了確認(`confirm_quit`、#46): `y`/`Enter` で終了する前に `stop_playback(session)` を呼ぶ(Ctrl+C の即終了と同じ扱い)。バックグラウンド中に `q` で確認 → `y` としたとき、再生を持ったまま終了しないようにする

## 対象外(v1)

- マウスでの前面化/背景化
- ミニプレイヤーのサイズ・位置の設定項目化(固定値のみ)
- ミニプレイヤー表示中のシーク・音量操作(前面へ戻ってから操作する)
- 複数動画の同時バックグラウンド再生(常に1本まで)
- `window` 表示中の追加インジケータ(状態行の `▶ <タイトル>` はそのまま出るので、それ以上は作らない)
