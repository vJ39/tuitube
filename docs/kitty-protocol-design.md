mpv の `--vo=kitty` 出力(Kitty graphics protocol の APC `G` コマンド)を ratatui の TUI 内に埋め込み描画するための設計。現行の `--vo=tct` + vt100 方式(セル解像度の疑似画像)を置き換える。実装は TDD(Red→Green)で進める前提で、外部プロセスを伴わない純粋関数の単位を先に切り出す。

対象バージョン: mpv 0.41.0 / ratatui 0.29 / crossterm 0.28 / 実機端末 iTerm2 3.6.11(APC `G` の表示は実機確認済み)。

## 1. 実装が依存する事実

設計の根拠になる挙動を先に固定する。出典は mpv v0.41.0 のソース(`video/out/vo_kitty.c`、`osdep/terminal.h`、`osdep/terminal-unix.c`。https://raw.githubusercontent.com/mpv-player/mpv/v0.41.0/video/out/vo_kitty.c )、Kitty graphics protocol 仕様( https://sw.kovidgoyal.net/kitty/graphics-protocol/ )、ratatui 0.29 のソース。

### 1-1. mpv `--vo=kitty` の挙動

| 項目 | 事実 | 設計への影響 |
|---|---|---|
| 出力先 | 画像・制御列とも `write(STDOUT_FILENO)` / `printf` で **stdout** に書く。`/dev/tty` は `--no-terminal` では開かれない | stdout をパイプにすれば全バイトを tuitube が捕まえられる。実端末への漏れは無い |
| `--vo-kitty-width/height` | **ピクセル**。mpv はこの矩形に映像をアスペクト維持で内接させ、内接矩形の寸法を APC の `s=`/`v=` に載せる。既定 0 = 端末から取得、失敗時 320×240 | ブリーフィングの「セル幅・高さ」は誤り。セル数から自分でピクセルを計算して渡す(§2-5) |
| `--vo-kitty-cols/rows` | セル数。`left/top` 自動計算(`rows * dst.y0 / dheight`)にのみ使う。既定 0 = 端末から取得、失敗時 80×25 | `left/top` を固定すれば実質未使用。整合のため映像領域のセル数を渡す |
| `--vo-kitty-left/top` | **1 始まり**のセル座標。0(既定)= 自動。自動値は 0 始まりのオフセットをそのまま 1 始まりの HVP に載せるため 1 行ズレる | 1 に固定し、mpv の座標は使わない。位置決めは tuitube が行う |
| 端末サイズ取得 | `ioctl(tty_in, TIOCGWINSZ)`。`--no-terminal` では `tty_in = -1` で失敗し既定値に落ちる(ユーザー観測の `s=320,v=180` はこれ) | cols/rows/width/height の 4 つを **必ず明示**する |
| 起動時(preinit) | `ESC[?25l`(カーソル非表示) `ESC[?1003h`(マウス追跡) [+ `ESC[?1049h`(alt-screen=yes 時)] | 全部捨てる |
| 構成時(reconfig) | `ESC_Ga=d;ESC\`(全 placement 削除) [+ `ESC[2J`(config-clear=yes 時)]。再生開始・リサイズ・panscan 変更で走る | `a=d` は「消去要求」として解釈し tuitube が正規化して出す。`2J` は捨てる |
| 毎フレーム(flip_page) | `ESC[<top>;<left>f`(HVP) → `ESC_Ga=T,f=24,s=<W>,v=<H>,C=1,q=2,m=1;<base64 ≤4096>ESC\` → `ESC_Gm=1;<4096>ESC\` … → `ESC_Gm=0;<残り>ESC\`。画像 ID(`i=`)・表示セル数(`c=`/`r=`)は付かない | フレーム境界 = `m=0` チャンクの ST。synchronized output(`?2026`)は kitty VO には無い |
| 終了時(uninit) | `ESC_Ga=d;`(**ST 無し**、mpv のバグ) → `ESC[?25h` `ESC[?1003l` → `ESC[?1049l`(alt-screen=yes) または `ESC[<cols>;0f`(no) | APC は ST だけでなく素の ESC でも打ち切れるパーサにする。打ち切った後続 CSI を巻き込まない |
| 小さすぎる画像 | base64 が 4096 バイト以下(生 3072 バイト以下)のとき `m=0` チャンクが出ない(mpv のバグ) | 実用寸法では起きない。「次の `a=T` が来たら未完成フレームを捨てる」規則で自然に回復する |
| 寸法の動的変更 | `set_property vo-kitty-*` だけでは反映されない(tct と同じ。opts は VO 生成時に読まれる)。`vid no` → `vid auto` で VO が作り直され新しい値で描き始める | tct 方式の `resize_video` と同じ手順を踏む |
| SIGWINCH | mpv は制御端末を共有するので端末リサイズで reconfig が走る(`a=d` + 同寸法で再描画) | 無害。寸法変更自体は IPC 経由で別途行う |
| 同期・ID 無し | 毎フレーム新しい画像+placement を同じ位置に重ね描き。古い画像は端末側の容量制限で破棄される | 毎フレーム削除はしない(ちらつく)。ユーザー実機検証はこの挙動で成立している |

### 1-2. Kitty graphics protocol

| 項目 | 事実 | 設計への影響 |
|---|---|---|
| `C=1` | 画像配置後にカーソルを動かさない | tuitube が打った CUP の位置にカーソルが留まる |
| `q=2` | 端末からの応答(OK/エラー)を出さない | 応答は **tuitube の stdin** に届いてしまうため、`q=2` は絶対に外さない・書き換えない |
| チャンク転送 | 先頭チャンクに全制御キー、2 個目以降は `m`(と `q`)のみ。4096 バイト以下。1 画像のチャンクを送り切るまで他の graphics コマンドを挟まない | フレーム単位でまとめて書く |
| `a=d`(キー無し) | 画面上の全 placement を削除。画像データは残る(`d=A` で完全削除) | 消去には `ESC_Ga=d;ESC\` を使う |
| 文字消去との関係 | `ESC[2J` と端末リセットは画像も消す。それ以外の文字消去(EL/ECH/文字上書き)は画像に影響しない | ratatui が下のセルを描き直しても画像は残る。消したいときは必ず `a=d` を出す |
| alt screen | `?1049h/l` の切替時、alt 側の画像は消える | `ratatui::restore()` の `?1049l` で残骸は消える。念のため停止時にも `a=d` を出す |
| z 順 | 既定 `z=0` は文字より上に描かれる | 映像領域の下に空白セルがあっても隠れない |

### 1-3. ratatui 0.29 / crossterm 0.28

| 項目 | 事実 | 設計への影響 |
|---|---|---|
| `Terminal::draw()` | `autoresize`(サイズ変化時 `ESC[2J` + 全再描画)→ 差分セルだけ書く → カーソル非表示 `ESC[?25l` → flush | 差分の先頭は必ず `MoveTo`、SGR は Reset から始まり Reset で終わる。draw の**後**に他の書き込みを挟んでも次の draw は壊れない |
| 書き込み単位 | `CrosstermBackend<Stdout>` は `std::io::Stdout`(LineWriter、1KB 程度で分割 write) | draw の**途中**に別スレッドが書くとカーソル位置・SGR の前提が崩れる。書き手は 1 本に直列化する |
| 未描画領域 | `swap_buffers` が裏バッファを reset するため、描かなかった領域は空白セルになる | 映像領域には何も描かなければ空白セル(前画面の文字は差分で消える) |
| 外部書き込み口 | `Terminal::backend_mut()` は安定 API で、`CrosstermBackend<W: Write>` は `Write` を実装 | tuitube はこれ経由で stdout に書く(グローバル `stdout()` を別途握らない) |
| `CompletedFrame.area` | draw が実際に使った画面サイズ | present 時の映像領域はこれから `ui::video_area()` で求める(リサイズ直後も ratatui と一致) |
| `terminal::window_size()` | TIOCGWINSZ の `ws_xpixel/ws_ypixel` を返す(端末が報告しない場合 0) | セルのピクセル寸法の取得元。iTerm2 が非 0 を返すかは実機確認項目(§4-5) |

## 2. 設計判断

### 2-1. ストリーム処理方式

| 案 | 内容 | 利点 | 欠点 | 判定 |
|---|---|---|---|---|
| (a) 分離して素通し | APC `G` は生のまま実端末へ、CSI 等は捨てる | 実装が薄い | mpv の HVP は画面原点基準かつ 1 行ズレる(§1-1)ので座標を捨てると位置が決まらず、使うと壊れる。ST 欠落 `a=d` を素通しすると後続の解釈が端末実装依存 | 不採用 |
| (b) mpv の座標を信頼 | `left/top` を固定し mpv の HVP をそのまま使う | 座標書き換え不要 | 固定できるのは画面原点基準の値だけで、ratatui の映像領域 (x,y) への平行移動は結局 tuitube が行う。信頼しても得るものがない | 不採用 |
| (c) 意味解釈して再送(採用) | APC `G` を「フレーム先頭 / 継続チャンク / 全削除」の 3 種として解釈し、APC 以外は全て捨てる。位置決め(CUP)は tuitube が打ち、画像チャンクのバイト列は無変換で流す | mpv の座標・画面制御に一切依存しない。base64 のデコード不要。ST 欠落・分断 read・ゴミ列を一箇所で正規化できる | パーサと組み立て器が要る(ただし純粋関数でテストしやすい) | **採用** |

mpv 側は `--vo-kitty-left=1 --vo-kitty-top=1` に固定し、出てくる HVP は常に同じ値(=読み捨て)。

### 2-2. stdout への書き込み順序

| 案 | 内容 | 割り込み耐性 | 入力遅延 | 複雑さ | 判定 |
|---|---|---|---|---|---|
| A. メインループ単一書き手(採用) | 読み取りタスクは解釈して「最新フレーム」をスロットに置き `AppEvent::VideoFrame` で起こす。メインループは `terminal.draw()` 完了直後に `present()` でスロットの内容を書く | 書き手が 1 スレッドなので構造的に割り込みが起きない | フレーム書き込み中はループが止まる(ピクセル上限 §2-5 で抑える) | 低。既存の `VideoFrame` イベント流と同型 | **採用** |
| B. 読み取り側が書く + ゲート Mutex | 読み取りタスクがフレーム完成時に Mutex を取り stdout に書く。メインループは `draw()` を同じ Mutex で囲む | Mutex の取り忘れが即座に画面崩れになる | メインループはフレーム書き込みで止まらない | 中。std スレッド化と Mutex 規律が要る | 実測で入力遅延が問題になったら移行 |

A のフレーム間引き: ループ 1 周につき最新 1 フレームだけ書く。読み取りタスクはパイプを常に空にするので mpv は書き込みで詰まらず、mpv 自身の A/V 同期(音声基準)は保たれる。
B への移行コストを下げるため、`present()` は `&mut dyn Write` を受ける自由関数にしておく。

### 2-3. 画像 placement の削除

| 契機 | 出す側 | 書くもの | 理由 |
|---|---|---|---|
| 再生開始 | mpv(reconfig)→ tuitube が正規化 | `ESC_Ga=d;ESC\` | 前回の残骸を掃除 |
| 毎フレーム | 出さない | — | 同じ位置に重ね描き。削除→描画はちらつく |
| 一時停止 / バッファリング | 出さない | — | 最後のフレームが残るのが正しい見え方 |
| 端末リサイズ | ratatui(`2J`)+ tuitube(`apply_resize`) | `a=d` を保留にして次の present で書き、続けて mpv を作り直す | `2J` で画像も消えるのが仕様だが、端末実装差を吸収する保険として明示する。古い寸法のフレームは placement 判定(§3-3)で弾く |
| 再生停止(Esc / q) | mpv(uninit、ST 無し)→ 正規化 + tuitube(`end_playback`) | `a=d` | SIGKILL 経路では mpv が出せないので tuitube 側でも必ず出す |
| mpv 異常終了 | tuitube(`end_playback`) | `a=d` | 同上 |
| tuitube 終了 | tuitube(`stop_playback`)→ `ratatui::restore()`(`?1049l`) | `a=d` → alt screen 離脱 | alt 側の画像は仕様上消えるが、明示も入れる |

### 2-4. tct 方式の扱い

**置き換える**(共存させない)。`src/video.rs` の `VideoScreen` / `TctRewriter` / `render_screen` とそのテスト、`Cargo.toml` の `vt100` 依存、`mpv.rs` の `vo-tct-*` プロパティ名を削除する。

失うもの: Kitty graphics protocol 非対応端末(Terminal.app、多くの Linux 端末)での映像表示。tct 実装はコミット `0877d16` に残るので必要になれば復元できる。新設計の `VideoSink::feed` / `present` は VO 方式に依存しない I/F なので、後から tct パーサを差し込む形での共存も可能。

### 2-5. セル/ピクセル幾何とスループット

mpv に渡すピクセル窓 = 映像領域のセル数 × セル 1 個のピクセル寸法。セル寸法は `crossterm::terminal::window_size()` から `width / columns`, `height / rows` で求める。0 が返る端末では控えめな既定値(8×16 px)に落とす。既定値が実際より小さければ画像が領域より小さく中央寄せされるだけで済み、大きければ領域を突き抜けてステータス行に被る。既定値は小さめに置く。

ピクセル数には上限 `MAX_FRAME_PIXELS` を設け、超える場合はアスペクトを保って縮小する(v1 では画像が領域より小さくなり中央寄せ。端末側で拡大させる `c=`/`r=` の追記は Phase 2、§7)。上限が必要なのはパイプと端末の処理量が画像面積に比例するため:

| ピクセル窓 | RGB 生データ / フレーム | base64 / フレーム | 30fps の転送量 |
|---|---|---|---|
| 320×180(ユーザー実測) | 173 KB | 230 KB | 6.9 MB/s |
| 640×360 | 691 KB | 922 KB | 27.6 MB/s |
| 1080×600(120×30 セル × 9×20 px) | 1.94 MB | 2.59 MB | 77.8 MB/s |
| 1920×1080 | 6.22 MB | 8.29 MB | 249 MB/s |

初期値は `MAX_FRAME_PIXELS = 640 * 360`(230,400)とし、実機で入力遅延・フレーム落ち・CPU を見て調整する。追加の抑制手段(必要になったときの候補、v1 では使わない): `--vo-kitty-use-shm=yes`(共有メモリ転送、端末側 `t=s` 対応が必要)、`--vf=fps=30`(高フレームレート動画の間引き)、`--profile=sw-fast`(man page 推奨のスケーラ軽量化)。

## 3. アーキテクチャ

### 3-1. データフロー

```
mpv(--vo=kitty, stdout=pipe)
  │ bytes(制御列 + APC G チャンク)
  ▼
spawn_video_reader (tokio task)            ── mpv.rs
  │ VideoSink::feed(&[u8])
  ▼
ApcParser ──GraphicsCommand──▶ FrameAssembler ──FrameEvent──▶ Slot{clear, frame}   ── kitty.rs / video.rs
  │ (APC 以外は捨てる)          (m=0 で 1 フレーム / a=d で Clear)      │ 最新 1 件
  │                                                                    │ request_redraw() → AppEvent::VideoFrame
  ▼                                                                    ▼
main loop: terminal.draw(ui) ──▶ CompletedFrame.area ──▶ present(sink.take(), video_area, cell, backend_mut())
                                                          │  a=d(必要時) + CUP(row;col) + フレームの APC バイト列
                                                          ▼
                                                        stdout(実端末)
```

読み取りタスク → メインループの受け渡しは「最新 1 件のスロット + 再描画要求の畳み込み」で、現行の `VideoScreen` と同じ形。

### 3-2. モジュール構成

| ファイル | 区分 | 内容 |
|---|---|---|
| `src/kitty.rs` | 新規 | Kitty graphics protocol の**純粋な**解釈。`ApcParser`(バイト列 → `GraphicsCommand`)、`FrameAssembler`(`GraphicsCommand` → `FrameEvent`)。ratatui・tokio・mpv に依存しない |
| `src/video.rs` | 書き換え | `CellSize` / `Geometry`(mpv へ渡す寸法)、`placement()`(画像を領域内に中央配置)、`encode()`(書き込むバイト列の生成)、`VideoSink`(読み取りタスクとメインループの共有スロット) |
| `src/mpv.rs` | 変更 | `launch()` の引数生成を `Geometry::mpv_args()` に置き換え。`resize_video()` を `vo-kitty-{cols,rows,width,height}` の `set_property` ×4 + `vid no/auto` に変更。`spawn_video_reader` は `VideoSink` に feed(構造は現状維持) |
| `src/ui.rs` | 変更 | `video_area()` は現状維持。`draw_playing()` は映像領域に何も描かない(空白セルにする) |
| `src/main.rs` | 変更 | `video_screen()` → `video_geometry()`(`window_size()` を使う)。ループで `terminal.draw()` の直後に `present_video()`。`apply_resize` で `Geometry` 再計算 → `VideoSink::resize()`(Clear 保留)→ mpv へ寸法送信。`end_playback` で Clear を保留(`Session.owe_clear`) |
| `src/app.rs` | 変更 | `video: Option<VideoScreen>` → `Option<VideoSink>`。`AppEvent::VideoFrame` は現状維持 |
| `Cargo.toml` | 変更 | `vt100` を削除。新規依存は不要(base64 はデコードしないので不要) |

### 3-3. 型と関数(TDD の足場)

コードは書かない前提だが、テストを先に書くために境界のシグネチャだけ固定する。

`src/kitty.rs`

```rust
/// 完結した APC G コマンド 1 個。
pub struct GraphicsCommand {
    /// 制御部 "a=T,f=24,s=320,..." を (キー, 値) に分解したもの。順序保持。
    pub keys: Vec<(char, String)>,
    /// ESC _ G から ESC \ までの完全なバイト列。ST 欠落は付け直し済み。
    pub raw: Vec<u8>,
}
impl GraphicsCommand {
    pub fn get(&self, key: char) -> Option<&str>;
}

/// 分断された read を跨いで状態を持つストリームパーサ。APC G 以外は全て捨てる。
#[derive(Default)]
pub struct ApcParser { /* state, buf */ }
impl ApcParser {
    pub fn feed(&mut self, input: &[u8], out: &mut Vec<GraphicsCommand>);
}

pub struct VideoFrame {
    pub width_px: u32,
    pub height_px: u32,
    /// 先頭チャンクから m=0 チャンクまでの raw を連結したもの。
    pub bytes: Vec<u8>,
}
pub enum FrameEvent {
    Frame(VideoFrame),
    /// a=d を見た。未完成フレームは捨てる。
    Clear,
}
#[derive(Default)]
pub struct FrameAssembler { /* open frame */ }
impl FrameAssembler {
    pub fn push(&mut self, cmd: GraphicsCommand) -> Option<FrameEvent>;
}
```

`src/video.rs`

```rust
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CellSize { pub width_px: u16, pub height_px: u16 }

/// window_size() の値からセル寸法を求める。ピクセルが 0 なら None。
pub fn cell_size(columns: u16, rows: u16, width_px: u16, height_px: u16) -> Option<CellSize>;
pub const FALLBACK_CELL: CellSize = CellSize { width_px: 8, height_px: 16 };
pub const MAX_FRAME_PIXELS: u32 = 640 * 360;

/// mpv に渡す寸法。area は画面座標のセル矩形(ui::video_area の戻り)。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Geometry { pub area: Rect, pub cell: CellSize, pub frame_px: (u32, u32) }
impl Geometry {
    pub fn new(area: Rect, cell: CellSize, max_pixels: u32) -> Self;
    /// --vo-kitty-cols/rows/width/height/left/top/alt-screen/config-clear
    pub fn mpv_args(&self) -> Vec<String>;
}

/// 1 始まり・画面絶対座標の CUP 位置。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Placement { pub row: u16, pub col: u16 }
/// 画像が占めるセル数 = ceil(px / cell_px)。領域に収まらなければ None(描かない)。
pub fn placement(area: Rect, cell: CellSize, image_px: (u32, u32)) -> Option<Placement>;

pub struct Pending { pub clear: bool, pub frame: Option<VideoFrame> }
/// clear なら ESC_Ga=d;ESC\ を、frame があり placement が取れれば CUP + frame.bytes を out に積む。
/// 戻り値はフレームを書いたか(収まらず捨てた場合 false)。
pub fn encode(pending: &Pending, area: Rect, cell: CellSize, out: &mut Vec<u8>) -> bool;
pub fn encode_clear(out: &mut Vec<u8>);

#[derive(Clone)]
pub struct VideoSink { /* Arc<Mutex<Inner>>, Arc<AtomicBool> */ }
impl VideoSink {
    pub fn new(geometry: Geometry) -> Self;
    pub fn geometry(&self) -> Geometry;
    /// 表示すべき更新(Frame または Clear)が新たに生じたら true。
    pub fn feed(&self, bytes: &[u8]) -> bool;
    /// 保留中の更新を取り出す。取り出すと空になる。frame は最新 1 件、clear は取り出すまで保持。
    pub fn take(&self) -> Option<Pending>;
    /// 寸法変更。保留中フレームを捨て、clear を保留にする。
    pub fn resize(&self, geometry: Geometry);
    pub fn request_redraw(&self) -> bool;
    pub fn clear_redraw(&self);
}
```

`Pending` の規則: `frame` は後着優先で上書き、`clear` は一度立ったら `take()` されるまで下ろさない(位置・寸法が変わった古い画像を消し残さないため)。`encode` は `clear` → `frame` の順に書く。

### 3-4. mpv 起動引数

```
mpv --input-ipc-server=<sock> --log-file=<log> --no-terminal
    --vo=kitty
    --vo-kitty-cols=<area.width>  --vo-kitty-rows=<area.height>
    --vo-kitty-width=<frame_px.0> --vo-kitty-height=<frame_px.1>
    --vo-kitty-left=1 --vo-kitty-top=1
    --vo-kitty-alt-screen=no --vo-kitty-config-clear=no
    <url>
```

| オプション | 値 | 理由 |
|---|---|---|
| `--vo-kitty-width/height` | `Geometry::frame_px`(セル数 × セル px、上限で縮小) | mpv はこの窓に内接するよう映像を縮尺し、内接矩形を `s/v` で報告する。パイプ出力では端末から取れないため必須 |
| `--vo-kitty-cols/rows` | 映像領域のセル数 | `left/top` 固定時は実質未使用。整合のため渡す |
| `--vo-kitty-left/top` | 1 | mpv の位置決めを無効化する。HVP は常に `ESC[1;1f` になり読み捨てる |
| `--vo-kitty-alt-screen=no` | — | `?1049h/l` はパイプ内で捨てられるので正しさには無関係だが、不要な列を減らす |
| `--vo-kitty-config-clear=no` | — | `2J` を出させない(同上) |
| `--vo-kitty-use-shm` / `--vo-kitty-auto-multiplexer-passthrough` | 既定(no) | shm は端末対応が要る。multiplexer passthrough は APC を DCS で包むためパーサ非対応(§6) |

再生中の寸法変更(IPC): `set_property vo-kitty-cols` → `rows` → `width` → `height` → `set_property vid no` → `set_property vid auto`。tct 方式の `resize_video()` と同じ骨格で、プロパティ名と本数だけ変わる。

### 3-5. ANSI ストリーム処理規則

| 入力 | 処理 |
|---|---|
| `ESC _ G <制御部> ; <データ> ESC \` | 解釈。`a=T` あり → フレーム先頭。`a` 無しで `m` あり → 継続チャンク。`a=d` → Clear。`raw` は原文のまま保持して再送に使う |
| `ESC _ G ...` が `ESC \` でなく素の `ESC` で打ち切られた | その時点で完結扱いにし `raw` に `ESC \` を付けて正規化。打ち切った `ESC` は次の列の先頭として解釈を続ける(uninit の `a=d` 対策) |
| `ESC _`(G 以外)/ `ESC ]`(OSC)/ `ESC P`(DCS)/ `ESC X` `ESC ^` | ST または BEL まで読み飛ばす |
| `ESC [ ... <終端>`(CSI) | 全て捨てる(`?25l/h`、`?1003h/l`、`?1049h/l`、`2J`、HVP、SGR) |
| その他の文字・改行 | 捨てる |
| 1 個の APC が `MAX_APC_LEN`(8192 バイト)を超えた | 破棄して Ground に戻る。壊れたストリームでメモリを食わないため |

フレーム組み立て(`FrameAssembler`)の規則:

| 事象 | 動き |
|---|---|
| `a=T` | 未完成フレームがあれば捨てて新規開始。`s/v` を記録。`m=0` なら即完成 |
| `m=1` 継続 | 開いているフレームに追記。開いていなければ無視 |
| `m=0` 継続 | 追記して完成 → `FrameEvent::Frame` |
| `a=d` | 未完成フレームを捨てて `FrameEvent::Clear` |
| `s/v` が無い・数値でない `a=T` | 無視(フレームを開かない) |

### 3-6. ratatui との統合手順

メインループ 1 周:

1. `let completed = terminal.draw(|f| ui::draw(f, &app))?;` — 再生中は映像領域に何も描かない。ステータス行・ヘルプ行は従来通り。
2. `let area = ui::video_area(completed.area);` — ratatui が今描いた画面サイズから映像領域を求める(リサイズ直後も一致する)。
3. `present_video(&mut session, &app, area, terminal.backend_mut())` — `Session.owe_clear` または `sink.take()` の内容を `encode()` で 1 つの `Vec<u8>` にまとめ、`write_all` → `flush`。書くものが無ければ何もしない。

この順序が安全な理由: ratatui の draw は書き込みの先頭で必ず `MoveTo` し、SGR を Reset で閉じてから flush する(§1-3)。その後に tuitube が CUP と APC を書いてもカーソル位置・属性の前提は次の draw で崩れない。逆に draw の途中に挟むと崩れるので、書き手はメインループだけにする(§2-2)。

描画タイミング: 読み取りタスクが `feed()` で true を得たら `request_redraw()`(畳み込み)→ `AppEvent::VideoFrame`。メインループはイベント受信で `clear_redraw()` し、次の周で draw → present。1 周につき最新 1 フレーム。

### 3-7. ライフサイクル別の動き

| 場面 | 動き |
|---|---|
| 再生開始 | `window_size()` → `cell_size()`(None なら `FALLBACK_CELL`)→ `Geometry::new(video_area(terminal.size()), cell, MAX_FRAME_PIXELS)` → `VideoSink::new` → `MpvController::launch(url, nonce, tx, sink)`。mpv の reconfig `a=d` が Clear として届き、最初の present で残骸を掃除 |
| フレーム到着 | 上記 §3-6 |
| 一時停止 / シーク / 音量 | IPC のみ。映像は最後のフレームが残る。ステータス行の更新は映像に影響しない(文字上書きは画像を消さない) |
| 端末リサイズ | crossterm Resize → 既存の 200ms デバウンス → `apply_resize`: 新 `Geometry` を計算し、変化があれば `sink.resize(geometry)`(Clear 保留 + 古いフレーム破棄)→ `controller.resize_video(geometry)`。デバウンス待ちの間に届く古い寸法のフレームは `placement()` が None を返せば書かれない |
| 映像なし(音声のみ) | reconfig が走らずフレームも来ない。映像領域は空白のまま、ステータス行は再生状況を出す。任意: 一定時間フレームが無ければステータス行に「映像なし」を添える |
| 再生停止・mpv 終了・異常終了 | `end_playback` → `stop_playback`(quit / 猶予後 kill)→ `Session.owe_clear = true` → 次の present で `a=d` |
| tuitube 終了 | `stop_playback` → present で `a=d`(可能な範囲で)→ `ratatui::restore()`(alt screen 離脱で残骸も消える) |
| Ctrl-C / panic | ratatui の panic hook が `restore()` を呼び alt screen を離脱 → 画像は仕様上消える。mpv は `kill_on_drop` |
| 別の動画へ切替 | `start_playback` が旧 mpv を止めて nonce を進める。旧読み取りタスクが旧 sink に feed しても `app.video` は新 sink なので present されない |

## 4. テスト戦略

### 4-1. テスト可能単位

| 単位 | 種別 | 外部依存 |
|---|---|---|
| `kitty::ApcParser` / `FrameAssembler` | ユニット(バイト列固定) | なし |
| `video::cell_size` / `Geometry` / `placement` / `encode` | ユニット | なし |
| `video::VideoSink` | ユニット(feed → take) | なし |
| `mpv::resize_video` / `Geometry::mpv_args` | ユニット(文字列比較) | なし |
| `mpv::spawn_video_reader` | 統合(`sh -c printf` で擬似 mpv。既存 `video_reader_asks_for_a_redraw_once_per_frame` と同型) | sh のみ |
| `main.rs` の present 順序・実端末表示 | 手動(§4-5) | iTerm2 |

### 4-2. フィクスチャ(mpv 実測に合わせたバイト列)

```rust
/// alt-screen=no のときの起動列。
const KITTY_PROLOGUE: &[u8] = b"\x1b[?25l\x1b[?1003h";
/// config-clear=no のときの構成列。
const KITTY_RECONFIG: &[u8] = b"\x1b_Ga=d;\x1b\\";
/// 終了列。a=d に ST が無い・最後の HVP に cols(80) が入るのが mpv の実装どおり。
const KITTY_UNINIT: &[u8] = b"\x1b_Ga=d;\x1b[?25h\x1b[?1003l\x1b[80;0f";
/// 1 フレーム: HVP + 先頭チャンク(m=1 固定) + 継続 + 最終(m=0)。data は base64 相当の任意 ASCII。
fn frame(s: u32, v: u32, data: &[u8]) -> Vec<u8>;
```

`frame()` の生成規則は vo_kitty.c の `flip_page` と同じにする: 先頭は `ESC[1;1f` + `ESC_Ga=T,f=24,s=<s>,v=<v>,C=1,q=2,m=1;` + 先頭 4096 バイト + `ESC\`、以降 4096 バイトごとに `ESC_Gm=1;…ESC\`、最後は `ESC_Gm=0;…ESC\`。

### 4-3. 最初に書く Red テスト

| # | テスト名 | 入力 | 期待 |
|---|---|---|---|
| 1 | `apc_parser_keeps_graphics_commands_and_drops_everything_else` | `KITTY_PROLOGUE` + `KITTY_RECONFIG` + `b"\x1b[1;1f"` + `b"\x1b_Ga=T,f=24,s=2,v=1,C=1,q=2,m=0;AAAAAAAA\x1b\\"` + `b"\x1b[0m\n"` | コマンド 2 個。1 個目 `get('a') == Some("d")`。2 個目 `get('a') == Some("T")`, `get('s') == Some("2")`, `get('v') == Some("1")`, `raw` は APC 部分のバイト列と完全一致 |
| 2 | `apc_parser_gives_the_same_result_for_every_split_point` | 上と同じ入力を、全ての分割位置で 2 分割して feed / 1 バイトずつ feed | 一括 feed の結果と `keys`・`raw` が全て一致 |
| 3 | `assembler_joins_chunks_until_the_final_one` | `frame(320, 180, 9000 バイト)` をパーサに通し順に `push` | 先頭・2 個目は None、3 個目(m=0)で `Frame { width_px: 320, height_px: 180, bytes }`。`bytes` は 3 個の `raw` の連結と一致し、HVP は含まない |
| 4 | `bare_esc_terminated_delete_becomes_clear_without_eating_the_next_sequence` | `KITTY_UNINIT` + `frame(2, 1, 8 バイト)` | 1 個目 `Clear`。続けて `Frame` が正しく取れる(後続 CSI を巻き込んでいない) |
| 5 | `placement_centers_the_image_and_refuses_overflow` | `placement(Rect::new(0,0,80,22), CellSize{8,16}, (320,176))` / `placement(Rect::new(5,2,80,22), …同)` / `placement(Rect::new(0,0,80,22), …, (800,400))` | `Some(Placement{row: 6, col: 21})`(40×11 セル、余白 20/5、1 始まり)/ `Some(Placement{row: 8, col: 26})` / `None` |

### 4-4. 続いて書くテスト

| テスト名 | 要点 |
|---|---|
| `assembler_discards_an_unfinished_frame_when_a_new_one_starts` | `a=T,m=1` の後に `m=0` 無しで次の `a=T` → 先のデータが混ざらない(mpv の小画像バグと VO 作り直しの両方をカバー) |
| `continuation_without_an_open_frame_is_ignored` | 起動直後に `m=1` だけ来ても何も出ない |
| `oversized_apc_is_dropped_and_parsing_resumes` | 8192 バイト超の APC → 出力なし、その後の正常な APC は取れる |
| `cell_size_is_none_when_the_terminal_reports_no_pixels` | `cell_size(80, 24, 0, 0) == None`、`cell_size(80, 24, 720, 384) == Some(9×16)` |
| `geometry_scales_down_to_the_pixel_budget_keeping_aspect` | 80×22 セル × 8×16 px = 640×352(上限内、そのまま)。上限 100_000 なら 426×234 前後で幅/高さ比が 640/352 と一致(floor) |
| `mpv_args_pin_left_top_and_disable_alt_screen_and_clear` | `mpv_args()` に `--vo-kitty-cols=80` `--vo-kitty-rows=22` `--vo-kitty-width=640` `--vo-kitty-height=352` `--vo-kitty-left=1` `--vo-kitty-top=1` `--vo-kitty-alt-screen=no` `--vo-kitty-config-clear=no` が全て含まれる |
| `resize_sets_kitty_sizes_then_reinitializes_the_video` | `resize_video(geometry)` の 6 コマンドの JSON 行(tct 版テストの置き換え) |
| `encode_writes_clear_then_cup_then_frame_bytes` | `Pending{clear: true, frame: Some(16×16 px)}`, area 80×22, cell 8×16 → `ESC_Ga=d;ESC\` + `ESC[11;40H` + `frame.bytes`。収まらないフレームなら `a=d` だけ書いて false |
| `sink_keeps_the_latest_frame_and_never_drops_a_pending_clear` | Clear → Frame A → Frame B を feed → `take()` は `{clear: true, frame: B}`。2 回目の `take()` は None |
| `sink_resize_discards_stale_frames_and_owes_a_clear` | Frame 後に `resize()` → `take()` は `{clear: true, frame: None}` |
| `redraw_requests_are_collapsed_until_cleared` | 既存テストの移植 |
| `video_reader_asks_for_a_redraw_once_per_frame` | 既存テストの移植。printf で `frame()` 2 枚を流し、`VideoFrame{nonce}` が届き `take()` が 2 枚目を返す |

### 4-5. 実機確認項目(自動テスト不能)

| 項目 | 確認方法 | 外れたときの対処 |
|---|---|---|
| iTerm2 が `window_size()` でピクセルを返す | iTerm2 上で `crossterm::terminal::window_size()` を印字する最小バイナリ、または `python3 -c 'import fcntl,struct,termios,os;print(struct.unpack("HHHH",fcntl.ioctl(os.open("/dev/tty",os.O_RDONLY),termios.TIOCGWINSZ,b"\0"*8)))'` | 0 なら `FALLBACK_CELL` で動く。精度が要るなら XTWINOPS `CSI 16 t` の応答読みを検討 |
| `ESC_Ga=d;ESC\` で placement が消える | 再生停止後に映像が残らない | 残るなら `d=A` を試す |
| リサイズ後に古い画像が残らない | ドラッグでサイズ変更 | 残るなら `apply_resize` の Clear が出ているかを確認 |
| 一時停止中のリサイズで再描画される | 一時停止 → リサイズ → 新寸法で静止画が出る(reconfig の `want_redraw`) | 出ないなら `vid no/auto` の後に `frame-step` 等を検討 |
| 入力遅延・フレーム落ち | 通常サイズの端末で 30fps 動画を再生しキー操作の体感を見る | 遅ければ `MAX_FRAME_PIXELS` を下げる。改善しなければ §2-2 の B へ |
| 音声のみ動画 | 映像領域が空白のまま再生が進む |  |
| Ctrl-C / `q` 終了後 | 元のシェル画面に画像が残らない | 残るなら `stop_playback` 後の present を確認 |

## 5. 影響範囲

| パス | 変更 |
|---|---|
| `src/kitty.rs` | 新規 |
| `src/video.rs` | 全面書き換え(tct 実装削除) |
| `src/mpv.rs` | `launch()` 引数、`resize_video()`、`spawn_video_reader()` の型 |
| `src/ui.rs` | `draw_playing()` から `screen.render()` 呼び出しを外す |
| `src/main.rs` | `video_screen()`/`video_size()` → `video_geometry()`、ループに present 追加、`apply_resize`、`end_playback`、`Session.owe_clear` |
| `src/app.rs` | `video` フィールドの型、コメント |
| `Cargo.toml` | `vt100` 削除 |

## 6. 制限・非対象

- tmux / GNU screen 内では動かない(APC を DCS で包む passthrough が必要。パーサは DCS を読み飛ばす)。
- Kitty graphics protocol 非対応端末では映像領域が空白のまま(エラーは出ない。`q=2` で端末も何も返さない)。検出して案内する機能は対象外。
- 共有メモリ転送(`--vo-kitty-use-shm`)、画像 ID / placement ID による差分更新は対象外。
- 端末側スケーリング(`c=`/`r=` の追記)は Phase 2。

## 7. 実装順

1. `src/kitty.rs`: §4-3 の #1〜#4 を Red → `ApcParser` → `FrameAssembler`。
2. `src/video.rs`: #5 と §4-4 の幾何・encode・sink テストを Red → 実装。ここまで mpv 不要。
3. `src/mpv.rs`: `mpv_args` / `resize_video` のテストを差し替え → `launch()` と reader の結線。
4. `src/main.rs` / `src/ui.rs` / `src/app.rs`: present の組み込み、リサイズ・終了時の Clear。
5. tct 実装と `vt100` の削除、`cargo test` 全緑。
6. 実機確認(§4-5)、`MAX_FRAME_PIXELS` 調整。
7. Phase 2(必要なら): 先頭チャンク制御部に `c=<cols>,r=<rows>` を追記して端末側で領域いっぱいに拡大(iTerm2 の対応確認が先)、shm 転送、非対応端末の検出。
