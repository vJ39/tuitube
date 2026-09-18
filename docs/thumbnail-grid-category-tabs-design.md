サムネイル付きグリッド表示と擬似カテゴリタブの設計

対象バージョン: yt-dlp 2026.08.19 系 / curl 8.7.1 / ratatui 0.29 / crossterm 0.28 / 実機端末 iTerm2 3.6.11。
前提コミット: `7fb1a52`。

## 0. 合意済み仕様とスコープ

| 項目 | 内容 |
| --- | --- |
| 機能1 | 検索結果をサムネイル画像+タイトルの格子で表示する。現行のテキスト1行リストを置き換える |
| 機能2 | 固定キーワードによる擬似カテゴリタブ。タブを選ぶとそのタブのクエリで検索する。「すべて」タブは現行の検索ボックス入力を使う |
| 前提 | YouTube の本物のカテゴリタブ(`/gaming`・`/feed/trending`)は YouTube 側が廃止済みで再現不可。固定キーワードで代替する |
| 対象外 | 画像のマウス操作、無限スクロール、チャンネル/再生リストの表示、再生画面の変更 |

スコープ外だが影響するもの: 再生開始・終了時の画像消去の順序(§2-7)。ここだけは既存の再生経路に手を入れる。

## 1. 実装が依存する事実(実測)

### 1-1. `thumbnails` 配列の URL は WebP を返す

依頼文にある「サムネイルは .jpg で配信される」は成り立たない。`--flat-playlist --dump-json` の `thumbnails` 配列に入る URL は拡張子が `.jpg` でも、実際の応答は WebP だった。

```
$ yt-dlp "ytsearch2:rust tui" --flat-playlist --dump-json
  thumbnails: 2 件
    {"url": "https://i.ytimg.com/vi/awX7DUp-r14/hq720.jpg?sqp=-oaymwEcCOgC...&rs=AOn4CLBp...", "height": 202, "width": 360}
    {"url": "https://i.ytimg.com/vi/awX7DUp-r14/hq720.jpg?sqp=-oaymwEcCNAF...&rs=AOn4CLAC...", "height": 404, "width": 720}

$ curl -o /dev/null -w '%{content_type} %{size_download}' "<上の1件目>"
image/webp 10022
$ file <保存したもの>
RIFF (little-endian) data, Web/P image, VP8 encoding, 360x202
```

`Accept: image/jpeg` を明示しても、`Accept: */*` でも、`Accept: image/webp,image/jpeg,*/*` でも応答は `image/webp` で変わらなかった(3 通りとも実測)。`sqp=` 付きの URL は YouTube 側のリサイザを通っており、形式はサーバーが決める。

### 1-2. 動画 ID から組み立てた固定 URL は JPEG を返す

| URL | 応答 | 寸法 | サイズ |
| --- | --- | --- | --- |
| `https://i.ytimg.com/vi/<id>/mqdefault.jpg` | `image/jpeg` | 320x180 (16:9) | 9-20 KB |
| `https://i.ytimg.com/vi/<id>/hq720.jpg` | `image/jpeg` | 1280x720 (16:9) | 107 KB |

`mqdefault.jpg` は `Accept` の指定に関わらず JPEG だった(3 通りとも実測)。10 件の検索結果すべてで 200 が返り、全て JPEG 320x180 だった。
`hqdefault.jpg` / `sddefault.jpg` は 4:3 に上下の黒帯が付くので使わない。

### 1-3. curl 1 プロセスで 10 枚を 0.14 秒で取れる

```
$ curl -sS --fail --max-time 10 --parallel --parallel-max 6 \
    -o /tmp/thumbs/<id1>.jpg https://i.ytimg.com/vi/<id1>/mqdefault.jpg ... (10 組)
  0.139 total   (--parallel なしの直列でも 0.200 total)
```

同一ホストなので接続が再利用され、直列でも十分速い。`--parallel` は curl 7.66 以降、実機は 8.7.1。

### 1-4. JPEG デコード手段の比較(実測)

| 方式 | 追加クレート数 | 所要 | 備考 |
| --- | --- | --- | --- |
| `zune-jpeg` | 2 (`zune-jpeg` + `zune-core`) | 320x180 を 0.58 ms | 純 Rust。`cargo tree` で確認 |
| `image`(`default-features = false, features = ["jpeg"]`) | 8 (`image` `bytemuck` `byteorder-lite` `moxcms` `num-traits` `pxfm` `zune-core` `zune-jpeg`) + build-dep `autocfg` | 同上 | 中身の JPEG デコーダは `zune-jpeg` そのもの。差分は `imageops::resize` などの付帯機能 |
| mpv を1枚ごとに起動 | 0 | 1枚 0.3 秒(実測 `0.308 total`) | `mpv --no-config --really-quiet --frames=1 --vf=scale=W:H,format=rgb24 --of=rawvideo --ovc=rawvideo --o=out.rgb in.jpg` で `W*H*3` バイトちょうどが出る。WebP も同じ手順で通った |

`zune-jpeg` 0.5.15 の I/F は実際に動かして確認した。

```rust
let opts = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::RGB);
let mut d = JpegDecoder::new_with_options(ZCursor::new(&bytes), opts);
let px = d.decode()?;            // RGB 3 バイト/px の Vec<u8>
d.dimensions();                  // Some((320, 180))
```

`ZCursor` / `ColorSpace` / `DecoderOptions` は `zune-core` から来るので、`zune-core` も直接依存に入れる(推移依存として既に入るため、クレート数は増えない)。
壊れた入力(先頭 20 バイトだけ)と空入力はいずれも `Err` を返し、panic はしなかった。

### 1-5. 自分から Kitty graphics protocol で画像を送る

既存の映像経路(`docs/kitty-protocol-design.md`)で実機確認済みの事実のうち、そのまま使えるもの。

| 事実 | 出典 | 利用 |
| --- | --- | --- |
| `a=T,f=24,s=<幅px>,v=<高さpx>,C=1,q=2,m=...` で raw RGB を base64 で送れば iTerm2 に出る | 既存実装が毎フレームこれを書いている | サムネイルも同じ形で送る |
| 先頭チャンクへ `c=<cols>,r=<rows>` を足すと端末側がそのセル矩形へ拡大する | `video::push_scaled` | 画像矩形への拡大に使う |
| `q=2` を外すと端末の応答が tuitube の stdin に入りキー誤読になる | 既存設計 §1-2 | 必ず付ける |
| `ESC[2J` と端末リセット以外の文字消去では画像は消えない | 既存設計 §「文字消去との関係」 | ratatui の部分再描画では貼り直し不要 |
| 端末リサイズでは ratatui が `2J` を出すので画像は消える | 既存設計 §「端末リサイズ」 | リサイズ後は必ず貼り直す |
| `\x1b_Ga=d,q=2;\x1b\\` で画面上の画像が消える | `video::encode_clear` | グリッドの貼り直し前と再生開始時に使う |
| APC 1 個のペイロードは 4096 バイトまで。`m=1` で継続、`m=0` で終端 | `kitty::fixtures::frame` | 送信側も同じ分割にする |

未確認の1点: **同時に複数枚の画像を別々の位置へ置いたときの iTerm2 の挙動**。既存経路は常に1枚を同じ位置へ描き直しているだけなので、8-10 枚同時は未検証。これは実装前に潰す(§6-0)。

### 1-6. 現行コードの構造(行参照)

| 箇所 | 内容 |
| --- | --- |
| `src/main.rs:92` | `app.screen = terminal.draw(...)` の直後に `present_video` を呼び、実端末へ APC を書く。書き手はここだけ |
| `src/main.rs:131` `present_video` | `VideoSink::take()` の保留と `session.owe_clear` をまとめて `video::encode` に渡す |
| `src/ui.rs:63` `draw_search` | 入力ボックス(3行) / 結果リスト(残り) / ステータス(1行) / ヘルプ(1行)の4分割 |
| `src/ui.rs:194` `help_text` | モードごとの操作説明 |
| `src/app.rs:111` `App` | `results` / `selected` / `query` / `mode` などの画面状態 |
| `src/app.rs:164,171` | `select_next` / `select_prev`。末尾で巻き戻る |
| `src/app.rs:182` `set_results` | 結果を差し替え、0 件ならエラーにして Input へ戻す |
| `src/input.rs:33` `handle_key_input` | Input のキー: `Enter` `Backspace` `Char` `Esc` |
| `src/input.rs:59` `handle_key_results` | Results のキー: `↑` `↓` `Enter` `/` `Esc` `q` |
| `src/actions.rs:147` `start_search_with` | 検索を tokio タスクで投げる。`YtDlp` trait を差し替えるとテストで外部プロセスへ行かない |
| `src/actions.rs:188` `cancel_search` | nonce を進めて先行検索の結果を捨てる |
| `src/actions.rs:305` `apply_resize` | 200ms デバウンス後に映像寸法を作り直す。`app.video` が無い(非再生中)ときは何もせず戻る |
| `src/search.rs:14` `SearchResult` | `id` / `title` / `duration` / `uploader` |
| `src/cookies.rs:309` `Target` | `Search(String)` と `Feed(Feed)`。`yt_dlp_url()` が `ytsearch10:` か `:ytrec` 等を返す |

### 1-7. 現行のキー割り当てと空き

| モード | 使用中 | 空き(今回使えるもの) |
| --- | --- | --- |
| Input | `Enter` `Backspace` 文字 `Esc` | `Tab` `BackTab` `←` `→` `↑` `↓` |
| Results | `↑` `↓` `Enter` `/` `Esc` `q` | `Tab` `BackTab` `←` `→` `r` 数字 |
| Playing | `space` `←` `→` `↑` `↓` `[` `]` `Backspace` `w` `q` `Esc` | (触らない) |

crossterm では Shift+Tab は `KeyCode::Tab` ではなく `KeyCode::BackTab` で届く。

## 2. 設計判断

### 2-1. ダウンロード方式: curl のサブプロセス

| 案 | 追加クレート | 判断 |
| --- | --- | --- |
| `reqwest` | 80 前後(hyper/h2/tower/TLS 一式) | 不採用。現在の Cargo.lock は 22KB で、この1機能のために依存の桁が変わる |
| `ureq` | 15 前後 | 不採用。同期 I/F なので `spawn_blocking` も要る |
| **curl のサブプロセス** | 0 | **採用**。yt-dlp / mpv と同じ「外部プロセスを呼ぶ」形。`YtDlp` trait と同じ差し替え方でテストできる |

1 回の検索につき curl は**1 プロセス**。全 URL を `-o <path> <url>` の対で並べ、`--parallel --parallel-max 6` を付ける(§1-3 で 10 枚 0.14 秒)。

curl が無い環境では `ErrorKind::NotFound` が返るので、`yt-dlp が見つかりません` と同じ作りで1回だけ通知を出し、以後サムネイル取得を止める。グリッドは枠だけで動き続ける。

### 2-2. デコード方式: `zune-jpeg` + `zune-core`

採用理由。

- 追加は 2 クレートで、どちらも純 Rust・推移依存なし(§1-4 で `cargo tree` 実測)。`image` は同じデコーダを内包して 8 クレートになるので、差額 6 クレートで買えるのは `imageops::resize` だけ。リサイズは §2-3 の 40 行程度で足り、しかも自前のほうが固定バイト列でテストしやすい。
- インプロセスなので 1 枚 0.58 ms。10 枚で 6 ms。一時ファイルもプロセスも増えない。
- デコードとリサイズが純関数になるので、依頼にある「固定バイト列で検証する」形にそのまま乗る。

mpv を 1 枚ごとに起動する案(§1-4 の3行目)は実際に動くことを確認済みだが、1 枚 0.3 秒で 10 枚だと並列にしてもプロセスが 10 個増え、単体テストは偽サブプロセス越しにしか書けない。**追加クレートを 1 つも入れたくない場合の代替**として残す。この場合 WebP も通るので §1-1 の制約が外れる利点がある。

### 2-3. リサイズ: 面積平均(縮小専用)

サムネイルは 320x180、表示先は 144x80 程度なので**常に縮小**になる。拡大は起きない(端末セルが極端に大きい場合だけ理論上あり得るが、その場合は等倍のまま `c=`/`r=` で端末に拡大させる)。

縮小は出力 1 px に対応する入力矩形の平均を取る。最近傍だと文字が潰れて何の動画か分からなくなる。実装は入力座標を整数で刻むだけで、浮動小数の丸め差が出ない形にする。

```rust
/// src/rgb.rs
pub fn shrink(src: &RgbImage, dst_w: u32, dst_h: u32) -> Option<RgbImage>;
```

`dst >= src` のときは複製して返す(拡大しない)。`dst_w` か `dst_h` が 0 なら `None`。

### 2-4. base64: 自前(RFC 4648 標準アルファベット)

エンコードだけで 20 行程度、デコードは要らない。`base64` クレート(推移依存 0)を足してもよいが、ここは自前にして依存を 2 クレートに抑える。誤りが出るとすれば末尾 1-2 バイトの詰め方だけで、RFC 4648 のテストベクタ(`""` `"f"` `"fo"` `"foo"` `"foob"` `"fooba"` `"foobar"`)がそこを正確に突く。

### 2-5. サムネイル URL は動画 ID から組み立てる

§1-1 の実測により、`thumbnails` 配列の URL は使わない。`SearchResult` に URL のフィールドを足す必要も無くなる(既存のテスト用コンストラクタを一切触らずに済む)。

```rust
/// src/thumbs.rs
pub fn url_for(id: &str) -> String   // https://i.ytimg.com/vi/<id>/mqdefault.jpg
```

320x180 で足りるかの検算: 画像矩形は最大でも 6 列 × セル幅 33 桁 = 32 桁 ≒ 256 px(セル幅 8px の場合)。320 px を超えるのは 1 セルあたり 40 桁を超える極端に横長の端末だけなので、`mqdefault` 一本で足りる。`hq720`(1280x720, 107KB)への切り替えは §9 に回す。

ファイル名は動画 ID から作るので、`[A-Za-z0-9_-]` 以外を含む ID はその場で弾く(パス操作の材料にしない)。

### 2-6. APC の送り方: `a=T` の貼り直し、画像 ID は使わない

Kitty protocol には画像 ID(`i=`)で一度送って何度も配置し直す使い方があり、再送量を減らせる。しかし実機 iTerm2 で確かめてあるのは mpv と同じ `a=T`(送信と同時に表示)だけなので、まずは確かめてある形に揃える。

```
\x1b[<row>;<col>H                                                 ← CUP で左上へ
\x1b_Ga=T,f=24,s=144,v=80,C=1,q=2,c=18,r=5,m=1;<base64 4096 バイト>\x1b\\
\x1b_Gm=1;<base64 4096 バイト>\x1b\\
...
\x1b_Gm=0;<base64 端数>\x1b\\
```

これを画像の枚数ぶん並べる。`C=1` でカーソルが動かないので、次の CUP は絶対座標のままでよい。

貼り直しの前には必ず `video::encode_clear()`(`\x1b_Ga=d,q=2;\x1b\\`)を1回出して画面上の画像を消す。消さないと、列数が変わったときに前の配置が残る。

転送量の見積り: 144x80x3 = 34,560 バイト → base64 46,080 バイト → 12 チャンク。8 枚で約 370 KB。映像経路は 640x336 のフレームを 15fps(毎秒 12 MB 前後)で流して実用になっているので、貼り直しが起きるたびに 370 KB は問題にならない。ただし**毎周書くと話が変わる**ので §2-7 を守る。

### 2-7. 貼り直す条件(dirty)

メインループは1周ごとに `terminal.draw` を呼ぶ。1秒ごとのティッカーでも、マウスが1セル動いただけでも周回する。毎周 370 KB を書くと端末が詰まるので、**変化したときだけ**書く。

貼り直しが要るのは次の場合だけ。

| きっかけ | 理由 |
| --- | --- |
| 検索結果が入れ替わった | 中身が変わる |
| サムネイルのデコードが完了した | 新しく出せる画像が増えた |
| タブを切り替えた | 中身が変わる |
| スクロールした | 可視範囲が変わる |
| 端末がリサイズされた | ratatui の `2J` で画像が消える(§1-5) |
| 再生が終わって Results に戻った | mpv の `a=d` で消えている |

選択の移動だけでは貼り直さない。選択の強調は画像の外側(タイトル行と余白)に描くので、画像は動かない(§2-9)。

`Mode::Playing` の間は1枚も書かない。映像と重なるため。

書く順序は `present_video` → `present_thumbs` に固定する。逆にすると、再生終了時に持ち越された `owe_clear` の `a=d` が、貼ったばかりのサムネイルを消す。

### 2-8. キャッシュ

2 段。

| 層 | 置き場 | キー | 破棄 |
| --- | --- | --- | --- |
| ディスク | `$XDG_CACHE_HOME/tuitube/thumbs/<id>.jpg`(無ければ `$HOME/.cache/...`) | 動画 ID | 起動時に更新時刻の新しい順で `max_cached` 枚を残して削除 |
| メモリ | `App` の中 | 動画 ID | 新しい検索結果に無い ID は捨てる |

メモリ側が持つのは**リサイズ後の RGB**。144x80 なら 1 枚 34 KB、10 枚で 340 KB。元の JPEG は持たない(必要になればディスクから読み直す。0.58 ms)。

ディスクキャッシュがあると、再起動後もネットワーク無しでサムネイルが出る。同じ動画が別のタブや別の検索語で出てきたときも取り直さない。

パスの決定はファイルシステムに触らない純関数にする(既存の `settings::config_path(xdg, home)` と同じ形)。

```rust
pub fn cache_dir(xdg_cache_home: Option<&OsStr>, home: Option<&OsStr>) -> Option<PathBuf>;
```

### 2-9. グリッドの割り付け

検索画面の縦分割を1行増やす。

```
┌ 検索 ─────────────────────────┐  3 行 (既存)
└───────────────────────────────┘
 すべて │ 音楽 │ ゲーム │ ニュース │ …   1 行 (新設・タブ)
┌ 結果 1-8/10 ──────────────────┐  残り (グリッド)
│ ┌────┐ ┌────┐ ┌────┐ ┌────┐   │
│ │画像│ │画像│ │画像│ │画像│   │
│ └────┘ └────┘ └────┘ └────┘   │
│ タイトル…  タイトル…            │
│ 12:34 · 投稿者                 │
└───────────────────────────────┘
 ステータス                        1 行 (既存)
 ヘルプ                            1 行 (既存)
```

1 セルの構成は上から `画像(可変行) / タイトル(1行) / 時間・投稿者(1行) / 下余白(1行)`。右にも 1 桁の余白を置く。

割り付けの決め方(すべて整数演算)。

1. `columns = clamp(inner.width / 16, 1, 6)`
2. `cell_width = inner.width / columns`、画像の桁数は `cell_width - 1`
3. `image_rows = round(画像桁数 * cell.width_px * 9 / (16 * cell.height_px))`、最低 2 行
4. `cell_height = image_rows + 3`
5. `rows = inner.height / cell_height`、1 行も入らなければグリッドを諦めてリスト表示へ落とす

80x24 の端末(セル 8x16 px)での実際の数値。

| 値 | 結果 |
| --- | --- |
| 結果ブロックの内側 | 78 桁 × 16 行 |
| columns | `78/16 = 4` |
| cell_width / 画像桁数 | 19 / 18 |
| image_rows | `round(18*8*9/(16*16)) = round(5.06) = 5` |
| cell_height | 8 |
| rows | `16/8 = 2` |
| 1 画面 | 4 × 2 = 8 件 |
| デコード目標 | 18 桁 × 8px = 144 px、5 行 × 16px = 80 px |

120x40 なら 6 列 × 3 行 = 18 件、画像は 144x80。

タイトルは 18 桁に収まらないことが多い。**選択中の項目の完全なタイトルはステータス行に出す**ので、格子の中は識別できる程度で足りるとみなす。

セル幅は表示幅で数える(全角は 2 桁)。既存 `ui::cursor_x` が `Span::raw(s).width()` を使っているのと同じ。

### 2-10. 選択とスクロール

| キー | 動き |
| --- | --- |
| `→` | `selected + 1`。末尾では止まる |
| `←` | `selected - 1`。先頭では止まる |
| `↓` | `selected + columns`。はみ出す場合は末尾へ寄せる |
| `↑` | `selected - columns`。はみ出す場合は動かない |

グリッドでは巻き戻さない(2 次元では戻り先が直感に合わない)。リスト表示に落ちたときは既存の `select_next` / `select_prev` の巻き戻り動作をそのまま使う。既存のテストは変えない。

スクロールは行単位。先頭表示位置 `scroll`(項目インデックス)を持ち、選択が可視範囲から出たときだけ動かす。

```rust
pub fn ensure_visible(selected: usize, columns: usize, rows: usize, scroll: usize) -> usize;
```

`columns` は端末幅で変わるので、`scroll` は描画のたびに `columns` 単位へ丸める。

### 2-11. 擬似カテゴリタブ

タブは「ラベル」と「クエリ文字列」の対。クエリ文字列は既存の `Target::for_query()` にそのまま通す。これにより、`:ytrec` のようなログイン連動の指定もタブとして書ける(既定では入れない。§9)。

```rust
/// src/category.rs
pub struct Category { pub label: String, pub query: String }
```

既定のタブ(設定ファイルで丸ごと差し替え可能)。

| 位置 | ラベル | クエリ |
| --- | --- | --- |
| 0 | すべて | (検索ボックスの入力) |
| 1 | 音楽 | `音楽` |
| 2 | ゲーム | `ゲーム` |
| 3 | ニュース | `ニュース` |
| 4 | アニメ | `アニメ` |
| 5 | スポーツ | `スポーツ` |

タブ行の表示幅は区切り込みで 53 桁。80 桁の端末に収まる(実測計算)。

**タブごとに結果・選択・スクロールを保持する。** 一度見たタブに戻ったとき、再検索を待たされない。ネットワークも使わない。サムネイルのキャッシュは動画 ID で共通なので、タブをまたいでも取り直しは起きない。

「すべて」タブの扱い。

- 検索ボックスの文字列は「すべて」タブのものとする。
- Input モードで `Enter` を押したら、どのタブを見ていても「すべて」へ移り、ボックスの文字列で検索する。ボックスとタブの関係を 1 対 1 に保つため。
- カテゴリタブを選んでいる間もボックスの文字は消さない。戻れば元の入力が残っている。

### 2-12. キー割り当て

| モード | 追加 | 既存との衝突 |
| --- | --- | --- |
| Input | `Tab` = 次のタブ / `BackTab` = 前のタブ | 無し(§1-7) |
| Results | `Tab` `BackTab` = タブ切替 / `←` `→` = 格子内の移動 / `r` = 現在のタブを取り直す | 無し |
| Playing | 変更なし | - |

`←` `→` をタブ切替に使わなかった理由: Results では格子内の移動に要る。Input で `←` `→` をタブに割り当てると Results と意味が変わるので、両モードで同じ `Tab` に統一する。

ヘルプ行の文言と表示幅(実測計算)。

| モード | 文言 | 幅 |
| --- | --- | --- |
| Input (現行) | `Enter:検索  :ytrec/:ythis/:ytsubs/:ytwatchlater:ログイン連動の一覧  Esc:結果へ/終了` | 83 桁 (80 桁端末で既に切れている) |
| Input (新) | `Enter:検索  Tab:カテゴリ  :yt*:ログイン連動の一覧  Esc:結果へ/終了` | 66 桁 |
| Results (新) | `↑↓←→:選択  Enter:再生  Tab:カテゴリ  r:再取得  /:検索へ  q:終了` | 63 桁 |

Input のヘルプを縮めると既存テスト `ui::tests::input_help_mentions_feed_keywords`(4 つのキーワードが全部載っていることを確認している)が落ちる。**意図的に落として書き換える**。現行の文言は 80 桁端末で既に末尾が切れており、テストが守っているのは画面に出ていない文字列なので、キーワード列を `:yt*` に畳む。

### 2-13. 操作系(誰がどこでどう使うか)

| 場面 | 起きること | 設計上の手当て |
| --- | --- | --- |
| 起動直後 | 何を検索するか決まっていない | タブが出ているので `Tab` を押せば何か見られる。起動時に自動で検索はしない(cookie 許可ダイアログなど副作用のある処理を勝手に走らせない) |
| ネットワークが遅い / 画像が落ちてこない | サムネイルが来ない | 枠だけ出して操作は全部通す。画像は選択・再生の前提にしない |
| 電車で中断して後で戻る | セッションは生きたまま | タブごとに結果を保持しているので、戻ったときに再検索が走らない |
| 一度終了して翌日また開く | 同じ動画をまた検索する | ディスクキャッシュからサムネイルが出る。ネットワーク待ちが無い |
| 席を外した間に端末サイズが変わった | 画像が消えている | リサイズのデバウンス後に再デコードして貼り直す。JPEG はディスクから読むのでネットワークは使わない |
| 再生して戻る | mpv が `a=d` で画像を全部消している | Results に戻る時点で貼り直しを予約する |
| Kitty protocol 非対応の端末 | 画像が出ない(エラーも出ない) | `[search] layout = "list"` で従来表示に戻せる。端末の判定はしない |
| 端末が小さい | 格子が組めない | 1 セル行も入らなければ自動でリスト表示に落ちる |
| タブを連打する | 検索が積み重なる | 既存の `cancel_search` と nonce がそのまま効く。最後の1つだけが画面に出る |
| カテゴリを自分用に変えたい | 「アニメ」より「将棋」が見たい | 設定ファイルの `[[categories]]` で差し替え。コード変更は不要 |
| 放っておくとキャッシュが増える | ディスクを食う | 起動時に `max_cached` 枚まで間引く |

### 2-14. 劣化時の振る舞い

| 事象 | 画面 | 再試行 |
| --- | --- | --- |
| curl が無い | 通知を 1 回だけ。以後サムネイル取得は止める | しない |
| ダウンロード失敗(404・タイムアウト) | そのセルだけ枠のまま | 次の検索まではしない |
| デコード失敗 | 同上 | しない |
| キャッシュディレクトリを作れない | 通知を 1 回。メモリキャッシュだけで動く | 毎回試みない |
| 端末が画像を出せない | 枠のまま。エラーは出さない | - |

いずれの場合もキー操作・検索・再生は通常どおり動く。

## 3. アーキテクチャ

### 3-1. モジュール構成

新規 6 ファイル。1 ファイル 1 関心にして、後から並行で触っても衝突しない形にする。

| ファイル | 責務 | 外部依存 |
| --- | --- | --- |
| `src/rgb.rs` | `RgbImage` 型、縮小、base64、APC 1 枚ぶんのエンコード | 無し |
| `src/jpeg.rs` | JPEG バイト列 → `RgbImage` | `zune-jpeg` |
| `src/fetch.rs` | curl の引数組み立てと起動(trait で差し替え可) | `tokio::process` |
| `src/thumbs.rs` | サムネイルの URL・キャッシュパス・状態表・貼り直しの要否 | `jpeg` `rgb` `fetch` |
| `src/grid.rs` | 格子の割り付け、選択移動、スクロール、タイトルの切り詰め | `ratatui::layout` |
| `src/category.rs` | タブ定義、選択状態、タブごとの結果保持 | `cookies::Target` |

既存ファイルへの変更。

| ファイル | 変更 |
| --- | --- |
| `src/app.rs` | `App` に `tabs` / `thumbs` / `scroll` を追加。`set_results` をタブへ書き込む形に。検索中のステータス文言に選択タイトルとタブ名を足す |
| `src/ui.rs` | `search_areas()` を新設(5 分割)。`draw_search` を「タブ行 + グリッド or リスト」に。`help_text` を §2-12 の文言に |
| `src/input.rs` | Input と Results に `Tab` / `BackTab`、Results に `←` `→` `r` |
| `src/actions.rs` | `start_thumbnails()` 追加、`switch_tab()` 追加、`apply_resize` の先頭に非再生時の処理を足す |
| `src/main.rs` | `present_video` の直後に `present_thumbs` を呼ぶ。`AppEvent::ThumbsReady` を捌く |
| `src/settings.rs` | `[search]` `[thumbnails]` `[[categories]]` を追加 |
| `src/search.rs` | `Target::yt_dlp_url` の件数を設定から渡せるようにする(既定 10 のまま) |
| `Cargo.toml` | `zune-jpeg` `zune-core` を追加 |

`src/kitty.rs` `src/video.rs` `src/mpv.rs` `src/display.rs` `src/tct.rs` `src/seekbar.rs` `src/speed.rs` は変更しない。`video::encode_clear` と `video::placement` と `video::CellSize` を使うだけ。

### 3-2. 型と関数(TDD の足場)

```rust
// ---- src/rgb.rs ----
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RgbImage {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,          // 3 バイト/px。長さは width*height*3
}

impl RgbImage {
    pub fn new(width: u32, height: u32, pixels: Vec<u8>) -> Option<Self>;  // 長さが合わなければ None
}

/// 縮小のみ。dst が src 以上なら複製を返す。
pub fn shrink(src: &RgbImage, dst_w: u32, dst_h: u32) -> Option<RgbImage>;

/// アスペクトを保ったまま箱に内接する寸法。
pub fn fit_box(src: (u32, u32), box_px: (u32, u32)) -> (u32, u32);

/// RFC 4648 標準アルファベット。
pub fn base64_into(bytes: &[u8], out: &mut String);

/// CUP + チャンク分割した APC。placement は video::placement の戻り。
pub fn encode_image(image: &RgbImage, at: Placement, out: &mut Vec<u8>);

// ---- src/jpeg.rs ----
#[derive(Debug, PartialEq, Eq)]
pub enum DecodeError { Empty, Broken(String), TooLarge }

pub fn decode(bytes: &[u8]) -> Result<RgbImage, DecodeError>;

// ---- src/fetch.rs ----
pub struct Download { pub id: String, pub url: String, pub path: PathBuf }

pub fn curl_args(items: &[Download], timeout: Duration, parallel_max: u8) -> Vec<String>;

pub trait Fetcher {
    fn run(&self, args: Vec<String>) -> impl Future<Output = std::io::Result<Output>> + Send;
}
pub struct RealCurl;

// ---- src/thumbs.rs ----
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThumbState { Pending, Ready(RgbImage), Failed }

#[derive(Default)]
pub struct Thumbs {
    entries: HashMap<String, ThumbState>,
    decoded_px: (u32, u32),       // 今デコードしてある目標寸法
    dirty: bool,
    disabled: Option<String>,      // curl が無い等、止めた理由
}

impl Thumbs {
    /// 新しい結果集合に入れ替える。消える ID の状態は捨てる。
    pub fn reset(&mut self, ids: &[String]);
    /// 取得が要る ID(未取得・寸法が変わった)を返す。
    pub fn wanted(&self, ids: &[String], target_px: (u32, u32)) -> Vec<String>;
    pub fn apply(&mut self, images: Vec<(String, Result<RgbImage, ()>)>, target_px: (u32, u32));
    pub fn get(&self, id: &str) -> Option<&RgbImage>;
    pub fn mark_dirty(&mut self);
    pub fn take_dirty(&mut self) -> bool;
}

/// 動画 ID からの URL・キャッシュパス。ファイルシステムに触らない。
pub fn url_for(id: &str) -> String;
pub fn safe_id(id: &str) -> Option<&str>;
pub fn cache_dir(xdg_cache_home: Option<&OsStr>, home: Option<&OsStr>) -> Option<PathBuf>;
pub fn cache_path(dir: &Path, id: &str) -> Option<PathBuf>;
/// 更新時刻の新しい順に keep 枚だけ残す対象を選ぶ(削除は呼び出し側)。
pub fn prune_targets(files: Vec<(PathBuf, SystemTime)>, keep: usize) -> Vec<PathBuf>;

// ---- src/grid.rs ----
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellRect { pub image: Rect, pub title: Rect, pub meta: Rect }

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    pub columns: usize,
    pub rows: usize,
    pub offset: usize,
    pub image_px: (u32, u32),     // デコード目標(画像矩形のピクセル寸法)
    pub cells: Vec<CellRect>,     // 可視ぶんだけ。cells[i] は結果 offset+i
}

/// 格子を組めない(狭すぎる)ときは None。呼び出し側はリスト表示へ落とす。
pub fn layout(inner: Rect, cell: CellSize, count: usize, scroll: usize) -> Option<Layout>;
pub fn move_selection(selected: usize, count: usize, columns: usize, dir: Dir) -> usize;
pub fn ensure_visible(selected: usize, columns: usize, rows: usize, scroll: usize) -> usize;
/// 表示幅で切り詰める。切ったときは末尾に "…"。
pub fn truncate(text: &str, width: usize) -> String;

// ---- src/category.rs ----
pub struct Category { pub label: String, pub query: String }

#[derive(Default)]
pub struct TabState {
    pub results: Vec<SearchResult>,
    pub selected: usize,
    pub scroll: usize,
    pub loaded: bool,
}

pub struct Tabs {
    categories: Vec<Category>,     // [0] は「すべて」
    selected: usize,
    states: Vec<TabState>,
}

impl Tabs {
    pub fn with_categories(categories: Vec<Category>) -> Self;  // 先頭へ「すべて」を差し込む
    pub fn next(&mut self);
    pub fn prev(&mut self);
    pub fn labels(&self) -> Vec<&str>;
    pub fn selected(&self) -> usize;
    pub fn is_all(&self) -> bool;
    /// 「すべて」なら検索ボックスの文字列、それ以外はタブのクエリ。
    pub fn target(&self, query: &str) -> Option<Target>;
    pub fn state(&self) -> &TabState;
    pub fn state_mut(&mut self) -> &mut TabState;
}
```

`App` への追加。

```rust
pub struct App {
    // 既存...
    pub tabs: Tabs,
    pub thumbs: Thumbs,
    pub layout_mode: LayoutMode,   // Grid | List (設定 + 端末の広さで決まる)
}
```

`results` / `selected` は `tabs.state()` へ移す案もあるが、既存の多数のテストと `selected_result()` が `App.results` を見ているので、**`App.results` / `App.selected` は現在のタブの内容を映す写しとして残す**。タブ切替時に `TabState` と差し替える。既存コードの変更量を抑えるため。

新しいイベント。

```rust
pub enum AppEvent {
    // 既存...
    ThumbsReady {
        nonce: u64,                                  // 検索と同じ nonce を使い、古い結果を捨てる
        target_px: (u32, u32),
        images: Vec<(String, Result<RgbImage, ()>)>,
    },
}
```

`Session` への追加は `thumbs_task: Option<JoinHandle<()>>` の 1 つ。nonce は検索用の `search_nonce` を流用する(検索が入れ替わればサムネイルも用済みになるため)。

### 3-3. パイプライン

```
検索完了 (SearchDone)
   ↓ set_results → tabs へ保存 → thumbs.reset(ids)
start_thumbnails(nonce, ids, target_px)
   ↓ tokio::spawn  (1 タスク)
   ├ ディスクキャッシュにある ID は読むだけ
   ├ 無い ID → curl を 1 プロセスで一括取得 (--parallel)
   ├ 読めたファイルを spawn_blocking で decode → fit_box → shrink
   └ AppEvent::ThumbsReady { nonce, target_px, images }
   ↓
thumbs.apply(...) → dirty = true
   ↓ メインループ
terminal.draw()            ← 画像の無いセルはプレースホルダ、タイトル・選択枠は毎回描く
present_video()            ← 保留があれば a=d や映像フレーム
present_thumbs()           ← dirty のときだけ a=d + (CUP + APC) × 枚数
```

デコードを別タスクに置く理由は、10 枚で 6 ms という実測から見て必須ではないが、通信待ちと同じタスクにまとめると書き方が一直線になるため。`spawn_blocking` に入れておけばワーカーも塞がない。

### 3-4. 状態遷移(タブ)

```
Tab 押下
  → tabs.next()
  → 現在のタブへ results/selected/scroll を書き戻す
  → 次のタブの TabState を App へ写す
  → thumbs.reset(次のタブの ID 群) + mark_dirty
  → loaded が false なら start_search(そのタブの Target)
  → loaded が true なら検索しない(保持していた結果をそのまま出す)
```

`r`(再取得)は `loaded = false` にしてから同じ経路を通す。

## 4. 設定ファイル仕様

```toml
[search]
# "grid" = サムネイル付きの格子、"list" = 従来の1行リスト。
# Kitty graphics protocol 非対応の端末では "list" にする。
layout = "grid"
# 1 回の検索で取る件数 (ytsearchN の N)。1..=50。
limit = 10

[thumbnails]
# false にすると取得しない。格子のまま枠だけが出る。
enabled = true
# 取得したサムネイルの置き場。既定は $XDG_CACHE_HOME/tuitube/thumbs
# cache_dir = "~/.cache/tuitube/thumbs"
# 起動時にここまで間引く枚数。
max_cached = 500
# 1 枚あたりのダウンロード上限秒数。
timeout_secs = 10

# カテゴリタブ。書いた場合は既定の一覧を丸ごと置き換える。
# 先頭の「すべて」タブは常に自動で付くので書かない。
# [[categories]]
# label = "音楽"
# query = "音楽"
```

検証の方針は既存 `settings::validate` と同じで、壊れた値でも起動は止めず、読み替えた旨を通知に積む。

- `layout` が未知の綴り → `grid` に倒す + 通知
- `limit` が範囲外 → 丸める + 通知
- `[[categories]]` が空配列 → 既定の一覧を使う + 通知
- `label` か `query` が空の項目 → その項目だけ捨てる + 通知

## 5. 表示文言

| 場面 | 文言 |
| --- | --- |
| 結果ブロックの題名 | ` 結果 1-8/10 ` (可視範囲/総数) |
| ステータス(Results) | `8 件  |  <選択中のタイトル>  |  cookies: chrome` |
| ステータス(サムネイル取得中) | `8 件  |  サムネイル取得中...  |  cookies: chrome` |
| curl が無い | `curl が見つかりません (PATH を確認してください)。サムネイルなしで表示します` |
| キャッシュを作れない | `サムネイルの保存先を作れませんでした: <理由>。今回は保存せずに表示します` |
| カテゴリの検索が 0 件 | 既存の `Target::empty_message()` をそのまま使う |
| ヘルプ(Input) | `Enter:検索  Tab:カテゴリ  :yt*:ログイン連動の一覧  Esc:結果へ/終了` |
| ヘルプ(Results) | `↑↓←→:選択  Enter:再生  Tab:カテゴリ  r:再取得  /:検索へ  q:終了` |

## 6. テスト戦略

### 6-0. 実装前の実機スパイク(最優先)

§1-5 の未確認事項「複数枚を別々の位置に同時に置けるか」を、実装に入る前に 1 分で潰す。iTerm2 で次を実行し、4 枚が別々の位置に並ぶことを目で確認する。

```sh
python3 - <<'EOF'
import base64, sys
def img(row, col, w, h, rgb):
    data = base64.b64encode(bytes(rgb) * (w * h)).decode()
    out = f"\x1b[{row};{col}H"
    for i in range(0, len(data), 4096):
        chunk = data[i:i+4096]
        more = 1 if i + 4096 < len(data) else 0
        if i == 0:
            out += f"\x1b_Ga=T,f=24,s={w},v={h},C=1,q=2,c=10,r=3,m={more};{chunk}\x1b\\"
        else:
            out += f"\x1b_Gm={more};{chunk}\x1b\\"
    return out
sys.stdout.write("\x1b[2J")
for i, (r, c, color) in enumerate([(2,2,(255,0,0)), (2,20,(0,255,0)), (8,2,(0,0,255)), (8,20,(255,255,0))]):
    sys.stdout.write(img(r, c, 80, 48, color))
sys.stdout.write("\x1b[20;1H")
sys.stdout.flush()
EOF
```

- 4 枚とも見える → 設計どおり進められる
- 1 枚しか残らない / 位置がずれる → §9 の「画像 ID を使う方式」へ切り替えるか、格子の画像を 1 枚(選択中のみ大きく表示)に変える。ここで分かれば設計をやり直す手戻りは小さい

続けて `\x1b_Ga=d,q=2;\x1b\\` を書いて 4 枚とも消えることも見ておく(§2-7 の貼り直しが成立する条件)。

### 6-1. 最初に書く Red(モジュール別)

ネットワークもサブプロセスも端末も要らないものから並べる。1-6 は全て純関数で、ここだけで機能の芯が固まる。

**1. `rgb::base64_into`**

```rust
fn base64_matches_rfc4648_vectors()
    // "" → "", "f" → "Zg==", "fo" → "Zm8=", "foo" → "Zm9v",
    // "foob" → "Zm9vYg==", "fooba" → "Zm9vYmE=", "foobar" → "Zm9vYmFy"
fn base64_handles_non_ascii_bytes()
    // [0x00,0xff,0x80] → "AP+A"
fn base64_appends_without_clearing_the_buffer()
```

**2. `rgb::fit_box` / `rgb::shrink`**

```rust
fn fit_box_keeps_aspect_and_fits_inside()
    // (320,180) を (144,80) の箱へ → (142,80) 相当。箱からはみ出さないこと
fn fit_box_returns_the_source_when_it_already_fits()
fn shrink_averages_the_source_area()
    // 2x2 の [黒,白 / 白,黒] を 1x1 へ → (127,127,127) 前後。市松が灰色になる
fn shrink_halves_a_4x4_checkerboard_into_2x2()
fn shrink_copies_when_the_target_is_not_smaller()
fn shrink_rejects_a_zero_sized_target()
fn shrink_rejects_a_pixel_buffer_whose_length_does_not_match()
```

**3. `rgb::encode_image`**

```rust
fn encode_image_writes_cup_then_a_single_apc_for_a_small_image()
    // 2x2 → "\x1b[3;5H" + "\x1b_Ga=T,f=24,s=2,v=2,C=1,q=2,c=..,r=..,m=0;<b64>\x1b\\"
fn encode_image_always_carries_q2()
    // q=2 が無いと端末の応答が stdin に入る (既存設計 §1-2)
fn encode_image_splits_the_payload_into_4096_byte_chunks()
    // 先頭 m=1 / 中間 m=1 / 最後 m=0、制御キーは先頭チャンクだけ
fn encode_image_round_trips_through_the_existing_parser()
    // 出力を kitty::ApcParser + FrameAssembler へ食わせると
    // VideoFrame { width_px, height_px } が元の寸法と一致する。
    // 既存の検証済みパーサをそのまま答え合わせに使う
```

**4. `grid::layout`**

```rust
fn layout_of_an_80x24_terminal_is_4_columns_by_2_rows()
    // inner 78x16 / セル 8x16px → columns=4, cell 19 桁, image_rows=5, rows=2, image_px=(144,80)
fn layout_cells_never_overlap_and_stay_inside_the_inner_area()
fn layout_caps_the_column_count_on_a_wide_terminal()
    // inner 300 桁 → columns=6 (青天井にしない)
fn layout_is_none_when_one_cell_row_does_not_fit()
    // 高さ 4 行 → None → 呼び出し側はリスト表示へ
fn layout_image_rows_follow_the_cell_aspect_ratio()
    // セル 8x16 と 8x8 で image_rows が変わる
fn layout_offset_is_aligned_to_the_column_count()
    // scroll=5, columns=4 → offset=4
```

**5. `grid::move_selection` / `ensure_visible` / `truncate`**

```rust
fn right_and_left_move_by_one_and_stop_at_the_ends()
fn down_and_up_move_by_a_full_row()
fn down_from_a_partial_last_row_lands_on_the_last_item()
fn move_selection_is_noop_without_results()
fn scroll_follows_the_selection_out_of_the_bottom_row()
fn scroll_returns_to_zero_when_the_selection_goes_back_up()
fn scroll_stays_at_zero_when_everything_fits()
fn truncate_counts_display_width_not_chars()
    // "ラーメン" を幅 5 で切ると 2 文字 + "…"
fn truncate_never_splits_a_wide_char_in_half()
```

**6. `thumbs` の純関数**

```rust
fn url_for_builds_the_mqdefault_url()
    // "abc123" → "https://i.ytimg.com/vi/abc123/mqdefault.jpg"
fn safe_id_rejects_path_separators_and_dots()
    // "../etc/passwd" → None, "ab/cd" → None, "a-b_C9" → Some
fn cache_dir_prefers_xdg_cache_home_over_home()
fn cache_dir_is_none_without_either_variable()
fn prune_targets_keeps_the_newest_files()
fn prune_targets_returns_nothing_when_under_the_limit()
```

**7. `jpeg::decode`(固定バイト列)**

```rust
fn decode_returns_rgb_pixels_for_a_known_jpeg()
    // include_bytes!("testdata/tiny2x2.jpg") → 2x2、pixels.len() == 12
fn decode_expands_grayscale_to_rgb()
fn decode_rejects_empty_input()
fn decode_rejects_truncated_input_without_panicking()
    // 実測: zune-jpeg は先頭 20 バイトだけ・空入力とも Err を返す
fn decode_rejects_an_image_larger_than_the_limit()
    // ヘッダの寸法だけで弾く。巨大画像でメモリを取らない
```

**8. `fetch::curl_args`**

```rust
fn curl_args_pair_each_output_path_with_its_url()
fn curl_args_include_fail_silent_and_timeout()
fn curl_args_enable_parallel_transfers()
fn curl_args_of_a_single_item_have_the_same_shape()
fn curl_args_are_empty_for_an_empty_list()   // 空で起動しない
```

**9. `thumbs::Thumbs`**

```rust
fn reset_drops_entries_that_left_the_result_set()
fn wanted_skips_ids_that_are_already_ready_at_the_same_size()
fn wanted_includes_everything_again_when_the_target_size_changes()
fn wanted_skips_ids_that_already_failed()      // 失敗を繰り返し取りに行かない
fn apply_marks_dirty_only_when_something_changed()
fn take_dirty_clears_the_flag()
```

**10. `category::Tabs`**

```rust
fn tabs_always_start_with_the_all_tab()
fn next_and_prev_wrap_around()
fn target_of_a_category_tab_is_a_keyword_search()
    // 「音楽」タブ → Target::Search("音楽")
fn target_of_the_all_tab_uses_the_query_box()
fn target_of_the_all_tab_is_none_when_the_box_is_blank()
fn a_tab_whose_query_is_a_feed_keyword_becomes_a_feed_target()
    // query = ":ytrec" → Target::Feed(Recommended)。既存 Target::for_query を通すだけ
fn switching_tabs_keeps_each_tabs_results_and_selection()
fn switching_back_does_not_mark_the_tab_as_unloaded()
```

**11. `ui::search_areas`**

```rust
fn search_areas_do_not_overlap_and_cover_the_screen()
fn tab_row_sits_between_the_input_box_and_the_results()
fn results_area_shrinks_by_one_row_compared_to_the_current_layout()
fn search_areas_survive_a_terminal_too_short_for_every_row()
```

**12. `input`(偽 `YtDlp` を使う)**

```rust
fn tab_switches_the_category_in_the_input_mode()
fn tab_switches_the_category_in_the_results_mode()
fn tab_starts_a_search_only_for_a_tab_that_has_not_loaded_yet()
fn enter_in_the_input_mode_returns_to_the_all_tab()
fn typing_still_appends_to_the_query_while_a_category_tab_is_selected()
fn left_and_right_move_inside_the_grid_in_the_results_mode()
fn r_reloads_the_current_tab()
fn tab_is_ignored_while_playing()
```

**13. `main::present_thumbs`**

```rust
fn present_thumbs_writes_nothing_when_not_dirty()
fn present_thumbs_writes_clear_then_one_apc_per_ready_image()
fn present_thumbs_skips_cells_without_an_image()
fn present_thumbs_writes_nothing_while_playing()
fn present_thumbs_clears_the_dirty_flag_after_writing()
fn present_video_runs_before_present_thumbs()
    // 再生終了直後の owe_clear が、貼ったばかりのサムネイルを消さない順序であること
```

### 6-2. 偽 runner を使う結合テスト

`search::YtDlp` の偽物と同じ形で `fetch::Fetcher` の偽物を作り、外部プロセスへ行かせない。

```rust
fn a_finished_search_starts_a_thumbnail_fetch_for_its_ids()
fn thumbs_ready_from_a_superseded_nonce_is_discarded()
fn a_failed_curl_leaves_the_cells_empty_and_does_not_retry()
fn a_missing_curl_binary_disables_thumbnails_with_a_notice()
fn cached_ids_are_not_passed_to_curl()
fn a_resize_re_decodes_from_the_cache_without_calling_curl()
```

### 6-3. テスト用の固定バイト列の作り方

JPEG はプログラムで作れない(エンコーダを持たない)ので、小さいファイルをリポジトリに置いて `include_bytes!` する。

| ファイル | 内容 | サイズ |
| --- | --- | --- |
| `src/testdata/tiny2x2.jpg` | 2x2 のカラー JPEG | 822 バイト(実測) |
| `src/testdata/tiny8x4.jpg` | 8x4 のカラー JPEG | 843 バイト(実測) |
| `src/testdata/gray4x4.jpg` | グレースケール(components=1) | 1 KB 前後 |

生成手順(再現できるようにコメントへ書く)。

```sh
sips -s format jpeg -z 2 2 <元画像> --out src/testdata/tiny2x2.jpg
sips -s format jpeg -Z 8    <元画像> --out src/testdata/tiny8x4.jpg
```

期待値(幅・高さ・先頭画素)はテストの中に直書きする。`zune-jpeg` のバージョンを上げたときに値がずれたら、それは検知したい変化なのでテストが落ちてよい。

RGB 側のテストは全て手書きの `Vec<u8>` で足りる(2x2 の市松など)。

### 6-4. 手動確認(実機 iTerm2)

| 項目 | 見るもの |
| --- | --- |
| 4 枚同時表示 | §6-0 のスパイク |
| 検索 → 格子 | 8 枚が正しい位置に並ぶ。タイトルと画像の対応がずれていない |
| 選択移動 | 画像がちらつかない(貼り直しが起きていない) |
| スクロール | 前のページの画像が残らない |
| タブ切替 | 画像が入れ替わる。戻ると即座に出る(再取得しない) |
| 端末リサイズ | 画像の大きさが変わって貼り直される。ステータス行・ヘルプ行に被らない |
| 再生 → Esc | 再生開始で画像が消え、戻ると貼り直される |
| 終了 | シェルに戻った後に画像が残らない |
| 機内モード | 枠だけ出て操作は通る。エラーで画面が埋まらない |
| `layout = "list"` | 従来表示に戻る。画像は1枚も出ない |

## 7. 実装順序

外部に触らない部分から積み上げ、途中でも動く状態を保つ。

| # | 内容 | 依存 | 終わった時点で見えるもの |
| --- | --- | --- | --- |
| 0 | §6-0 のスパイク | - | 複数枚同時表示の可否。ここで設計の分岐が決まる |
| 1 | `Cargo.toml` に `zune-jpeg` `zune-core` | 0 | - |
| 2 | `rgb.rs`(base64 / fit_box / shrink) | 1 | 純関数のテストが緑 |
| 3 | `rgb::encode_image` + 既存パーサでのラウンドトリップ | 2 | APC を正しく組めることが既存コードで裏取りできる |
| 4 | `jpeg.rs` + テスト用 JPEG の配置 | 1 | 固定バイト列のデコードが緑 |
| 5 | `grid.rs`(割り付け / 選択 / スクロール / 切り詰め) | - | 数値がテストで固定される |
| 6 | `ui.rs` の格子描画(画像なし、枠とタイトルだけ) | 5 | **画面が格子になる**。ここまでで機能1の半分が実用になる |
| 7 | `input.rs` の `←` `→` 対応 | 5,6 | 格子内を移動できる |
| 8 | `thumbs.rs`(URL / パス / 状態表)+ `fetch.rs` | 2,4 | 偽 runner でのテストが緑 |
| 9 | `actions::start_thumbnails` + `AppEvent::ThumbsReady` | 8 | 取得が動く(まだ画面には出ない) |
| 10 | `main::present_thumbs` + dirty の配線 | 3,9 | **画像が出る**。機能1完成 |
| 11 | リサイズ・再生復帰の貼り直し | 10 | 崩れが無くなる |
| 12 | `category.rs` + タブ行の描画 | 6 | タブが見える |
| 13 | `input.rs` の `Tab` / `BackTab` / `r` + タブごとの保持 | 12 | 機能2完成 |
| 14 | `settings.rs`(`[search]` `[thumbnails]` `[[categories]]`) | 10,13 | 設定で切り替えられる |
| 15 | ディスクキャッシュの間引き | 8 | 放置しても増え続けない |
| 16 | §6-4 の手動確認 | 全部 | - |

6 と 12 の時点でそれぞれ人に見せられる状態になるので、そこで一度止めて方向を確認できる。

## 8. 実装前に確認したいこと

1. **§6-0 のスパイクの結果**。4 枚同時が出ないなら設計を変える。実装着手前に見たい。
2. **追加クレート 2 つ(`zune-jpeg` / `zune-core`)を入れてよいか**。入れない場合は mpv を 1 枚ごとに起動する案(§1-4)へ倒す。その場合はテストが偽サブプロセス越しになり、デコードの単体テストは書けなくなる。
3. **既存テスト `ui::tests::input_help_mentions_feed_keywords` を書き換えてよいか**(§2-12)。現行のヘルプは 83 桁で 80 桁端末では末尾が切れている。
4. **カテゴリ 6 個の内容**。「音楽 / ゲーム / ニュース / アニメ / スポーツ」で進めてよいか。
5. **80x24 で 1 画面 8 件・タイトル 18 桁**という粒度でよいか。タイトルを 2 行にすると 1 画面 4 件になる。
6. **`[search] layout = "list"` という逃げ道を用意すること自体**の是非。不要なら格子だけにして分岐を減らせる。

## 9. 会話に出ていないが追加検討してほしい要素

ここは今回の依頼に含まれていない。採否を分けて判断してほしい。

| # | 内容 | 効果 | 費用 |
| --- | --- | --- | --- |
| 1 | ログイン連動タブ(おすすめ / 履歴 / 登録チャンネル / 後で見る)を、cookie 設定がある時だけタブに出す | `:ytrec` 等をキーボードで打たずに済む。`Target::for_query` がそのまま使えるので実装は小さい | タブが増えて幅を食う。未ログインだと毎回エラーになるので出し分けが要る |
| 2 | 画像 ID(`i=`)を使い、1 度送った画像を配置し直すだけにする | 貼り直しの転送量が 370 KB → 数百バイトになる | iTerm2 の対応が未確認。§6-0 の結果次第 |
| 3 | 1 枚ずつ届き次第表示する(curl を URL ごとに起動) | 体感が少し早くなる | 実測 10 枚 0.14 秒なので効果は小さい。プロセスが 10 個に増える |
| 4 | 画像矩形が 320px を超える端末で `hq720.jpg` を使う | 大きい端末で精細になる | 1 枚 107 KB。10 枚で 1 MB。404 のときの退避が要る |
| 5 | マウスでセルをクリックして選択・ダブルクリックで再生 | 操作が楽。`grid::cell_at(column,row)` は純関数で書ける | マウス経路が Results にも増える |
| 6 | 数字キー 1-9 でタブを直接選ぶ(Results のみ) | 速い | Input では数字が文字入力なので、モードで意味が変わる |
| 7 | カテゴリタブで検索ボックスの語を絞り込みに使う(「ニュース」タブ + 「猫」→ `ニュース 猫`) | 組み合わせられる | ボックスとタブの関係が 1 対 1 でなくなる |
| 8 | `[search] limit` を 20 前後へ上げる | 1 画面 8 件なので 10 件だとスクロールが 1 回で終わる | yt-dlp の所要時間が伸びる |
| 9 | `fetch::Fetcher` と `search::YtDlp` を共通の `Runner` trait にまとめる | 同じ形の trait が 2 つ並ぶのを防ぐ | search.rs とそのテストを触る |
| 10 | サムネイル取得中に選択中のセルを優先して取る | 見たいものから出る | 並べ替えの分だけ複雑になる |
