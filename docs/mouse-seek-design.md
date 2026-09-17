再生中(`Mode::Playing`)にマウスで再生位置をシークできるようにし、シーク先をプログレスバーで見せるための設計。実装は TDD(Red→Green)で進める前提で、外部プロセス(mpv・端末)を伴わない純粋関数の単位を先に切り出す。

対象バージョン: mpv 0.41.0 / ratatui 0.29 / crossterm 0.28 / 実機端末 iTerm2。映像は Kitty graphics protocol 方式(`docs/kitty-protocol-design.md`)。

## 0. 合意済み仕様

1. 再生中にマウスでシークできる
2. マウスホバー時、または ←→ でシークした時に、シーク先の位置をプログレスバーで見せる
3. バーはステータス行の上に幅一杯の 1 行。例 `███████████████░░░░░░░░░░░░░░░░░░░░  01:23 / 34:05`
4. バー上でクリックした位置を時刻に変換して絶対シーク。ドラッグ中は表示だけ更新し、離した時に 1 回だけシークする
5. ホバー中はラベルを「再生位置」から「ポインタ位置の時刻」に一時的に切り替える
6. ←→ シークは送信時にローカルの時刻表示を先行更新し(optimistic update)、後続のポーリングで確定値に上書きする

## 1. 実装が依存する事実

設計の根拠になる挙動を先に固定する。mpv の項目は 2026/09/18 に mpv 0.41.0 を `--ao=null --vo=null --input-ipc-server` で起動し、IPC を直接叩いて確認した実測。crossterm / ratatui の項目はローカルの cargo registry のソースを読んだもの。

### 1-1. mpv JSON IPC の `seek`

| 項目 | 事実 | 設計への影響 |
|---|---|---|
| コマンド形式 | `{"command":["seek",<target>,"<flags>"]}`。flags は文字列で `absolute` / `absolute+exact` など `+` 連結。省略時は `relative` | 絶対シークは `["seek", 秒, "absolute"]`。既存の `seek(seconds)` は相対のまま残す |
| target の型 | 整数(`50`)・小数(`123.5`)・数字文字列(`"30"`)のいずれも `error: success` | `f64` をそのまま JSON 数値で送る |
| 応答 | `{"request_id":N,"error":"success","data":null}`。続けてイベント `{"event":"seek"}` → `{"event":"playback-restart"}` が届く | 現行の `parse_response` は request_id の無い行を None で捨てるため、イベントは今は読めない(§2-6 で任意対応) |
| 絶対シークの既定モード | `exact`(正確・遅い)。相対シークの既定は `keyframes` | クリック着地はほぼ target 通り。←→ の着地は target から数秒ずれうる(§2-5 の許容幅の根拠) |
| **負の絶対値** | `seek -5 absolute` は「末尾から 5 秒」に飛ぶ(実測: duration 62.09 → time-pos 55.49) | 送る前に必ず 0 以上へクランプする |
| **duration 以上への絶対シーク** | `--keep-open` 無しでは `end-file` が出て mpv が終了する(tuitube の起動引数は keep-open 無し)。`--keep-open=yes` なら `time-pos = duration`・`pause = true`・`eof-reached = true` で止まる | バー右端をクリックすると再生が終わってしまう。送る前に `duration - マージン` へクランプする(§2-4) |
| seek 直後の `get_property time-pos` | `playback-restart` が来る前に返ると旧位置(または中間値)を返す | 先行更新した表示を、直後のポーリング 1 回で古い値に戻されないようにする(§2-5) |
| 連投 | 絶対シーク 5 本を続けて送ると全て `success`、`seek` イベントが本数分、`playback-restart` は最後に 1 回 | 連投しても壊れないが、ネットワーク再生では 1 回ごとにデマクサが再要求するので、ドラッグ中は送らない(仕様 4) |
| `seeking` プロパティ | シーク処理中 true | 今回は使わない(ポーリング周期 1 秒より短い時間で終わることが多く、拾えない) |

### 1-2. crossterm 0.28 のマウス

| 項目 | 事実 | 設計への影響 |
|---|---|---|
| 有効化 | `execute!(stdout, EnableMouseCapture)` が `CSI ?1000h ?1002h ?1003h ?1015h ?1006h` を書く。`ratatui::init()` は raw mode と alt screen だけで、マウスは触らない | `ratatui::init()` の後で自分で有効化し、`ratatui::restore()` の前で `DisableMouseCapture` を書く |
| `?1003h`(any-event) | ボタンを押していない移動も `MouseEventKind::Moved` として毎セル届く | ホバー(仕様 5)はこれで実現。イベントが多いので 1 回の描画にまとめる(§3-6) |
| 座標 | `MouseEvent { kind, column, row, modifiers }`。SGR 応答の 1 始まりを crossterm が `-1` して **0 始まり**で返す | ratatui の `Rect`(0 始まり)とそのまま比較できる |
| 種別 | `Down(button)` / `Up(button)` / `Drag(button)` / `Moved` / `ScrollUp` 等。端末によっては `Up`/`Drag` のボタン種別が取れず `Left` が入る(`parse_cb` が button 3 を `Up(Left)` に落とす) | `Up` も含めて `Left` だけ扱う。種別不明の `Up` は `Left` で届くので取りこぼさず、左ドラッグ中の右クリックでシークが確定する事故も防げる |
| panic 時 | ratatui の panic hook は `restore()`(raw mode 解除・alt screen 離脱)だけ | マウス追跡が残るとシェルに戻ってもポインタ移動でゴミが出る。hook を包んで `DisableMouseCapture` を足す(§3-5) |
| mpv 側の `?1003h` / `?1003l` | `--vo=kitty` の mpv は起動時に `?1003h`、終了時に `?1003l` を書くが、出力先は stdout パイプで `ApcParser` が CSI として捨てる | 実端末には届かない。mpv の終了で tuitube のマウス追跡が切れることはない |

### 1-3. ratatui 0.29

| 項目 | 事実 | 設計への影響 |
|---|---|---|
| `Gauge` | ラベルは常にバーの**中央**に描かれ、ラベル部分は前景/背景を反転した空白になる。空ラベルでも境界条件 `x > label_col + 0` により 1 セルが反転空白になる(穴が開く)。未充填部は空白のみ | 仕様 3 の見た目(右端ラベル・`░` の未充填・ホバー印)は出せない。カスタム描画にする(§2-3) |
| `LineGauge` | ラベルは**左**、バーは細線(`─`)。1 行固定 | 同上 |
| テスト手段 | `Buffer::empty(area)` に `Widget::render` して `Buffer::with_lines([...])` と比較できる。`TestBackend` も使える | バーの描画は端末なしでテストできる |
| `Terminal::draw()` の戻り | `CompletedFrame.area` は今描いた画面サイズ | マウスの当たり判定に使う画面サイズはここから取る(リサイズ直後も描いたものと一致) |
| 幅計算 | `Span::width()` が表示幅(全角 2)を返す。`ui::cursor_x` で既に使っている | ラベル幅の計算に使う |

## 2. 設計判断

### 2-1. 使われ方

機能一覧の前に、誰がどう触るかを並べる。ここから出てくる要件は §2-2 以降に落とす。

| 場面 | 起きること | 要件 |
|---|---|---|
| 視聴中に「今どこ、あとどれくらい」 | バーを見る | バーは再生中ずっと表示する(ホバー時だけ出さない)。行を固定すると当たり判定も安定する |
| 「あの辺に飛びたい」 | ポインタを当てて時刻を確かめ、クリック | ホバー中はラベルがポインタ位置の時刻になる。クリックで 1 回だけ絶対シーク |
| 「少しずつ探りたい」 | 押したまま左右に動かし、良い所で離す | ドラッグ中は印とラベルだけ動く。離した位置で 1 回シーク。YouTube のストリームは 1 回のシークに数百 ms〜数秒かかるので連続送信しない |
| ドラッグしたまま行の上下に外れた | 端末は `Drag` を返し続ける | 行から外れても取り消さない。列だけ見てトラック端に吸着させる |
| ドラッグ中にポインタを端末ウィンドウ外へ出して離した | `Up` が届かない場合がある。次に届くのはボタン無しの `Moved` | `Moved` が来た時点でボタンは離れているので、ドラッグを**シークせずに取り消す**(どこで離したか分からない) |
| キーボード派が ←→ を押す・押し続ける | 5 秒刻みの相対シークが連続で送られる | 押した回数分だけ表示が先に進む(基準は直前のシーク目標)。ホバー表示が残っていたらキー操作を優先して消す |
| バー右端をクリック | duration ちょうどに飛ぶと mpv が終了する(§1-1) | 送る前に `duration - 1 秒` へ丸める |
| 音声のみ・ライブ配信 | `duration` が取れない(None) | バーは未充填のまま、ラベルは `--:--:-- / --:--:--`。ホバーしても印を出さず、クリックしても何もしない |
| 端末が狭い | ラベルすら入らない | ラベルを右端で切る。トラック幅 0 ならシーク不可。落ちない |
| 端末の文字をドラッグ選択したい | マウスキャプチャ中は端末側の選択が効かない(端末共通) | 制限として明記(§6)。iTerm2 は Option を押しながらで選択できるか実機確認(§4-5) |
| 検索画面・結果一覧でクリック | 今回は対象外 | イベントは受け取るが `Mode::Playing` 以外は捨てる |

### 2-2. 状態の置き場所

| 状態 | 置き場所 | 理由 |
|---|---|---|
| ホバー列・ドラッグ列 | `App.seek_bar: SeekBarState`(画面絶対列 `u16`) | `ui::draw(frame, &App)` が読むので App 側。秒でなく**列**で持つ。印の描画はそのまま列に置け、秒への変換は確定時に 1 回だけ行えばよい(秒で持つと描画時に逆変換が要り、duration 未取得時の扱いも増える) |
| 直近に描いた画面サイズ | `App.screen: Rect` | マウスイベントは描画の合間に届く。当たり判定は「ユーザーが今見ている画面」の割り付けで行うべきで、それは直前の `terminal.draw()` が返した `CompletedFrame.area`。`crossterm::terminal::size()` を都度聞くとリサイズ直後に描画とずれる |
| 先行更新中のシーク | `Playback.pending_seek: Option<PendingSeek { target, sent_at }>` | 再生位置の一部。`end_playback` で `Playback::default()` に戻るので後始末が要らない |
| バーの割り付け(トラック矩形・ラベル矩形) | 状態として持たない。`ui::seek_bar_layout(&App)` で `App.screen` と duration から毎回計算 | 純粋関数。描画とヒットテストが同じ関数を通るので食い違わない |

`Session` には何も足さない。マウスは player の生死に関係なく処理し、送信だけが `send_to_player` で飛ばされる(キー処理と同じ)。

### 2-3. バーの描画方法

| 案 | 内容 | 判定 |
|---|---|---|
| `Gauge` | 中央ラベル・未充填は空白。空ラベルにしても 1 セル穴が開く(§1-3) | 不採用 |
| `LineGauge` | 左ラベル・細線 | 不採用 |
| `Gauge` + 別 `Paragraph` でラベル | バー部分は Gauge、右にラベルを別描き。穴の問題は残り、ホバー印は描けない | 不採用 |
| **カスタム `Widget`(採用)** | `seekbar::SeekBar` が `Widget` を実装。トラックを `█`(充填)/`░`(未充填)/`┃`(ホバー・ドラッグの印)で描き、右にラベル | 見た目を仕様通りにできる。当たり判定と同じ `SeekBarLayout` を使うので列がずれない。`Buffer` 比較でテストできる |

行の割り付け: `[トラック][空白 2][ラベル(固定幅)]`。ラベル幅は **duration だけ**から決める(`"mm:ss / mm:ss"` なら 13 桁、`"h:mm:ss / h:mm:ss"` なら 17 桁)。再生位置側の文字列は duration 側と同じ桁形式に揃える(duration に時間桁があれば位置も `0:01:15`)。これで再生中にトラック幅が伸縮しない。

左側が予約幅を食い破るとラベルが右端で切られ、duration が消える。塞ぐ規則は 3 つ:

- duration が不明(ライブ・音声のみ)なら再生位置が 1 時間を越えうるので、時間桁ぶんを確保する(`--:--:-- / --:--:--` の 19 桁)。
- duration に時間桁が無いとき、末尾で再生位置が 1 時間を越えても分へ繰り上げて `60:00` と出す(`1:00:00` にしない)。
- 再生位置が未取得のときの伏せ字は duration と同じ桁数にする(`1:01:01` なら `-:--:--`)。

充填セル数は `floor(time_pos / duration × トラック幅)`、`time_pos ≥ duration` で全幅。列→秒は**セル左端**基準 `(列 - track.x) / track.width × duration`。左端セルは 0 秒、右端セルは `duration × (1 - 1/幅)` になり duration に届かない。充填が floor なので「列 c をクリック → 充填が c の直前まで伸びる」で描画と一致する。

ラベルの内容と色:

| 状態 | ラベル左側 | 色 |
|---|---|---|
| 通常 | `playback.time_pos`(先行更新中はその目標値) | Cyan(ステータス行と同じ) |
| ホバー中 | ポインタ列の時刻 | Yellow |
| ドラッグ中 | ドラッグ列の時刻 | Yellow |

優先順位はドラッグ > ホバー > 再生位置。ステータス行の `01:23 / 34:05` はそのまま残す(ステータス行は再生状態、バーはシーク操作の表示と役割が分かれる。既存テストも触らない)。

### 2-4. シーク目標のクランプ

送信直前に `clamp_target(target, duration)` を必ず通す。

```
clamp_target(t, Some(d)) = t.clamp(0.0, (d - SEEK_END_MARGIN_SECS).max(0.0))
clamp_target(t, None)    = t.max(0.0)
SEEK_END_MARGIN_SECS = 1.0
```

- 下限 0: 負値は「末尾から」の意味になる(§1-1)。
- 上限 `duration - 1 秒`: duration 以上で mpv が終了する(§1-1)。1 秒残せばポーリング 1 回分は末尾の表示が出て、その後は自然に再生終了する。左端基準の列→秒変換では右端セルでも duration に届かないが、duration の丸めや相対シークの積み上げがあるので送信側でも丸める。
- 代替案 `--keep-open=yes` を起動引数に足す: 末尾で一時停止して止まるようになるが、今の「再生が終わったら結果一覧に戻る」動きが変わる(Esc を押すまで止まったまま)。今回のスコープ外として不採用。

相対シーク(←→)で mpv へ送るコマンドは今まで通り `["seek", ±5]`(クランプしない。末尾を越えたら今と同じく mpv が終了する)。クランプするのは**表示側**の目標値だけ。

### 2-5. optimistic update と確定値の整合

問題: ポーリングは 1 秒周期で、シーク直後の 1 回目は旧位置を返しうる(§1-1)。素朴に「送信時に `time_pos = target`、次のポーリングで上書き」とすると、表示が target → 旧位置 → 新位置 と往復する。

| 案 | 内容 | 利点 | 欠点 | 判定 |
|---|---|---|---|---|
| A. 上書き無条件 | 送信時に target を表示、次のポーリングでそのまま上書き | 最小 | 旧位置へ一瞬戻る | 不採用 |
| **B. 保持時間 + 許容幅(採用)** | 送信から `SEEK_HOLD` の間は、target から `SEEK_TOLERANCE_SECS` 以上離れた値と None を「古い」とみなして捨てる。保持時間を過ぎたら何でも受け入れる | 純粋関数でテストできる。シークが黙って失敗しても保持時間後に自然回復する | 保持時間中に本当に遠くへ着地した場合、最大 `SEEK_HOLD` だけ表示が遅れる | **採用** |
| C. `playback-restart` イベント駆動 | イベント行を解釈し、届いた時点で即ポーリング・保持解除 | 確定が速く正確 | reader の行解釈を広げる必要がある。イベントが来ない経路(mpv 異常)の保険に B が要る | B の上に任意で足す(§2-6) |

```
SEEK_HOLD = 2 秒
SEEK_TOLERANCE_SECS = 3.0
```

- `SEEK_HOLD = 2 秒`: ポーリング周期が 1 秒なので、シーク後 1 回目のポーリングは必ず 1 秒以内に来て古い値を返しうる。2 秒ならその 1 回と、ネットワーク再生で遅れたもう 1 回を覆う。これより長くすると、シークが効かなかった時に誤った表示が残る時間がそのぶん延びる。
- `SEEK_TOLERANCE_SECS = 3.0`: ←→ の相対シークは keyframes モードで着地が target から数秒ずれる。一方、1 回押しの「古い値」は target から 5 秒離れている。3 秒はその中間で、着地のずれは受け入れ、旧位置は弾く。4 秒ずれて着地した場合は保持時間の間だけ target 表示が続き、2 秒後に実値へ揃う。

`Playback` の規則:

| 操作 | 動き |
|---|---|
| `begin_seek(target, now)` | `time_pos = Some(target)`、`pending_seek = Some { target, sent_at: now }`。既に pending があれば上書き(新しい目標・新しい時刻) |
| `seek_base()` | 相対シークの基準。`pending_seek.target` があればそれ、無ければ `time_pos`。押し続けたとき 5 秒ずつ積み上がる |
| `reconcile_time_pos(polled, now)` | pending が無ければ `time_pos = polled`。pending があり、`now - sent_at < SEEK_HOLD` かつ (`polled` が None または `|polled - target| > TOLERANCE`) なら捨てる。それ以外は `pending_seek = None; time_pos = polled` |
| `end_playback` | `Playback::default()` で pending も消える(既存の動き) |

`App::apply_property(REQ_TIME_POS, data)` は `reconcile_time_pos(data.as_f64(), Instant::now())` を呼ぶ形に変える。他のプロパティは今まで通り。時刻は `std::time::Instant` を引数で渡し、テストは `Instant::now() + Duration` で任意の時刻を作る(tokio の paused clock は不要)。

### 2-6. `playback-restart` による即時確定(任意・v1.1)

`mpv.rs` に `parse_event(line) -> Option<String>`(`{"event":"..."}` の名前だけ取る)を足し、reader が `AppEvent::MpvEvent { nonce, name }` を送る。メインループは `name == "playback-restart"` で `poll_properties()` を即時に 1 回呼ぶ。B の保持時間はそのまま残す(イベントが来ない場合の保険)。v1 では実装せず、実機で表示の追従が遅いと感じたら入れる。

## 3. アーキテクチャ

### 3-1. データフロー

```
input thread: crossterm::event::read()
  Event::Mouse(m) ──▶ AppEvent::Mouse(m)                               main.rs spawn_input_reader
                          │
                          ▼
main loop: handle_event ──▶ input::handle_mouse(app, m, session)      main.rs → input.rs
  Mode::Playing 以外 → 捨てる
  mouse_input(m.kind) → Option<MouseInput>     (左ボタンと移動だけ。右/中/スクロールは None)
  layout = ui::seek_bar_layout(app)            (app.screen + duration のラベル幅)
  app.seek_bar.on_mouse(input, m.column, m.row, &layout) → Option<MouseAction>
      │ Some(Seek { column })
      ▼
  secs = layout.seconds_at(column, duration)   (duration None なら何もしない)
  actions::seek_absolute(app, session, secs, now)
      clamp_target → mpv::seek_absolute(secs) を send_to_player → playback.begin_seek(secs, now)
                          │
                          ▼
次の周: terminal.draw ──▶ ui::draw_playing ──▶ seekbar::SeekBar { filled, marker, label, hovering }.render
         app.screen = completed.area
```

キー経路(←→)は `handle_key_playing` → `seek_step(code)` → `actions::seek_relative(app, session, ±5.0, now)`:
`mpv::seek(±5)` を送信 → `playback.seek_base()` があれば `begin_seek(clamp_target(base ± 5, duration), now)` → `seek_bar.clear_hover()`。

ティッカー(1 秒)の `poll_properties` → `AppEvent::MpvProperty` → `apply_property` → `reconcile_time_pos` は既存経路のまま。

### 3-2. モジュール構成

| ファイル | 区分 | 内容 |
|---|---|---|
| `src/seekbar.rs` | 新規 | バーの**純粋な**部分。`SeekBarLayout`(行の割り付け・当たり判定・列⇄秒)、`SeekBarState`(ホバー/ドラッグの状態機械)、`MouseInput` / `MouseAction`、`clamp_target`、`label_text` / `label_width`、`SeekBar`(`Widget`)。依存は ratatui の `Rect` / `Buffer` / `Style` のみ。crossterm・tokio・mpv に依存しない |
| `src/app.rs` | 変更 | `AppEvent::Mouse(MouseEvent)`、`App.screen: Rect`、`App.seek_bar: SeekBarState`、`Playback.pending_seek` と `begin_seek` / `seek_base` / `reconcile_time_pos`。`apply_property` の `REQ_TIME_POS` 分岐を `reconcile_time_pos` 経由に |
| `src/mpv.rs` | 変更 | `seek_absolute(seconds: f64) -> MpvCommand`。(v1.1: `parse_event`) |
| `src/ui.rs` | 変更 | 再生中を 4 段 `[映像 Min(1), バー Length(1), ステータス Length(1), ヘルプ Length(1)]` に。`seek_bar_area(area)`、`seek_bar_layout(&App)`、`draw_playing` で `SeekBar` を描く。ヘルプ文に `クリック/ドラッグ:シーク` を足す |
| `src/input.rs` | 変更 | `handle_mouse(app, mouse, session)`、`mouse_input(kind)`、`seek_step(code)`。`playing_command` から Left/Right を外す(シークは先行更新を伴うので別経路) |
| `src/actions.rs` | 変更 | `seek_relative(app, session, delta, now)`、`seek_absolute(app, session, target, now)` |
| `src/main.rs` | 変更 | `ratatui::init()` 直後に `EnableMouseCapture`、panic hook を包む、終了時 `DisableMouseCapture`。`spawn_input_reader` で `Event::Mouse` を転送。`app.screen` の更新。連続イベントの取りまとめ(§3-6) |
| `src/geometry.rs` | 変更なし(テストの期待値のみ) | `video_area` が 1 行減るので `geometry_for(80, 24)` の `frame_px` は `(640, 336)` |
| `Cargo.toml` | 変更なし | crossterm の既定 feature に `events` が含まれる(`EnableMouseCapture` は `#[cfg(feature = "events")]`)。新規依存なし |

### 3-3. 型と関数(TDD の足場)

コードは書かない前提だが、テストを先に書くために境界のシグネチャを固定する。

`src/seekbar.rs`

```rust
pub const FILLED: &str = "█";
pub const EMPTY: &str = "░";
pub const MARKER: &str = "┃";
/// トラックとラベルの間の空白。
pub const LABEL_GAP: u16 = 2;
/// duration ちょうどへ飛ぶと mpv が終了するため、末尾に残す秒数。
pub const SEEK_END_MARGIN_SECS: f64 = 1.0;

/// バー 1 行の割り付け。左がトラック、右が固定幅ラベル。幅が足りなければトラック幅 0。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SeekBarLayout { pub track: Rect, pub label: Rect }

impl SeekBarLayout {
    /// row は ui::seek_bar_area() の 1 行。label_width は label_width(duration)。
    pub fn new(row: Rect, label_width: u16) -> Self;
    /// (column, row) がトラック上なら Some(column)。ラベル・空白・他の行は None。
    pub fn hit(&self, column: u16, row: u16) -> Option<u16>;
    /// ドラッグ中にトラック外へ出た列を端に吸着させる。トラック幅 0 なら track.x。
    pub fn clamp_column(&self, column: u16) -> u16;
    /// 列の左端に対応する秒。(column - track.x) as f64 * duration / track.width as f64。
    /// 乗算を先にすると整数値の答えが f64 でも正確に出る。幅 0 なら 0.0。
    /// 割り付けが変わる前の古い列が来るので、換算前に clamp_column でトラック内へ吸着させる。
    pub fn seconds_at(&self, column: u16, duration: f64) -> f64;
    /// 充填セル数。floor(time_pos / duration * width)、time_pos >= duration で width。
    /// どちらかが None、または duration <= 0 なら 0。
    pub fn filled_cells(&self, time_pos: Option<f64>, duration: Option<f64>) -> u16;
}

/// 送信直前の丸め(§2-4)。
pub fn clamp_target(target: f64, duration: Option<f64>) -> f64;

/// ラベル幅。duration だけで決まり、再生中に変わらない。
pub fn label_width(duration: Option<f64>) -> u16;
/// "01:23 / 34:05"。shown は duration と同じ桁形式に揃える(時間桁があれば "0:01:15")。
pub fn label_text(shown: Option<f64>, duration: Option<f64>) -> String;

/// crossterm の MouseEventKind から、この機能が見る分だけを取り出したもの。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseInput { Move, Press, Drag, Release }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseAction { Seek { column: u16 } }

/// ホバー列とドラッグ列。どちらも画面絶対列。
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
pub struct SeekBarState { pub hover: Option<u16>, pub drag: Option<u16> }

impl SeekBarState {
    /// 状態遷移(§3-4)。シークすべきときだけ Some。
    pub fn on_mouse(&mut self, input: MouseInput, column: u16, row: u16, layout: &SeekBarLayout) -> Option<MouseAction>;
    /// ラベルと印に使う列。drag があれば drag、無ければ hover。
    pub fn shown_column(&self) -> Option<u16>;
    /// キーでシークしたときに呼ぶ。drag は触らない。
    pub fn clear_hover(&mut self);
}

/// 描画。ui.rs が App から組み立てる。
pub struct SeekBar<'a> {
    pub layout: SeekBarLayout,
    pub filled: u16,
    pub marker: Option<u16>,
    pub label: &'a str,
    /// ホバー/ドラッグ中はラベルの色を変える。
    pub highlighted: bool,
}
impl Widget for SeekBar<'_> { fn render(self, area: Rect, buf: &mut Buffer); }
```

`src/app.rs`

```rust
pub enum AppEvent { /* 既存 */ Mouse(crossterm::event::MouseEvent), }

pub struct App {
    /* 既存 */
    /// 直近の terminal.draw() が描いた画面。マウスの当たり判定はこれで割り付ける。
    pub screen: Rect,
    pub seek_bar: SeekBarState,
}

/// シーク送信から確定までの先行表示。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PendingSeek { pub target: f64, pub sent_at: std::time::Instant }

pub const SEEK_HOLD: Duration = Duration::from_secs(2);
pub const SEEK_TOLERANCE_SECS: f64 = 3.0;

#[derive(Debug, Default, Clone)]
pub struct Playback { /* 既存 */ pub pending_seek: Option<PendingSeek> }

impl Playback {
    pub fn begin_seek(&mut self, target: f64, now: Instant);
    pub fn seek_base(&self) -> Option<f64>;
    pub fn reconcile_time_pos(&mut self, polled: Option<f64>, now: Instant);
}
```

`src/mpv.rs`

```rust
/// {"command":["seek",<秒>,"absolute"]}。呼び出し側で clamp_target 済みの値を渡す。
pub fn seek_absolute(seconds: f64) -> MpvCommand;
```

`src/ui.rs`

```rust
fn playing_areas(area: Rect) -> [Rect; 4];
pub fn video_area(area: Rect) -> Rect;      // [0]
pub fn seek_bar_area(area: Rect) -> Rect;   // [1]
pub fn status_area(area: Rect) -> Rect;     // [2]
pub fn help_area(area: Rect) -> Rect;       // [3]
/// 描画とヒットテストが共有する割り付け。
pub fn seek_bar_layout(app: &App) -> SeekBarLayout;
```

`src/input.rs`

```rust
pub async fn handle_mouse(app: &mut App, mouse: MouseEvent, session: &mut Session);
/// 左ボタンと移動だけ。それ以外は None。
fn mouse_input(kind: MouseEventKind) -> Option<MouseInput>;
/// Left → -5.0、Right → 5.0。
fn seek_step(code: KeyCode) -> Option<f64>;
```

`src/actions.rs`

```rust
pub const SEEK_STEP_SECS: f64 = 5.0;
pub async fn seek_relative(app: &mut App, session: &mut Session, delta: f64, now: Instant);
pub async fn seek_absolute(app: &mut App, session: &mut Session, target: f64, now: Instant);
```

### 3-4. マウス状態遷移

`SeekBarState::on_mouse` の規則。列は `layout.hit` / `layout.clamp_column` で判定する。

| 入力 | drag 無し | drag 有り |
|---|---|---|
| `Move` | `hover = hit(col, row)` | ボタンが離れているのに `Up` を受け取れなかった(ウィンドウ外で離した)。**drag を捨て、シークしない**。`hover = hit(col, row)` |
| `Press` | `hit` なら `drag = Some(col)`。外なら何もしない | 二重押し。`hit` なら列を更新、外なら無視 |
| `Drag` | 押し始めがバー外だったドラッグ。無視 | `drag = Some(clamp_column(col))`。行は見ない(行から外れても続く) |
| `Release` | 無視 | `column = clamp_column(col)` で `Some(Seek { column })` を返し、`drag = None`、`hover = hit(col, row)` |

`clear_hover()` は `hover = None` だけ。キーシークで呼ぶ(§2-1)。

### 3-5. マウスキャプチャの有効化と解除

`main()`:

```
mpv::sweep_stale_sockets();
let mut terminal = ratatui::init();           // raw mode + alt screen + ratatui の panic hook
enable_mouse_capture()?;                       // execute!(stdout(), EnableMouseCapture)
install_mouse_panic_hook();                    // 既存 hook を take_hook で包み、先に DisableMouseCapture を書く
let result = run(&mut terminal).await;
disable_mouse_capture();                       // 失敗しても続ける
ratatui::restore();
```

- 順序: `EnableMouseCapture` は alt screen に入った後(`init()` の後)。`DisableMouseCapture` は alt screen を出る前(`restore()` の前)。逆にすると通常画面側に有効化が残る。
- panic hook: `ratatui::init()` が自分の hook を `set_hook` するので、その**後**に `take_hook()` で受け取って包む。包んだ hook は `DisableMouseCapture` を書いてから元の hook(`restore()`)を呼ぶ。
- 書き込み先は `std::io::stdout()`。ratatui のバックエンドも同じ `Stdout` に書くが、`draw()` の外で書くので割り込みにならない(kitty 設計 §1-3 と同じ理由)。

### 3-6. メインループの変更

1. `let completed = terminal.draw(|frame| ui::draw(frame, &app))?; app.screen = completed.area; let area = ui::video_area(completed.area);` — 既存の 1 行を分けるだけ。
2. `handle_event` に `AppEvent::Mouse(m) => handle_mouse(app, m, session).await` を足す。
3. `spawn_input_reader` で `Ok(CrosstermEvent::Mouse(m)) => tx.send(AppEvent::Mouse(m))`。フィルタは入れない(判断は `input::mouse_input` に寄せる)。
4. 連続イベントの取りまとめ: `rx.recv()` で 1 件処理した後、`rx.try_recv()` で溜まっている分を最大 `EVENT_DRAIN_LIMIT = 64` 件まで続けて処理してから描画に進む。`?1003h` の `Moved` はポインタが 1 セル動くごとに届くので、掃引 1 回で数十件になる。1 件ごとに `terminal.draw()` すると描画が追いつかない。64 は一般的な端末幅を一度に横切る程度の数で、ここで区切れば描画が最低それごとに 1 回入る。`VideoFrame` 等の他イベントも同じ列に並ぶが、どれも状態を更新するだけで描画は次の 1 回にまとまるので問題ない。

### 3-7. ライフサイクル別の動き

| 場面 | 動き |
|---|---|
| 再生開始 | `Playback::default()`(pending 無し)、`seek_bar` は前回の値が残りうるので `start_playback` で `SeekBarState::default()` に戻す。duration が届くまでバーは空・ラベル `--:--:-- / --:--:--` |
| ホバー | `Moved` → `hover` 更新 → 次の描画でラベルが黄色のポインタ時刻に。バーの外へ出れば元に戻る。端末ウィンドウの外へ出ると `Moved` が来ないので最後の位置が残る(制限 §6) |
| クリック | `Down` → `drag = Some`、`Up` → `Seek` → `seek_absolute` → 送信 + `begin_seek`。次の描画で充填が飛び先まで伸び、ラベルは黄色(まだ hover が乗っている)から、ポインタを外すと Cyan の再生位置に戻る |
| ドラッグ | `Drag` ごとに印とラベルだけ動く。`Up` で 1 回だけ送信 |
| ←→ | `seek_relative`: 送信 → `begin_seek(base ± 5)` → `clear_hover`。表示は即座に進む。1〜2 秒後のポーリングで実値に揃う(keyframes モードのずれ込み) |
| ポーリング | `reconcile_time_pos`。保持時間中の古い値・None は捨てる |
| 端末リサイズ | 既存の `apply_resize`。`video_area` が 1 行減っているだけで、mpv に渡す `vo-kitty-rows` も自動で追従する(`geometry_for` が `ui::video_area` を使う)。`app.screen` は次の `draw()` で更新される |
| 再生終了・異常終了 | `end_playback` → `Playback::default()`。`seek_bar` も default に戻す。マウスキャプチャは tuitube 終了まで有効のまま(再生中以外は捨てる) |
| tuitube 終了 | `disable_mouse_capture()` → `restore()`。Ctrl-C も同じ経路(`should_quit`)。panic は包んだ hook |
| duration が無い(ライブ・音声のみで取れない) | `seconds_at` を呼ぶ前に `duration` を見て何もしない。印も出さない(シークできるように見せない)。`filled_cells` は 0。相対シークの表示更新は `clamp_target(t, None)` で下限だけ丸める |

## 4. テスト戦略

### 4-1. テスト可能単位

| 単位 | 種別 | 外部依存 |
|---|---|---|
| `seekbar::SeekBarLayout`(割り付け・当たり判定・列⇄秒・充填) | ユニット | なし |
| `seekbar::clamp_target` / `label_text` / `label_width` | ユニット | なし |
| `seekbar::SeekBarState::on_mouse` | ユニット(状態機械) | なし |
| `seekbar::SeekBar` の描画 | ユニット(`Buffer::empty` に render → `Buffer::with_lines` と比較) | なし |
| `app::Playback::{begin_seek, seek_base, reconcile_time_pos}` | ユニット(`Instant` を引数で渡す) | なし |
| `mpv::seek_absolute` | ユニット(JSON 文字列比較。既存 `serializes_seek_both_directions` と同型) | なし |
| `ui::playing_areas` / `seek_bar_layout` | ユニット | なし |
| `input::handle_mouse` / `handle_key_playing` → `actions::seek_*` | 統合(player 無しの `Session::default()`。送信は飛ばされ、App の状態だけ検証。既存 `only_q_quits_the_app_while_playing` と同型) | なし |
| `main.rs` のキャプチャ有効化・panic hook・イベント取りまとめ | 手動(§4-5) | iTerm2 |

### 4-2. 最初に書く Red テスト

いずれも外部プロセス無し。座標は 80×24 の端末、再生中の 4 段でバー行は `y = 21`、`duration = 650.0` を共通の前提にする。ラベル `"mm:ss / mm:ss"` は 13 桁なので、トラックは `x = 0..65`(幅 65)、空白 `65..67`、ラベル `67..80`。幅 65 で 650 秒なら 1 セル = 10 秒。表の f64 期待値はこの前提では等値比較で成立する(確認済み)が、値を変えるときは `(a - b).abs() < 1e-9` で比べる。

| # | テスト名 | 入力 | 期待 |
|---|---|---|---|
| 1 | `seek_bar_layout_reserves_a_fixed_label_on_the_right` | `SeekBarLayout::new(Rect::new(0, 21, 80, 1), 13)` / `new(Rect::new(0, 0, 10, 1), 13)`(狭い) | `track == Rect::new(0, 21, 65, 1)`, `label == Rect::new(67, 21, 13, 1)` / `track.width == 0`, `label == Rect::new(0, 0, 10, 1)`(切り詰め) |
| 2 | `columns_on_the_track_map_to_seconds_from_the_left_edge` | 上の layout。`hit(0, 21)`, `hit(64, 21)`, `hit(65, 21)`, `hit(70, 21)`, `hit(10, 20)`; `seconds_at(0, 650.0)`, `seconds_at(13, 650.0)`, `seconds_at(64, 650.0)`; `clamp_column(0)`, `clamp_column(70)`, `clamp_column(200)` | `Some(0)`, `Some(64)`, `None`(空白), `None`(ラベル), `None`(別の行); `0.0`, `130.0`, `640.0`; `0`, `64`, `64` |
| 3 | `filled_cells_follow_the_ratio_and_saturate` | `filled_cells(Some(0.0), Some(650.0))`, `(Some(325.0), Some(650.0))`, `(Some(650.0), Some(650.0))`, `(Some(700.0), Some(650.0))`, `(None, Some(650.0))`, `(Some(10.0), None)`, `(Some(10.0), Some(0.0))` | `0`, `32`, `65`, `65`, `0`, `0`, `0` |
| 4 | `clamp_target_keeps_the_seek_inside_the_file` | `clamp_target(-3.0, Some(100.0))`, `(99.9, Some(100.0))`, `(50.0, Some(100.0))`, `(0.3, Some(0.5))`, `(50.0, None)`, `(-1.0, None)` | `0.0`, `99.0`, `50.0`, `0.0`, `50.0`, `0.0`。負の絶対値が「末尾から」になる事故と、duration 到達で mpv が終了する事故の両方を塞ぐ |
| 5 | `hover_follows_the_pointer_only_on_the_track` | `on_mouse(Move, 10, 21)` → `(Move, 10, 5)` → `(Move, 70, 21)` | `hover == Some(10)` → `None` → `None`。戻り値は全て `None`。`shown_column()` は hover を返す |
| 6 | `drag_moves_the_marker_and_seeks_once_on_release` | `(Press, 10, 21)` → `(Drag, 20, 21)` → `(Drag, 200, 3)` → `(Release, 30, 21)` | `drag == Some(10)`, None / `Some(20)`, None / `Some(64)`(列は吸着・行は無視), None / 戻り `Some(Seek { column: 30 })`, `drag == None`, `hover == Some(30)` |
| 7 | `a_press_outside_the_track_never_starts_a_drag` | `(Press, 10, 5)` → `(Drag, 20, 21)` → `(Release, 30, 21)` | 全て `None`、`drag == None`。`(Press, 10, 21)` → `(Move, 12, 21)`(Up が来なかった)→ `drag == None`、戻り `None`、`hover == Some(12)` |
| 8 | `optimistic_seek_survives_a_stale_poll_within_the_hold` | `Playback { time_pos: Some(10.0), .. }`。`begin_seek(100.0, t0)` → `reconcile_time_pos(Some(11.0), t0 + 500ms)` → `(None, t0 + 800ms)` → `(Some(101.5), t0 + 900ms)` | `time_pos == Some(100.0)` かつ pending 有り → 変わらず → 変わらず → `Some(101.5)`、`pending_seek == None` |
| 9 | `a_stale_poll_wins_after_the_hold_expires` | `begin_seek(100.0, t0)` → `reconcile_time_pos(Some(11.0), t0 + SEEK_HOLD)` | `time_pos == Some(11.0)`、`pending_seek == None`(シークが効かなかった場合の自然回復) |
| 10 | `relative_seeks_stack_on_the_pending_target` | `time_pos: Some(10.0)`。`seek_base()` → `begin_seek(15.0, t0)` → `seek_base()` → `begin_seek(20.0, t0 + 100ms)` | `Some(10.0)` → `Some(15.0)` → `time_pos == Some(20.0)`、`pending_seek.target == 20.0`、`sent_at == t0 + 100ms`(上書き) |
| 11 | `seek_absolute_serializes_with_the_absolute_flag` | `mpv::seek_absolute(83.5).to_line()` / `seek_absolute(30.0).to_line()` | `"{\"command\":[\"seek\",83.5,\"absolute\"]}\n"` / `"{\"command\":[\"seek\",30.0,\"absolute\"]}\n"`(実測で mpv が受理する形式) |
| 12 | `label_keeps_the_same_width_while_playing` | `label_width(Some(300.0))`, `label_width(Some(3661.0))`, `label_width(None)`; `label_text(Some(75.0), Some(300.0))`, `label_text(Some(75.0), Some(3661.0))`, `label_text(None, Some(300.0))`, `label_text(None, None)` | `13`, `17`, `19`; `"01:15 / 05:00"`, `"0:01:15 / 1:01:01"`, `"--:-- / 05:00"`, `"--:--:-- / --:--:--"` |
| 13 | `seek_bar_renders_fill_marker_and_label` | `SeekBar { layout: new(Rect::new(0,0,30,1), 13), filled: 5, marker: Some(8), label: "01:23 / 34:05", highlighted: true }` を `Buffer::empty(Rect::new(0,0,30,1))` に render | `Buffer::with_lines(["█████░░░┃░░░░░░  01:23 / 34:05"])` と文字が一致(トラック 15 = `█`×5 + `░`×3 + `┃` + `░`×6)。ラベルのセルが Yellow |
| 14 | `mouse_release_on_the_bar_records_an_optimistic_seek` | `App { mode: Playing, screen: Rect::new(0,0,80,24), playback: Playback { duration: Some(650.0), time_pos: Some(0.0), .. }, .. }`、`Session::default()`。`handle_mouse(Down Left @ (0,21))` → `handle_mouse(Up Left @ (13,21))` | `playback.time_pos == Some(130.0)`、`pending_seek.is_some()`、`error == None`(player 無しは送信を飛ばす)、`seek_bar.drag == None` |
| 15 | `mouse_is_ignored_outside_playing_mode` | `mode: Results` で同じ操作 | `seek_bar == default`、`time_pos` 不変 |
| 16 | `arrow_keys_seek_relative_to_the_pending_target_and_clear_hover` | `mode: Playing`, `time_pos: Some(10.0)`, `duration: Some(650.0)`, `seek_bar.hover: Some(3)`。`Right` → `Right` → `Left` | `time_pos` が `15.0` → `20.0` → `15.0`、`hover == None`。`playing_command(KeyCode::Left/Right) == None`(既存テストの該当行を差し替え) |

### 4-3. 既存テストの更新

| テスト | 変更 |
|---|---|
| `ui::tests::playing_rows_do_not_overlap_the_video` | `video_area == Rect::new(0, 0, 80, 21)`、`seek_bar_area == Rect::new(0, 21, 80, 1)`、`status_area == (0, 22)`、`help_area == (0, 23)` |
| `geometry::tests::geometry_matches_the_video_area_of_the_same_terminal_size` | `frame_px == (640, 336)`(21 行 × 16 px) |
| `input::tests::playing_keys_map_to_mpv_commands` | Left/Right の行を `None` に。代わりに `seek_step(Left) == Some(-5.0)`、`seek_step(Right) == Some(5.0)`、`seek_step(Char('x')) == None` |
| `main.rs` / `actions.rs` の `sink()` フィクスチャ | `Rect::new(0, 0, 80, 22)` は映像領域の例として使っているだけなので変更不要(`present_writes_clear_then_cup_then_frame` の `c=80,r=22` も同じ矩形から出るので不変) |
| `app::tests::applies_polled_properties` | pending 無しなら `reconcile_time_pos` はそのまま代入するので不変 |

### 4-4. 実機確認項目(自動テスト不能)

| 項目 | 確認方法 | 外れたときの対処 |
|---|---|---|
| iTerm2 がマウスを報告する | 再生中にバーへポインタを乗せてラベルが黄色に変わる | iTerm2 の設定「Terminal → Enable mouse reporting」を確認 |
| クリックで飛ぶ・右端で終了しない | バー右端をクリックして 1 秒手前に飛び、その後自然に終わる | 終了するなら `SEEK_END_MARGIN_SECS` を増やす |
| ドラッグ中に送信していない | mpv のログ(`--log-file`)で `seek` が離した時の 1 回だけか | 複数出るなら `Drag` 経路を確認 |
| ←→ の表示が即座に動く・往復しない | 押した瞬間に進み、1〜2 秒後にわずかに補正されるだけ | 旧位置へ戻るなら `SEEK_HOLD` / `SEEK_TOLERANCE_SECS` を見直す。改善しなければ §2-6 |
| ポインタ掃引で描画が追いつく | バー上を素早く往復し、映像・入力の遅れが出ない | `EVENT_DRAIN_LIMIT` を調整、または `Moved` を reader 側で間引く |
| 終了後にマウス追跡が残らない | `q` / Ctrl-C / panic(意図的に `panic!` を入れて試す)の後、シェルでポインタを動かしてもゴミが出ない | `DisableMouseCapture` の順序・hook の包み方を確認 |
| 端末の文字選択 | 再生中に Option(⌥)+ドラッグで選択できるか | できなければ制限として README に記す |
| 音声のみ・ライブ | バーが空、クリックで何も起きない、エラーが出ない | |

## 5. 影響範囲

| パス | 変更 |
|---|---|
| `src/seekbar.rs` | 新規 |
| `src/app.rs` | `AppEvent::Mouse`、`App.screen` / `App.seek_bar`、`PendingSeek`、`Playback` のメソッド 3 つ、`apply_property` |
| `src/mpv.rs` | `seek_absolute` 追加(v1.1: `parse_event`、`AppEvent::MpvEvent`) |
| `src/ui.rs` | 4 段レイアウト、`seek_bar_area` / `seek_bar_layout`、`draw_playing`、`help_text`、`status_area` のコメント(クリックはステータス行でなくバー行で拾う) |
| `src/input.rs` | `handle_mouse` / `mouse_input` / `seek_step`、`playing_command` から Left/Right を外す、`handle_key_playing` |
| `src/actions.rs` | `seek_relative` / `seek_absolute`、`start_playback` と `end_playback` で `seek_bar` を default に |
| `src/main.rs` | マウスキャプチャの有効化・解除・panic hook、`Event::Mouse` の転送、`app.screen`、イベント取りまとめ |
| `src/geometry.rs` | テスト期待値のみ |
| `docs/kitty-protocol-design.md` | §3-2 の main.rs 行「マウスイベントは捨てている」と `ui.rs` 行「再生中の3段」が古くなる。参照時に注意(書き換えは任意) |
| `Cargo.toml` | 変更なし |

## 6. 制限・非対象

- マウスキャプチャ中は端末側の文字選択・右クリックメニューが通常通りには使えない(端末共通)。再生中以外もキャプチャは有効のままだが、イベントは捨てる。
- ポインタが端末ウィンドウの外に出ても `Moved` は来ないので、最後のホバー表示が残る。次に中へ戻ったとき、またはキーでシークしたときに消える。
- ドラッグ中にウィンドウ外で離した場合、シークしない(取り消し)。
- スクロールホイール(音量・シーク)、検索画面・結果一覧でのクリック選択、右・中ボタンは対象外。
- `duration` が取れない動画(ライブ配信・一部の音声のみ)ではシークできない。
- 相対シーク(←→)で末尾を越えた場合の mpv 終了は今と同じ(表示側だけクランプする)。

## 7. 実装順

1. `src/seekbar.rs`: §4-2 の #1〜#4、#12 を Red → `SeekBarLayout` / `clamp_target` / `label_*`。
2. `src/seekbar.rs`: #5〜#7 を Red → `SeekBarState::on_mouse`。
3. `src/app.rs`: #8〜#10 を Red → `PendingSeek` と `Playback` のメソッド。`apply_property` を `reconcile_time_pos` 経由に(既存テストは緑のまま)。
4. `src/mpv.rs`: #11 を Red → `seek_absolute`。
5. `src/seekbar.rs` + `src/ui.rs`: #13 と §4-3 のレイアウト/geometry テスト差し替えを Red → `SeekBar` の `render`、4 段レイアウト、`seek_bar_layout`、ヘルプ文。
6. `src/input.rs` + `src/actions.rs`: #14〜#16 と `playing_keys_map_to_mpv_commands` の差し替えを Red → `handle_mouse` / `seek_step` / `seek_relative` / `seek_absolute`。ここまで mpv・端末不要で `cargo test` 全緑。
7. `src/main.rs`: `AppEvent::Mouse` の転送、キャプチャ有効化・解除・panic hook、`app.screen`、イベント取りまとめ。
8. 実機確認(§4-4)。`SEEK_HOLD` / `SEEK_TOLERANCE_SECS` / `EVENT_DRAIN_LIMIT` を調整。
9. v1.1(必要なら): `parse_event` と `playback-restart` での即時ポーリング(§2-6)。
