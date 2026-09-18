倍速再生(0.1 倍刻み)、テキストモード(`--vo=tct`)再生の復活と 3 モード切替、cookie のブラウザ指定の設定ファイル統合、の 3 機能の設計。実装は TDD(Red→Green)で進める前提で、外部プロセス(mpv・yt-dlp)を伴わない純粋関数の単位を先に切り出す。

対象バージョン: mpv 0.41.0 / yt-dlp 2026.08.19 / ratatui 0.29 / crossterm 0.28 / vt100 0.15.2 / macOS 14 / iTerm2。行番号の参照はコミット `339fa07` 時点。実測は 2026/09/18 に `av://lavfi:testsrc` を入力、`--ao=null`、stdout をファイルへ、で行った。

## 0. 合意済み仕様

1. 倍速再生: mpv の IPC で `{"command":["set_property","speed",<value>]}` を送る。キー操作で 0.1 倍刻みに上げ下げでき、範囲は **x0.1〜x4.0**。範囲外を拒否する値オブジェクト(`Speed`)を設け、キー操作での増減はこの範囲で止まる。ステータス行に現在の速度(例 `1.5x`)を出す。1.0 倍へ戻すキーも設ける
2. テキストモード: コミット `c2c7c46` で削除した `--vo=tct` + vt100 方式を復活させ、`DisplayMode` の 3 つ目の選択肢にする。キー操作で切り替えられること(利用者の要望は「toggle できるように」)。設定ファイル `[display] mode` にも 3 つ目の値を足す
3. cookie 設定: `TUITUBE_COOKIES_FROM_BROWSER` で渡しているブラウザ指定(yt-dlp の `--cookies-from-browser` の spec)を設定ファイルの新セクションに保存できるようにする。**保存するのはブラウザ指定の文字列のみ。cookie の値・データそのものは絶対に保存しない**。環境変数が設定されていれば設定ファイルの値を上書きする(`TUITUBE_FPS_LIMIT` と同じ優先順位)

## 1. 実装が依存する事実

### 1-1. mpv の `speed` プロパティ(IPC)

| 操作 | 結果 | 設計への影響 |
|---|---|---|
| `--speed` オプション | `Double (0.01 to 100) (default: 1)` | tuitube の範囲 0.1〜4.0 は mpv の範囲に含まれる |
| `set_property speed 1.5` → `get_property speed` | `success` / `1.5` | 送った値がそのまま読める |
| `set_property speed 0.001`(下限未満) | `error: "unsupported format for accessing property"`、値は直前の 0.1 のまま | 範囲外は mpv 側でも弾かれるが、送る前に tuitube 側で止める(無駄な送信とエラー表示を避ける) |
| `set_property speed 101`(上限超) | 同上。値は 100 のまま | 同上 |
| `set_property speed "2.0"`(文字列) | `success`、2.0 | 数値で送るので使わない |
| `add speed 0.1` | 2.0 → 2.1 | 使わない(§2-2)。tuitube 側で値を確定してから `set_property` で送る |
| `multiply speed 1.1`(mpv 既定キー `]` の中身) | 2.5 → **2.75** | mpv ウィンドウ側で操作されると 0.1 刻みに乗らない値になる。ポーリングで取り込むときは 0.1 刻みへ丸める(§2-3) |
| `audio-pitch-correction` | 既定 `true` | 倍速でも音程は mpv(scaletempo2)が保つ。tuitube では触らない |
| `--speed=1.5` を起動引数に | 起動直後の `get_property speed` = 1.5 | 動画をまたいで速度を持ち越すなら起動引数で渡せる(§2-4) |
| VO 切替(`kitty` → `tct` → `""`)をまたいだ `speed` | 2.5 のまま | 表示モード切替で速度は変わらない。切替コマンド列に speed を含めなくてよい |

### 1-2. mpv `--vo=tct` の出力(stdout がパイプ、`--no-terminal`)

`--vo-tct-width=20 --vo-tct-height=4` で約 1.5 秒、102,074 バイト。削除前(コミット `0877d16`)の `TctRewriter` / `VideoScreen` が前提にしていた形と同じであることを確認した。

| 項目 | 実測 | 設計への影響 |
|---|---|---|
| 起動時(preinit / reconfig) | `ESC[?25l` `ESC[?1003h` `ESC[?1049h` `ESC[2J` | 全て仮想端末(vt100)へ流し、実端末には出さない。`?1003h`(マウス追跡)が実端末に届かないので、シークバーのマウス操作は影響を受けない |
| フレーム開始 / 終了 | `ESC[?2026h` … `ESC[0m` `\n` `ESC[?2026l` | `?2026l` をフレーム境界として `feed` の戻り値にする(削除前と同じ)。末尾の改行で最下行が 1 行スクロールするため、仮想端末は指定行数 +1 で作る(削除前と同じ) |
| 座標 | `ESC[<row>;<col>f`(HVP)、**row は 0 始まり**(実測 `0;3f` `1;3f` `2;3f` `3;3f`)、col は中央寄せのオフセットぶんずれる | vt100 は CUP(`H`)しか解釈しないので HVP → CUP に書き換え、パラメータを +1 する(削除前の `TctRewriter` のまま) |
| セル | `ESC[48;2;R;G;Bm` `ESC[38;2;R;G;Bm` `▄`(U+2584) を 1 セルずつ。約 41 バイト/セル | 139×33 セルの映像領域なら約 188 KB/フレーム、15fps で約 2.8 MB/s(kitty 640×360 の 27.6 MB/s@30fps より小さい)。fps 上限は tct にも付ける(mpv 側のスケーリングと vt100 の解釈が 1 フレームごとに走るため) |
| 終了時(uninit) | `ESC[?25h` `ESC[?1003l` `ESC[?1049l` | 仮想端末へ流して捨てる |
| `set_property vo-tct-width 10` | `success` だが出力寸法は変わらない | 寸法変更は `vid no` → `vid auto` で VO を作り直す(削除前の `resize_video` と同じ手順) |
| `current-vo` | `"tct"` | `DisplayMode::from_current_vo` に `"tct"` → `Text` を足す |
| `--vo-tct-256` / `--vo-tct-algo` / `--vo-tct-buffering` | 存在する(既定: no / half-blocks / line) | 本設計では既定のまま。256 色は §9 |

### 1-3. kitty ⇄ tct の VO 切替(IPC のみ、mpv 再起動なし)

| 手順 | 結果 |
|---|---|
| `--vo=kitty` で起動 → `set_property vo-tct-width 20` / `vo-tct-height 4` → `set_property vo tct` | `current-vo` = `tct`。`time-pos` は連続(3.23 秒)。kitty の uninit が `ESC_Ga=d;ESC\` `ESC_Ga=d;`(ST 無し)`ESC[?25h` `ESC[?1003l` `ESC[20;0f` を出し、その直後に tct の preinit(`?25l ?1003h ?1049h 2J`)が始まる |
| → `set_property vo kitty` | `current-vo` = `kitty`。tct の uninit(`?1049l`)の後に kitty の `a=T` フレームが始まる |
| 速度 | 切替をまたいで維持(§1-1) |

設計への影響: 切替は「入る先の VO オプション → `set_property vo`」の列で足りる(既存の `to_embedded_commands` と同型)。切替の瞬間に出る「前の VO の後始末」は、次の VO のデコーダに入る。kitty の残骸は vt100 が読み飛ばし(§1-5)、tct の残骸(CSI のみ)は `ApcParser` が捨てる。ただし kitty の `a=d` が vt100 側に入ると placement の削除を tuitube が見落とすので、埋め込みから離れるときは `Session.owe_clear` を立てる(現行の別ウィンドウ行きと同じ)。

### 1-4. fps 上限フィルタのラベル操作

| 操作 | 結果 | 設計への影響 |
|---|---|---|
| `--vf-append=@tuitube-cap:…` で起動後に同じラベルで `vf add` | `success`。`vf` の一覧は 1 個のまま(重複しない) | 二重に足しても壊れない。切替列でのフィルタ追加は「別ウィンドウから戻るときだけ」に絞るが、誤って重ねても害はない |
| 存在しないラベルを `vf remove` | `success` | 同上 |

### 1-5. vt100 0.15.2

| 項目 | 実測 | 設計への影響 |
|---|---|---|
| 入手 | `~/.cargo/registry` に 0.15.2(依存の vte 0.11.1、vte_generate_state_changes 0.1.2、unicode-width 0.1.14)がキャッシュ済み。`cargo build --offline` で通る | `Cargo.toml` に `vt100 = "0.15"` を戻す。ネットワーク不要 |
| kitty の残骸 | `ESC_Ga=d;ESC\` ×2(ST 無し版含む)+ `ESC[21;1H` を流しても文字は入らない。base64 相当 5,000 バイトの APC も読み飛ばす | 切替の瞬間に kitty の出力が vt100 へ入っても画面は汚れない |
| 画面の初期化 | 残骸で動いたカーソルは tct の `2J` + CUP で戻る | 追加の処理は要らない |
| truecolor 半ブロック | `▄` fg=Rgb(0,0,255) bg=Rgb(255,0,0) として取れる | 削除前の `render_screen` / `convert_color` がそのまま使える |

### 1-6. cookie 連携の現状

| 箇所 | 現状 |
|---|---|
| `cookies.rs` L4 | `ENV_VAR = "TUITUBE_COOKIES_FROM_BROWSER"` |
| `cookies.rs` L10-53 `CookieSource` | spec 文字列だけを持つ。`from_env_value(Option<&str>)` が trim と空文字の拒否、`from_env()` が環境変数読み。ブラウザ名の一覧は持たない(検証は yt-dlp に任せる) |
| `cookies.rs` L197-203 `CookieState::from_env()` | 指定があれば `Armed`、無ければ `Off` |
| `cookies.rs` L257-269 `refusal()` | フィードを断る文言で `ENV_VAR` を案内 |
| `main.rs` L81 | `cookies: CookieState::from_env()` |
| `from_env_value` の呼び出し | 14 箇所(cookies.rs のテスト、actions.rs / app.rs / main.rs ×2 / search.rs の各テスト) |
| yt-dlp / mpv へ渡すもの | `--cookies-from-browser <spec>`(search.rs L85-87)、`--ytdl-raw-options-append=cookies-from-browser=<spec>`(cookies.rs L47-52)。`--cookies <file>` は使っていない。cookie ストアを読むのは yt-dlp だけで、tuitube は cookie の値に触れない |
| `settings.rs` の環境変数の受け方 | `validate(raw, env_fps_limit: Option<&str>)`(L125)、`parse_fps_limit_env`(L269-296)は「未設定 / 無効化 / 上書き」の 3 値を返す |

### 1-7. 現行コードの構造(行参照)

| 箇所 | 現状 | 関係する機能 |
|---|---|---|
| `input.rs` L78-92 `handle_key_playing` | シーク(`seek_step`)→ 単発コマンド(`playing_command`)→ `w` → `q` の順に見る | 1, 2 |
| `input.rs` L131-149 | 再生中のキー: space / ← → / ↑ ↓ / Esc / q(+ `w`、Ctrl-C) | 1, 2 |
| `actions.rs` L218-250 `toggle_display_mode` | 2 モードの toggle。別ウィンドウ行きで `owe_clear` | 2 |
| `actions.rs` L258-280 `apply_resize` | 埋め込みのときだけ `mpv::resize_video` | 2 |
| `display.rs` L15-58 `DisplayMode` | `Embedded` / `Window`、`toggled` / `from_current_vo` / `label` / `key` / `from_key` | 2 |
| `display.rs` L200-240 `LaunchPlan` | モード別の起動引数 | 1, 2 |
| `display.rs` L242-267 | `to_window_commands` / `to_embedded_commands` | 2 |
| `mpv.rs` L20-25 | `REQ_*` は 1〜4 と 6(5 は欠番) | 1 |
| `mpv.rs` L105-117 `poll_commands` | 5 プロパティ。テスト L753-765 が `len() == 5` を固定 | 1 |
| `mpv.rs` L124-138 `resize_video` | kitty 4 オプション + `vid no/auto` | 2 |
| `video.rs` L201-208 `Sink`、L210-300 `VideoSink` | kitty 専用(`ApcParser` + `FrameAssembler`) | 2 |
| `video.rs` L31-86 `Geometry` | `size_options` / `kitty_options` / `mpv_args` | 2 |
| `app.rs` L65-75 `Playback`、L107-129 `App` | 速度の置き場は無い | 1 |
| `app.rs` L238-257 `display_label`、L259-277 `playback_line` | ステータス行 | 1, 2 |
| `ui.rs` L104-112 `draw_playing`、L184-199 `help_text` | モード別の描画と文言 | 1, 2 |
| `settings.rs` L23-61 `Raw*`、L78-97 `Settings`、L124-181 `validate`、L298-368 `render` | 設定の形と検証とテンプレート | 2, 3 |
| `DisplayMode` を網羅 match している箇所 | display.rs L226-236、actions.rs L224-236、ui.rs L190-197、app.rs L240-250 | 2(`Text` を足すとコンパイラが書き分け漏れを拾う) |

## 2. 設計判断

### 2-1. 速度の表現

| 案 | 内容 | 利点 | 欠点 | 判定 |
|---|---|---|---|---|
| (a) `f64` をそのまま | `App.speed: f64`、±0.1 を足し引き | 実装が薄い | 0.1 の累積で 0.30000000000000004 のような値になり、表示・比較・境界判定が汚れる。範囲外を型で防げない | 不採用 |
| (b) 十分の一単位の整数(採用) | `Speed(u8)`(1〜40)。`from_tenths` / `from_f64` は範囲外を `None`、`stepped` は境界で止まる | 累積誤差が無い。範囲を型が保証する。表示文字列を整数演算で作れる | mpv へ送る直前に `/10.0` する 1 手間 | **採用** |

`Speed::MIN` = 1(0.1x)、`Speed::MAX` = 40(4.0x)、`Speed::NORMAL` = 10(1.0x)、刻み 1(0.1x)。mpv の受け付け範囲(0.01〜100)の内側なので、tuitube が送る値が mpv 側で弾かれることはない。

### 2-2. 速度の送り方

| 案 | 内容 | 利点 | 欠点 | 判定 |
|---|---|---|---|---|
| (a) `add speed ±0.1` | mpv 側で加算 | 現在値を知らなくてよい | mpv 側の値が 2.75 のような端数のとき刻みが崩れる。範囲を tuitube が止められない。表示に出す値を別に持つ必要がある | 不採用 |
| (b) `set_property speed <value>`(採用) | tuitube が次の値を決めて送る | 合意済み仕様どおり。範囲・刻みを tuitube が確定できる。送った値 = 表示する値 | 現在値を tuitube が持つ | **採用** |

### 2-3. 速度の追従(ポーリング)

| 案 | 内容 | 判定 |
|---|---|---|
| (a) tuitube の値だけを信じる | ポーリングしない | mpv ウィンドウ側(§2-10)で変えられた速度が表示に出ない。不採用 |
| (b) `speed` をポーリングに足す(採用) | `REQ_SPEED = 7` を `poll_commands` に足し、届いた値を 0.1 刻みへ丸めて範囲に収めて `App.speed` に入れる | `set_property` と `get_property` は同じソケットで順に処理されるので、送った直後の読み取りは新しい値を返す。ただし送信前に発行された `get_property` の応答は古い値で後着するため、シークと同じ保持窓を張る(下記)。**採用** |

ローカルで速度を送ってから `SPEED_HOLD`(2 秒)の間は、食い違うポーリング値を捨てる(`Playback::pending_seek` / `SEEK_HOLD` と同じ形)。音量は `add volume` で mpv 側が次の値を計算するため後着で巻き戻っても実害が無いが、速度は tuitube が次の値を `App.speed` から決めるので、巻き戻ると次の `]` が mpv の現在値を再送するだけになり押下が 1 回消える。

丸めは `Polled::from_f64`(四捨五入して範囲に収め、丸めたかどうかも返す)。mpv 側で 8.0 にされていたときは表示を `4.0x` にするだけでなく `set_property speed 4.0` を送り返し、表示と実際の再生を揃える。

### 2-4. 速度の持続(動画をまたぐか)

| 案 | 内容 | 利点 | 欠点 | 判定 |
|---|---|---|---|---|
| (a) 動画ごとに 1.0x へ戻す | `Playback` に置く | 「次の動画は等速で始まる」が分かりやすい | 講義シリーズを 1.5x で続けて見るとき毎回押し直す | — |
| (b) セッション中は持ち越す(採用) | `App.speed` に置き、次の再生の起動引数に `--speed=<value>` を付ける(1.0x のときは付けない) | mpv 自身(プレイリストで速度維持)と YouTube の Web UI と同じ挙動。戻すのは Backspace 1 回 | 前の動画で上げたのを忘れると次が速い(ステータス行に常時出るので気づける) | **採用**(§8 #2) |

### 2-5. キー割り当て

現行の再生中キー: space / ← → / ↑ ↓ / w / Esc / q / Ctrl-C、マウス(左クリック・ドラッグ・移動)。結果一覧: ↑ ↓ / Enter / `/` / Esc / q。入力モードは文字キー全部が検索語。

| キー | 動作 | 選定理由 | 衝突 |
|---|---|---|---|
| `]` | 速度 +0.1 | mpv 既定の速度キーと同じ位置(mpv は ×1.1 だが方向は同じ)。US / JIS ともシフト不要 | 無し。前設計 §8 #8 で画質切替の候補に挙げていたが未実装なので解放する。画質切替を後で付けるなら別キー |
| `[` | 速度 -0.1 | 同上 | 無し |
| `Backspace` | 速度を 1.0x へ | mpv 既定の「速度リセット」と同じキー。再生中は文字入力が無いので空いている | 無し(入力モードの Backspace は検索語の削除。モードで分かれる) |
| `w` | 表示モードを次へ(3 モードの循環。§2-6) | 現行キーの継承 | 無し |

検討して外した候補: `-` / `+`(`+` はシフトが要る。`=` で代用すると記号の意味が分かりにくい)、`<` / `>`(シフト要)、`,` / `.`(意味が読めない)。

別ウィンドウ中に mpv ウィンドウへフォーカスが移っているときは、キーは mpv が受ける(前設計 §2-6 と同じ)。mpv 側の `[` `]` は ×0.9 / ×1.1、Backspace は 1.0 なので、押す感覚は近いが刻みが違う。tuitube の表示はポーリングで丸めた値になる(§2-3)。

### 2-6. テキストモードの切替方式

| 案 | 内容 | 利点 | 欠点 | 判定 |
|---|---|---|---|---|
| (a) `w` で 3 モードを循環(採用) | Embedded → Text → Window → Embedded | キー 1 つ、追加の状態なし。ヘルプ行に次のモード名を出せば迷わない。「toggle できる」要望を満たす | 目当てのモードまで最大 2 回押す | **採用** |
| (b) `w` = 別ウィンドウ ⇄ 端末内、`t` = テキスト ⇄ 埋め込み | 2 キー + 「端末内の直前モード」を覚える状態 | どのモードも 1 回で行ける | キーと状態が増える。ヘルプ行がさらに長くなる。別ウィンドウ中の `t` の意味を決める必要がある | 予備 |
| (c) `w` の 2 モード toggle を残し、テキストは設定ファイルだけ | キー操作なし | 変更が最小 | 「toggle できるように」に反する | 不採用 |

循環の順序は **Embedded → Text → Window → Embedded** にする。途中で経由するモードが端末内(Text)であって、意図しない GUI ウィンドウが一瞬開いてフォーカスを奪う(前設計 §1-2: フォーカス移動は一貫しない)ことが無いため。埋め込みから `w` 1 回の結果が現行(別ウィンドウ)から変わる点はヘルプ行の `w:テキスト` で示す。別ウィンドウから `w` 1 回で埋め込みへ戻るのは現行と同じ。

### 2-7. 映像デコーダの切替

読み取りタスク(`mpv.rs` `spawn_video_reader`)は mpv の stdout を 1 本の `VideoSink` へ流し続ける。VO を切り替えても同じパイプなので、`VideoSink` の中身(デコーダ)を差し替える。

| 案 | 内容 | 判定 |
|---|---|---|
| (a) kitty と tct の両パーサに常に流す | 切替の状態を持たなくてよい | kitty のフレーム(1 MB 級)を vt100 に毎回通す無駄が大きい。不採用 |
| (b) `VideoSink` の内側を `enum Decoder { Kitty, Text }` にして切替時に入れ替える(採用) | 読み取りタスクの I/F(`feed` → `request_redraw` → `VideoFrame`)は不変。切替時は `reset(kind, geometry)` で新しいデコーダを作る | 切替直後に前の VO の残骸が新しいデコーダへ入るが、§1-3 / §1-5 のとおり両方向とも無害。**採用** |
| (c) `App.video` を `enum` にして sink を 2 種類持つ | 型で分かれる | 読み取りタスクが握っている clone を差し替えられない(タスクを作り直す = mpv の stdout を読む手を一瞬離す)。不採用 |

テキスト側のデコーダ(`TextScreen`)は削除前の `VideoScreen` から `Arc<Mutex>` と再描画フラグを外したもの(どちらも `VideoSink` が持つ)。描画は ratatui の `Buffer` へ転写するので、kitty のように `present_video` で APC を書く経路は使わない。`present_video` は kitty の保留(`take()`)と `owe_clear` だけを見るので変更不要。

### 2-8. 遷移コマンド列

現行の `to_window_commands` / `to_embedded_commands` を、`switch_commands(from, to, geometry, cap, window)` 1 つにまとめる。列は「入る先」でほぼ決まり、fps 上限フィルタの出し入れだけが「出る元」に依存する。

| 入る先 | 列 | 備考 |
|---|---|---|
| Window | [`vf remove @tuitube-cap`(cap があるとき)] → `set_property vo <window.vo_value()>` | 現行の `to_window_commands` と同じ |
| Embedded | `vo-kitty-*` 8 個 → [`vf add @tuitube-cap:…`(cap があり、from が Window のとき)] → `set_property vo kitty` | 現行の `to_embedded_commands` に from 条件が付く。Text から来たときはフィルタが残っているので足さない(足しても §1-4 のとおり害は無い) |
| Text | `vo-tct-width` / `vo-tct-height` → [`vf add`(同上)] → `set_property vo tct` | 新設。VO オプションは生成前に入っていなければ読まれないので順序を守る |

`Session.owe_clear` は **from が Embedded のとき**に立てる(現行は to が Window のときだけ。Text 行きでも kitty の `a=d` が vt100 側へ入るため)。

### 2-9. cookie 設定の統合

| 論点 | 判断 | 理由 |
|---|---|---|
| 置き場 | `[cookies] browser = "<spec>"`(新セクション、キー 1 つ) | 環境変数名と同じ語(browser)で、値も同じ spec |
| 保存するもの | spec 文字列だけ。`RawCookies` はフィールドが `browser` 1 つで、`validate` は `let RawCookies { browser } = raw;` と網羅分解する | フィールドを足すとコンパイルが止まり、「他に何か保存していないか」を型で確認できる。cookie の値を読むコードは元から tuitube に無い(§1-6) |
| 検証 | trim して空なら「指定なし」+ notice。ブラウザ名の一覧照合はしない | 現行 `CookieSource` の方針(yt-dlp に任せる)を変えない。綴り違いは yt-dlp の `unsupported browser` → 既存の `Unreadable` 経路で通知される |
| 環境変数の優先 | 環境変数が空白以外なら設定ファイルの値を上書き。空 / 未設定なら上書きしない | `TUITUBE_FPS_LIMIT` と同じ |
| 無効化の値 | `TUITUBE_COOKIES_FROM_BROWSER=none` で設定ファイルの値を無視して連携を切る | fps の `0` / `unlimited` に相当する 3 値目。設定ファイルに書いたまま一時的に cookie 無しで動かすため。`none` は yt-dlp のブラウザ名(brave / chrome / chromium / edge / firefox / opera / safari / vivaldi / whale)と衝突しない(§8 #5) |
| 状態の作り方 | `CookieState::from_source(Option<CookieSource>)`。`CookieSource::from_env` / `CookieState::from_env` は削除 | 環境変数の読み取りを `settings::load()` に一本化する(fps と同じ) |
| `from_env_value` の名前 | `from_spec` に改名 | 環境変数以外からも作るので。呼び出し 14 箇所は機械的な置換(§8 #6) |
| 環境変数の受け渡し | `validate(raw, env: EnvOverrides)`。`EnvOverrides { fps_limit: Option<&str>, cookies: Option<&str> }` | 引数を 3 つ 4 つと増やさない。テストは `EnvOverrides::default()` |

### 2-10. 操作系(誰がどこでどう使うか)

倍速再生:

| 場面 | 起きること | 設計上の受け方 |
|---|---|---|
| 講義動画を速く見たい | `]` を 5 回 → ステータス行が `1.5x` | 音程は mpv が保つ(§1-1)。1 回ごとに `set_property` 1 個 |
| 聞き取れない箇所 | `[` で `0.8x` など。終わったら Backspace で `1.0x` | Backspace は現在値が 1.0x なら何も送らない |
| 次の動画を再生 | 速度は前のまま(§2-4)。ステータス行で分かる | 起動引数 `--speed=` で渡す。1.0x なら付けない |
| `4.0x` で `]` | 何も起きない(送らない・エラーにしない) | `Speed::stepped` が境界で止まり、値が変わらなければ送信しない |
| 別ウィンドウで mpv 側の `]` | mpv が ×1.1 → 端数(例 2.75) | 毎秒のポーリングで `2.8x` と出す。tuitube 側で次に `]` を押すと 2.9 を送る |
| ライブ配信で速度を上げる | mpv がバッファを使い切って止まりうる | mpv の挙動に任せ、tuitube では制限しない(ステータス行に速度が出ているので原因は分かる) |
| 送信に失敗(パイプが閉じた) | エラー表示。値は変えない | 続く `MpvExited` で結果一覧へ(現行どおり) |
| 一時停止中に速度を変える | 受け付ける(mpv も受け付ける) | 追加処理なし |

テキストモード:

| 場面 | 起きること | 設計上の受け方 |
|---|---|---|
| Kitty graphics 非対応の端末(Terminal.app、tmux 経由、Linux 端末)で使う | 埋め込み(kitty)は何も映らない | `[display] mode = "text"` を設定。または再生中に `w` 1 回 |
| iTerm2 で埋め込み中、画像の表示が乱れた | `w` で一時的にテキストへ | 端末内で完結し、ウィンドウは開かない(§2-6) |
| テキスト中に端末をリサイズ | 文字数が変わる | `vo-tct-width/height` を送って `vid no/auto`(削除前と同じ)。作り直し中の旧寸法フレームは描かない |
| テキスト中にマウスでシーク | シークバー行は別 | 変更なし。tct の `?1003h` は実端末に届かない |
| テキスト → 別ウィンドウ | 映像領域の文字が消えてプレースホルダ | ratatui が描かなかったセルは空白になる(前設計 §1-3)。追加処理なし |
| 埋め込み → テキスト | kitty の画像が消えて文字が出る | `owe_clear` で `a=d` を書く(§2-8) |
| tmux で色がおかしい | tmux が truecolor を通していない | tmux 側の設定(`terminal-features … RGB`)の問題。tuitube では扱わない |
| Terminal.app で色がおかしい | Terminal.app は truecolor 非対応 | `--vo-tct-256` の設定化を §9 で提案 |
| CPU | tct は 1 フレームごとに mpv 側で縮小 + tuitube 側で vt100 解釈 | fps 上限を kitty と同様に付ける。実測は §6-3 |

cookie 設定:

| 場面 | 起きること | 設計上の受け方 |
|---|---|---|
| 初回起動 | テンプレートに `# browser = "chrome"` がコメントで入る | コメントを外して再起動 → ステータス行 `cookies: chrome` |
| 一度だけ別ブラウザで試す | `TUITUBE_COOKIES_FROM_BROWSER=safari tuitube` | 環境変数が優先。設定ファイルは書き換えない |
| 一時的に cookie 無しで動かす | `TUITUBE_COOKIES_FROM_BROWSER=none tuitube` | 連携 Off。ステータス行に cookie 表示が出ない |
| `browser = ""` と書いた | 指定なし + notice | 他のキーは巻き添えにしない(現行の `parse_choice` と同じ方針) |
| `browser = "arc"` と書いた | yt-dlp が `unsupported browser` | 既存の `Unreadable` → `Suspended` 経路。設定側では検証しない |
| 設定ファイルを他人に見せる / バックアップする | ブラウザ名とプロファイル名だけが入っている | cookie の値は無い。プロファイル名が個人情報になりうる点は利用者の判断 |
| フィード(`:ytrec` 等)を cookie 無しで要求 | 断り文に「設定ファイルの `[cookies] browser` か環境変数」を案内 | 文言変更(§5) |

## 3. アーキテクチャ

### 3-1. モジュール構成

| ファイル | 役割 | 変更種別 | 機能 |
|---|---|---|---|
| `src/speed.rs` | `Speed` 値オブジェクト(範囲・刻み・表示文字列・`MpvCommand` / 起動引数の生成)。I/O 無し | **新規** | 1 |
| `src/tct.rs` | `TextScreen`(vt100 の仮想端末 + フレーム捕捉 + ratatui `Buffer` への転写)、`TctRewriter`(HVP → CUP、0 始まり → 1 始まり、`?2026l` でフレーム区切り)。削除前 `video.rs` の復元。I/O 無し | **新規**(復元) | 2 |
| `src/video.rs` | `VideoSink` の内側を `enum Decoder { Kitty, Text }` に。`DecoderKind`、`reset(kind, geometry)`、`render_text`。`Geometry` に `tct_options` / `tct_args` | 変更 | 2 |
| `src/display.rs` | `DisplayMode::Text`、`next()`(`toggled` の置き換え)、`decoder_kind()`、`TCT_VO`。`LaunchPlan` に `speed` と Text の引数。`switch_commands(from, to, …)` | 変更 | 1, 2 |
| `src/mpv.rs` | `REQ_SPEED = 7` と `poll_commands` への追加。`set_tct_option`、`resize_text_video` | 変更 | 1, 2 |
| `src/app.rs` | `App.speed: Speed`。`apply_property(REQ_SPEED)`。`playback_line` に速度。`display_label` に Text | 変更 | 1, 2 |
| `src/actions.rs` | `change_speed` / `reset_speed`。`toggle_display_mode` → `cycle_display_mode`(3 モード、デコーダ切替、`owe_clear` 条件)。`apply_resize` に Text 分岐 | 変更 | 1, 2 |
| `src/input.rs` | `speed_step`(`[` `]`)、Backspace → reset | 変更 | 1 |
| `src/ui.rs` | `draw_playing` に Text 分岐(`render_text`)。プレースホルダ文言を次モード名に。`help_text` に速度キーと 3 モード | 変更 | 1, 2 |
| `src/settings.rs` | `RawCookies` / `[cookies]`、`Settings.cookies`、`EnvOverrides`、`parse_cookies_env`。`[display] mode` のコメントに `"text"` | 変更 | 2, 3 |
| `src/cookies.rs` | `from_env_value` → `from_spec`、`from_env` 系を削除、`CookieState::from_source`、`refusal` の文言 | 変更 | 3 |
| `src/main.rs` | `mod speed; mod tct;`。`cookies: CookieState::from_source(loaded.settings.cookies.clone())` | 変更(小) | 1, 2, 3 |
| `Cargo.toml` / `Cargo.lock` | `vt100 = "0.15"` | 変更 | 2 |
| `src/search.rs` / `src/seekbar.rs` / `src/kitty.rs` / `src/geometry.rs` | 変更なし | なし | — |

### 3-2. 既存コードの変更点(行参照)

機能 1(倍速):

| 箇所 | 現状 | 変更 |
|---|---|---|
| `mpv.rs` L20-25 | `REQ_CURRENT_VO = 6` まで | `pub const REQ_SPEED: u64 = 7;` |
| `mpv.rs` L105-117 `poll_commands` | 5 プロパティ | `("speed", REQ_SPEED)` を追加。テスト L764 の `len() == 5` を 6 に |
| `display.rs` L200-240 `LaunchPlan` | `mode / geometry / fps_cap / window / extra_args` | `speed: Speed` を追加(`new` では `Speed::NORMAL`)。`args()` の末尾(`extra_args` の前)に `speed.launch_arg()` があれば push。構造体リテラルのテスト(display.rs L358-364, L378-384, L439-445、mpv.rs L726-732)に `speed` を足す |
| `actions.rs` L191-195 `start_playback` | `plan.extra_args.extend(…)` | 直前に `plan.speed = app.speed;` |
| `actions.rs`(新規) | — | `change_speed(app, session, steps: i8)`、`reset_speed(app, session)`、私有 `set_speed(app, session, next)` |
| `input.rs` L78-92 `handle_key_playing` | seek → command → w → q | seek の後に `speed_step` と Backspace の分岐を足す |
| `input.rs` L131-138 付近(新規) | — | `fn speed_step(code) -> Option<i8>`: `[` → -1、`]` → +1 |
| `app.rs` L107-129 `App` | — | `pub speed: Speed`(Default = `Speed::NORMAL`) |
| `app.rs` L183-197 `apply_property` | 5 分岐 | `REQ_SPEED` → `data.and_then(as_f64)` があれば `reconcile_speed(Polled::from_f64(v), now)`。送り返しが要るときは `MpvCommand` を返す |
| `app.rs` L259-277 `playback_line` | `{state}  {title}  {pos} / {dur}{volume}  {display}` | `{volume}` の後に `  {speed.label()}` を挟む。既存テスト L576(`starts_with("PAUSED  song  00:30 / 01:00  vol 70")`)と L537-541(末尾が display_label)はそのまま通る |
| `ui.rs` L184-199 `help_text` | 再生中の文言 | `[ ]:速度±0.1  BS:等速` を足す(§5) |
| `main.rs` L1-13 | — | `mod speed;` |

機能 2(テキストモード):

| 箇所 | 現状 | 変更 |
|---|---|---|
| `Cargo.toml` L6-13 | — | `vt100 = "0.15"` |
| `src/tct.rs`(新規) | — | 削除前 `video.rs`(`git show 0877d16:src/video.rs`)の `TctRewriter` / `push_incremented` / `render_screen` / `convert_color` / `capture_frame` / `copy_frame` と、`VideoScreen` を `Arc<Mutex>` と `redraw` 無しの `TextScreen` として復元。テスト 21 本を復元(§6-1) |
| `display.rs` L12-13 | `KITTY_VO` | `const TCT_VO: &str = "tct";` を追加 |
| `display.rs` L15-20 `DisplayMode` | 2 値 | `Text` を追加 |
| `display.rs` L22-28 `toggled` | 2 値の入れ替え | `next()` に改名し Embedded → Text → Window → Embedded |
| `display.rs` L30-36 `from_current_vo` | kitty / それ以外 | `TCT_VO` → `Text` を追加 |
| `display.rs` L38-58 `label` / `key` / `from_key` | 2 値 | `"テキスト"` / `"text"` を追加。`from_key` の配列に `Text` |
| `display.rs`(新規) | — | `DisplayMode::decoder_kind(self) -> Option<video::DecoderKind>`(Embedded → Kitty、Text → Text、Window → None) |
| `display.rs` L224-239 `LaunchPlan::args` | Embedded / Window | `Text` 分岐: `--vo=tct` + `geometry.tct_args()` + cap の `launch_arg` |
| `display.rs` L242-267 | `to_window_commands` / `to_embedded_commands` | `switch_commands(from, to, geometry, cap, window) -> Vec<MpvCommand>` に統合(§2-8)。テスト L461-540 を新 I/F に移す |
| `display.rs` L338-354 テスト | `toggled` の 2 値 | `next()` の 3 値循環と `from_current_vo(Some("tct"))` |
| `video.rs` L31-86 `Geometry` | kitty 用 | `tct_options() -> [(&str, Value); 2]`(`("width", cols)`, `("height", rows)`)、`tct_args() -> Vec<String>`(`--vo-tct-width=`, `--vo-tct-height=`) |
| `video.rs` L201-208 `Sink` | `parser` + `assembler` | `decoder: Decoder`(`Kitty { parser, assembler }` / `Text(TextScreen)`) |
| `video.rs` L216-228 `VideoSink::new` | kitty 固定 | そのまま(kitty)。`with_kind(kind, geometry)` を追加 |
| `video.rs` L234-263 `feed` | kitty のみ | `Decoder` で分岐。Text は `TextScreen::feed` の戻り(フレーム完成)をそのまま返す |
| `video.rs` L274-285 `resize` | kitty のみ | `reset(self.kind(), geometry)` に委譲。`reset(kind, geometry)` を追加(デコーダを作り直し、kitty なら `pending.clear = true`) |
| `video.rs`(新規) | — | `render_text(&self, area, buf)`(Text のときだけ描く)、`kind() -> DecoderKind` |
| `mpv.rs` L119-122 | `set_kitty_option` | `set_tct_option(key, value)`(`vo-tct-{key}`)を追加 |
| `mpv.rs` L124-138 | `resize_video` | `resize_text_video(geometry) -> [MpvCommand; 4]`(width, height, `vid no`, `vid auto`)を追加 |
| `actions.rs` L191 `start_playback` | `VideoSink::new(video_geometry(…))` | `VideoSink::with_kind(app.display.decoder_kind().unwrap_or(DecoderKind::Kitty), …)` |
| `actions.rs` L218-250 `toggle_display_mode` | 2 モード | `cycle_display_mode`: `to = from.next()` → `switch_commands` を `send_all` → Ok なら `to.decoder_kind()` があれば `video.reset(kind, geometry)`、`app.display = to`、`from == Embedded` なら `owe_clear = true` |
| `actions.rs` L258-280 `apply_resize` | Embedded だけ送る | `match app.display`: Embedded → `resize_video`、Text → `resize_text_video`、Window → 送らない |
| `actions.rs` L580-593, L644-699 テスト | 2 モードの列 | 3 モードの列に更新 |
| `app.rs` L238-257 `display_label` | Embedded は fps + quality、Window は空 | `Text` は fps だけ(`[テキスト 15fps]`)。quality はピクセル予算なので出さない |
| `ui.rs` L10-11 `WINDOW_PLACEHOLDER` | 固定文言 `w: 埋め込みに戻す` | 次モード名で組む: `別ウィンドウで再生中  w: {next.label()}へ`(循環では別ウィンドウの次は埋め込みなので現行文言と同じ意味になるが、順序に依存させない) |
| `ui.rs` L104-112 `draw_playing` | Window ならプレースホルダ | `match app.display`: Window → プレースホルダ、Text → `video.render_text(video_area, frame.buffer_mut())`、Embedded → 何も描かない |
| `ui.rs` L184-199 `help_text` | 2 変種 | `w:{next.label()}` を 3 モードで。戻り値は `String` に |
| `settings.rs` L308-310 `render` | `"embedded" = …、"window" = …` | `"text" = 文字ブロック(Kitty 非対応端末向け)` を足す |
| `settings.rs` L543-551 テスト | window / Window | `"text"` を足す |
| `main.rs` L1-13 | — | `mod tct;` |

機能 3(cookie 設定):

| 箇所 | 現状 | 変更 |
|---|---|---|
| `settings.rs` L23-30 `RawConfig` | 4 セクション | `pub cookies: Option<RawCookies>` |
| `settings.rs`(新規) | — | `pub struct RawCookies { pub browser: Option<String> }`(フィールドはこれだけ) |
| `settings.rs` L78-97 `Settings` | — | `pub cookies: Option<CookieSource>`(Default = None) |
| `settings.rs` L125 `validate(raw, env_fps_limit)` | 引数 `Option<&str>` | `validate(raw, env: EnvOverrides)`。中で `let RawCookies { browser } = raw.cookies.unwrap_or_default();` → trim 空なら notice、他は `CookieSource::from_spec` → `parse_cookies_env(env.cookies)` が `Some(_)` なら上書き |
| `settings.rs` L151-156 | `parse_fps_limit_env(env_fps_limit)` | `parse_fps_limit_env(env.fps_limit)` |
| `settings.rs`(新規) | — | `pub fn parse_cookies_env(raw: Option<&str>) -> Option<Option<CookieSource>>`(None = 上書きしない、Some(None) = `none` で無効化、Some(Some) = 上書き) |
| `settings.rs` L298-368 `render` | 4 セクション | `[mpv]` の前に `[cookies]` セクション(§4) |
| `settings.rs` L391-474 `load_from` / `fallback` / `create_template` / `load` | `env_fps_limit` を引き回す | `env: EnvOverrides` を引き回す。`load()` は `std::env::var(FPS_LIMIT_VAR)` と `std::env::var(cookies::ENV_VAR)` から組む |
| `settings.rs` テスト L491-497 `settings_of` / `notices_of` | `validate(…, None)` | `validate(…, EnvOverrides::default())`。直接呼んでいる L616-627, L759-763 も同様 |
| `cookies.rs` L16-21 | `from_env_value` | `from_spec` に改名 |
| `cookies.rs` L23-25, L197-203 | `CookieSource::from_env` / `CookieState::from_env` | 削除。`CookieState::from_source(source: Option<CookieSource>) -> Self` を追加 |
| `cookies.rs` L264-267 `refusal` | `{ENV_VAR} を設定してください` | `設定ファイルの [cookies] browser か環境変数 {ENV_VAR} を設定してください`(既存テストの `contains(ENV_VAR)` は通ったまま) |
| `main.rs` L81 | `CookieState::from_env()` | `CookieState::from_source(loaded.settings.cookies.clone())` |
| 各テストの `CookieSource::from_env_value(Some("chrome"))` | 14 箇所 | `from_spec` に置換 |

### 3-3. 型と関数(TDD の足場)

`src/speed.rs`

```rust
/// 再生速度。0.1 倍刻みの整数(十分の一単位)で持ち、浮動小数の累積誤差と範囲外を型で防ぐ。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Speed(u8);

impl Speed {
    pub const MIN: Speed = Speed(1);      // 0.1x
    pub const MAX: Speed = Speed(40);     // 4.0x
    pub const NORMAL: Speed = Speed(10);  // 1.0x
    pub fn from_tenths(tenths: u8) -> Option<Self>;   // 1..=40 以外は None
    pub fn from_f64(value: f64) -> Option<Self>;      // 0.1 刻みに四捨五入。有限でない・範囲外は None
    pub fn stepped(self, steps: i8) -> Self;          // 境界で止まる(飽和)
    pub fn tenths(self) -> u8;
    pub fn value(self) -> f64;                        // tenths / 10.0
    pub fn decimal(self) -> String;                   // "1.5" / "1.0" / "0.1"(整数演算で組む)
    pub fn label(self) -> String;                     // decimal() + "x"
    pub fn command(self) -> MpvCommand;               // set_property speed <value>
    pub fn launch_arg(self) -> Option<String>;        // NORMAL なら None、他は "--speed=1.5"
}
impl Default for Speed { NORMAL }

/// ポーリングで届いた速度。範囲外を丸めたときは clamped = true(呼び出し側が送り返す)。
pub struct Polled { pub speed: Speed, pub clamped: bool }

impl Polled {
    pub fn from_f64(value: f64) -> Self;              // 0.1 刻みに四捨五入して範囲へ。有限でなければ NORMAL
}
```

`src/tct.rs`(削除前 `video.rs` の復元。同期は `VideoSink` が持つので `Arc<Mutex>` 無し)

```rust
pub struct TextScreen { parser: vt100::Parser, rewriter: TctRewriter, scratch: Vec<u8>, frame: Option<Buffer>, saw_frame: bool }
impl TextScreen {
    pub fn new(cols: u16, rows: u16) -> Self;         // 内部の仮想端末は rows + 1 行
    pub fn size(&self) -> (u16, u16);                 // (cols, rows)
    pub fn feed(&mut self, bytes: &[u8]) -> bool;     // フレームが 1 枚以上完成したら true
    pub fn resize(&mut self, cols: u16, rows: u16);   // 同寸法なら何もしない
    pub fn render(&self, area: Rect, buf: &mut Buffer);
}
struct TctRewriter { … }                              // rewrite(input, out) -> (used, frame_end)
fn push_incremented(params: &[u8], out: &mut Vec<u8>);
pub fn render_screen(screen: &vt100::Screen, area: Rect, buf: &mut Buffer);
fn convert_color(color: vt100::Color) -> ratatui::style::Color;
```

`src/video.rs`

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecoderKind { Kitty, Text }

enum Decoder {
    Kitty { parser: ApcParser, assembler: FrameAssembler },
    Text(TextScreen),
}

impl VideoSink {
    pub fn new(geometry: Geometry) -> Self;                       // 現行互換(Kitty)
    pub fn with_kind(kind: DecoderKind, geometry: Geometry) -> Self;
    pub fn kind(&self) -> DecoderKind;
    pub fn feed(&self, bytes: &[u8]) -> bool;                     // Decoder で分岐
    pub fn take(&self) -> Option<Pending>;                        // Kitty の保留だけ(Text では常に None)
    pub fn resize(&self, geometry: Geometry);                     // reset(self.kind(), geometry)
    pub fn reset(&self, kind: DecoderKind, geometry: Geometry);   // デコーダを作り直す。Kitty なら pending.clear = true
    pub fn render_text(&self, area: Rect, buf: &mut Buffer);      // Text のときだけ描く
    // request_redraw / clear_redraw / geometry は現行のまま
}

impl Geometry {
    pub fn tct_options(&self) -> [(&'static str, Value); 2];      // ("width", cols), ("height", rows)
    pub fn tct_args(&self) -> Vec<String>;                        // --vo-tct-width= / --vo-tct-height=
}
```

`src/display.rs`

```rust
pub enum DisplayMode { #[default] Embedded, Text, Window }
impl DisplayMode {
    pub fn next(self) -> Self;                                    // Embedded → Text → Window → Embedded
    pub fn from_current_vo(vo: Option<&str>) -> Option<Self>;     // "kitty" / "tct" / それ以外
    pub fn decoder_kind(self) -> Option<DecoderKind>;             // Window は None
    pub fn label(self) -> &'static str;                           // 埋め込み / テキスト / 別ウィンドウ
    pub fn key(self) -> &'static str;                             // embedded / text / window
    pub fn from_key(key: &str) -> Option<Self>;
}

pub struct LaunchPlan { pub mode, pub geometry, pub fps_cap, pub window, pub speed: Speed, pub extra_args }

/// from → to の切替コマンド列。fps 上限フィルタの出し入れだけが from に依存する。
pub fn switch_commands(from: DisplayMode, to: DisplayMode, geometry: Geometry, cap: Option<FpsCap>, window: &WindowOptions) -> Vec<MpvCommand>;
```

`src/mpv.rs`

```rust
pub const REQ_SPEED: u64 = 7;
pub fn set_tct_option(key: &str, value: Value) -> MpvCommand;    // vo-tct-{key}
pub fn resize_text_video(geometry: Geometry) -> [MpvCommand; 4]; // width, height, vid no, vid auto
```

`src/actions.rs`

```rust
pub async fn change_speed(app: &mut App, session: &mut Session, steps: i8);   // set_speed(app.speed.stepped(steps))
pub async fn reset_speed(app: &mut App, session: &mut Session);               // set_speed(Speed::NORMAL)
async fn set_speed(app: &mut App, session: &mut Session, next: Speed);        // 同値なら送らない。Ok で app.speed = next、Err で app.error
pub async fn cycle_display_mode(app: &mut App, session: &mut Session);        // toggle_display_mode の後継
```

`src/settings.rs`

```rust
#[derive(Debug, Default, Clone, PartialEq, Deserialize, Serialize)]
pub struct RawCookies { pub browser: Option<String> }              // フィールドはこれだけ。増やすときは §2-9 を読み直す

pub struct Settings { …, pub cookies: Option<CookieSource> }

/// 環境変数による上書き。全て Option<&str>(未設定 = None)。
#[derive(Debug, Default, Clone, Copy)]
pub struct EnvOverrides<'a> { pub fps_limit: Option<&'a str>, pub cookies: Option<&'a str> }

pub fn validate(raw: RawConfig, env: EnvOverrides) -> (Settings, Vec<String>);
pub fn parse_cookies_env(raw: Option<&str>) -> Option<Option<CookieSource>>;  // None / Some(None)=none / Some(Some)
pub fn load_from(path: Option<&Path>, env: EnvOverrides) -> Loaded;
```

`src/cookies.rs`

```rust
impl CookieSource { pub fn from_spec(value: Option<&str>) -> Option<Self>; }   // 旧 from_env_value
impl CookieState { pub fn from_source(source: Option<CookieSource>) -> Self; } // Some → Armed、None → Off
```

### 3-4. フロー

速度変更(キー `]`):

1. `handle_key_playing` → `speed_step(']')` = +1 → `change_speed(app, session, 1)`
2. `next = app.speed.stepped(1)`。`next == app.speed`(4.0x で押した)なら終了
3. `session.player` が無ければ終了。あれば `next.command()` を送る
4. Ok → `app.set_speed_sent(next, now)`(値と送信時刻)。Err → `app.error`
5. 次の描画でステータス行に `next.label()`。毎秒のポーリングが `speed` を返し、同じ値なら保持窓を解いてそのまま入る

表示モード切替(キー `w`):

1. `from = app.display`、`to = from.next()`
2. `geometry = video_geometry(max_pixels)`(現在の端末寸法)
3. `switch_commands(from, to, geometry, fps_cap, window)` を `send_all`
4. Ok → `to.decoder_kind()` が Some なら `video.reset(kind, geometry)`、`app.display = to`。`from == Embedded` なら `session.owe_clear = true`。Err → `app.error`(モードもデコーダも変えない)
5. 以後の描画: Embedded は何も描かず `present_video` が APC を書く。Text は `draw_playing` が `render_text`。Window はプレースホルダ

リサイズ(デバウンス後):

1. `geometry_for(cols, rows, cell, max_pixels)` が現在と違えば `video.resize(geometry)`(デコーダ作り直し)
2. `match app.display`: Embedded → `resize_video`、Text → `resize_text_video`、Window → 送らない

起動時の cookie:

1. `settings::load()` が `EnvOverrides { fps_limit: env, cookies: env }` で `load_from`
2. `validate` が `[cookies] browser` → `CookieSource::from_spec`、`parse_cookies_env` が Some なら上書き
3. `main.rs` が `CookieState::from_source(settings.cookies.clone())`。以後は現行の Armed → Active / Suspended の遷移

### 3-5. 状態と失敗の扱い

| 状況 | 扱い |
|---|---|
| 速度の送信失敗 | `app.error` に文言、`app.speed` は変えない(切替と同じ方針) |
| ポーリングの `speed` が範囲外(mpv 側で 8.0 等) | 表示を `4.0x` にし、同じ値を mpv へ送り返して範囲に戻す |
| ポーリングの `speed` が `None`(property unavailable) | 触らない |
| ローカルで速度を送った直後のポーリング | `SPEED_HOLD` の間、食い違う値は捨てる(§2-3) |
| 切替の送信失敗 | `app.display` も sink のデコーダも変えない(`video.reset` は `send_all` 成功後に行う) |
| Text 中に kitty のフレームが来る / Embedded 中に tct の CSI が来る | 前者は vt100 が読み飛ばす(§1-5)、後者は `ApcParser` が捨てる(現行)。両方向とも画面は汚れない |
| `[display] mode = "text"` で起動して Kitty 対応端末にいる | そのまま動く。`w` で埋め込みへ(kitty オプションは切替列が全て送る) |
| `[cookies] browser` が空 | 指定なし + notice `[cookies] browser が空です。指定なしとして扱います` |
| 環境変数 `none` | Off。notice は出さない(fps の `0` と同じ) |
| 環境変数と設定ファイルが違う値 | 環境変数。notice は出さない(ステータス行の `cookies: <browser>` で分かる) |

## 4. 設定ファイル仕様

TOML の変更点(既存キーは変えない):

| セクション / キー | 型 | 値 | 既定 | 機能 |
|---|---|---|---|---|
| `[display] mode` | string | `"embedded"` / `"window"` / **`"text"`** | `"embedded"` | 2 |
| **`[cookies] browser`** | string | yt-dlp の `BROWSER[+KEYRING][:PROFILE][::CONTAINER]` | 無し(連携 Off) | 3 |

速度は設定ファイルに持たない(§9 に既定値キーの案)。

環境変数の優先順位(`TUITUBE_FPS_LIMIT` と同じ 3 値):

| `TUITUBE_COOKIES_FROM_BROWSER` | 結果 |
|---|---|
| 未設定 / 空 / 空白のみ | 設定ファイルの値 |
| `none`(大文字小文字を問わない) | 連携 Off(設定ファイルの値を無視) |
| それ以外 | その値で上書き |

`render` が出すテンプレート(該当部分。`[mpv]` の前に置く):

```toml
[display]
# 再生開始時の表示。"embedded" = TUI 内に埋め込み(Kitty graphics protocol)、"text" = 文字ブロック(Kitty 非対応端末向け)、
# "window" = mpv の別ウィンドウ。再生中は w で順に切り替え。
mode = "embedded"

[cookies]
# YouTube のログイン連携。yt-dlp の --cookies-from-browser に渡すブラウザ指定 (BROWSER[+KEYRING][:PROFILE][::CONTAINER])。
# 例: "chrome" / "safari" / "firefox" / "chrome:Profile 1"。ここに入るのはブラウザ名だけで、cookie の値は保存しない。
# 環境変数 TUITUBE_COOKIES_FROM_BROWSER があればそちらが優先 ("none" で一時的に連携を切る)。
# browser = "chrome"
```

値があるときは `browser = "chrome:Profile 1"` の行になる(現行 `string_line` と同じ)。`render` → `parse` → `validate` の往復で `Settings` が一致すること(既存 `render_round_trips_through_parse` に `cookies` を足す)。

## 5. 表示文言

| 場所 | 文言 |
|---|---|
| ステータス行(再生中) | `PLAYING  <title>  00:30 / 01:00  vol 70  1.5x  [埋め込み 15fps medium]`。速度は常に出す(1.0x も) |
| 表示モードのラベル | `[埋め込み 15fps medium]` / `[テキスト 15fps]` / `[別ウィンドウ]`。切替中は `(切替中)` を付ける(現行) |
| ヘルプ行(再生中) | `space:一時停止  ←→/クリック:シーク  ↑↓:音量  [ ]:速度±0.1  BS:等速  w:<次モード名>  Esc:停止  q:終了`(約 95 桁。80 桁の端末では右端が切れる。現行の約 100 桁と同程度) |
| 別ウィンドウ中のプレースホルダ | `別ウィンドウで再生中  w: 埋め込みへ`(次モード名で組む) |
| cookie 無しでフィードを断る | `<フィード名> には cookie 連携が必要です。設定ファイルの [cookies] browser か環境変数 TUITUBE_COOKIES_FROM_BROWSER を設定してください` |
| `[cookies] browser` が空 | `[cookies] browser が空です。指定なしとして扱います` |
| `[display] mode` の綴り違い | 現行どおり `[display] mode="…" は読めません。embedded で再生します` |

## 6. テスト戦略

外部プロセス無しで書けるものから始める。既存テストの書き方(1 テスト = 1 文の snake_case、日本語のコメントで意図)に合わせる。偽の player(`actions.rs` テストの `Recorder`)と偽の yt-dlp(`search.rs` テストの `FakeYtDlp`)は既にある。

### 6-1. 最初に書く Red(モジュール別)

機能 1 `src/speed.rs`

| テスト | 検証内容 |
|---|---|
| `speed_rejects_tenths_outside_one_to_forty` | `from_tenths(0)` / `(41)` が None、`(1)` / `(40)` が Some。`MIN.tenths() == 1`、`MAX.tenths() == 40`、`NORMAL.value() == 1.0`、`Speed::default() == NORMAL` |
| `speed_from_f64_rounds_to_a_tenth_and_rejects_out_of_range` | `from_f64(1.25)` = 1.3x(四捨五入)、`(0.04)` = None、`(0.05)` = 0.1x、`(4.04)` = 4.0x、`(4.05)` = None、`(f64::NAN)` / `(f64::INFINITY)` / `(-1.0)` = None |
| `a_polled_value_is_rounded_and_reports_whether_it_was_clamped` | `Polled::from_f64(2.75)` = 2.8x/false、`(8.0)` = MAX/true、`(0.01)` = MIN/true、`(f64::NAN)` = NORMAL/false |
| `speed_steps_saturate_at_the_bounds` | `MAX.stepped(1) == MAX`、`MIN.stepped(-1) == MIN`、`NORMAL.stepped(5)` = 1.5x、`NORMAL.stepped(-9)` = 0.1x、`NORMAL.stepped(-10)` = MIN(飽和) |
| `speed_label_has_one_decimal_and_a_trailing_x` | `"1.0x"` / `"0.1x"` / `"4.0x"` / `"1.5x"`。`decimal()` は `"1.0"` / `"0.1"` |
| `speed_command_sets_the_property_with_the_decimal_value` | `Speed::from_tenths(15).command().to_line()` == `{"command":["set_property","speed",1.5]}\n`、`NORMAL` は `…,1.0]`、`MIN` は `…,0.1]` |
| `speed_launch_arg_is_omitted_at_normal_speed` | `NORMAL.launch_arg() == None`、1.5x は `Some("--speed=1.5")` |

機能 1 `src/mpv.rs` / `src/display.rs` / `src/app.rs`

| テスト | 検証内容 |
|---|---|
| `poll_asks_for_speed_every_time`(既存 `poll_asks_for_current_vo_every_time` を拡張) | 行に `{"command":["get_property","speed"],"request_id":7}` が含まれ、`len() == 6` |
| `launch_args_carry_the_speed_only_when_it_is_not_normal` | `LaunchPlan { speed: 1.5x, … }.args()` に `--speed=1.5` が `extra_args` の直前に入る。`NORMAL` では `--speed` が無い。既存 `extra_args_come_last_in_the_plan` は通ったまま |
| `plan_starts_at_normal_speed` | `LaunchPlan::new(…).speed == Speed::NORMAL` |
| `speed_is_applied_from_the_poll_and_rounded` | `apply_property(REQ_SPEED, json!(2.75))` → `app.speed` = 2.8x。`json!(8.0)` → MAX。`None` → 変わらない |
| `the_status_line_shows_the_speed_after_the_volume` | `speed = 1.5x` のとき `playing_status()` に `vol 70  1.5x  [` の並びがある。既定でも `1.0x` が出る |

機能 1 `src/input.rs` / `src/actions.rs`(偽 player)

| テスト | 検証内容 |
|---|---|
| `bracket_keys_step_the_speed_and_are_not_plain_mpv_commands` | `speed_step(Char('['))` = Some(-1)、`(Char(']'))` = Some(1)、`(Char('x'))` = None。`playing_command(Char('['))` / `(Char(']'))` / `(Backspace)` = None |
| `speed_keys_change_only_while_playing` | Results で `]` を押しても `app.speed` 不変。Input では query に `]` が入る |
| `change_speed_sends_set_property_and_updates_the_app` | `Recorder` 入り、`change_speed(+1)` → 送信列 `[set_property speed 1.1]`、`app.speed` = 1.1x、`error` 無し |
| `change_speed_at_the_bound_sends_nothing` | `app.speed = MAX` で `+1` → 送信列が空、`app.speed == MAX` |
| `reset_speed_returns_to_normal_and_is_idempotent` | 1.5x から `reset_speed` → `[set_property speed 1.0]`、`NORMAL`。もう一度 → 送信なし |
| `change_speed_keeps_the_value_when_sending_fails` | `Recorder` が Err → `app.speed` 不変、`error` に文言 |
| `change_speed_without_a_player_changes_nothing` | player 無しで `+1` → `app.speed` 不変、`error` 無し |
| `start_playback_passes_the_current_speed_to_the_plan` | `app.speed = 1.5x` で組んだ `LaunchPlan` の `speed` が 1.5x(`start_playback` は mpv を起動するので、plan を組む部分を関数に切り出して検証する) |
| `backspace_resets_the_speed_while_playing` | `handle_key_playing(Backspace)` が `reset_speed` を呼ぶ(`Recorder` で送信列を見る) |
| `playing_help_mentions_the_speed_keys` | `help_text(Playing, Embedded)` に `[ ]` と `BS` と `速度` |

機能 2 `src/tct.rs`(削除前 `video.rs` のテスト 21 本を復元 + 1 本)

| テスト | 出典 / 検証内容 |
|---|---|
| `transfers_true_color_half_blocks` `control_sequences_do_not_leak_into_cells` `mpv_row_addressing_keeps_every_row` `every_row_survives_the_newline_mpv_puts_at_the_end_of_a_frame` `frames_after_the_first_are_not_shifted_by_the_trailing_newline` `horizontal_offset_is_not_shifted_left` `video_is_placed_at_the_area_origin` `clips_to_the_smaller_of_area_and_screen` `maps_every_color_kind` `indexed_colors_survive_the_round_trip` `size_reports_columns_then_rows` `hvp_becomes_cup_with_one_based_parameters` `hvp_split_across_reads_is_still_rewritten` `overlong_parameters_are_passed_through_untouched` `rewrite_stops_at_the_frame_boundary` `feed_reports_only_completed_frames` `half_written_frames_are_not_rendered` `several_frames_in_one_read_are_all_parsed` `resize_changes_the_grid_and_drops_the_stale_frame` `resize_to_the_same_size_keeps_the_current_frame` `feeding_in_chunks_matches_a_single_feed` | `git show 0877d16:src/video.rs` からそのまま。`VideoScreen` → `TextScreen`(`&mut self`)、`redraw_requests_are_collapsed_until_cleared` は `video.rs` に残っているので復元しない |
| `text_screen_ignores_kitty_leftovers`(新規) | §1-3 の実測列 `ESC_Ga=d;ESC\ ESC_Ga=d; ESC[?25h ESC[?1003l ESC[20;0f` の後に tct のプロローグと 1 フレームを流し、セルにゴミが入らずフレームが正しい位置に出る。5,000 バイトの APC も同様 |

機能 2 `src/video.rs`

| テスト | 検証内容 |
|---|---|
| `geometry_tct_options_are_the_cell_size_of_the_area` | 80×22 の `tct_options()` == `[("width", 80), ("height", 22)]`、`tct_args()` == `["--vo-tct-width=80", "--vo-tct-height=22"]`。ピクセル予算に依存しない |
| `a_text_sink_feeds_frames_to_the_text_screen_and_has_no_kitty_pending` | `with_kind(Text, g)` に tct の 1 フレームを `feed` → true。`take()` は None。`render_text` で `▄` が出る。`kind() == Text` |
| `a_kitty_sink_does_not_render_text` | 現行 `new(g)` に kitty フレームを feed → `render_text` は何も描かない、`take()` に frame |
| `reset_to_text_drops_the_kitty_state_and_owes_no_clear_from_the_text_side` | kitty 途中フレームを feed → `reset(Text, g)` → 続きのチャンクを feed しても `take()` は clear だけ(直前の kitty 保留の clear)。`kind() == Text` |
| `reset_to_kitty_from_text_starts_a_fresh_parser_and_owes_a_clear` | Text で 1 フレーム → `reset(Kitty, g)` → `take()` の `clear == true`、`render_text` は何も描かない |
| `resize_keeps_the_decoder_kind` | Text の sink を `resize` → `kind()` は Text、`TextScreen::size()` が新寸法 |
| `text_leftovers_do_not_disturb_the_kitty_decoder` | Kitty の sink に `ESC[?25h ESC[?1003l ESC[?1049l` を feed → false、`take()` None。続く kitty フレームは通る |

機能 2 `src/display.rs`

| テスト | 検証内容 |
|---|---|
| `display_mode_cycles_embedded_text_window` | `Embedded.next() == Text`、`Text.next() == Window`、`Window.next() == Embedded` |
| `display_mode_is_read_back_from_current_vo`(既存を拡張) | `Some("tct")` → Text。kitty / gpu-next は現行どおり |
| `display_mode_keys_and_labels_cover_text` | `from_key("text") == Some(Text)`、`Text.key() == "text"`、`Text.label() == "テキスト"`、`from_key("Text") == None` |
| `decoder_kind_is_none_only_for_the_window` | Embedded → Kitty、Text → Text、Window → None |
| `text_launch_args_use_tct_with_the_cell_size_and_the_fps_cap` | `LaunchPlan { Text, 80×22, cap 15 }.args()` に `--vo=tct`、`--vo-tct-width=80`、`--vo-tct-height=22`、`--vf-append=@tuitube-cap:…`。`--vo-kitty-` と `--title` は無い |
| `switch_to_text_from_embedded_sends_tct_size_then_vo_without_touching_the_cap` | 列 == `[vo-tct-width 80, vo-tct-height 22, set_property vo "tct"]` |
| `switch_to_text_from_the_window_adds_the_cap_back` | 列 == `[vo-tct-width, vo-tct-height, vf add @tuitube-cap:…, vo "tct"]`。cap None なら add 無し |
| `switch_to_embedded_from_text_sends_the_kitty_options_and_keeps_the_cap` | 8 個の kitty オプション + `vo "kitty"`、`vf add` 無し |
| `switch_to_embedded_from_the_window_matches_the_current_sequence`(既存 `to_embedded_commands_send_geometry_then_cap_then_vo` の移設) | 現行の 10 個の列 |
| `switch_to_the_window_removes_the_cap_from_either_terminal_mode`(既存 `to_window_commands_remove_the_cap_before_switching_the_vo` の移設) | from = Embedded / Text とも `[vf remove, vo ""]`。cap None なら `[vo ""]`。`window.vo = Some("gpu")` なら `vo "gpu"` |

機能 2 `src/mpv.rs` / `src/app.rs` / `src/ui.rs` / `src/settings.rs`

| テスト | 検証内容 |
|---|---|
| `resize_text_video_sets_tct_size_then_reinitializes_the_video` | 列 == `[set_property vo-tct-width 80, vo-tct-height 22, vid "no", vid "auto"]`(削除前 `resize_sets_size_then_reinitializes_the_video` の復元) |
| `display_label_for_text_shows_the_fps_without_a_quality` | `display = Text` + cap 15 → `[テキスト 15fps]`。cap None → `[テキスト]`。`current_vo = Some("kitty")` なら `(切替中)` |
| `help_text_names_the_next_display_mode` | Embedded → `w:テキスト`、Text → `w:別ウィンドウ`、Window → `w:埋め込み` |
| `window_placeholder_names_the_next_mode` | プレースホルダに `w: 埋め込みへ` |
| `video_area_layout_is_unchanged_by_the_display_mode`(既存) | Text でも同じ矩形 |
| `display_mode_text_is_parsed_and_rendered` | `[display]\nmode = "text"` → `Text`。`render` の往復で保たれる。テンプレートのコメントに `"text"` |

機能 2 `src/actions.rs`(偽 player。既存の toggle テスト 5 本を 3 モードに更新)

| テスト | 検証内容 |
|---|---|
| `cycle_from_embedded_to_text_resets_the_sink_and_owes_a_clear` | `display = Embedded`、sink Kitty → `cycle` → 送信列が `switch_commands(Embedded, Text, …)` と一致、`display == Text`、`video.kind() == Text`、`owe_clear == true` |
| `cycle_from_text_to_the_window_removes_the_cap_and_owes_no_clear` | `display == Window`、`owe_clear == false`(kitty の画像は無い)、sink の kind は Text のまま(Window では触らない) |
| `cycle_from_the_window_to_embedded_matches_the_current_behaviour`(既存 `toggle_back_to_embedded_…` の更新) | 現行と同じ列、`video.kind() == Kitty` |
| `cycle_keeps_the_mode_when_sending_fails`(既存の更新) | `display` 不変、`error` に文言 |
| `cycling_without_a_player_changes_nothing`(既存の更新) | 不変 |
| `resize_in_text_mode_sends_the_tct_resize_sequence` | `display = Text` で `apply_resize` → 列 == `resize_text_video(geometry)` |
| `resize_in_the_window_mode_sends_nothing`(既存) | 変更なし |
| `start_playback_picks_the_decoder_from_the_display_mode` | plan / sink を組む部分を関数に切り出し、`display = Text` → `kind() == Text`、`Window` → Kitty |

機能 3 `src/settings.rs`

| テスト | 検証内容 |
|---|---|
| `cookies_browser_is_read_into_a_cookie_source` | `[cookies]\nbrowser = "chrome:Profile 1"` → `settings.cookies == Some(CookieSource::from_spec(Some("chrome:Profile 1")))`、notice 無し |
| `a_blank_cookies_browser_is_ignored_with_a_notice` | `browser = ""` / `"  "` → None + notice に `[cookies] browser`。同じファイルの他のキーは効く |
| `an_absent_cookies_section_means_no_cookies` | `""` → None、notice 無し(`Settings::default().cookies == None`) |
| `the_cookies_environment_variable_overrides_the_file` | ファイル `chrome` + env `"safari"` → safari。env `""` / `"  "` / None → chrome。env `"none"` / `"NONE"` → None |
| `parse_cookies_env_has_three_outcomes` | None / `""` → None、`"none"` → Some(None)、`" firefox "` → Some(Some(firefox))(trim される) |
| `render_writes_the_cookies_section_and_only_the_browser_spec` | `render` に `[cookies]` と `browser = "chrome:Profile 1"`。None なら `# browser = "chrome"`。`render_round_trips_through_parse` の custom に `cookies` を足す |
| `render_says_that_cookie_values_are_not_stored` | テンプレートのコメントに `cookie の値は保存しない` |
| `load_from_without_a_path_still_applies_the_cookies_variable`(既存 fps 版と対) | `load_from(None, EnvOverrides { cookies: Some("chrome"), .. })` → Some(chrome) |
| 既存テスト全部 | `validate(…, None)` → `validate(…, EnvOverrides::default())` への機械的な置換で通る |

機能 3 `src/cookies.rs` / `src/main.rs`

| テスト | 検証内容 |
|---|---|
| `from_spec_trims_and_rejects_blank`(既存 `from_env_value_trims_and_rejects_blank` の改名) | 同内容 |
| `from_source_arms_when_present_and_is_off_otherwise` | `CookieState::from_source(Some(src)) == Armed(src)`、`from_source(None) == Off` |
| `refusal_names_the_config_key_and_the_environment_variable` | `Off.refusal(History)` に `[cookies] browser` と `ENV_VAR` |
| main.rs のテスト | `CookieSource::from_env_value` → `from_spec` の置換のみ |

### 6-2. 偽 controller での結合テスト

`actions.rs` の `Recorder`(送った `MpvCommand` を溜める `PlayerSink`)をそのまま使う。§6-1 の actions 行がこれに当たる。`VideoSink` は実物を使う(外部プロセス不要)。

### 6-3. 手動確認(実機)

| 項目 | 期待 |
|---|---|
| 再生中 `]` ×5 | ステータス行 `1.5x`、音程はそのまま、体感で速い |
| `[` で `0.1x` まで下げてさらに `[` | 表示・再生とも変わらない。エラーなし |
| Backspace | `1.0x` |
| 次の動画を Enter | 前の速度で始まる。ステータス行に出ている |
| 別ウィンドウで mpv 側の `]` | 1 秒以内に `1.1x`(mpv の 1.1 が丸められた値) |
| `w` ×1(埋め込みから) | 文字ブロックの映像。ステータス `[テキスト 15fps]`。kitty の画像が残っていない |
| `w` ×2 | 別ウィンドウ。文字ブロックが消えてプレースホルダ `w: 埋め込みへ` |
| `w` ×3 | 埋め込み。fps 上限が効いている |
| `mode = "text"` で起動して再生 | 最初から文字ブロック。`w` で別ウィンドウ、もう一度で埋め込み |
| テキスト中に端末リサイズ | 新しい文字数で描き直る。ステータス行・ヘルプ行に被らない |
| テキスト中にシークバーをクリック | シークする。マウス移動でホバー表示 |
| tmux 内でテキスト | 色が出る(tmux が RGB を通す設定のとき) |
| CPU(テキスト、15fps 上限、139×33 セル) | 埋め込み kitty の同条件(前設計 §1-5 C1: 34.7-39.7%)と比べて記録する。tuitube 側(vt100)の CPU も `top` で見る |
| `[cookies] browser = "chrome"` で起動 | ステータス行 `cookies: chrome`。`:ytrec` が取れる |
| `TUITUBE_COOKIES_FROM_BROWSER=none` | cookie 表示なし。`:ytrec` は設定ファイルのキー名を含む文で断られる |
| `~/.config/tuitube/config.toml` を消して起動 | テンプレートに `[cookies]` と `"text"` の説明がある |

## 7. 実装順序と依存

| 順 | 機能 | 理由 | 完了条件 |
|---|---|---|---|
| 1 | 機能 3(cookie 設定) | 最小。`settings.rs` の `validate` の引数を `EnvOverrides` に変える変更を先に入れると、後続の 2 機能は settings.rs に「キーを足す」だけになる。他機能と触るファイルが重ならない(`main.rs` の 1 行を除く) | `cargo test` 全通過、手動確認の cookie 3 項目、コミット |
| 2 | 機能 1(倍速) | 新規ファイル `speed.rs` + 既存の小変更。`input.rs` / `ui.rs` / `app.rs` のステータス行・ヘルプ行を先に整えてから機能 2 が 3 モード化する(逆順だと機能 2 の 3 変種のヘルプ文言に後から速度を足す二度手間) | 全通過、手動確認の速度 5 項目、コミット |
| 3 | 機能 2(テキストモード) | 最大。`vt100` 追加、`tct.rs` 復元、`VideoSink` の内部構造変更、`DisplayMode` 3 値化(網羅 match 4 箇所)、遷移列の統合。機能 1 の `LaunchPlan.speed` が入っていることを前提に `args()` の Text 分岐を書く | 全通過、手動確認のテキスト 8 項目 + CPU 記録、コミット |

機能 1 と 2 の相互作用: 速度は VO と独立(§1-1)なので切替列に含めない。`LaunchPlan` は両方が触る(`speed` フィールドと Text 分岐)ため、順序を守れば衝突しない。機能 3 と 1・2 の相互作用は無い。

各機能の中の順序は §6-1 の表の上から(値オブジェクト / 純粋関数 → App の状態 → actions(偽 player)→ input / ui の文言 → settings のテンプレート)。1 機能ずつ `cargo test` が緑になったところでコミットする。

## 8. 実装前に確認したいこと

| # | 項目 | 案 | 推奨 |
|---|---|---|---|
| 1 | 速度キー | `[` `]` + Backspace / `-` `=` + Backspace | `[` `]`(mpv と同じ位置、シフト不要) |
| 2 | 速度を動画をまたいで持ち越す | 持ち越す(`--speed=` で起動)/ 動画ごとに 1.0x | 持ち越す(§2-4)。変える場合は `plan.speed = app.speed` の 1 行を外し、`end_playback` で `app.speed = NORMAL` にする |
| 3 | ステータス行の速度表示 | 常に出す / 1.0x のときは出さない | 常に出す(戻ったことが分かる。5 桁) |
| 4 | `w` の循環順 | Embedded → Text → Window / Embedded → Window → Text / 別キー `t` | Embedded → Text → Window(§2-6。途中で GUI ウィンドウを経由しない) |
| 5 | 環境変数の無効化値 | `none` を設ける / 設けない(空 = 上書きしない、のみ) | 設ける(fps の `0` と同じ 3 値。設定ファイルを書き換えずに cookie 無しで試せる) |
| 6 | `from_env_value` の改名 | `from_spec` に改名(14 箇所置換)/ 名前を残す | 改名(環境変数以外からも作るので名前が嘘になる) |
| 7 | ヘルプ行の幅 | §5 の約 95 桁 / 2 行に分ける(映像領域が 1 行減る) | 95 桁で 1 行。80 桁端末で右端が切れるのは現行(約 100 桁)と同じ |
| 8 | `vt100` の追加 | 0.15(キャッシュ済み、オフラインで通る) | 追加 |
| 9 | `validate` の引数 | `EnvOverrides` 構造体 / `Option<&str>` を 2 つ並べる | 構造体(環境変数が増えても引数が増えない) |
| 10 | Text の fps 上限 | kitty と同じ `fps_cap` を使う / 別キー | 同じ(設定キーを増やさない。`[playback] fps_cap` のコメントを「埋め込み・テキスト表示の」に直す) |

## 9. 会話に出ていないが追加検討してほしい要素

合意済み仕様の外。採否は別途。

| 項目 | 内容 | 根拠 | 実装コスト |
|---|---|---|---|
| 粗い速度キー | `{` `}` で ±1.0(または ±0.5) | 1.0x → 2.0x に `]` 10 回は多い。mpv は `{` `}` を半分 / 2 倍に使っている | `speed_step` の表に 2 行。ヘルプ行がさらに長くなる |
| 速度の既定値 | `[playback] speed = 1.0`(起動時の `App.speed`) | 常に 1.25x で見る人向け。`Speed::from_f64` で検証、範囲外は notice | settings に 1 キー、`render` に 1 行 |
| テキストモードの 256 色 | `[text] colors = "truecolor" / "256"` → `--vo-tct-256=yes` / `vo-tct-256` プロパティ | Terminal.app は truecolor 非対応で、テキストモードの主な利用先(Kitty 非対応端末)と重なる。vt100 は Idx 色も取れる(削除前 `indexed_colors_survive_the_round_trip`) | settings に 1 セクション 1 キー、`LaunchPlan` / `switch_commands` の Text 分岐に 1 引数 |
| テキストモードのアルゴリズム | `--vo-tct-algo=plain`(半ブロックでなく 1 セル 1 画素) | 縦解像度が半分になるが古い端末で `▄` が崩れるとき用 | 上と同じ場所に 1 引数 |
| 画質切替キー | 前設計 §8 #8 の `[` `]` 案は速度に使うので、付けるなら `-` `=` 等 | 未実装の案の候補キーを更新するだけ | — |
| mpv ウィンドウ側の速度キーとの刻み違い | mpv の `[` `]` は ×0.9 / ×1.1。tuitube と同じ ±0.1 にするなら `--input-conf` か `[mpv] extra_args` で `input.conf` を差し込む | 別ウィンドウでフォーカスが mpv に移ったときの操作感の差 | 設定ファイルで利用者が対応できる範囲。tuitube 側では扱わない |
| Kitty 非対応端末の自動判定 | 起動時に端末を判定して Text を既定にする | 設定なしで動く | 判定手段(`TERM` / `TERM_PROGRAM` / kitty の query)が端末ごとに違い、tmux 越しは判定できない。本設計では設定ファイルで明示する |
