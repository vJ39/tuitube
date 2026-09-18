YouTube の自動生成字幕(automatic_captions)を tuitube で表示・切替するための設計。実装は TDD(Red→Green)で進める前提で、外部プロセス(mpv・yt-dlp)を伴わない純粋関数の単位を先に切り出す。

対象バージョン: mpv 0.41.0 / yt-dlp 2026.08.19 / ratatui 0.29 / crossterm 0.28 / macOS 14。コードの参照はコミット `92f4a76` + 作業ツリー(`c` キーの URL コピーとエラー期限の未コミット分を含む)時点。実測は 2026/09/18 に動画 `NAXzYDnQfrg`(公式字幕なし・自動生成字幕あり)と、ローカル生成の `sample.mp4` + `sample.srt` で行った。

## 0. 合意済み仕様

1. 設定ファイルに `[subtitles]` を新設し、字幕の有効/無効と言語コードを持つ
2. mpv の起動引数を組み立てる関数を設け、cookie 連携の引数と同時に渡っても壊れないようにする
3. 再生中にキー 1 つで字幕の表示/非表示を切り替える。既存キー(space / ←→↑↓ / `[` `]` / Backspace / w / c / Esc / q / マウス)と衝突しない
4. 切替は mpv を再起動せず IPC で行う
5. ステータス行に現在の字幕状態を出す
6. 指定言語の字幕が無い動画での振る舞いを決める

## 1. 実装が依存する事実(実測)

### 1-1. 自動生成字幕の要求(ytdl_hook → yt-dlp)

| 渡した mpv オプション | yt-dlp に渡る argv | 結果 |
|---|---|---|
| なし(現行の tuitube) | `--sub-format ass/srt/best --sub-langs all --write-srt` | 字幕トラック 0 本(この動画に公式字幕が無いため)。`sid` = `false` |
| `--ytdl-raw-options-append=write-auto-subs=` のみ | `--write-auto-subs --sub-langs all` | **字幕トラック 157 本**。自動翻訳の全言語が並ぶ |
| `write-auto-subs=` + `sub-langs=ja` | `--write-auto-subs --sub-langs ja` | 字幕トラック 1 本(`id:1 type:sub lang:ja title:"Japanese" external:true`) |
| `write-auto-subs=` + `sub-langs=ja,en` | `--sub-langs ja,en` が 1 つの argv 要素として渡る | 字幕トラック 2 本(`en` と `ja`)。`--slang=ja` で `ja` が選ばれる |
| `write-auto-subs=` + `sub-langs=ja-orig` | `--sub-langs ja-orig` | 字幕トラック 1 本(`id:1 type:sub lang:ja-orig title:"Japanese (Original)" external:true`)。`--slang=ja-orig` で選ばれ、`current-tracks/sub/lang` = `"ja-orig"` |
| `write-auto-subs=` + `sub-langs=ja-orig,ja`(既定) | `--sub-langs ja-orig,ja` | 字幕トラック 2 本(`id:1 lang:ja title:"Japanese"` / `id:2 lang:ja-orig title:"Japanese (Original)"`)。`--slang=ja-orig,ja` を渡しても選ばれるのは **`ja`**(`sid` = `1`、`current-tracks/sub/lang` = `"ja"`) |
| `write-auto-subs=` + `sub-langs=zz`(存在しない言語) | `--sub-langs zz` | 字幕トラック 0 本。mpv は正常に再生を続ける。エラーにはならない |

設計への影響:

- `write-auto-subs` は **必ず `sub-langs` とセットで渡す**。ytdl_hook の既定が `--sub-langs all` なので、片方だけだと 157 本のトラックが載る
- `-append` 形式は値を `,` で分割しない。`sub-langs=ja,en` はそのまま 1 要素で届く(cookie の spec と同じ性質)
- ytdl_hook は既定で公式字幕(`--sub-langs all`)を要求している。`sub-langs=ja` を渡すとこの既定を **置き換える**ので、要求は ja 系だけになる。tuitube は 1 言語しか選ばないので実害はない
- 存在しない言語を指定しても再生は壊れない。字幕が「無い」状態として扱えばよい

`automatic_captions` には原語の文字起こし(`ja-orig`、タイトル `Japanese (Original)`)と自動翻訳(`ja`、`en`、…)の両方が並ぶ。日本語の動画では `ja` が原語相当、英語の動画では `ja` は機械翻訳になる。

既定の `ja-orig,ja` は「原語を優先し、無ければ自動翻訳」のつもりで置いたが、両方あるときに mpv が選んだのは `ja` だった(上表)。カンマ区切りの並びは yt-dlp に取得させる範囲を決めるもので、どのトラックを出すかは mpv の判定になる。原語の文字起こしだけを見たいときは `lang = "ja-orig"` と 1 つだけ書く。設定の先頭をステータス行に出すと実態とずれるので、印には `current-tracks/sub/lang` を使う(§2-7)。

### 1-2. 字幕トラックの選択(IPC・mpv 再起動なし)

`--ytdl-raw-options-append=write-auto-subs= --ytdl-raw-options-append=sub-langs=ja --slang=ja` で起動して実測。

| 操作 | 応答 | 読み出し |
|---|---|---|
| 起動直後 | — | `sid` = `1`、`current-tracks/sub/lang` = `"ja"`、`sub-text` = `"[音楽]"` |
| `--sub-visibility=no` を足して起動 | — | `sid` = `1`(選択されたまま)、`sub-visibility` = `false`、`sub-text` は取れる |
| `--sid=no` を足して起動 | — | `sid` = `false`。トラックは track-list に載っている(`selected:false`) |
| `set_property sid no` | `success` | `sid` = `false`、`sub-text` = `property unavailable`、`current-tracks/sub/lang` = `property unavailable` |
| `set_property sid auto` | `success` | 約 2 秒後に `sid` = `1`、`lang` = `"ja"`、`sub-text` が戻る。**`--slang` が再適用される** |
| `set_property sid 1`(数値) | `success` | `sid` = `1` |
| `set_property sid 99`(無いトラック) | `success` が返るが `sid` = `false` | 数値指定は黙って外れる。使わない |
| `set_property sub-visibility true/false`、`cycle sub-visibility` | `success` | 切り替わる。ただし `sub-text` は非表示中でも取れたままになる |
| 字幕が無い動画で `set_property sid auto` | `success` | `sid` = `false` のまま |

設計への影響:

- 表示/非表示は `sid` の `auto` / `no` で切り替える。`--slang` が起動引数に入っていれば `auto` で同じ言語へ戻るので、トラック ID を tuitube 側で覚えなくてよい
- `sid` をポーリングすれば「字幕トラックが選ばれているか」が分かる。数値なら表示中、`false` なら無し(または自分で消した状態)
- `--sid=no` で起動しても字幕トラックは用意される。後から `sid auto` で出せる(実測で `sub-text` まで確認)
- 数値の `sid` を直接送るのは不可。無いトラックでも `success` が返り、状態だけが黙って外れる

### 1-3. VO ごとの字幕の見え方(最重要)

ローカルの `sample.mp4`(640x360 黒画面)+ `sample.srt`(0.5〜4.5 秒に `SUBTITLE TEST`)で、`--frames=1 --start=2` の出力バイト列を比較した。

| VO | `--sid=1` の出力 | `--sid=no` の出力 | 判定 |
|---|---|---|---|
| `kitty` | 2,645,974 バイト | 1,764,004 バイト | **字幕が映像に合成される** |
| `kitty`(字幕の表示時間外 `--start=4.8`) | 1,764,004 バイト | 同左 | 差分の原因が字幕であることの対照確認 |
| `tct` | 100,074 バイト | 100,074 バイト(完全一致) | **字幕は合成されない** |
| `tct` + `--osd-level=3` | 100,074 バイト | 同左 | OSD 自体が出ない |

設計への影響:

- 埋め込み(kitty)と別ウィンドウ(gpu 系)は mpv が字幕を描くので、tuitube 側の描画は要らない
- **テキストモード(tct)では字幕が一切出ない**。ここだけは tuitube が `sub-text` を読んで自分で描かないと、`s` を押しても何も起きない画面になる(§2-8)

### 1-4. cookie 連携との併存

`--ytdl-raw-options-append=cookies-from-browser=... --ytdl-raw-options-append=write-auto-subs= --ytdl-raw-options-append=sub-langs=ja` の 3 つを同時に渡した場合の yt-dlp argv(実測):

```
yt-dlp --no-warnings -J --flat-playlist --sub-format ass/srt/best \
  --cookies-from-browser nosuchbrowser --write-auto-subs --sub-langs ja \
  --write-srt --no-playlist -- <url>
```

`-append` は key-value の追加なので 3 つとも並んで届く。mpv のコマンドラインでの並び順がそのまま argv の順になる。互いに上書きしない。

### 1-5. 起動時間への影響

`duration` が取れるまでの時間(同じ動画・同じ回線、各 1 回):

| 条件 | 時間 |
|---|---|
| 字幕オプションなし | 12.1 秒 |
| `sub-langs=zz`(0 本) | 11.8 秒 |
| `sub-langs=ja`(1 本) | 14.2 秒 |
| `--sid=no` 付き | 14.2 秒 |

ばらつきの範囲で、字幕を要求することによる明確な遅延は測れなかった。字幕ファイル本体は `edl://!no_clip;!delay_open,media_type=sub;…` として登録され、トラックが選ばれたときに初めて取得される(`--sid=no` 起動でも track-list には載る)。

字幕の URL には `expire=<unix time>` が入っている。長時間の一時停止のあとで字幕を有効にすると取得に失敗する可能性がある(未確認・§3-5)。

### 1-6. 現行コードの構造

| 場所 | 役割 | 字幕で使うもの |
|---|---|---|
| `settings.rs` `RawConfig` / `Settings` / `validate` / `render` | TOML の読み書きと検証 | `[subtitles]` を同じ形で足す。`parse_choice` が選択肢の検証と notice をまとめて面倒を見る |
| `display.rs` `LaunchPlan` / `LaunchPlan::args` | 起動引数の組み立て | 字幕の引数をここに足す。`extra_args` は最後という既存の約束を保つ |
| `cookies.rs` `CookieSource::mpv_arg` | `--ytdl-raw-options-append=` の作り方 | 同じ形を踏襲する |
| `speed.rs` `Speed` | 値オブジェクト + `launch_arg` + `command` | 字幕モジュールの雛形 |
| `mpv.rs` `REQ_*` / `poll_commands` / `parse_response` | 毎秒のポーリング | `REQ_SID` を足す。`parse_response` は `request_id` のある応答だけを拾う(イベントは捨てる) |
| `app.rs` `App::apply_property` / `playback_line` / `display_label` | 取り込みとステータス行 | `sid` の取り込みと字幕の印 |
| `actions.rs` `playback_plan` / `set_speed` / `poll_player` / `enter_playback` | mpv への送信 | `toggle_subtitles` は `set_speed` と同じ形にする |
| `input.rs` `handle_key_playing_with` | 再生中のキー | `s` を足す |
| `ui.rs` `help_text` / `draw_playing` | ヘルプ行と映像領域 | ヘルプの追記、テキストモードの字幕行 |

再生中に使用済みのキー: `space` `←` `→` `↑` `↓` `[` `]` `Backspace` `w` `c` `q` `Esc`、マウス左ボタン。`s` は空いている。

## 2. 設計判断

### 2-1. 設定の持ち方

`[subtitles]` に 2 キーだけ置く。

| キー | 型 | 既定 | 意味 |
|---|---|---|---|
| `enabled` | bool | `true` | 字幕を要求するかどうか。`false` なら mpv に字幕の引数を渡さない |
| `lang` | string | `"ja-orig,ja"` | 取得する言語。カンマ区切りで複数書ける。yt-dlp の `sub-langs` と mpv の `--slang` に同じ文字列を渡す。複数あるときどれを出すかは mpv が決める(§1-1) |

検討した代案:

| 案 | 内容 | 判断 |
|---|---|---|
| A(採用) | `enabled` + `lang` の 2 キー。`enabled=false` なら字幕の引数を一切渡さない | キーが少なく意味が素直。`false` にした人には yt-dlp の余計な要求も行かない |
| B | `enabled`(要求するか) + `visible`(起動時に出すか)の 3 キー | 「要求はするが出さない」を設定で持てるが、実行時の状態(§2-6)と二重管理になる。起動時の表示状態は前の動画から引き継ぐ方が自然 |
| C | `lang = ""` を無効の意味にする | 無効化の方法が暗黙になる。notice も出しにくい |

### 2-2. 言語指定の値

`sub-langs`(yt-dlp)と `--slang`(mpv)の両方に同じ文字列を渡す。どちらもカンマ区切りのリストを受け付けるので、`lang = "ja-orig,ja"` と書けば原語の文字起こしと自動翻訳の両方を取ってきて、どちらか 1 本が出る。並びどおりに選ばれるとは限らない(§1-1 の実測では両方あるとき `ja` が出た)。

ただし `sub-langs` は yt-dlp 独自の記法(`all`、正規表現、`-live_chat` のような除外)も受け付ける一方、`--slang` は言語コードの列しか解釈しない。両方に同じ値を渡す設計なので、受け付ける値を **言語コードのカンマ区切りだけ**に絞る。

- 各要素は `^[A-Za-z][A-Za-z0-9-]*$`(`ja` `en` `ja-orig` `pt-BR` `zh-Hans` を通す)
- `all` は拒否する(§1-1 のとおり 157 本のトラックが載る)
- 空・空白のみ・上記に合わない要素があれば、その値を捨てて既定 `ja-orig,ja` に倒し notice を出す(`parse_choice` と同じ扱い)

### 2-3. 表示/非表示の切替方式

| 方式 | 送るもの | 利点 | 欠点 | 判断 |
|---|---|---|---|---|
| A(採用) `sid` の `auto`/`no` | `set_property sid auto` / `set_property sid no` | トラック ID を覚えなくてよい。`sid` を読めば「字幕が無い」ことまで分かる。非表示中は `sub-text` も取れなくなるので、テキストモードの字幕行が自動で消える | 非表示のたびにトラックが外れ、再表示で字幕ファイルを開き直す(実測 約 2 秒で復帰) | 採用 |
| B `sub-visibility` | `cycle sub-visibility` | トラックは付いたまま。復帰が速い | 「字幕が無い」ことを別プロパティで判定する必要がある。非表示でも `sub-text` が返るので、テキストモードの字幕行を別途消す分岐が要る | 不採用 |
| C `cycle sub` | `cycle sub` | 1 コマンド | トラックが複数あると巡回の回数で状態が変わり、tuitube 側で状態を持てない | 不採用 |

`--sub-visibility=yes` は字幕を要求するときは常に起動引数へ入れる。利用者の `mpv.conf` に `sub-visibility=no` があると、トラックを選んでも何も出ない状態になるため。これで実行時の切替は `sid` だけで完結する。

### 2-4. 起動時に何を渡すか

| 状態 | 渡す引数 |
|---|---|
| 設定 `enabled=false` | 何も渡さない(現行と同じ) |
| `enabled=true` かつ表示状態 | `--ytdl-raw-options-append=write-auto-subs=` / `--ytdl-raw-options-append=sub-langs=<lang>` / `--slang=<lang>` / `--sub-visibility=yes` |
| `enabled=true` かつ非表示状態 | 上の 4 つ + `--sid=no` |

非表示で始めてもトラックは用意されるので(§1-2)、再生中に `s` を押せば mpv の再起動なしで出せる。

### 2-5. キー割り当て

`s` を再生中の表示/非表示トグルに割り当てる。

- 再生中の未使用キーであること
- 検索画面の `s` は文字入力のままで、モードごとに分かれているので衝突しない
- mpv 既定の字幕キーは `v`(表示切替)/ `j`(トラック巡回)だが、tuitube は mpv にキーを渡していないので合わせる理由が薄い。`subtitle` の頭文字の方が覚えやすい
- 将来トラック巡回を足すなら `j`(mpv と同じ)を空けておく

### 2-6. 状態の持ち越し

字幕の表示状態は速度(`App.speed`)と同じく **動画をまたいで持ち越す**。設定の `enabled` は起動時の初期値としてだけ使う。

- 1 本目で `s` を押して消したら、次に選んだ動画も消えたまま始まる
- 実装上は `LaunchPlan` に表示状態を渡し、非表示なら `--sid=no` を足す(§2-4)

### 2-7. ステータス行のどこに出すか

再生中のステータス行は現状でも 112 桁あり(タイトルが長ければさらに伸びる)、80 桁端末では末尾から切れる。末尾に足すと字幕の印がほぼ見えない。

| 案 | 例 | 判断 |
|---|---|---|
| A(採用) 状態語の直後 | `PLAYING  字幕ja  <タイトル>  12:34 / 45:06  vol 100  1.0x  [埋め込み 15fps medium]` | 端末が狭くても必ず見える。追加は 6 桁 |
| B `display_label` の中 | `… [埋め込み 15fps medium 字幕ja]` | 行の末尾なので切れやすい |
| C 出さない(押したときの知らせだけ) | — | 3 秒で消えるので、あとから状態を確認できない |

印は状態によって出し分け、非表示のときは何も出さない(桁を使わない)。

| 状態 | 印 |
|---|---|
| 表示中 | `字幕ja`(`current-tracks/sub/lang` = 実際に出ているトラックの言語。取れるまでは設定の先頭で代用) |
| 要求したが `sid` がまだ確定しない | `字幕...` |
| 要求したのに字幕が無い | `字幕なし` |
| 非表示 / 設定で無効 | なし |

切り替えた直後は既存の期限つき知らせ(`set_temporary_notice`、3 秒)で返事を出す。

### 2-8. テキストモード(tct)の扱い

§1-3 のとおり tct では mpv が字幕を描かない。

| 案 | 内容 | 判断 |
|---|---|---|
| A | 何もしない。`s` を押したら「テキスト表示では字幕が出ない」と知らせる | 実装は最小だが、3 つある表示モードの 1 つで機能が欠ける |
| B(採用) | テキストモードのときだけ `sub-text` を読み、映像領域の下端に字幕行を描く | tct でも字幕が読める。埋め込み・別ウィンドウでは mpv が描くので二重にならない |
| C | どのモードでも TUI に字幕行を描く | 埋め込みでは焼き込みと二重に出る |

描画位置は映像領域の下端に重ねる(`playing_areas` の行数は変えない)。行を 1 つ増やすと映像の寸法が変わり、mpv の VO を作り直すことになるため。テキストモードの映像は `VideoSink::render_text` が ratatui のバッファへ描くので、その後に同じバッファの下端を上書きすれば足りる。

段階としては Phase 2 とし、Phase 1(埋め込み・別ウィンドウ)を先に通す(§7)。

### 2-9. 字幕テキストの取り込み方(Phase 2)

| 案 | 内容 | 遅れ | 影響範囲 |
|---|---|---|---|
| A | 既存の毎秒ポーリングに `sub-text` を足す | 最大 1 秒。短いセリフを取りこぼす | 小 |
| B(採用) | テキストモードかつ表示中のときだけ 250ms のティックで `sub-text` を読む | 最大 0.25 秒 | 小(`main.rs` の `select!` に 1 本足す) |
| C | `observe_property` でイベントとして受ける | 遅れなし | `mpv::parse_response` をイベントも返す形に変える + `AppEvent` 追加。既存テストの前提が変わる |

C が本筋だが、`parse_response` は「`request_id` のある応答だけを拾い、イベント行は捨てる」という前提で既存テストが書かれている。まず B で通し、字幕行の見え方に不満があれば C へ移す(§9)。

### 2-10. 操作系(誰がどこでどう使うか)

| 場面 | 起きること | 設計での受け止め |
|---|---|---|
| 音を出せない場所で見る | 最初から字幕が要る | 設定 `enabled=true` で最初から表示。動画ごとに押し直さなくてよい |
| 聞き取れなかった所だけ出す | 一瞬で出したい | `s` 1 打。mpv 再起動なしなので再生位置も音も途切れない |
| 字幕が邪魔なとき | すぐ消したい | 同じ `s` で消え、次の動画にも持ち越す |
| 字幕の無い動画を開いた | 押しても何も出ない | `字幕なし` を出す。`s` は止めずに受け、要求を切った上で「この動画に ja-orig,ja の字幕がありません」と返す |
| 字幕の無い動画で消したい | 消せないと次の動画にも要求が残る | 「字幕なし」は推定なので `s` を拒まない。押したら `sid no` を送り、持ち越す要求も false になる |
| 別ウィンドウで mpv 側のキーを押された | mpv 既定の `j` / `v` で `sid` が外れると、tuitube からは「字幕なし」と同じに見える | `s` で一度消し、もう一度 `s` で `sid auto` を送れば出し直せる |
| 字幕が付くまでの数秒 | 無いのか遅いのか分からない | `字幕...` を出し、`duration` が取れてから `SELECT_GRACE` たつまで「なし」と言わない。`duration` が来ない再生(ライブ配信など)は要求から `LOAD_GRACE` で打ち切る |
| Kitty 非対応端末(テキストモード) | mpv が字幕を描かない | Phase 2 の字幕行。Phase 1 では押したときに「テキスト表示では映像に字幕が出ない」と返す |
| 外国語の動画 | 自動翻訳と原語のどちらが出るか選びたい | `lang="ja-orig"` なら原語だけ、`lang="ja"` なら自動翻訳だけを取ってくる(§2-2) |
| 端末が狭い | ステータス行が切れる | 印を行の前方に置く(§2-7) |
| 設定で切っている人が `s` を押した | 何も起きないと壊れて見える | 「`[subtitles] enabled` が false」と返す |

### 2-11. mpv 引数の並び順

`LaunchPlan::args()` の並びを次の順にする。

```
[VO とその寸法 / fps 上限 or ウィンドウ系]  →  --speed  →  字幕の引数  →  extra_args
```

- `extra_args` は最後(既存テスト `extra_args_come_last_in_the_plan` の約束)。利用者が自分の `--slang` や `--sid` を書けば後勝ちで上書きできる
- cookie の引数は現行どおり `playback_plan` が `extra_args` の末尾へ足す。字幕の引数はその手前に入る。両方が同時に渡っても yt-dlp の argv には別々の要素として並ぶ(§1-4)

## 3. アーキテクチャ

### 3-1. モジュール構成

新規 `src/subtitles.rs`。`speed.rs` / `cookies.rs` と同じく、外部プロセスに触れず値と文言だけを持つ。`main.rs` に `mod subtitles;` を足す。

```
settings.rs  --(SubtitleSettings)-->  display.rs (LaunchPlan) --> mpv.rs (launch_args)
                     |                        ^
                     |                        | SubtitleLaunch
                     v                        |
                subtitles.rs  <-- app.rs (SubtitleState) <-- actions.rs (toggle / poll)
                     ^                                            ^
                     |                                            |
                   ui.rs (ヘルプ・字幕行)                      input.rs ('s')
```

### 3-2. 既存コードの変更点

| ファイル | 変更 |
|---|---|
| `src/subtitles.rs` | 新規。`SubLang` / `SubtitleSettings` / `SubtitleLaunch` / `SubtitleState` / `SubtitleStatus` / 文言 |
| `src/main.rs` | `mod subtitles;`。Phase 2 で 250ms ティックを `select!` に追加 |
| `src/settings.rs` | `RawSubtitles` 追加、`RawConfig.subtitles`、`Settings.subtitles`、`validate_subtitles`、`render` に `[subtitles]` 節 |
| `src/display.rs` | `LaunchPlan.subtitles: SubtitleLaunch` を追加。`LaunchPlan::new` で設定から作り、`args()` で `--speed` の後・`extra_args` の前に展開 |
| `src/mpv.rs` | `REQ_SID: u64 = 8` / `REQ_SUB_LANG: u64 = 9` / (Phase 2) `REQ_SUB_TEXT: u64 = 10` を追加。`poll_commands()` に `sid` と `current-tracks/sub/lang` を追加(6→8 件)。`sub_text_command()` を追加 |
| `src/app.rs` | `App.subtitles: SubtitleState`。`apply_property` に `REQ_SID` / `REQ_SUB_LANG` / (Phase 2) `REQ_SUB_TEXT`、`REQ_DURATION` で `observe_loaded`。`playback_line` に印を追加 |
| `src/actions.rs` | `toggle_subtitles` 追加。`playback_plan` で `plan.subtitles` を設定。`enter_playback` で `subtitles.begin_playback(now)`。`poll_player` で必要なときだけ `sub-text` を追加送信 |
| `src/input.rs` | `handle_key_playing_with` に `KeyCode::Char('s')` |
| `src/ui.rs` | `help_text` に `s:字幕`。Phase 2 で `draw_playing` のテキストモード分岐に字幕行 |

### 3-3. 型と関数(TDD の足場)

```rust
// src/subtitles.rs

/// yt-dlp の sub-langs と mpv の --slang に同じ文字列で渡す言語指定。
/// 受け付けるのは言語コードのカンマ区切りだけ (yt-dlp の正規表現記法や all は --slang が解釈できない)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubLang(String);

impl SubLang {
    pub const DEFAULT: &'static str = "ja-orig,ja";

    /// 空・all・言語コードでない要素があれば None。呼び出し側が既定へ倒して notice を出す。
    pub fn parse(value: &str) -> Option<Self>;
    pub fn as_str(&self) -> &str;
    /// 表示用。先頭の言語コード。
    pub fn primary(&self) -> &str;
}

impl Default for SubLang; // "ja-orig,ja"

/// 検証済みの [subtitles]。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubtitleSettings {
    pub enabled: bool,
    pub lang: SubLang,
}

impl Default for SubtitleSettings; // enabled: true, lang: ja-orig,ja

/// 起動引数の元。再生開始時に字幕を出すかどうかまで含む。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum SubtitleLaunch {
    /// 設定で無効。字幕の引数を一切渡さない。
    #[default]
    Disabled,
    Requested { lang: SubLang, shown: bool },
}

impl SubtitleLaunch {
    pub fn new(settings: &SubtitleSettings, shown: bool) -> Self;
    /// write-auto-subs は sub-langs とセットで渡す (単独だと ytdl_hook の既定 all が残り 157 トラックになる)。
    pub fn args(&self) -> Vec<String>;
}

/// 再生中の字幕の状態。表示したいかは動画をまたいで持ち越す。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SubtitleState {
    wanted: bool,
    /// ポーリングした sid。数値なら選ばれている。
    selected: Option<u64>,
    /// 選ばれているトラックの言語 (current-tracks/sub/lang)。設定の先頭とは限らない。
    selected_lang: Option<String>,
    /// 表示を要求した時刻。ここから SELECT_GRACE の間は「字幕なし」と言わない。
    requested_at: Option<Instant>,
    /// 動画の長さが取れた時刻。「字幕なし」を言い出す起点。
    loaded_at: Option<Instant>,
    /// テキストモード用に取り込んだ現在の字幕。
    text: Option<String>,
}

impl SubtitleState {
    pub fn from_settings(settings: &SubtitleSettings) -> Self;
    pub fn wanted(&self) -> bool;

    /// 押されたときの行き先。送信に成功するまで状態は動かさない (set_speed と同じ形)。
    /// Err は出せない理由 (設定で無効)。
    pub fn toggle_command(&self, settings: &SubtitleSettings) -> Result<(bool, MpvCommand), String>;

    /// 送信できたときに確定させる。
    pub fn set_wanted(&mut self, wanted: bool, now: Instant);

    /// 新しい mpv を起動したとき。選択・言語・長さを捨て、要求時刻を打ち直す。
    pub fn begin_playback(&mut self, now: Instant);

    /// 応答が取れなかったとき (property unavailable) は触らない (speed と同じ扱い)。
    pub fn observe_sid(&mut self, data: Option<&Value>);
    pub fn observe_sub_lang(&mut self, data: Option<&Value>);
    pub fn observe_text(&mut self, data: Option<&Value>);

    /// loaded は再生する動画の長さが取れたか (playback.duration.is_some())。
    /// 初めて取れた時刻を控え、そこから SELECT_GRACE を数える。
    pub fn observe_loaded(&mut self, loaded: bool, now: Instant);

    pub fn status(&self, settings: &SubtitleSettings, now: Instant) -> SubtitleStatus;

    /// ステータス行の印。非表示・無効のときは None。
    pub fn marker(&self, settings: &SubtitleSettings, now: Instant) -> Option<String>;

    /// テキストモードで sub-text を読む必要があるか。
    pub fn needs_text(&self, display: DisplayMode) -> bool;

    /// 映像の下端に重ねる行。空なら描かない。
    pub fn overlay_lines(&self, display: DisplayMode, width: u16) -> Vec<String>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubtitleStatus {
    /// 設定で無効。
    Disabled,
    /// 利用者が消している。
    Off,
    /// 要求したが sid がまだ確定しない。
    Loading,
    Shown,
    /// 要求したのに、この動画には指定言語の字幕が無い。
    Missing,
}

/// sid の確定を待つ時間。遅延選択で字幕ファイルを開くのに実測 約2秒かかる。
/// 要求してからと、動画の長さが取れてからの両方でこれだけ待つ。
pub const SELECT_GRACE: Duration = Duration::from_secs(3);

/// 動画の長さが取れないまま「字幕なし」と言い出すまでの時間。
/// 長さの取得自体に実測 12〜14 秒かかる (§1-5) ので、それに SELECT_GRACE 分を足す。
pub const LOAD_GRACE: Duration = Duration::from_secs(20);

/// 表示する / 隠すコマンド。--slang を起動引数に入れてあるので auto で同じ言語へ戻る。
pub fn show_command() -> MpvCommand;  // set_property sid "auto"
pub fn hide_command() -> MpvCommand;  // set_property sid "no"

/// 切り替えた直後の知らせ。
pub fn toggle_notice(wanted: bool, lang: &SubLang) -> String;
/// 出せない / 出なかったときの知らせ。
pub fn disabled_notice() -> String;
pub fn missing_notice(lang: &SubLang) -> String;
pub fn text_mode_notice() -> String; // Phase 1 のみ
```

`SubtitleLaunch::args()` が返す並び(`Requested { lang: "ja", shown: false }` の場合):

```
--ytdl-raw-options-append=write-auto-subs=
--ytdl-raw-options-append=sub-langs=ja
--slang=ja
--sub-visibility=yes
--sid=no
```

`shown: true` なら最後の `--sid=no` だけ落ちる。`Disabled` は空の `Vec`。

mpv 側の追加:

```rust
// src/mpv.rs
pub const REQ_SID: u64 = 8;
pub const REQ_SUB_LANG: u64 = 9;
pub const REQ_SUB_TEXT: u64 = 10; // Phase 2

pub fn sub_text_command() -> MpvCommand; // get_property sub-text (request_id: 10)
```

`poll_commands()` に `("sid", REQ_SID)` と `("current-tracks/sub/lang", REQ_SUB_LANG)` を足す(8 件)。`sub-text` は毎秒のポーリングには入れず、必要なときだけ別に送る(§2-9)。

アクション側:

```rust
// src/actions.rs
pub async fn toggle_subtitles(app: &mut App, session: &mut Session, now: std::time::Instant) {
    let (wanted, command) = match app.subtitles.toggle_command(&app.settings.subtitles) {
        Ok(next) => next,
        Err(reason) => { app.set_temporary_notice(reason, now); return; }
    };
    let Some(player) = session.player.as_mut() else { return };
    match player.sink.send(&command).await {
        Ok(()) => {
            app.subtitles.set_wanted(wanted, now);
            app.set_temporary_notice(subtitles::toggle_notice(wanted, &app.settings.subtitles.lang), now);
        }
        Err(e) => app.set_error(Some(e)),
    }
}
```

### 3-4. フロー

再生開始:

```
start_playback
  └ playback_plan
      ├ LaunchPlan::new(...)        … settings.subtitles から SubtitleLaunch::new(settings, app.subtitles.wanted())
      ├ plan.speed = app.speed
      └ plan.extra_args += cookie の引数
  └ MpvController::launch(url, …, plan)   … launch_args = 固定 + plan.args() + url
  └ enter_playback
      └ app.subtitles.begin_playback(now)   … selected/text を捨て、要求時刻を打ち直す
```

キー `s`:

```
handle_key_playing_with('s')
  └ toggle_subtitles
      ├ 設定で無効        → 知らせだけ出して終わり
      ├ 表示 → 非表示     → set_property sid "no"
      └ 非表示 → 表示     → set_property sid "auto"
          └ 送信できたら wanted を反転し、知らせを出す
```

毎秒のポーリング:

```
on_tick → poll_player
  ├ poll_commands()      … time-pos / duration / pause / volume / current-vo / speed / sid / current-tracks/sub/lang
  └ needs_text ならば sub_text_command()   (Phase 2 では 250ms ティックからも送る)
      └ 応答は AppEvent::MpvProperty → App::apply_property
          ├ REQ_DURATION  → subtitles.observe_loaded
          ├ REQ_SID       → subtitles.observe_sid
          ├ REQ_SUB_LANG  → subtitles.observe_sub_lang
          └ REQ_SUB_TEXT  → subtitles.observe_text
```

### 3-5. 状態と失敗の扱い

| 事象 | 検知 | 振る舞い |
|---|---|---|
| 設定 `enabled=false` で `s` | `toggle_command` が `Err` | 「`[subtitles] enabled` が false」の知らせ。mpv へは何も送らない |
| 再生していないときの `s` | `session.player` が `None` | 何もしない(`set_speed` と同じ) |
| 指定言語の字幕が無い | `duration` が取れた後、`SELECT_GRACE` を過ぎても `sid` が数値にならない | ステータス行に `字幕なし`。`s` は止めずに受け、`sid no` を送ったうえで「この動画に ja-orig,ja の字幕がありません」 |
| 字幕の選択に時間がかかる | 上の条件に達する前 | `字幕...` を出す。「なし」とは言わない |
| `duration` が来ない再生(ライブ配信など) | `observe_loaded` が一度も true にならない | 要求から `LOAD_GRACE` で `字幕なし` に倒す。`字幕...` のまま固定しない |
| `sid` のポーリングが一度取れない | 応答が `property unavailable` | 前の選択を保つ。1 回の欠測で `字幕なし` へ落とさない |
| mpv 側のキーで字幕を外された | `sid` が `false` になる | `字幕なし` と同じ扱い。`s` 2 回(`sid no` → `sid auto`)で出し直せる |
| mpv への送信失敗 | `sink.send` が `Err` | `app.set_error`。状態(`wanted`)は動かさない |
| `lang` の綴り間違い | `SubLang::parse` が `None` | 起動時の notice。既定 `ja-orig,ja` で動く |
| テキストモードで字幕を出した | `display == Text` | Phase 1: 「テキスト表示では映像に字幕が出ない」と知らせる / Phase 2: 映像の下端に字幕行 |
| 字幕 URL の期限切れ(未確認) | `sid auto` を送っても数値にならない | 「字幕なし」と同じ扱いになる。再生し直せば新しい URL になる |
| 表示モードを `w` で切り替えた | — | `sid` は VO と独立なので触らない。テキストモードへ入る/出るときに字幕行の要否だけが変わる |

## 4. 設定ファイル仕様

`[playback]` の次に置く(再生時の mpv の振る舞いという括りで隣り合うため)。

```toml
[subtitles]
# YouTube の自動生成字幕を要求するか。false のときは再生中に s を押しても出せない。
enabled = true
# 取得する字幕の言語。カンマ区切りで複数書ける。
# 例: "ja-orig" (原語の文字起こし) / "ja" (自動翻訳) / "ja-orig,ja"
# 複数書いたときにどれを出すかは mpv が決める。先頭が選ばれるとは限らない (実測)。
# yt-dlp の sub-langs と mpv の --slang に同じ値を渡すので、言語コード以外 ("all" や正規表現) は書けない。
lang = "ja-orig,ja"
```

`render` は既存の書き方に合わせ、値をそのまま出す。`parse` → `validate` → `render` の往復が既存テスト(`render_round_trips_through_parse`)と同じ形で通ること。

notice の文言:

| 入力 | notice |
|---|---|
| `lang = ""` | `[subtitles] lang="" は読めません。ja-orig,ja で取得します` |
| `lang = "all"` | `[subtitles] lang="all" は読めません。ja-orig,ja で取得します` |
| `lang = "ja*"` | `[subtitles] lang="ja*" は読めません。ja-orig,ja で取得します` |

## 5. 表示文言

| 場面 | 文言 |
|---|---|
| ステータス行(表示中) | `字幕ja`(出ているトラックの言語) |
| ステータス行(選択待ち) | `字幕...` |
| ステータス行(字幕なし) | `字幕なし` |
| `s` で表示にした | `字幕を出します (ja-orig)`(設定の先頭。まだどれが選ばれるか分からないため) |
| `s` で非表示にした | `字幕を消しました` |
| 設定で無効なのに `s` | `[subtitles] enabled が false です。設定ファイルを直して再生し直すと字幕を出せます` |
| 字幕が無い動画で `s`(要求は切る) | `この動画に ja-orig,ja の字幕がありません`(先頭だけだと ja なら出ると読めるので列ごと出す) |
| テキストモードで `s`(Phase 1 のみ) | `テキスト表示では映像に字幕が出ません` |
| ヘルプ行 | `s:字幕` |

再生中のヘルプ行は現状でも 80 桁に収まっていない(実測: 埋め込み 101 桁 / 別ウィンドウ 105 桁。`ui.rs` の桁数テストは検索画面の 2 行だけを見ている)。`s:字幕` を足すと 112 桁になる。

| 案 | 桁数(別ウィンドウ時) |
|---|---|
| 現行のまま `s:字幕` を足す | 112 |
| `←→/クリック:シーク`→`←→:シーク`、`速度±0.1`→`速度`、`URLコピー`→`URL` | 93 |
| さらに `一時停止`→`停止`、`Esc:停止`→`Esc:戻る`、`[ ]`→`[]` | 88 |
| 上に加えて `w:別ウィンドウ`→`w:表示` | 80 |

どこまで削るかは §8 で確認する。

## 6. テスト戦略

外部プロセスを起動しないで確かめられる範囲を先に固める。mpv・yt-dlp を呼ぶのは手動確認(§6-3)だけにする。

### 6-1. 最初に書く Red(モジュール別)

`subtitles.rs`

1. `SubLang::parse` が `ja` / `ja-orig` / `ja,en` / `pt-BR` / `zh-Hans` を通す。前後の空白と要素間の空白を落として正規化する(`" ja , en "` → `"ja,en"`)
2. `SubLang::parse` が `""` / `"  "` / `"all"` / `"ja*"` / `"ja.en"` / `"-live_chat"` / `",ja"` / `"ja,,en"` を `None` にする
3. `SubLang::primary` が `"ja-orig,ja"` から `"ja-orig"` を返す
4. `SubtitleLaunch::Disabled.args()` が空
5. `SubtitleLaunch::new(有効, shown=true).args()` が `write-auto-subs=` / `sub-langs=ja` / `--slang=ja` / `--sub-visibility=yes` をこの順で返し、`--sid` を含まない
6. `shown=false` では末尾に `--sid=no` が付く
7. `write-auto-subs` を渡すときは必ず `sub-langs` も渡す(引数列に両方あることを 1 つのテストで固定する。単独で渡すと字幕トラックが 157 本になる実測への歯止め)
8. `-append` の値がそのまま渡る(`sub-langs=ja,en` が 1 要素で、`,` で分かれない)
9. `show_command().to_line()` == `{"command":["set_property","sid","auto"]}\n`
10. `hide_command().to_line()` == `{"command":["set_property","sid","no"]}\n`
11. `toggle_command` が設定無効のとき `Err`(文言に `[subtitles] enabled` を含む)
12. `toggle_command` が 表示中→`(false, hide_command)`、非表示→`(true, show_command)` を返す。呼んだだけでは状態が変わらない
13. `set_wanted(true, now)` の後 `wanted()` が true、`requested_at` が入る
14. `observe_sid` が `json!(1)` で選択済み、`json!(false)` / `json!("auto")` で未選択になる。応答が取れなかった(`None`)ときは前の選択を保つ
15. `status` の分岐: 設定無効→`Disabled` / `wanted=false`→`Off` / 長さ未取得で要求から `LOAD_GRACE` 未満→`Loading` / 長さが取れてから猶予内→`Loading` / 長さが取れてから猶予後も未選択→`Missing` / 長さが来ないまま `LOAD_GRACE`→`Missing` / `sid` が数値→`Shown`
15-2. 長さが取れた後に `set_wanted(true)` で出し直したときは、その時刻から改めて `SELECT_GRACE` 待つ
16. `begin_playback` が `selected` / `selected_lang` / `loaded_at` / `text` を捨て、`wanted` は保つ(動画をまたぐ持ち越し)
17. `marker` が `Shown`→`Some("字幕<選ばれた言語>")` / `Loading`→`Some("字幕...")` / `Missing`→`Some("字幕なし")` / `Off`・`Disabled`→`None`。`observe_sub_lang` が来るまでは設定の先頭で代用する
18. `toggle_notice(true, ja)` / `(false, ja)` の文言
19. `needs_text` が `DisplayMode::Text` かつ表示中のときだけ true(埋め込み・別ウィンドウでは false)
20. `overlay_lines` が `"[音楽]\nマグカップが…"` を 2 行に割り、幅で切り詰める。全角は表示幅で数える。`text` が None・空文字なら空の `Vec`

`settings.rs`

21. `[subtitles]` 未記載の設定が既定値(`enabled=true` / `lang="ja"`)になり、notice が出ない
22. `enabled = false` が読める
23. `lang = "all"` / `""` / `"ja*"` で notice が出て `ja` に倒れる。他のセクションの値は巻き添えにならない
24. `render` → `parse` → `validate` の往復で `SubtitleSettings` が一致する(既定値・変更値の両方)
25. `render` の出力に `[subtitles]` 節と `lang` の書き方の例が入る

`display.rs`

26. `LaunchPlan::new` が `settings.subtitles` から `SubtitleLaunch` を作る(`enabled=false` なら `Disabled`)
27. `LaunchPlan::args()` の並びが `--speed` → 字幕の引数 → `extra_args` の順(既存の `extra_args_come_last_in_the_plan` を保ったまま)
28. `Disabled` のとき、引数列に `slang` / `sub-langs` / `sid` / `sub-visibility` が 1 つも出ない
29. 埋め込み・テキスト・別ウィンドウのどのモードでも字幕の引数が同じように入る(VO と独立)
30. cookie の引数(`extra_args` の末尾)と字幕の引数が両方入り、順序は 字幕 → cookie になる

`mpv.rs`

31. `poll_commands()` に `{"command":["get_property","sid"],"request_id":8}` と `{"command":["get_property","current-tracks/sub/lang"],"request_id":9}` が入り、件数が 8 になる
32. `sub_text_command().to_line()` == `{"command":["get_property","sub-text"],"request_id":10}\n`
33. `launch_args` が `plan.args()` をそのまま挟む(既存テストに字幕付きの plan を 1 つ足す)

`app.rs`

34. `apply_property(REQ_SID, Some(json!(1)))` で `Shown` になる
35. `apply_property(REQ_SID, Some(json!(false)))` で選択が外れ、`None` では外れない
36. `apply_property(REQ_SUB_TEXT, …)` が `text` に入り、`None` で消える
37. `playback_line` が表示中は状態語の直後に `字幕ja` を挟み、非表示では何も挟まない
38. 長いタイトルでも印が状態語の直後にある(切り詰めで消えないことの確認)
38-2. `apply_property(REQ_SUB_LANG, Some(json!("ja")))` の後、印が設定の先頭でなく `字幕ja` になる
38-3. `apply_property(REQ_DURATION, None)` のまま `LOAD_GRACE` 過ぎたら `Missing` になる(ライブ配信)

`actions.rs`(偽 `PlayerSink` で送信内容を溜める既存の `Recorder` を使う)

39. `toggle_subtitles` が非表示のとき `{"command":["set_property","sid","auto"]}` だけを送り、`wanted` が true になる
40. もう一度呼ぶと `{"command":["set_property","sid","no"]}` を送り、`wanted` が false に戻る
41. 送信に失敗したら `app.error` が入り、`wanted` は変わらない
42. `player` が無いときは何も送らず、状態も変わらない
43. 設定無効のときは何も送らず、知らせだけが出る
44. `playback_plan` が `app.subtitles.wanted()` を `LaunchPlan` に反映する(非表示なら `--sid=no` が入る)
45. `enter_playback` の後に `selected` が空で、`wanted` は前の動画から引き継がれている
46. `poll_player` がテキストモードかつ表示中のときだけ `sub-text` を追加で送る
46-2. `字幕なし` の状態で `s` を押しても止めず、`sid no` を送って `wanted` が false になる(知らせは「この動画に … の字幕がありません」)
46-3. その状態からもう一度 `s` を押すと `sid auto` を送り直す(mpv 側で外されたときの復帰)

`input.rs`

47. 再生中の `s` が字幕のトグルを呼ぶ(偽 `PlayerSink` に届いたコマンドで判定する)
48. 再生中の `s` が他の操作(シーク・速度・一時停止・表示モード切替・URL コピー)を起こさない
49. 検索入力中の `s` は文字入力のまま(クエリに `s` が入り、字幕は動かない)
50. 結果一覧の `s` は何もしない

`ui.rs`

51. 再生中のヘルプ行に `s:字幕` が入る
52. 決めた桁数の上限に収まる(§8 で上限を決めてから固定する)
53. (Phase 2)テキストモードのとき、字幕行が映像領域の下端に描かれる。埋め込み・別ウィンドウでは描かれない

### 6-2. 偽 player での結合テスト

`actions.rs` の `Recorder`(送った JSON 行を溜める偽 `PlayerSink`)をそのまま使う。「起動 → `s` で表示 → `s` で非表示 → 次の動画を再生」を 1 本のテストで通し、次の 2 点を固定する。

- 2 本目の起動引数に `--sid=no` が入る(持ち越しの確認)
- 送った行が `sid auto` → `sid no` の 2 行だけで、`sub-visibility` を実行時に送っていない

### 6-3. 手動確認(実機)

自動テストで届かない範囲。

1. 埋め込み(kitty)で字幕が映像に焼き込まれて見えること、`s` で消えて再び出ること(再生位置が飛ばないこと)
2. 端末を小さくしたときの字幕の読めなさの程度(§9 の判断材料)
3. テキストモード(tct)で字幕行が出ること、表示モードを `w` で回しても二重に出ないこと
4. 別ウィンドウで mpv 側に字幕が出ること
5. 字幕の無い動画(公式字幕も自動生成もないもの)で `字幕なし` が出ること
6. cookie 連携を有効にした状態で字幕付き再生ができること(§1-4 は argv の確認までで、実際の cookie ストア越しの再生は未確認)
7. 英語の動画で `lang="ja"` のとき機械翻訳の日本語が出ること、`lang="ja-orig,ja"` で原語の文字起こしが出ること

## 7. 実装順序と依存

Phase 1(埋め込み・別ウィンドウで字幕が出て、`s` で切り替わる)

1. `subtitles.rs` の値オブジェクトと引数生成(Red 1〜10)
2. `settings.rs` の `[subtitles]`(Red 21〜25)
3. `display.rs` の `LaunchPlan` 組み込み(Red 26〜30)
4. `mpv.rs` の `REQ_SID` とポーリング(Red 31〜33)
5. `subtitles.rs` の状態遷移と文言(Red 11〜19)
6. `app.rs` の取り込みとステータス行(Red 34〜38)
7. `actions.rs` の `toggle_subtitles` ほか(Red 39〜45)
8. `input.rs` の `s`(Red 47〜50)
9. `ui.rs` のヘルプ行(Red 51〜52)

Phase 2(テキストモードの字幕行)

10. `subtitles.rs` の `needs_text` / `overlay_lines`(Red 20・19)
11. `actions.rs` / `main.rs` の `sub-text` 取り込み(Red 46・36)
12. `ui.rs` の描画(Red 53)

1〜4 は互いに独立なので並行して書ける。5 以降は 1 に依存する。

## 8. 実装前に確認したいこと(決定済み)

| # | 確認したこと | 決定 |
|---|---|---|
| 1 | 既定を `enabled = true` にしてよいか | `true` で実装。初回から字幕付きで始まる |
| 2 | 既定の `lang` は `"ja"` でよいか | `"ja-orig,ja"` を既定にした(`SubLang::DEFAULT`)。ただし両方あるとき mpv が出すのは `ja` で、原語が優先されるわけではない(§1-1)。原語だけ見たい人は `lang = "ja-orig"` と書く |
| 3 | テキストモードの字幕行(Phase 2)を今回の範囲に入れるか | 入れない。テキストモードの `s` は知らせを出し、トラックだけ選ぶ |
| 4 | 再生中のヘルプ行をどこまで削るか | 文言は削らず、`fit_hints` が幅に入るところまでで打ち切る形にした |
| 5 | キーは `s` でよいか | `s` で実装 |
| 6 | 表示状態を動画をまたいで持ち越すか | 持ち越す(速度と同じ扱い) |

残っている判断: 既定を `"ja-orig"`(原語だけ)に変えるか、`"ja-orig,ja"` のまま「どちらか出ればよい」とするか。§1-1 の実測を受けての再検討。

## 9. 会話に出ていないが追加検討してほしい要素

- 言語の切替キー。`lang` に複数書いたときに再生中でトラックを巡回する(`j` が空いている)
- `observe_property` への移行。字幕テキストの取り込みをイベント駆動にすると遅れが無くなる(§2-9 の案 C)。`mpv::parse_response` をイベントも返す形に変える必要がある
- 埋め込みモードでも TUI に字幕行を出す設定。端末が小さいと焼き込みの字幕が読みにくい可能性がある(実機確認の 2 番目の結果しだい)
- 現在の字幕テキストをクリップボードへコピーする操作(`c` が URL なので別キー)
- 字幕の見た目(`--sub-font-size` / `--sub-color` / `--sub-pos`)を `[subtitles]` で持つか、`[mpv] extra_args` に任せるか
- 公式字幕と自動生成字幕の優先順位を設定で選べるようにするか(現状は `write-auto-subs` を付けて yt-dlp と `--slang` の選択に任せる)
- 検索結果の一覧に字幕の有無を出すか(yt-dlp の JSON から取れるが、1 件ずつの取得が増える)
- 環境変数での一時的な上書き(`TUITUBE_FPS_LIMIT` / `TUITUBE_COOKIES_FROM_BROWSER` と同じ形)
