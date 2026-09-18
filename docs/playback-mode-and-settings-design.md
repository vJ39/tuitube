再生表示モード(TUI 内埋め込み / 別ウィンドウ)の切替、fps 上限の静的化、埋め込み画質の設定化、および設定ファイルの新設についての設計。実装は TDD(Red→Green)で進める前提で、外部プロセス(mpv・yt-dlp)を伴わない純粋関数の単位を先に切り出す。

対象バージョン: mpv 0.41.0 / yt-dlp 2026.08.19 / ratatui 0.29 / crossterm 0.28 / macOS 14.8.2 / iTerm2。行番号の参照はコミット `675b4c6` 時点。

cookie 連携(`docs/google-account-cookies-design.md`)の実装と並行する。本書は cookie 連携が入る前の `search.rs` / `mpv.rs` を前提に書く。両者が同じ関数に触る箇所は §7 に列挙し、統合の細部は実装フェーズで調整する。

## 0. 合意済み仕様

1. 別ウィンドウ表示モードを追加する。mpv を VO 指定なし(mpv 既定の GPU レンダラー)で起動し、再生中にキー操作で「埋め込み」⇔「別ウィンドウ」を切り替えられる
2. fps 上限を動的(再生中に `container-fps` を取得して `vf add fps=N`)から静的(mpv 起動時に `--vf` で付与)に変える
3. 埋め込み表示の画質(現状 `MAX_FRAME_PIXELS = 640*360` 固定)を段階または数値で設定できるようにする
4. 上記の既定値を保存するファイルベースの設定を新設する(TOML、XDG 準拠の `~/.config/tuitube/config.toml`)

前提として共有済みの実測: CPU 負荷の主要因はデコード(元動画の解像度・フレームレート)であり、VO 側の出力解像度やアルゴリズムを変えても CPU はほとんど変わらない。効くのは起動時の静的 fps 制限のみ。したがって画質設定は「表示の細かさ」の設定であって「軽さ」の設定ではない。UI の文言・設定ファイルのコメントもこの前提で書く(§5)。

## 1. 実装が依存する事実

2026/09/18 に実機で確認した内容。yt-dlp の実行は `--ignore-config` 付き・cookie/ブラウザ関連オプション無し。mpv のローカル検証は `av://lavfi:testsrc` を入力にし、音声は `--ao=null`。

### 1-1. yt-dlp の出力に fps が入る条件

| コマンド | fps の所在 | 所要時間 | 備考 |
|---|---|---|---|
| `ytsearch3:<q> --flat-playlist --dump-json`(現行の検索) | **無い**。キー一覧に `fps` / `formats` / `height` / `width` が一切含まれない | 11.1 秒 | 起動コスト約 10 秒(スタンドアロン版 yt-dlp)を含む |
| `ytsearch3:<q> --dump-json`(flat 無し) | top-level `fps`(yt-dlp が既定選択した映像フォーマットの値)、`height`、`formats[].fps` | 14.1 秒 | 1 件あたり約 1 秒の個別抽出が直列に走る |
| `ytsearch10:<q> --dump-json`(flat 無し) | 同上 | 19.8 秒 | 現行 `SEARCH_TIMEOUT` 30 秒(actions.rs L14)には収まるが、毎検索 +9 秒 |
| `--dump-json <watch URL>`(1 本) | 同上。`requested_formats` に選択された映像/音声の `fps`/`height` | 14.8 秒 | 再生前に 1 本だけ調べる案のコスト |

flat 無し 10 件の内訳: 選択フォーマットの fps は 30 が 8 件・60 が 2 件、全て高さ 1080 以上(1 件は 2160、1 件は 1912)。`formats` に 15fps のフォーマットを持つものが 2 件あるが、それは低解像度側の派生で、360p のフォーマットは 10 件とも 30fps だった。

設計への影響: 「検索結果から fps を取る」には flat を外す必要があり、検索が毎回 +9 秒遅くなる(§2-2)。fps を知らなくても目的(埋め込み時の VO 負荷を抑える)を達成できる方式があるため(§1-4)、本設計では検索結果に fps を持たせない。

### 1-2. mpv の GUI ウィンドウ起動

現行 `MpvController::launch`(mpv.rs L393-404)と同じ条件(`--no-terminal`、stdin は null、stdout/stderr はパイプ)から `--vo=kitty` と `--vo-kitty-*` を外して起動した結果:

| 項目 | 結果 |
|---|---|
| ウィンドウ | 開く。`current-vo` = `gpu-next`、`vo-configured` = true、`display-names` = `["Built-in Retina Display (…)"]`、`display-fps` = 60.0024 |
| stdout への書き込み | 0 バイト(GPU VO は端末に何も書かない) |
| `--no-terminal` の影響 | 無し。外す必要はない |
| 追加で必要なオプション | 無し。`--force-window` も不要(映像があれば開く) |
| 前面(フォーカス)の移動 | 一貫しない。1 回目は mpv が前面に来た(`lsappinfo list` で `(in front)`)、2 回目以降は iTerm2 のまま。`--focus-on=never` 指定時も iTerm2 のまま |
| `--focus-on` | 0.41.0 に存在(`never` / `open` / `all`、既定 `open`)。`--focus-on-open` は削除済み |
| ウィンドウ寸法・位置 | `--autofit` / `--geometry` / `--ontop` / `--title` が使える(`--list-options` で確認) |

設計への影響: 起動引数から `--vo=kitty` と `--vo-kitty-*` を落とすだけで別ウィンドウになる。フォーカスが mpv に移るかは環境依存なので、移っても移らなくても破綻しない操作系にする(§2-6)。

### 1-3. 再生中の VO 切替

`set_property vo <name>` を IPC で送るだけで切り替わり、mpv の再起動は不要。

| 手順 | 結果 |
|---|---|
| `--vo=kitty` で起動、3 秒後に `set_property vo gpu` | 応答 `success`。kitty の stdout 出力が止まる(3 秒で 105 フレーム → その後 2 フレームだけ増えて停止)。`current-vo` = `gpu`、ウィンドウが開く。`time-pos` は 7.0 → 15.0 と連続(再生位置は保たれる) |
| `set_property vo-kitty-width 320` / `vo-kitty-height 176` を送ってから `set_property vo kitty` | kitty 出力が再開し、新しいフレームは `s=234,v=176`(旧 `s=469,v=352`)。VO 生成時に `vo-kitty-*` を読み直すので、寸法は切替前に送っておけばよい |
| `set_property vo gpu-next` | 同様に切り替わる |
| `set_property vo ""`(空文字)/ `set_property vo []` | mpv の自動選択に戻り `current-vo` = `gpu-next`。別ウィンドウの VO 名を tuitube が決め打ちしなくてよい |
| `set_property vo nonexistent-vo` | 応答は `success` だが、ログに `[e][vo] Video output nonexistent-vo not found!` を出して **mpv が終了する**(exit 2)。現行の `MpvExited` → `end_playback` → `log_detail` の経路でこのエラー文が画面に出る |
| kitty 側の後始末 | 切替のたびに kitty VO の uninit/reconfig が `a=d`(全 placement 削除。ST 無しの形を含む)を stdout に出す。現行 `ApcParser` / `FrameAssembler` はこれを `FrameEvent::Clear` として扱うので、埋め込み画像は自動的に消える |
| `display-names` | kitty VO では `property unavailable`。GPU VO では値が入る。「今どちらで表示しているか」は `current-vo` を見る |

設計への影響: 切替 = 数個の `set_property`。現行 `resize_video`(mpv.rs L202-211)の `vid no` → `vid auto` による VO 作り直しは、`vo` の変更自体が VO を作り直すため不要。

### 1-4. fps フィルタの挙動

kitty VO の出力フレーム数(`a=T` の個数)を 4 秒間のテストソースで数えた。

| フィルタ | 30fps | 60fps | 24fps | 25fps | 15fps | 10fps | 2fps |
|---|---|---|---|---|---|---|---|
| 無し | 120 | — | — | — | — | — | 9 |
| `fps=15` | 61 | 61 | — | — | — | — | **54**(2fps を 15fps に複製) |
| `lavfi=[select=floor((t-prev_selected_t)*15+0.001)]` | 61 | 60 | 49(12fps) | 50(12.5fps) | 60 | 41 | 9(複製なし) |

- `fps=15` は定レート変換。上限より遅いソースではフレームを複製する(現行コードが `container-fps` を先に調べていた理由。mpv.rs L127-129)
- `select=floor((t-prev_selected_t)*N+0.001)` は「前に通したフレームから 1/N 秒以上経ったフレームだけ通す」上限フィルタ。遅いソースはそのまま通り、速いソースは整数分の 1 に間引かれる(24fps → 12fps、25fps → 12.5fps)。`+0.001` は `2/30*15` が浮動小数で 0.999… になり 1 フレーム余計に落ちる(実測 30fps → 約 10fps)のを防ぐ補正
- `select` の式に `,` を含む書き方(`gte(a,b)`)は mpv の `[ ]` クォートでも `%n%` クォートでも `\,` でも libavfilter 側で `No such filter: '1/15)'` になり使えない。`,` を含まない式にする
- `--vf-append=@tuitube-cap:…` は利用者の `mpv.conf` にある `vf` を残したまま末尾に足す(実測: `--vf=@user:hflip` と併用してフィルタ列が `hflip (user)` → `lavfi (tuitube-cap)`)。`--vf=` は置換なので使わない
- ラベル付きフィルタは再生中に `vf remove @tuitube-cap` / `vf add @tuitube-cap:…` で外したり戻したりできる(実測: 15fps → 30fps → 15fps と追従)。`vf toggle` も使えるが、状態を tuitube 側で持つので明示的な remove/add にする

設計への影響: ソースの fps を知らなくても、起動時に上限フィルタを静的に付けられる。`container-fps` の取得(`REQ_CONTAINER_FPS`、`FpsFilter`、`limit_fps`、`apply_fps_limit`)は不要になる。

### 1-5. CPU 実測(YouTube 実動画)

対象: `awX7DUp-r14`(1080p30、H.264 + Opus、40 分の講演動画)を `--start=120` から再生。kitty VO の出力は実端末ではなくファイルへ(端末側の描画負荷は含まない)。音声は `--ao=null`(デコードはするが出力しない)。埋め込み寸法は実機と同じ 139x34 セル / 647x356 px。CPU は mpv プロセス単体を `top -l 3 -s 4`(4 秒間隔の瞬時値 2 回)で取得。先行実測(kitty 122-151% / 静的 fps=15 で 39.9%)とは計測方法・動画・出力先が違うので絶対値ではなく比で見る。

| # | 条件 | CPU%(2 回) | kitty フレーム数 / 8 秒 | 実際にデコードした映像 |
|---|---|---|---|---|
| C0 | 埋め込み、フィルタなし | 59.3 / 65.4 | 455 | 1920x1080 30fps H.264 |
| C1 | 埋め込み + 静的 `fps=15` | 34.7 / 39.7 | 166 | 同上 |
| C2 | 埋め込み + 静的 `select` 上限 15 | 34.4 / 39.6 | 166 | 同上(C1 と同等) |
| C3 | C1 + `--ytdl-format=bv*[height<=360]+ba/b[height<=360]` | 19.9 / 17.1 | 162 | **640x360 30fps VP9** |
| C4 | C1 + `--hwdec=videotoolbox-copy` | 45.2 / 61.2 | 164 | 1920x1080、`hwdec-current` = `videotoolbox-copy` |
| C5 | 別ウィンドウ(`gpu-next`、`--autofit=640x360`) | 15.7 / 10.8 | 0 | 1920x1080 |
| C6 | C5 + `--hwdec=videotoolbox` | 10.2 / 11.9 | 0 | 1920x1080、`hwdec-current` = `videotoolbox` |
| C7a | 埋め込み、フィルタなし(別の回。系全体の負荷が高い時間帯) | 74.0 / 105.8 | 443 | 1920x1080 |
| C7b | C7a の再生中に `vf add @tuitube-cap:fps=15`(動的) | 61.6 / 60.4 | 312 | 同上 |
| C7c | C7b を外して `select` 上限を動的に追加 | 45.0 / 56.6 | (計数が不安定) | 同上 |

読み取り:

- 静的 `fps=15` と静的 `select` 上限は CPU もフレーム数も同じ。上限方式に置き換えても効果は落ちない
- 動的に足した場合(C7b)は静的(C1)より下がり方が鈍い。先行実測(動的 48.8-87% vs 静的 39.9%)と同じ傾向。kitty 出力を調べると、実行中にフィルタ列を変えた後は前フレームとバイト単位で同一のフレームが 36% 混じる(静的な回は 8-13%)。フィルタ列の変更後に mpv が同じフレームを描き直す回数が増えているが、原因は特定していない
- 元動画を 360p に絞る(C3)とデコード量が減り、fps 上限との併用で C1 の約半分になる。「主要因はデコード」という前提と整合する(§9)
- `videotoolbox-copy`(C4)は kitty VO では逆効果。1080p のフレームをシステムメモリへコピーする分が乗る。別ウィンドウ(C6)では 1-5 ポイントの差で効果は小さい
- 別ウィンドウ(C5)は埋め込みの静的上限あり(C1)よりさらに低く、フィルタ無しの埋め込み(C0)の 1/4 程度

C8: §3-4 の切替コマンド列をそのまま IPC で送った往復(同じ動画・同じ計測方法)。

| 段階 | 送ったもの | CPU%(2 回) | kitty 出力 | 同一連続フレーム |
|---|---|---|---|---|
| 1. 埋め込み(起動時に静的 `select` 上限) | — | 30.8 / 36.8 | 118 MB / 9 秒 | 12%(切替前の全体) |
| 2. 別ウィンドウ | `vf remove @tuitube-cap` → `set_property vo ""` | (この回は `top` が値を返さず。C5 参照) | 0 バイト(kitty 停止) | — |
| 3. 埋め込みへ戻す | `vo-kitty-cols/rows/width/height` → `vf add @tuitube-cap:…` → `set_property vo kitty` | 31.2 / 35.1 | 117 MB / 8 秒 | 30%(戻した後の全体) |
| 3b. その 10 秒後 | — | 32.0 / 62.5 | 118 MB / 9 秒 | (同上) |

戻した後の `current-vo` = `kitty`、`vf` に `tuitube-cap` が入り、`time-pos` は連続。CPU は起動時(段階 1)と同水準に戻る。同一連続フレームの割合は上がるが(12% → 30%)、フレーム総量(バイト数)は同じで CPU にも差が出ていない。3b の 62.5 は 1 回だけの跳ねで、同時刻にログに `h264: co located POCs unavailable`(ストリームの参照エラー)が出ている。

C7b(VO を作り直さず `vf add` だけ)で効きが鈍かったのは、フィルタ列の変更だけを行った場合。設計の戻り経路は VO を作り直す(`set_property vo kitty`)ため、こちらの数値(C8 段階 3)が当てはまる。

### 1-6. 環境

| 項目 | 値 |
|---|---|
| `XDG_CONFIG_HOME` | 未設定(→ `~/.config` に倒す) |
| `~/.config/mpv` | 無し(利用者の mpv.conf は無いが、有る環境を壊さない前提で設計する) |
| `toml` crate | ローカルの cargo registry に 1.1.4 がキャッシュ済み(`toml_edit` 0.25.13、`winnow` 1.0.4 も) |
| 実機の埋め込み寸法 | 139x34 セルの端末で `--vo-kitty-width=647 --vo-kitty-height=356`(ピクセル予算 640*360 で縮んだ値)。現行 `Geometry::new` の挙動そのまま |

## 2. 設計判断

### 2-1. 表示モード切替の方式

| 案 | 内容 | 利点 | 欠点 | 判定 |
|---|---|---|---|---|
| (a) mpv を再起動 | 現在の `time-pos` を控えて `--start=` 付きで別引数の mpv を起動 | 起動引数だけで完結。切替後の状態が起動時と同じ | ytdl_hook が yt-dlp を再実行するため 10 秒超の空白。音も途切れる | 不採用 |
| (b) `set_property vo`(採用) | 同じ mpv に `vo` を変えるコマンドを送る。別ウィンドウ側は `""`(mpv の自動選択) | 再生が途切れない(§1-3)。実装は `MpvCommand` を数個増やすだけ。VO 名を tuitube が決め打ちしない | 切替に失敗したことを送信時には知れない(現行 `send` は応答を見ない)。存在しない VO 名だと mpv ごと終了する | **採用**。実際の VO は `current-vo` のポーリングで追う(§3-5)。VO 名を設定で上書きした場合の失敗は `MpvExited` 経路で表示される |

### 2-2. fps 上限の静的化と「ソースの fps」の要否

| 案 | 内容 | 利点 | 欠点 | 判定 |
|---|---|---|---|---|
| (a) 検索から flat を外し fps を取る | `SearchResult` に `fps: Option<f64>` を足し、起動時に `fps=N` を付けるか判断 | fps が分かるので `fps=N` の複製問題を回避できる | 毎検索 +9 秒(§1-1)。フィード(`:ytrec` 等、cookie 連携)は 30 件なので +30 秒でタイムアウト圏。検索結果 1 件ごとに YouTube へ個別リクエストが走る | 不採用 |
| (b) 再生開始前に 1 本だけ `--dump-json` | 起動直前に fps を調べる | 検索は速いまま | 再生開始が約 15 秒遅れる(ytdl_hook の分と二重) | 不採用 |
| (c) 上限フィルタを常に付ける(採用) | `select=floor((t-prev_selected_t)*N+0.001)` を `--vf-append` で起動時に付与 | ソースの fps を知る必要が無い。複製しない(§1-4)。検索・`SearchResult` に変更なし | 24fps → 12fps のように整数分の 1 に落ちる(上限値ちょうどにはならない)。式が見慣れない | **採用** |
| (d) `fps=N` を常に付ける | 判定せず定レート変換 | 単純 | 遅いソースを複製する。YouTube では稀(§1-1 の 10 件は全て 30fps 以上)だが上限値ぶんの VO 負荷が常にかかる | (c) が使えない環境の予備 |

補足: 動的 `vf add` が静的より効果が薄い先行実測(48.8-87% vs 39.9%)は、VO を変えずにフィルタだけ足す条件で今回も再現した(§1-5 C7b vs C1)。一方、別ウィンドウ → 埋め込みの戻り(`vf add` の直後に `set_property vo kitty` で VO を作り直す。§3-4)では起動時と同じ水準まで下がった(§1-5 C8: 31.2 / 35.1% vs 起動時 30.8 / 36.8%)。したがって静的化は「起動時は `--vf-append`、戻り時は `vf add` + VO 再生成」の組で成立し、再起動案 (a) は不要。

### 2-3. fps 上限と表示モードの関係

| モード | fps 上限フィルタ | 理由 |
|---|---|---|
| 埋め込み | 付ける(設定値 `fps_cap`、既定 15) | kitty VO はフレームごとに CPU で画素を作り base64 で端末へ送る。上限が効く |
| 別ウィンドウ | 付けない | GPU VO のフレーム処理は軽く、滑らかさが目的のモードで間引く意味が無い |

切替時に `vf remove @tuitube-cap` / `vf add @tuitube-cap:…` で付け外しする(§1-4 で追従を確認済み)。

### 2-4. 画質設定の方式

| 案 | 内容 | 利点 | 欠点 | 判定 |
|---|---|---|---|---|
| (a) 段階のみ | `quality = "low" / "medium" / "high" / "native"` | 選びやすい。文言で意味を伝えられる | 中間値が選べない | — |
| (b) 数値のみ | `max_frame_pixels = 230400` | 自由 | 数字の意味が分かりにくい | — |
| (c) 段階 + 数値上書き(採用) | 段階を基本にし、`max_frame_pixels` があればそれを優先 | 普段は段階、詰めたいときは数値 | 2 つのキーの優先順位を説明する必要 | **採用** |

段階と予算(ピクセル数)の対応。予算は `Geometry::new(area, cell, max_pixels)`(video.rs L39-47)の第 3 引数にそのまま渡す。

| 段階 | 予算 | 1 フレームの転送量(RGB24 → base64、上限いっぱいのとき) | 15fps での転送量 |
|---|---|---|---|
| `low` | 320*180 = 57,600 px | 約 230 KB | 約 3.5 MB/s |
| `medium`(既定・現行値) | 640*360 = 230,400 px | 約 920 KB | 約 13.8 MB/s |
| `high` | 960*540 = 518,400 px | 約 2.07 MB | 約 31 MB/s |
| `native` | 制限なし(セル数 × セルのピクセル寸法) | 端末サイズ次第 | 端末サイズ次第 |

`max_frame_pixels` の受理範囲は 64*36(2,304)以上 3840*2160(8,294,400)以下。範囲外は端に丸めて notice を出す(`FpsLimit::parse` と同じ方針、mpv.rs L61-90)。

この設定が変えるのは、映像の細かさと、mpv → tuitube → 端末のデータ量(端末側の描画負荷)。mpv のデコード負荷は変わらない。`native` は Retina 端末で「セルのピクセル寸法」がポイント単位で報告される場合、端末側で拡大されるだけで細かくならない可能性がある(未検証。数値上書きで試せるようにしておく)。

### 2-5. 設定ファイル

| 論点 | 案 | 判定 |
|---|---|---|
| 形式 | TOML(`toml` crate、serde derive)/ JSON(既存の `serde_json` で依存追加なし)/ 環境変数のみ | **TOML**。手で編集する設定にコメントを書ける。依存が `toml` + `toml_edit` + `winnow` 等で増える(ローカルにキャッシュ済み)。依存追加は実装着手時に確認を取る |
| 置き場所 | `$XDG_CONFIG_HOME/tuitube/config.toml`、未設定なら `$HOME/.config/tuitube/config.toml` | 採用。macOS でも mpv / yt-dlp が `~/.config` を使うので揃う。`dirs` 等の crate は使わず、環境変数 2 つを見る純粋関数にする |
| 読むタイミング | 起動時に 1 回(現行 `FpsLimit::from_env` と同じ位置、main.rs L73-78) | 採用。ホットリロードはしない。編集後は再起動 |
| 無いとき | 既定値で動き、既定値とコメントを書いたテンプレートを生成する | 採用(要確認 §8)。生成に失敗しても起動は止めず notice |
| 壊れているとき | 既定値に倒し、理由を notice に出す(`FpsLimit::parse` の方針) | 採用。TOML として読めないファイルは、部分的に読める場合も全体を既定値にする(半端に効いた状態は原因を追いにくい)。TOML としては読めて値の綴りだけが違う場合は §4 のとおりそのキーだけ落とす |
| 未知のキー | 無視する(将来のキーを古い版が拒否しない) | 採用 |
| 環境変数 `TUITUBE_FPS_LIMIT` | ファイルより優先する上書きとして残す | 採用。一時的な試行に便利。notice の文言は変数名を含める(現行どおり) |
| 保存 | (A) アプリからは書かない(手編集のみ)/(B) キー操作で切り替えるたび自動保存/(C) 明示的な保存キーでその時点の値を書く | **(A) を v1 とし、(C) を任意の後続**(要確認 §8)。(B) は「今回だけ窓で見たい」が次回の既定を変えてしまう |
| 保存時のコメント | テンプレートはコメント込みで tuitube が生成する。保存も同じテンプレートに値を埋めて書き直す | 利用者が足したコメントは消える旨をテンプレート先頭に書く。`toml_edit` による保全は v1 では見送る |
| 書き込み方 | 一時ファイルに書いて rename | 途中で落ちても壊れたファイルを残さない |

### 2-6. 操作系(誰がどこでどう使うか)

| 場面 | 起きること | 設計上の受け方 |
|---|---|---|
| 端末が狭くて映像が粗い。ちゃんと見たい | 再生中に `w` を押す → mpv のウィンドウが開き、TUI の映像領域は空になる | 映像領域に「別ウィンドウで再生中  w: 埋め込みに戻す」を出す。シークバー・ステータス・ヘルプは残す(ポーリングは続く) |
| ウィンドウが前面に来てキーが端末に届かない | mpv ウィンドウが前面なら、キーは mpv が受ける(mpv 既定のキー割り当て。space / ←→ / q など) | mpv の既定バインドは殺さない(`--input-default-bindings` は既定のまま)。端末に戻れば tuitube のキーが効く。ヘルプ行に「w: 埋め込みへ」を出す |
| mpv ウィンドウで `q` を押した / ウィンドウを閉じた | mpv が終了 → `MpvExited` → 現行の `end_playback` で結果一覧へ | 追加実装なし。tuitube の `q`(アプリ終了)と意味が違う点はヘルプ行で区別しない(mpv 側の挙動は mpv のもの) |
| 別ウィンドウ中に端末をリサイズ | 埋め込みではないので描き直すものが無い | `apply_resize` は sink の寸法だけ更新し、mpv には送らない。戻すときに現在の端末寸法で `vo-kitty-*` を送る |
| 別ウィンドウ中に tuitube で `Esc` / `q` / Ctrl-C | 現行どおり `quit` を送る → ウィンドウが閉じる | 追加実装なし |
| 埋め込みに戻す | `w` → `vo-kitty-*` を現在寸法で送り、fps 上限フィルタを足し、`vo kitty` | §3-4 |
| 設定を変えたい | `~/.config/tuitube/config.toml` を編集して再起動 | 初回起動でテンプレートが生成される。誤記は起動時の notice で分かる |
| 起動時の既定を「別ウィンドウ」にしたい | `[display] mode = "window"` | 起動直後の再生から別ウィンドウ。fps 上限フィルタは付けずに起動 |
| フォーカスが mpv に移るのが嫌 | `[window] focus_on = "never"` | mpv の `--focus-on` にそのまま渡す |
| ウィンドウの大きさを決めたい | `[window] autofit = "640x360"` / `geometry = "50%+0+0"` | mpv の同名オプションにそのまま渡す(値の検証は非空のみ) |
| mpv のオプションを直接足したい | `[mpv] extra_args = ["--hwdec=videotoolbox-copy"]` | URL の直前に追加する。§9 の検討項目を設定キー無しで試せる |

キーは再生中の `w`(window)。現行の再生中キー(space / ←→ / ↑↓ / Esc / q / マウス)と衝突しない。結果一覧・入力モードでは無効。

## 3. アーキテクチャ

### 3-1. モジュール構成

| ファイル | 役割 | 変更種別 |
|---|---|---|
| `src/settings.rs` | 設定ファイルのパス決定・読み込み・検証・テンプレート生成・保存。`Settings`(検証済みの値)と `Notice` を返す。I/O は `load_from(path)` / `save_to(path)` の薄い層に閉じ、本体は文字列 → `Settings` の純粋関数 | **新規** |
| `src/display.rs` | `DisplayMode`、`FpsCap`、`Quality` と、それらから mpv の起動引数・切替コマンド列を組む純粋関数。`MpvCommand` の生成は `mpv.rs` の公開コンストラクタ(`set_property` を pub にする)を使う | **新規** |
| `src/mpv.rs` | `FpsLimit` / `FpsFilter` / `REQ_CONTAINER_FPS` / `limit_fps` / `add_fps_filter` を削除。`launch` の引数組み立てを `launch_args` に出し、`--vo=kitty` 系を `display.rs` の結果で差し替える。`poll_properties` に `current-vo` を追加。`set_property` / `vf_add` / `vf_remove` を pub に | 変更 |
| `src/geometry.rs` | `geometry_for(cols, rows, cell)` → `geometry_for(cols, rows, cell, max_pixels)`。`video_geometry()` も予算を受ける | 変更 |
| `src/video.rs` | `MAX_FRAME_PIXELS` は `Quality::Medium` の予算値として `display.rs` へ移す(定数名は残してもよい) | 変更(小) |
| `src/app.rs` | `fps_limit: Option<u32>` を `settings: Settings`(起動時の確定値)と `display: DisplayMode`(要求中のモード)に置き換える。`Playback` に `current_vo: Option<String>` を足す。`status_line` にモード表示 | 変更 |
| `src/actions.rs` | `apply_fps_limit` を削除。`start_playback` が `display::LaunchPlan` を組んで渡す。`toggle_display_mode` を追加。`apply_resize` をモード別に | 変更 |
| `src/input.rs` | 再生中の `w` → `toggle_display_mode` | 変更 |
| `src/ui.rs` | 別ウィンドウ中の映像領域にプレースホルダ。ヘルプ行に `w`。(キー→文言の対応は `help_text` のまま) | 変更 |
| `src/main.rs` | 起動時に `settings::load()` → `App` へ。`MpvProperty` の `REQ_CONTAINER_FPS` 分岐を削除 | 変更 |
| `Cargo.toml` | `toml = "1"`(serde 機能)を追加 | 変更(要確認) |
| `src/search.rs` | 変更なし(fps を持たせない) | なし |

`mpv.rs` は現状約 1,000 行で cookie 連携も同時に触るため、新しい型と組み立てロジックは `display.rs` / `settings.rs` に置き、`mpv.rs` への変更は「削除」と「引数の受け渡し」に絞る。

### 3-2. 既存コードの変更点(行参照)

| 箇所 | 現状 | 変更 |
|---|---|---|
| main.rs L73-78 | `FpsLimit::from_env()` を読み `App { fps_limit, notice }` | `settings::load()` を読み `App { settings, display: settings.display.mode, notice }` |
| main.rs L214-223 | `REQ_CONTAINER_FPS` なら `apply_fps_limit` | 分岐を削除。`REQ_CURRENT_VO` は `app.apply_property` に流す |
| actions.rs L118-151 `start_playback` | `MpvController::launch(url, nonce, tx, video, app.fps_limit)` | `let plan = LaunchPlan::new(app.display, video.geometry(), &app.settings)` → `launch(url, nonce, tx, video, &plan)`。`VideoSink` は別ウィンドウ起動でも作る(後で埋め込みに戻せるように stdout の読み手を常に立てる) |
| actions.rs L153-164 `apply_fps_limit` | `container-fps` 到着で `limit_fps` | 削除 |
| actions.rs L172-190 `apply_resize` | 寸法が変わったら `resize_video` を送る | `app.display == Embedded` のときだけ送る。Window では `video.resize(geometry)` のみ |
| actions.rs `geometry_for(cols, rows, cell_size())` | 予算は定数 | `geometry_for(cols, rows, cell_size(), app.settings.display.max_pixels())` |
| mpv.rs L19-24 | `REQ_CONTAINER_FPS: u64 = 5` | 削除し `REQ_CURRENT_VO: u64 = 6` を追加(5 は再利用しない。古いログとの取り違え防止) |
| mpv.rs L26-139 | `FpsLimit` / `FpsFilter` / `add_fps_filter` と定数 | 削除。`DEFAULT_FPS_LIMIT` / `MAX_FPS_LIMIT` / `FPS_LIMIT_VAR` は `settings.rs` へ移す |
| mpv.rs L193-198 `set_property` | private | pub に(`display.rs` が使う) |
| mpv.rs L200-211 `resize_video` | `vo-kitty-*` 4 つ + `vid no/auto` | 埋め込み中のリサイズ用にそのまま残す。切替用の列は `display.rs` が別に組む |
| mpv.rs L367-374 `MpvController` | `fps: FpsFilter` フィールド | 削除 |
| mpv.rs L393-404 `launch` の引数 | `--vo=kitty` + `video.geometry().mpv_args()` を直書き | `launch_args(&socket_path, &log_path, plan, url)` の結果を `.args()` で渡す |
| mpv.rs L493-508 `poll_properties` | 4 プロパティ + 条件付き `container-fps` | 4 プロパティ + `current-vo`(常に) |
| mpv.rs L510-517 `limit_fps` | | 削除 |
| video.rs L18 `MAX_FRAME_PIXELS` | 定数 | `display::Quality::Medium.max_pixels()` の値として参照。既存テストが使うので定数は残す |
| geometry.rs L8-11, L22-28 | 予算は `video::MAX_FRAME_PIXELS` | 引数で受ける |
| app.rs L111-112, L132 | `fps_limit` | 削除。`settings` / `display` を追加 |
| app.rs L171-181 `apply_property` | 4 プロパティ | `REQ_CURRENT_VO` → `playback.current_vo` |
| app.rs L216-233 `playback_line` | 状態 / タイトル / 時刻 / 音量 | 末尾にモード表示(§5) |
| input.rs L78-88 `handle_key_playing` | seek / command / q | `w` → `toggle_display_mode(app, session).await` |
| input.rs L137-145 `playing_command` | キー → `MpvCommand` | `w` はここに入れない(複数コマンド + App の状態更新を伴うため、seek と同じく別経路) |
| ui.rs L100-105 `draw_playing` | 映像領域に何も描かない | `app.display == Window` なら中央にプレースホルダ |
| ui.rs L158-166 `help_text` | 再生中の文言 | `w:別ウィンドウ` / `w:埋め込みへ` をモードで出し分け(`help_text(mode, display)`) |

### 3-3. 型と関数(TDD の足場)

シグネチャと意図のみ。本体はテストを先に書いてから埋める。

```rust
// src/display.rs

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DisplayMode { #[default] Embedded, Window }

impl DisplayMode {
    pub fn toggled(self) -> Self;
    /// mpv の `current-vo` から判定。"kitty" → Embedded、それ以外の Some → Window、None → None。
    pub fn from_current_vo(vo: Option<&str>) -> Option<Self>;
    pub fn label(self) -> &'static str;          // "埋め込み" / "別ウィンドウ"
}

pub const CAP_LABEL: &str = "tuitube-cap";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FpsCap(u32);   // 1..=120

impl FpsCap {
    pub fn new(fps: u32) -> Option<Self>;        // 0 と 120 超は None
    pub fn get(self) -> u32;
    /// "@tuitube-cap:lavfi=[select=floor((t-prev_selected_t)*15+0.001)]"
    pub fn filter_spec(self) -> String;
    /// "--vf-append=" + filter_spec()
    pub fn launch_arg(self) -> String;
    /// ["vf","add", filter_spec()]
    pub fn add_command(self) -> MpvCommand;
    /// ["vf","remove","@tuitube-cap"]
    pub fn remove_command() -> MpvCommand;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Quality { Low, #[default] Medium, High, Native }

impl Quality {
    pub fn max_pixels(self) -> u32;    // 57_600 / 230_400 / 518_400 / u32::MAX
    pub fn label(self) -> &'static str;
}

/// 起動引数の元。`--vo` / `--vo-kitty-*` / `--vf-append` / ウィンドウ系はここから決まる。
#[derive(Debug, Clone, PartialEq)]
pub struct LaunchPlan {
    pub mode: DisplayMode,
    pub geometry: Geometry,            // 埋め込み用。Window 起動でも戻り用に持つ
    pub fps_cap: Option<FpsCap>,       // Embedded のときだけ引数になる
    pub window: WindowOptions,
    pub extra_args: Vec<String>,       // [mpv] extra_args + cookie 連携の追加引数
}

impl LaunchPlan {
    pub fn new(mode: DisplayMode, geometry: Geometry, settings: &Settings) -> Self;
    /// `--input-ipc-server` / `--log-file` / `--no-terminal` の後ろ、URL の前に来る引数列。
    pub fn args(&self) -> Vec<String>;
}

#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Deserialize, serde::Serialize)]
pub struct WindowOptions {
    pub vo: Option<String>,        // None = mpv の自動選択(起動時は --vo を渡さない、切替時は vo "")
    pub autofit: Option<String>,   // "--autofit=" にそのまま
    pub geometry: Option<String>,  // "--geometry="
    pub ontop: bool,               // true なら "--ontop"
    pub focus_on: Option<FocusOn>, // "--focus-on=never|open|all"
    pub title: Option<String>,     // 既定 "tuitube - ${media-title}"
}

impl WindowOptions {
    pub fn args(&self) -> Vec<String>;
    /// set_property vo に渡す値。None なら "" (自動選択)。
    pub fn vo_value(&self) -> &str;
}

/// 埋め込み → 別ウィンドウ。fps 上限を外し、vo を変える。
pub fn to_window_commands(cap: Option<FpsCap>, window: &WindowOptions) -> Vec<MpvCommand>;
/// 別ウィンドウ → 埋め込み。kitty VO の設定 (寸法・位置・alt-screen・config-clear) を送り、
/// fps 上限を足し、vo を kitty に。別ウィンドウ起動では起動引数に --vo-kitty-* が無いため、
/// 寸法だけでは埋め込み起動と構成が揃わない。
pub fn to_embedded_commands(geometry: Geometry, cap: Option<FpsCap>) -> Vec<MpvCommand>;
```

```rust
// src/settings.rs
pub const FPS_LIMIT_VAR: &str = "TUITUBE_FPS_LIMIT";
pub const DEFAULT_FPS_CAP: u32 = 15;
pub const MAX_FPS_CAP: u32 = 120;
pub const MIN_FRAME_PIXELS: u32 = 64 * 36;
pub const MAX_FRAME_PIXELS_LIMIT: u32 = 3840 * 2160;

/// ファイルの生の形。全キー任意。未知キーは無視。
#[derive(Debug, Default, Clone, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct RawConfig {
    pub display: Option<RawDisplay>,
    pub playback: Option<RawPlayback>,
    pub window: Option<RawWindow>,
    pub mpv: Option<RawMpv>,
}
// 選択肢は文字列で受ける。serde の enum で受けるとファイル全体がデシリアライズエラーになり、
// 綴りの合っている他のキーまで既定値に倒れる。
pub struct RawDisplay { pub mode: Option<String>, pub quality: Option<String>, pub max_frame_pixels: Option<i64> }
pub struct RawWindow { /* WindowOptions と同じキー。focus_on だけ Option<String> */ }
pub struct RawPlayback { pub fps_cap: Option<i64> }          // 0 = 制限なし。負値・120 超は丸めて notice
pub struct RawMpv { pub extra_args: Option<Vec<String>> }

/// 検証済みの値。App が持つのはこれ。
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub display: DisplaySettings,   // mode, quality, max_frame_pixels(数値上書き)
    pub fps_cap: Option<FpsCap>,    // None = 制限なし
    pub window: WindowOptions,
    pub extra_args: Vec<String>,
}

impl DisplaySettings {
    /// max_frame_pixels があればそれ、無ければ quality の予算。
    pub fn max_pixels(&self) -> u32;
}

/// 読み込み結果。notice は利用者に見せる 1 行(複数あれば " / " で連結)。
#[derive(Debug, Clone, PartialEq)]
pub struct Loaded { pub settings: Settings, pub notice: Option<String> }

/// `$XDG_CONFIG_HOME/tuitube/config.toml` か `$HOME/.config/tuitube/config.toml`。両方無ければ None。
pub fn config_path(xdg_config_home: Option<&OsStr>, home: Option<&OsStr>) -> Option<PathBuf>;

/// 文字列 → RawConfig → Settings。TOML エラーは Err(表示文)。
pub fn parse(text: &str) -> Result<RawConfig, String>;
/// 検証と丸め。notice を積む。
pub fn validate(raw: RawConfig, env_fps_limit: Option<&str>) -> (Settings, Vec<String>);
/// 環境変数 TUITUBE_FPS_LIMIT の解釈(現行 FpsLimit::parse の移設)。Some(Some(n)) / Some(None)=制限なし / None=未指定。
pub fn parse_fps_limit_env(raw: Option<&str>) -> (Option<Option<u32>>, Option<String>);

/// コメント付きテンプレート。値は引数の Settings。
pub fn render(settings: &Settings) -> String;

/// I/O 層。テストは tempdir で。
pub fn load_from(path: Option<&Path>, env_fps_limit: Option<&str>) -> Loaded;
pub fn load() -> Loaded;                                        // 環境変数を読んで load_from
pub fn save_to(path: &Path, settings: &Settings) -> Result<(), String>;   // tmp に書いて rename
```

```rust
// src/mpv.rs(変更点のみ)
pub const REQ_CURRENT_VO: u64 = 6;
pub fn set_property(name: &str, value: Value) -> MpvCommand;   // pub 化
pub fn vf_add(spec: &str) -> MpvCommand;                       // ["vf","add",spec]
pub fn vf_remove(label: &str) -> MpvCommand;                   // ["vf","remove","@"+label]

/// 現行 launch() 内の引数組み立てを純粋関数に出す。
pub fn launch_args(socket: &Path, log: &Path, plan: &LaunchPlan, url: &str) -> Vec<String>;

impl MpvController {
    pub async fn launch(url: &str, nonce: u64, events: UnboundedSender<AppEvent>, video: VideoSink, plan: &LaunchPlan) -> Result<Self, String>;
    pub async fn send_all(&mut self, commands: &[MpvCommand]) -> Result<(), String>;
}
```

```rust
// src/app.rs(変更点のみ)
pub struct Playback { …, pub current_vo: Option<String> }
pub struct App {
    …,
    pub settings: Settings,
    /// 要求中の表示モード。実際にどちらで出ているかは playback.current_vo。
    pub display: DisplayMode,
}
impl App {
    /// 要求と実際が食い違う間は "(切替中)" を付ける。
    pub fn display_label(&self) -> String;
}
```

```rust
// src/actions.rs(変更点のみ)
pub async fn toggle_display_mode(app: &mut App, session: &mut Session);
//   player が無ければ何もしない
//   next = app.display.toggled()
//   Window へ: commands = to_window_commands(app.settings.fps_cap, &app.settings.window)
//              session.owe_clear = true(kitty の a=d が来ない経路の保険)
//   Embedded へ: geometry = geometry_for(現在の端末寸法, cell_size(), app.settings.display.max_pixels())
//                video.resize(geometry); commands = to_embedded_commands(geometry, app.settings.fps_cap)
//   send_all が Err なら app.error、Ok なら app.display = next
```

### 3-4. 切替フロー

埋め込み → 別ウィンドウ(`w`):

1. `vf remove @tuitube-cap`(fps 上限があるとき)
2. `set_property vo <window.vo か "">`(未設定なら `""` = mpv の自動選択。§1-3)
3. tuitube 側: `app.display = Window`、`session.owe_clear = true`。sink は保持(stdout の読み手も動き続ける)。kitty VO の uninit が出す `a=d` は sink が `Clear` として受け取り、次の `present_video` で画像が消える
4. 以後、ティッカーの `current-vo` ポーリングで `playback.current_vo` が `gpu-next` になれば表示上も確定

別ウィンドウ → 埋め込み(`w`):

1. 現在の端末寸法と設定の予算で `Geometry` を作る。`video.resize(geometry)`(組み立て途中のバイト列と古いフレームを捨て、clear を保留にする。video.rs L242-251)
2. `set_property vo-kitty-cols/rows/width/height`(4 つ)
3. `vf add @tuitube-cap:…`(fps 上限があるとき)
4. `set_property vo kitty`
5. tuitube 側: `app.display = Embedded`。kitty VO が生成され、新しい寸法のフレームが stdout に流れ始める

送信順の根拠: 寸法は VO 生成前に入っていなければならない(§1-3)。fps 上限は最初のフレームから効かせたいので `vo` より前。`vid no/auto` は不要(§1-3)。この順で送った往復の CPU 実測は §1-5 C8(戻り後も起動時と同水準)。

### 3-5. 状態と失敗の扱い

| 状態 | 保持場所 | 更新契機 |
|---|---|---|
| 要求中のモード | `App.display` | 起動時 = 設定の `mode`。`toggle_display_mode` の送信成功時に反転 |
| 実際の VO | `App.playback.current_vo` | 毎秒のポーリング(`REQ_CURRENT_VO`) |
| fps 上限フィルタの有無 | 持たない(モードから決まる: Embedded なら有、Window なら無) | — |
| 埋め込みの寸法 | `VideoSink.geometry` | 起動時、リサイズ時、埋め込みへの戻り時 |

| 失敗 | 現れ方 | 受け方 |
|---|---|---|
| `[window] vo` に存在しない VO 名 | 送信は成功、mpv がエラーを残して終了(§1-3) | `MpvExited` → `end_playback`。エラー文は `log_detail` が `[e][vo] Video output … not found!` を拾う。既定(自動選択)ではこの経路に入らない |
| `set_property vo` は通ったが VO が構成されない(GPU 環境が無い等) | `current-vo` が変わらない、または mpv が終了 | 前者は表示が「(切替中)」のまま。数秒で変わらなければ利用者が `w` で戻せる。自動で戻さない(往復を繰り返す方が困る) |
| 送信自体の失敗(パイプ切断) | `send_all` が Err | `app.error` に出す。`app.display` は変えない |
| ウィンドウ起動で mpv が落ちる(GPU 無し等) | `MpvExited { error }` | 現行の `end_playback`。エラー文はログの `[e]` 行から(現行 `log_detail`) |
| 別ウィンドウ中に kitty の `a=d` が来ない | 画像が残る | `owe_clear = true` を切替時に立てるので次の present で消える |

## 4. 設定ファイル仕様

パス: `$XDG_CONFIG_HOME/tuitube/config.toml`(未設定時 `~/.config/tuitube/config.toml`)。

| セクション | キー | 型 | 既定 | 意味 | 検証 |
|---|---|---|---|---|---|
| `[display]` | `mode` | `"embedded"` / `"window"` | `"embedded"` | 再生開始時の表示モード | 他の文字列は既定 + notice |
| `[display]` | `quality` | `"low"` / `"medium"` / `"high"` / `"native"` | `"medium"` | 埋め込み表示の細かさ(§2-4) | 同上 |
| `[display]` | `max_frame_pixels` | 整数 | 無し | `quality` を無視してピクセル予算を直接指定 | 2,304〜8,294,400 に丸めて notice |
| `[playback]` | `fps_cap` | 整数 | `15` | 埋め込み時の fps 上限。`0` で制限なし | 負値は既定 + notice。120 超は 120 に丸めて notice |
| `[window]` | `vo` | 文字列 | 無し(mpv の自動選択。この環境では `gpu-next`) | 別ウィンドウ時の mpv VO を固定したいとき | 非空のみ。存在しない名前は mpv が終了するので再生エラーとして見える |
| `[window]` | `autofit` | 文字列 | 無し | mpv `--autofit` | 非空のみ |
| `[window]` | `geometry` | 文字列 | 無し | mpv `--geometry` | 非空のみ |
| `[window]` | `ontop` | 真偽 | `false` | mpv `--ontop` | — |
| `[window]` | `focus_on` | `"never"` / `"open"` / `"all"` | 無し(mpv 既定 `open`) | mpv `--focus-on` | 他の文字列は無視 + notice |
| `[window]` | `title` | 文字列 | `"tuitube - ${media-title}"` | mpv `--title` | 非空のみ |
| `[mpv]` | `extra_args` | 文字列の配列 | `[]` | URL の直前に足す任意の mpv 引数 | `--` で始まらない要素は notice(渡しはする) |

環境変数 `TUITUBE_FPS_LIMIT` は `[playback] fps_cap` を上書きする(値の解釈は現行 `FpsLimit::parse` と同じ。`0` / `unlimited` で制限なし)。

テンプレート(初回起動で生成。`render(&Settings::default())` の出力):

```toml
# tuitube の設定。編集後は tuitube を再起動する。
# このファイルは tuitube が書き直すことがあり、自分で書いたコメントは残らない。

[display]
# 再生開始時の表示。"embedded" = TUI 内に埋め込み、"window" = mpv の別ウィンドウ。再生中は w で切り替え。
mode = "embedded"
# 埋め込み表示の細かさ。"low" / "medium" / "high" / "native"。
# 変わるのは映像の細かさと端末へ送るデータ量。mpv のデコード負荷(CPU)は変わらない。
quality = "medium"
# 細かさをピクセル数で直接指定したいとき(quality より優先)。例: 640*360 = 230400
# max_frame_pixels = 230400

[playback]
# 埋め込み表示の fps 上限。端末へ送るフレーム数を抑える。0 で制限なし。別ウィンドウには適用しない。
fps_cap = 15

[window]
# 別ウィンドウ時の mpv オプション。値はそのまま mpv に渡る。
# vo = "gpu-next"
# autofit = "640x360"
# geometry = "50%+0+0"
# ontop = false
# focus_on = "never"

[mpv]
# mpv にそのまま渡す追加引数。
# extra_args = ["--hwdec=videotoolbox-copy"]
```

## 5. 表示文言

| 場所 | 埋め込み | 別ウィンドウ |
|---|---|---|
| ヘルプ行(再生中) | `space:一時停止  ←→:5秒シーク  クリック/ドラッグ:シーク  ↑↓:音量±5  w:別ウィンドウ  Esc:停止  q:終了` | `space:一時停止  ←→:5秒シーク  クリック/ドラッグ:シーク  ↑↓:音量±5  w:埋め込みへ  Esc:停止  q:終了` |
| ステータス行の末尾 | `[埋め込み 15fps medium]`(制限なしなら `[埋め込み medium]`) | `[別ウィンドウ]`。要求と `current-vo` が食い違う間は `[別ウィンドウ (切替中)]` |
| 映像領域 | 映像 | 中央に `別ウィンドウで再生中  w: 埋め込みに戻す` |
| 起動時 notice(例) | `config.toml を作成しました: ~/.config/tuitube/config.toml` / `[display] quality="hi" は読めません。medium で表示します` / `TUITUBE_FPS_LIMIT=3O を数値として読めません。15 fps で再生します` | |

文言の規則: 画質・fps の説明で「軽い」「重い」「負荷」を quality 側に書かない。fps 上限の説明は「端末へ送るフレーム数を抑える」に留める(デコード負荷の話は書かない)。

## 6. テスト戦略

外部プロセス無しで書けるものから始める。既存テストの書き方(1 テスト = 1 文の snake_case、日本語のコメントで意図)に合わせる。

### 6-1. 最初に書く Red(モジュール別)

`src/display.rs`

| テスト | 検証内容 |
|---|---|
| `fps_cap_rejects_zero_and_values_above_the_maximum` | `FpsCap::new(0)` / `new(121)` が None、`new(1)` / `new(120)` が Some |
| `fps_cap_filter_spec_is_a_drop_only_select_with_epsilon` | `FpsCap::new(15).filter_spec()` == `"@tuitube-cap:lavfi=[select=floor((t-prev_selected_t)*15+0.001)]"`。`,` を含まないこと |
| `fps_cap_launch_arg_appends_instead_of_replacing` | `launch_arg()` が `--vf-append=` で始まる。`--vf=` ではない |
| `fps_cap_add_and_remove_commands_share_the_label` | `add_command().to_line()` == `{"command":["vf","add","@tuitube-cap:lavfi=[…]"]}`、`remove_command()` == `["vf","remove","@tuitube-cap"]` |
| `quality_budgets_are_ordered_and_medium_is_the_current_constant` | low < medium < high < native、`Medium.max_pixels() == video::MAX_FRAME_PIXELS` |
| `display_mode_is_read_back_from_current_vo` | `from_current_vo(Some("kitty"))` = Embedded、`Some("gpu-next")` / `Some("gpu")` = Window、`None` = None |
| `embedded_launch_args_contain_kitty_geometry_and_the_fps_cap` | `LaunchPlan { Embedded, geometry(80x22), Some(15) }.args()` に `--vo=kitty`、`--vo-kitty-cols=80`…(現行 `Geometry::mpv_args` の 8 個)、`--vf-append=@tuitube-cap:…` が全て含まれ、`--autofit` 等が含まれない |
| `window_launch_args_have_no_kitty_options_and_no_fps_cap` | `LaunchPlan { Window, … }.args()` に `--vo=` / `--vo-kitty-` / `--vf-append` が無く(VO は mpv の自動選択)、`--title=…` がある。`window.vo = Some("gpu")` なら `--vo=gpu` が入る |
| `window_options_pass_through_only_what_is_set` | autofit / geometry / ontop / focus_on の有無で引数が増減する |
| `extra_args_come_last_in_the_plan` | `extra_args` が `args()` の末尾に元の順で並ぶ |
| `to_window_commands_remove_the_cap_before_switching_the_vo` | 列が `[vf remove @tuitube-cap, set_property vo ""]`。cap 無しなら `[set_property vo ""]`。`window.vo = Some("gpu")` なら `set_property vo "gpu"` |
| `to_embedded_commands_send_geometry_then_cap_then_vo` | 列が `[cols, rows, width, height, vf add, set_property vo kitty]` の順。`vid` を含まない |

`src/settings.rs`

| テスト | 検証内容 |
|---|---|
| `config_path_prefers_xdg_config_home_then_home` | `(Some("/x"), Some("/h"))` → `/x/tuitube/config.toml`、`(None, Some("/h"))` → `/h/.config/tuitube/config.toml`、`(Some(""), Some("/h"))` → HOME 側、`(None, None)` → None |
| `an_empty_file_yields_the_defaults_without_a_notice` | `validate(parse("").unwrap(), None)` が `Settings::default()` と空 notice |
| `unknown_keys_and_sections_are_ignored` | `[display]\nfoo=1\n[bar]\nx=2` が既定値・notice 無し |
| `a_toml_syntax_error_is_reported_with_the_line` | `parse("mode = ")` が Err で行番号を含む文 |
| `display_mode_and_quality_are_parsed_case_sensitively` | `mode = "window"`, `quality = "high"` が取れる。`"Window"` は別の綴りとして既定 + notice |
| `an_unreadable_choice_only_affects_its_own_key` | `quality = "hi"` / `focus_on = "nevr"` は既定へ倒すが、同じファイルの `mode` / `max_frame_pixels` / `autofit` はそのまま効く。notice はキーと値を含む |
| `max_frame_pixels_overrides_quality_and_is_clamped` | `quality="low"` + `max_frame_pixels=300000` → `max_pixels()==300000`。`1` → 2,304 + notice、`10_000_000` → 8,294,400 + notice |
| `fps_cap_zero_disables_and_out_of_range_values_are_rounded_with_a_notice` | `0` → None、`15` → Some(15)、`121` → Some(120) + notice、`-5` → 既定 + notice(現行 `FpsLimit` の 4 テストを移設) |
| `the_environment_variable_overrides_the_file` | ファイル `fps_cap = 30` + env `"unlimited"` → None。env `"3O"` → ファイル値を保ち notice |
| `window_options_require_non_empty_strings` | `autofit = ""` は無視 + notice |
| `extra_args_that_do_not_look_like_options_are_passed_with_a_notice` | `["--hwdec=no", "foo"]` → 2 つとも残り notice 1 つ |
| `render_round_trips_through_parse` | `validate(parse(&render(&s)).unwrap(), None).0 == s`(既定値と非既定値の両方) |
| `render_contains_the_comment_about_being_rewritten` | 先頭コメントの存在(保存で消える旨) |
| `load_from_a_missing_path_creates_the_template_and_says_so` | tempdir 配下の無いパスでファイルが生成され、notice にパスが入る。生成先の親ディレクトリも作る |
| `load_from_a_broken_file_falls_back_to_defaults_and_keeps_the_file` | TOML の構文エラーは全体を既定値に倒し、壊れた内容を書き換えない |
| `save_to_replaces_atomically` | 保存後に一時ファイルが残らない。内容が `render` と一致 |

`src/app.rs`

| テスト | 検証内容 |
|---|---|
| `current_vo_is_applied_from_the_poll` | `apply_property(REQ_CURRENT_VO, json!("gpu-next"))` → `playback.current_vo == Some("gpu-next")`、None で消える |
| `display_label_marks_the_transition_until_mpv_confirms` | `display = Window` + `current_vo = Some("kitty")` → `[別ウィンドウ (切替中)]`、`Some("gpu-next")` → `[別ウィンドウ]`、`display = Embedded` + `Some("kitty")` + cap 15 + medium → `[埋め込み 15fps medium]` |
| `the_status_line_ends_with_the_display_label_while_playing` | `playing_status()` の末尾 |
| `default_app_takes_the_display_mode_from_settings` | `App::default().display == Settings::default().display.mode` |

`src/actions.rs`(player 無しで書けるもの)

| テスト | 検証内容 |
|---|---|
| `toggling_without_a_player_changes_nothing` | `toggle_display_mode` 後も `display` / `error` / `owe_clear` が不変 |
| `resize_in_window_mode_updates_the_sink_but_owes_no_mpv_commands` | `display = Window` で `apply_resize` → `video.geometry()` は新値、`error` 無し(mpv へは送らない経路。player 無しでも同じ結果になるので、送らないことの確認は §6-2 の偽 controller で) |
| `resize_uses_the_configured_pixel_budget` | `settings.display.quality = Low` で `apply_resize` → `frame_px` が 57,600 px 以下 |

`src/input.rs` / `src/ui.rs`

| テスト | 検証内容 |
|---|---|
| `w_is_not_a_plain_mpv_command` | `playing_command(KeyCode::Char('w')) == None`(seek と同じく別経路) |
| `w_toggles_only_while_playing` | Results / Input モードで `w` を押しても `display` が変わらない(Input では query に `w` が入る) |
| `help_text_names_the_other_display_mode` | Embedded → `w:別ウィンドウ`、Window → `w:埋め込みへ` |
| `video_area_layout_is_unchanged_by_the_display_mode` | `video_area` / `seek_bar_area` がモードに依存しない(プレースホルダは同じ矩形に描く) |

`src/mpv.rs`

| テスト | 検証内容 |
|---|---|
| `launch_args_put_the_plan_between_the_fixed_options_and_the_url` | `[--input-ipc-server=…, --log-file=…, --no-terminal, <plan.args()…>, url]` の順 |
| `poll_asks_for_current_vo_every_time` | `poll_properties` が送る行に `get_property current-vo` + `request_id: 6` が毎回含まれる(送信先を `Vec<u8>` にできるよう `writer` を `AsyncWrite` ジェネリックにするか、送る `MpvCommand` 列を返す純粋関数 `poll_commands()` に出す) |
| 既存の `fps_*` テスト 5 本 | `settings.rs` へ移設(削除ではない) |

### 6-2. 偽 controller での結合テスト(外部プロセス無し)

`MpvController` の `send` を trait(`PlayerSink`)に切り、テストでは送った `MpvCommand` を `Vec` に溜める偽物を `Session.player` に入れる。これで以下が書ける。

| テスト | 検証内容 |
|---|---|
| `toggle_to_window_sends_remove_then_vo_and_owes_a_clear` | 送信列(`vf remove` → `set_property vo ""`)と `display == Window`、`owe_clear == true` |
| `toggle_back_to_embedded_resizes_the_sink_and_sends_geometry_cap_vo` | `video.geometry()` が現在寸法と予算に一致、送信列の順序 |
| `toggle_keeps_the_mode_when_sending_fails` | 偽物が Err を返すと `display` 不変、`error` に文 |
| `resize_in_window_mode_sends_nothing` | 送信列が空 |
| `resize_in_embedded_mode_sends_the_existing_resize_sequence` | 現行 `resize_video` の 6 コマンド |

trait 化は cookie 連携が `mpv.rs` に入れる変更と重なるため、順序は §7 で調整する。

### 6-3. 手動確認(実機)

| 項目 | 期待 |
|---|---|
| 初回起動 | `~/.config/tuitube/config.toml` が生成され、ステータス行にその旨 |
| `quality = "hi"` と書いて起動 | 既定で動き、notice に `display.quality` が出る |
| 再生中 `w` | ウィンドウが開き、TUI の映像が消えてプレースホルダ。ステータスが `(切替中)` → 確定 |
| もう一度 `w` | 埋め込みに戻り、fps 上限が効く(端末の描画が 15fps 相当) |
| 別ウィンドウ中に端末リサイズ → `w` | 新しい端末寸法で埋め込みに戻る |
| mpv ウィンドウを閉じる | 結果一覧に戻る |
| `mode = "window"` で起動して再生 | 最初から別ウィンドウ。`w` で埋め込みへ |
| `TUITUBE_FPS_LIMIT=0` | 埋め込みでも上限なし(ステータスに fps 表記が無い) |
| CPU | 埋め込み + 15fps 上限で先行実測(39.9%)と同水準。別ウィンドウはその半分以下(§1-5 C5) |

## 7. cookie 連携との統合点

cookie 連携の設計(`docs/google-account-cookies-design.md` §3-3)が予定している変更と、本設計が触る箇所の重なり。

| 箇所 | cookie 連携の変更 | 本設計の変更 | 調整方針 |
|---|---|---|---|
| `search.rs` `SearchResult` / `parse_line` | `uploader` → `channel` のフォールバック | **変更なし**(fps を持たせない方式を採ったため) | 衝突しない。§2-2 (a) を採る場合のみ `fps: Option<f64>` を足すことになる(その場合は `parse_line` を両方が触る) |
| `mpv.rs` `launch` の引数 | `extra: &[String]`(cookie の `--ytdl-raw-options-append=…`) | `plan: &LaunchPlan` | `LaunchPlan.extra_args` に cookie の引数を含める(`start_playback` で `settings.extra_args` + `cookies.for_playback()` を連結)。`launch` の引数は `plan` 1 つに寄せる |
| `mpv.rs` `launch_args` | `launch_args(socket, log, geometry, extra, url)` | `launch_args(socket, log, plan, url)` | `geometry` と `extra` は `plan` に含まれるため後者に統一 |
| `mpv.rs` `log_detail` | `[e][ytdl_hook] ERROR:` 行を優先 | 触らない | 衝突しない |
| `app.rs` `App.notice` | cookie の状態通知に使用。次の検索開始で消す | 設定読み込みの notice に使用(現行どおり) | 同じ 1 スロットを共有する。起動時 notice は次の検索開始で消えてよい(見た時点で用は済む)。両方同時に出したいときは " / " で連結 |
| `app.rs` `AppEvent::SearchDone` | `report: SearchReport` に変更 | 触らない | 衝突しない |
| `actions.rs` `start_playback` | `extra` を組む | `plan` を組む | 1 関数内で連続して行う。どちらが先に入っても後から入る側が合流させる |
| `main.rs` 起動時 | `CookieState::from_env()` | `settings::load()` | 隣接する行。環境変数 `TUITUBE_COOKIES_FROM_BROWSER` を将来 `[cookies] browser` へ移す余地は残すが本設計では扱わない |
| ヘルプ行(`ui.rs` `help_text`) | 入力モードにフィード keyword | 再生中に `w` | 別モードの文言なので衝突しない |

統合の細部(引数の最終形、trait 化の有無と順序)は実装フェーズで、先に入った側のコードを前提に調整する。

## 8. 実装前に確認したいこと

| # | 項目 | 案 | 推奨 |
|---|---|---|---|
| 1 | fps 上限の方式 | §2-2 (c) `select` 上限 / (d) `fps=N` 固定 / (a) 検索から fps | (c)。24fps → 12fps の整数分の 1 になる点を許容できるか |
| 2 | 設定の保存 | §2-5 (A) 手編集のみ + 初回テンプレート生成 / (C) 保存キー追加 | v1 は (A)。(C) はキー(`S` 等)込みで後続 |
| 3 | 初回起動でのテンプレート生成 | 生成する / しない(パスを notice で案内するだけ) | 生成する。書けない環境(読み取り専用 HOME 等)は notice のみ |
| 4 | 切替キー | `w` / 他(`t`、`Tab`、`o`) | `w` |
| 5 | 既定の別ウィンドウ VO | mpv の自動選択 / `gpu-next` 固定 | 自動選択(起動時は `--vo` を渡さない、切替時は `vo ""`。両方実測済み)。`[window] vo` で固定可 |
| 6 | `TUITUBE_FPS_LIMIT` の存続 | 残す(上書き)/ 廃止 | 残す |
| 7 | `toml` crate の追加 | 追加 / JSON で代替 | 追加(1.x) |
| 8 | 埋め込み中に画質を切り替えるキー | 付ける(`[` `]` で段階を上下、現行 `resize_video` 経路で VO を作り直す)/ 付けない | v1 は付けない。設定を試す往復(編集 → 再起動 → 検索 10 秒超 → 再生)が重ければ後続で |
| 9 | 別ウィンドウ時の既定 `focus_on` | mpv 既定(`open`)/ `never` | mpv 既定。前面に来る方が「開いたことが分かる」。嫌なら設定で |

## 9. 会話に出ていないが追加検討してほしい要素

合意済み仕様の外。採否は別途。

| 項目 | 内容 | 根拠 | 実装コスト |
|---|---|---|---|
| 元動画の解像度上限(`--ytdl-format`) | mpv 既定の ytdl-format は最良画質を選ぶため、埋め込み 647x356 のために 1080p/2160p をデコードしている(§1-1 の 10 件は全て 1080 以上、§1-5 の C0-C2 も 1920x1080)。`bv*[height<=360]+ba/b[height<=360]` でソース側を絞ると、fps 上限との併用で CPU 34.7-39.7% → 17.1-19.9%(§1-5 C3。640x360 VP9 がデコードされた)。360p のフォーマットは 10 件とも 30fps | 「CPU の主要因はデコード」という共有済みの結論から導かれる削減手段で、実測でも fps 上限と同程度の効果 | `[playback] source_height_cap = 360` を 1 キー足し、埋め込み起動時だけ `--ytdl-format=` を付ける。別ウィンドウでは付けない。ytdl の選択は起動時に決まるので、切替後は起動時のモードの解像度のまま(別ウィンドウへ切り替えても 360p のまま、逆も同様)。この制約を許容できるかが論点 |
| ハードウェアデコード(`--hwdec`) | macOS の VideoToolbox でデコードする。mpv 既定は `hwdec=no` | 実測では kitty VO 向けの `videotoolbox-copy` は **逆効果**(§1-5 C4: 34.7-39.7% → 45.2-61.2%。1080p フレームのコピーが乗る)。別ウィンドウの `videotoolbox` は 10.8-15.7% → 10.2-11.9% で差は小さい | 設定キーには昇格させない。試したい場合は `[mpv] extra_args` で |
| 保存キー | §2-5 (C) | 設定を試す往復を短くする | `settings::save_to` は本設計に含む。キーと文言だけ |
| 画質の実行時切替 | §8 #8 | 同上 | 現行 `resize_video` 経路の再利用 |
