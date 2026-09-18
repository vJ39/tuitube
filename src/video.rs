use crate::kitty::{ApcParser, FrameAssembler, FrameEvent, GraphicsCommand, ST, VideoFrame};
use ratatui::layout::Rect;
use serde_json::{Value, json};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CellSize {
    pub width_px: u16,
    pub height_px: u16,
}

/// 端末がピクセル寸法を報告しないときの控えめな既定値。大きすぎると領域を突き抜ける。
pub const FALLBACK_CELL: CellSize = CellSize {
    width_px: 8,
    height_px: 16,
};
/// 1 フレームのピクセル数の上限。パイプと端末の処理量は画像面積に比例する。
pub const MAX_FRAME_PIXELS: u32 = 640 * 360;

/// window_size() の値からセル寸法を求める。ピクセルが 0 なら None。
pub fn cell_size(columns: u16, rows: u16, width_px: u16, height_px: u16) -> Option<CellSize> {
    let width_px = width_px.checked_div(columns).filter(|px| *px > 0)?;
    let height_px = height_px.checked_div(rows).filter(|px| *px > 0)?;
    Some(CellSize {
        width_px,
        height_px,
    })
}

/// mpv に渡す寸法。area は画面座標のセル矩形 (ui::video_area の戻り)。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Geometry {
    pub area: Rect,
    pub cell: CellSize,
    pub frame_px: (u32, u32),
}

impl Geometry {
    pub fn new(area: Rect, cell: CellSize, max_pixels: u32) -> Self {
        let width = u32::from(area.width.max(1)) * u32::from(cell.width_px.max(1));
        let height = u32::from(area.height.max(1)) * u32::from(cell.height_px.max(1));
        Self {
            area,
            cell,
            frame_px: fit_budget(width, height, max_pixels),
        }
    }

    /// 1 フレームのピクセル数。
    pub fn pixels(&self) -> u64 {
        u64::from(self.frame_px.0) * u64::from(self.frame_px.1)
    }

    /// 描画先の寸法。端末のリサイズで変わるのはここだけ。
    pub fn size_options(&self) -> [(&'static str, Value); 4] {
        [
            ("cols", json!(self.area.width.max(1))),
            ("rows", json!(self.area.height.max(1))),
            ("width", json!(self.frame_px.0)),
            ("height", json!(self.frame_px.1)),
        ]
    }

    /// kitty VO に渡す設定。起動引数と、別ウィンドウから埋め込みへ戻すときの
    /// set_property が同じ組を使う (片方だけに足すと経路ごとに構成が変わる)。
    pub fn kitty_options(&self) -> Vec<(&'static str, Value)> {
        let mut options = self.size_options().to_vec();
        options.extend([
            // 位置決めは tuitube が行う。mpv の自動計算は 1 行ズレるので 1 に固定する。
            ("left", json!(1)),
            ("top", json!(1)),
            // 画面の切替と消去も tuitube 側で持つ。
            ("alt-screen", json!(false)),
            ("config-clear", json!(false)),
        ]);
        options
    }

    pub fn mpv_args(&self) -> Vec<String> {
        self.kitty_options()
            .iter()
            .map(|(key, value)| format!("--vo-kitty-{key}={}", option_arg(value)))
            .collect()
    }
}

/// mpv のコマンドラインは真偽値を yes / no で書く。
fn option_arg(value: &Value) -> String {
    match value.as_bool() {
        Some(true) => "yes".to_string(),
        Some(false) => "no".to_string(),
        None => value.to_string(),
    }
}

/// 面積が上限を超えるぶんだけアスペクト比を保って縮める。
fn fit_budget(width: u32, height: u32, max_pixels: u32) -> (u32, u32) {
    let pixels = u64::from(width) * u64::from(height);
    if pixels <= u64::from(max_pixels) || pixels == 0 {
        return (width, height);
    }
    let scale = (f64::from(max_pixels) / pixels as f64).sqrt();
    let shrink = |v: u32| ((f64::from(v) * scale) as u32).max(1);
    (shrink(width), shrink(height))
}

/// 1 始まり・画面絶対座標の CUP 位置と、端末に拡大させる表示セル数 (c=/r=)。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Placement {
    pub row: u16,
    pub col: u16,
    pub cols: u16,
    pub rows: u16,
}

/// アスペクト比を保ったまま領域内で最大のセル矩形を求め、中央に置く。
/// セルは正方形でないので、拡大率はセル数でなくピクセル換算で決める。
/// 等倍のセル数が領域に収まらないフレームは None (描かない)。
pub fn placement(area: Rect, cell: CellSize, image_px: (u32, u32)) -> Option<Placement> {
    if area.width == 0 || area.height == 0 || image_px.0 == 0 || image_px.1 == 0 {
        return None;
    }
    // c=/r= を解釈しない端末では等倍で描かれるので、等倍で領域外に出るものは捨てる。
    // リサイズのデバウンス中に届く旧寸法のフレームもここで落ちる。
    if image_px.0.div_ceil(u32::from(cell.width_px.max(1))) > u32::from(area.width)
        || image_px.1.div_ceil(u32::from(cell.height_px.max(1))) > u32::from(area.height)
    {
        return None;
    }
    let cell_w = f64::from(cell.width_px.max(1));
    let cell_h = f64::from(cell.height_px.max(1));
    let scale = (f64::from(area.width) * cell_w / f64::from(image_px.0))
        .min(f64::from(area.height) * cell_h / f64::from(image_px.1));
    let cells = |px: u32, cell_px: f64, limit: u16| {
        ((f64::from(px) * scale / cell_px).round() as u32).clamp(1, u32::from(limit)) as u16
    };
    let cols = cells(image_px.0, cell_w, area.width);
    let rows = cells(image_px.1, cell_h, area.height);
    let col = u32::from(area.x) + u32::from(area.width - cols) / 2 + 1;
    let row = u32::from(area.y) + u32::from(area.height - rows) / 2 + 1;
    Some(Placement {
        row: u16::try_from(row).ok()?,
        col: u16::try_from(col).ok()?,
        cols,
        rows,
    })
}

#[derive(Default)]
pub struct Pending {
    pub clear: bool,
    pub frame: Option<VideoFrame>,
}

impl Pending {
    fn is_empty(&self) -> bool {
        !self.clear && self.frame.is_none()
    }
}

/// clear なら削除列を、置き場があるなら CUP + 表示セル数を足した APC バイト列を積む。
/// 等倍で領域に収まらないフレームは捨てる。戻り値はフレームを書いたか。
pub fn encode(pending: &Pending, area: Rect, cell: CellSize, out: &mut Vec<u8>) -> bool {
    if pending.clear {
        encode_clear(out);
    }
    let Some(frame) = &pending.frame else {
        return false;
    };
    let Some(at) = placement(area, cell, (frame.width_px, frame.height_px)) else {
        return false;
    };
    out.extend_from_slice(format!("\x1b[{};{}H", at.row, at.col).as_bytes());
    push_scaled(&frame.bytes, at.cols, at.rows, out);
    true
}

/// 先頭チャンクの制御部に c=/r= を足し、端末側でセル矩形いっぱいに拡大させる。
/// 制御部の終端は最初の ";" (後続の base64 データに ";" は現れない)。
/// 継続チャンクは m と q しか持てないので、探索は先頭チャンク (最初の ST まで) に限る。
fn push_scaled(bytes: &[u8], cols: u16, rows: u16, out: &mut Vec<u8>) {
    let head = bytes
        .windows(ST.len())
        .position(|w| w == ST)
        .unwrap_or(bytes.len());
    let Some(end) = bytes[..head].iter().position(|b| *b == b';') else {
        out.extend_from_slice(bytes);
        return;
    };
    out.extend_from_slice(&bytes[..end]);
    out.extend_from_slice(format!(",c={cols},r={rows}").as_bytes());
    out.extend_from_slice(&bytes[end..]);
}

/// q=2 が無いと端末の応答が tuitube の stdin に入りキーイベントとして誤読される (§1-2)。
pub fn encode_clear(out: &mut Vec<u8>) {
    out.extend_from_slice(b"\x1b_Ga=d,q=2;\x1b\\");
}

struct Sink {
    geometry: Geometry,
    parser: ApcParser,
    assembler: FrameAssembler,
    pending: Pending,
    /// feed のたびに確保し直さないための置き場。
    commands: Vec<GraphicsCommand>,
}

#[derive(Clone)]
pub struct VideoSink {
    sink: Arc<Mutex<Sink>>,
    redraw: Arc<AtomicBool>,
}

impl VideoSink {
    pub fn new(geometry: Geometry) -> Self {
        Self {
            sink: Arc::new(Mutex::new(Sink {
                geometry,
                parser: ApcParser::default(),
                assembler: FrameAssembler::new(geometry.pixels()),
                pending: Pending::default(),
                commands: Vec::new(),
            })),
            redraw: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn geometry(&self) -> Geometry {
        self.lock().geometry
    }

    /// 表示すべき更新 (Frame または Clear) が新たに生じたら true。
    pub fn feed(&self, bytes: &[u8]) -> bool {
        let mut sink = self.lock();
        let Sink {
            parser,
            assembler,
            pending,
            commands,
            ..
        } = &mut *sink;
        commands.clear();
        parser.feed(bytes, commands);
        let mut updated = false;
        for command in commands.drain(..) {
            match assembler.push(command) {
                Some(FrameEvent::Frame(frame)) => {
                    pending.frame = Some(frame);
                    updated = true;
                }
                // 削除より前のフレームは寸法も位置も当てにならないので捨てる。
                Some(FrameEvent::Clear) => {
                    pending.clear = true;
                    pending.frame = None;
                    updated = true;
                }
                None => {}
            }
        }
        updated
    }

    /// 保留中の更新を取り出す。取り出すと空になる。
    pub fn take(&self) -> Option<Pending> {
        let mut sink = self.lock();
        if sink.pending.is_empty() {
            return None;
        }
        Some(std::mem::take(&mut sink.pending))
    }

    /// 寸法変更。保留中フレームを捨て、clear を保留にする。
    pub fn resize(&self, geometry: Geometry) {
        let mut sink = self.lock();
        sink.geometry = geometry;
        // 組み立て途中のものを残すと旧 s/v のまま完成して 1 枚だけずれて出るので、
        // 読み取り中のバイト列ごと捨てる。次の a=T から新しい寸法で組み直す。
        sink.parser = ApcParser::default();
        // 予算は画質設定と端末寸法で変わるので、上限も新しい寸法で取り直す。
        sink.assembler = FrameAssembler::new(geometry.pixels());
        sink.pending.clear = true;
        sink.pending.frame = None;
    }

    /// 未処理の再描画要求が無いときだけ true。30fps の feed で通知を溜めないための畳み込み。
    pub fn request_redraw(&self) -> bool {
        !self.redraw.swap(true, Ordering::AcqRel)
    }

    pub fn clear_redraw(&self) {
        self.redraw.store(false, Ordering::Release);
    }

    /// 描画側が panic しても映像を止める理由はないので、poison は無視して中身を使う。
    fn lock(&self) -> MutexGuard<'_, Sink> {
        self.sink.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kitty::fixtures::{KITTY_RECONFIG, frame};

    const CELL: CellSize = CellSize {
        width_px: 8,
        height_px: 16,
    };

    fn geometry(width: u16, height: u16) -> Geometry {
        Geometry::new(Rect::new(0, 0, width, height), CELL, MAX_FRAME_PIXELS)
    }

    /// セルは正方形でないので、比が合っているかはピクセル換算で見る。
    fn aspect_error(at: Placement, image_px: (u32, u32)) -> f64 {
        let shown = f64::from(u32::from(at.cols) * u32::from(CELL.width_px))
            / f64::from(u32::from(at.rows) * u32::from(CELL.height_px));
        let source = f64::from(image_px.0) / f64::from(image_px.1);
        (shown / source - 1.0).abs()
    }

    #[test]
    fn placement_fills_the_area_when_the_aspect_matches() {
        // 640x352 px は 80x22 セル (8x16 px) ちょうど。領域いっぱいに広げる。
        assert_eq!(
            placement(Rect::new(0, 0, 80, 22), CELL, (640, 352)),
            Some(Placement {
                row: 1,
                col: 1,
                cols: 80,
                rows: 22
            })
        );
        // 領域の原点ぶんだけずれる。
        assert_eq!(
            placement(Rect::new(5, 2, 80, 22), CELL, (640, 352)),
            Some(Placement {
                row: 3,
                col: 6,
                cols: 80,
                rows: 22
            })
        );
    }

    #[test]
    fn placement_enlarges_a_small_frame_to_the_area() {
        // mpv の送る 640x360 上限のフレームが領域より小さくても、端末側で拡大させる。
        let at = placement(Rect::new(0, 0, 80, 22), CELL, (16, 16)).expect("置けるはず");
        assert_eq!(
            at,
            Placement {
                row: 1,
                col: 19,
                cols: 44,
                rows: 22
            }
        );
        assert!(aspect_error(at, (16, 16)) < 0.05);
    }

    #[test]
    fn placement_keeps_aspect_for_tall_and_wide_frames() {
        // 縦に余裕が無い比率: 高さを使い切り、幅が縮む。
        let tall = placement(Rect::new(0, 0, 80, 22), CELL, (320, 180)).expect("置けるはず");
        assert_eq!(
            tall,
            Placement {
                row: 1,
                col: 2,
                cols: 78,
                rows: 22
            }
        );
        assert!(aspect_error(tall, (320, 180)) < 0.05);

        // 横長: 幅を使い切り、高さが縮んで上下に余白が出る。
        let wide = placement(Rect::new(0, 0, 80, 22), CELL, (640, 180)).expect("置けるはず");
        assert_eq!(
            wide,
            Placement {
                row: 6,
                col: 1,
                cols: 80,
                rows: 11
            }
        );
        assert!(aspect_error(wide, (640, 180)) < 0.05);
    }

    #[test]
    fn placement_refuses_a_frame_that_overflows_the_area_at_native_size() {
        // 等倍で 100x25 セル要る画像は 80x22 の領域に置かない。c=/r= を解釈しない端末では
        // 縮まずステータス行・ヘルプ行に被るため。
        assert_eq!(placement(Rect::new(0, 0, 80, 22), CELL, (800, 400)), None);
        // 片側だけはみ出す場合も同じ。
        assert_eq!(placement(Rect::new(0, 0, 80, 22), CELL, (800, 176)), None);
        assert_eq!(placement(Rect::new(0, 0, 80, 22), CELL, (320, 400)), None);
        // リサイズのデバウンス中に届く旧寸法 (80x22 ぶん) のフレームは新領域では弾かれる。
        assert_eq!(placement(Rect::new(0, 0, 40, 12), CELL, (640, 352)), None);
        // 端数は切り上げで数える。1 セルに収まる画像は 1 セルの領域にも置ける。
        assert_eq!(placement(Rect::new(0, 0, 1, 1), CELL, (9, 16)), None);
        assert_eq!(
            placement(Rect::new(0, 0, 1, 1), CELL, (8, 16)),
            Some(Placement {
                row: 1,
                col: 1,
                cols: 1,
                rows: 1
            })
        );
    }

    #[test]
    fn placement_is_none_for_a_degenerate_area_or_image() {
        assert_eq!(placement(Rect::new(0, 0, 0, 22), CELL, (640, 352)), None);
        assert_eq!(placement(Rect::new(0, 0, 80, 0), CELL, (640, 352)), None);
        assert_eq!(placement(Rect::new(0, 0, 80, 22), CELL, (0, 352)), None);
        assert_eq!(placement(Rect::new(0, 0, 80, 22), CELL, (640, 0)), None);
    }

    #[test]
    fn cell_size_is_none_when_the_terminal_reports_no_pixels() {
        assert_eq!(cell_size(80, 24, 0, 0), None);
        assert_eq!(cell_size(80, 24, 720, 0), None);
        assert_eq!(cell_size(0, 24, 720, 384), None);
        assert_eq!(
            cell_size(80, 24, 720, 384),
            Some(CellSize {
                width_px: 9,
                height_px: 16
            })
        );
    }

    #[test]
    fn geometry_scales_down_to_the_pixel_budget_keeping_aspect() {
        let full = geometry(80, 22);
        assert_eq!(full.frame_px, (640, 352));

        let capped = Geometry::new(Rect::new(0, 0, 80, 22), CELL, 100_000);
        assert_eq!(capped.frame_px, (426, 234));
        assert!(capped.frame_px.0 * capped.frame_px.1 <= 100_000);
        assert_eq!(capped.frame_px.1, capped.frame_px.0 * 352 / 640);
    }

    #[test]
    fn mpv_args_pin_left_top_and_disable_alt_screen_and_clear() {
        let args = geometry(80, 22).mpv_args();
        for expected in [
            "--vo-kitty-cols=80",
            "--vo-kitty-rows=22",
            "--vo-kitty-width=640",
            "--vo-kitty-height=352",
            "--vo-kitty-left=1",
            "--vo-kitty-top=1",
            "--vo-kitty-alt-screen=no",
            "--vo-kitty-config-clear=no",
        ] {
            assert!(args.iter().any(|a| a == expected), "{expected} がない");
        }
    }

    #[test]
    fn mpv_args_do_not_use_shared_memory_transfer() {
        // t=s のフレームは m=1 の APC 1 個だけで終端の m=0 が来ず、組み立てが完了しない
        // (mpv 0.41.0 実測)。オブジェクト名も VO ごとに固定で毎フレーム上書きされるため、
        // stdout を読んでから端末へ送り直す tuitube の経路では中身が入れ替わる。
        let args = geometry(80, 22).mpv_args();
        assert!(
            !args.iter().any(|a| a.starts_with("--vo-kitty-use-shm")),
            "共有メモリ転送は使えない"
        );
    }

    #[test]
    fn a_shared_memory_frame_never_completes() {
        // mpv 0.41.0 の --vo-kitty-use-shm=yes が実際に出す列。1 フレーム = m=1 の APC 1 個で、
        // 終端の m=0 が来ないので組み立ては終わらない。データ部は共有メモリの名前 (毎回同じ)。
        let sink = VideoSink::new(geometry(80, 22));
        let shm =
            b"\x1b[1;1f\x1b_Ga=T,t=s,f=24,s=160,v=160,C=1,q=2,m=1;bXB2LWtpdHR5LTB4MTU4NjEzYjcw\x1b\\";
        assert!(!sink.feed(shm));
        assert!(!sink.feed(shm));
        assert!(sink.take().is_none(), "shm のフレームは完成しない");
    }

    #[test]
    fn a_frame_at_a_larger_budget_is_not_dropped_as_oversized() {
        // quality = "high" 相当。上限が 640x360 固定だと、このフレームは毎回捨てられていた。
        let geometry = Geometry::new(Rect::new(0, 0, 240, 68), CELL, 960 * 540);
        let sink = VideoSink::new(geometry);
        let (width, height) = geometry.frame_px;
        assert!(geometry.pixels() > u64::from(MAX_FRAME_PIXELS));

        let data = vec![b'Q'; geometry.pixels() as usize * 4];
        assert!(
            sink.feed(&frame(width, height, &data)),
            "フレームが完成しない"
        );
        let pending = sink.take().expect("フレームがあるはず");
        let frame = pending.frame.expect("フレームがあるはず");
        assert_eq!((frame.width_px, frame.height_px), (width, height));
    }

    #[test]
    fn encode_writes_clear_then_cup_then_frame_bytes() {
        let pending = Pending {
            clear: true,
            frame: Some(VideoFrame {
                width_px: 16,
                height_px: 16,
                bytes: b"\x1b_Ga=T,f=24,s=16,v=16,C=1,q=2,m=0;DATA\x1b\\".to_vec(),
            }),
        };
        let mut out = Vec::new();
        assert!(encode(&pending, Rect::new(0, 0, 80, 22), CELL, &mut out));
        assert_eq!(
            out,
            b"\x1b_Ga=d,q=2;\x1b\\\x1b[1;19H\x1b_Ga=T,f=24,s=16,v=16,C=1,q=2,m=0,c=44,r=22;DATA\x1b\\"
        );

        // 置き場が無いときはフレームを捨て、削除だけ書く。
        let mut out = Vec::new();
        assert!(!encode(&pending, Rect::new(0, 0, 0, 0), CELL, &mut out));
        assert_eq!(out, b"\x1b_Ga=d,q=2;\x1b\\");

        // 等倍で領域からはみ出すフレーム (リサイズ直後の旧寸法) も捨てる。
        let mut out = Vec::new();
        assert!(!encode(&pending, Rect::new(0, 0, 1, 1), CELL, &mut out));
        assert_eq!(out, b"\x1b_Ga=d,q=2;\x1b\\");

        // 端末の応答を stdin に流し込まないよう q=2 を必ず付ける。
        let mut out = Vec::new();
        encode_clear(&mut out);
        assert_eq!(out, b"\x1b_Ga=d,q=2;\x1b\\");
    }

    #[test]
    fn encode_adds_the_display_cell_size_to_the_first_chunk_only() {
        let sink = VideoSink::new(geometry(80, 22));
        assert!(sink.feed(&frame(640, 352, b"DATA")));
        let pending = sink.take().expect("フレームがあるはず");

        let mut out = Vec::new();
        assert!(encode(&pending, Rect::new(0, 0, 80, 22), CELL, &mut out));
        assert_eq!(
            out,
            b"\x1b[1;1H\
              \x1b_Ga=T,f=24,s=640,v=352,C=1,q=2,m=1,c=80,r=22;DATA\x1b\\\
              \x1b_Gm=0;\x1b\\"
        );
    }

    #[test]
    fn encode_leaves_bytes_alone_when_there_is_no_control_section() {
        let pending = Pending {
            clear: false,
            frame: Some(VideoFrame {
                width_px: 640,
                height_px: 352,
                bytes: b"<apc>".to_vec(),
            }),
        };
        let mut out = Vec::new();
        assert!(encode(&pending, Rect::new(0, 0, 80, 22), CELL, &mut out));
        assert_eq!(out, b"\x1b[1;1H<apc>");
    }

    #[test]
    fn encode_never_scales_a_continuation_chunk() {
        // 先頭チャンクが ";" の手前で切れた列 (素の ESC で打ち切られ ApcParser が正規化した形)。
        // 継続チャンクは m と q しか持てないので c=/r= を足さず、そのまま流す。
        let bytes = b"\x1b_Ga=T,f=24,s=16,v=16,C=1,q=2,m=1\x1b\\\x1b_Gm=0;DATA\x1b\\".to_vec();
        let pending = Pending {
            clear: false,
            frame: Some(VideoFrame {
                width_px: 16,
                height_px: 16,
                bytes: bytes.clone(),
            }),
        };
        let mut out = Vec::new();
        assert!(encode(&pending, Rect::new(0, 0, 80, 22), CELL, &mut out));
        let mut expected = b"\x1b[1;19H".to_vec();
        expected.extend_from_slice(&bytes);
        assert_eq!(out, expected);
    }

    #[test]
    fn sink_keeps_the_latest_frame_and_never_drops_a_pending_clear() {
        let sink = VideoSink::new(geometry(80, 22));
        assert!(sink.feed(KITTY_RECONFIG));
        assert!(sink.feed(&frame(2, 1, b"AAAA")));
        assert!(sink.feed(&frame(4, 2, b"BBBB")));

        let pending = sink.take().expect("更新があるはず");
        assert!(pending.clear);
        let frame = pending.frame.expect("フレームがあるはず");
        assert_eq!((frame.width_px, frame.height_px), (4, 2));
        assert!(sink.take().is_none());
    }

    #[test]
    fn feed_without_a_complete_frame_reports_no_update() {
        let sink = VideoSink::new(geometry(80, 22));
        assert!(!sink.feed(b"\x1b[?25l\x1b[1;1f"));
        assert!(!sink.feed(b"\x1b_Ga=T,f=24,s=2,v=1,C=1,q=2,m=1;AAAA\x1b\\"));
        assert!(sink.take().is_none());
        assert!(sink.feed(b"\x1b_Gm=0;BBBB\x1b\\"));
    }

    #[test]
    fn sink_resize_discards_stale_frames_and_owes_a_clear() {
        let sink = VideoSink::new(geometry(80, 22));
        assert!(sink.feed(&frame(2, 1, b"AAAA")));
        let next = geometry(100, 30);
        sink.resize(next);
        assert_eq!(sink.geometry(), next);

        let pending = sink.take().expect("clear が保留のはず");
        assert!(pending.clear);
        assert!(pending.frame.is_none());
        assert!(sink.take().is_none());
    }

    #[test]
    fn sink_resize_discards_a_frame_that_was_being_assembled() {
        let sink = VideoSink::new(geometry(80, 22));
        // 80x22 ぶんの寸法で組み立てが始まったところでリサイズが入る。
        assert!(!sink.feed(b"\x1b_Ga=T,f=24,s=320,v=176,C=1,q=2,m=1;AAAA\x1b\\"));
        sink.resize(geometry(40, 12));

        // 残りのチャンクが後から届いても、旧寸法のフレームは完成しない。
        assert!(!sink.feed(b"\x1b_Gm=0;BBBB\x1b\\"));
        let pending = sink.take().expect("clear が保留のはず");
        assert!(pending.clear);
        assert!(pending.frame.is_none());

        // 新しい寸法のフレームは通常どおり通る。
        assert!(sink.feed(&frame(160, 96, b"CCCC")));
        let pending = sink.take().expect("新しいフレームがあるはず");
        let frame = pending.frame.expect("フレームがあるはず");
        assert_eq!((frame.width_px, frame.height_px), (160, 96));
    }

    #[test]
    fn redraw_requests_are_collapsed_until_cleared() {
        let sink = VideoSink::new(geometry(80, 22));
        assert!(sink.request_redraw());
        assert!(!sink.request_redraw());
        sink.clear_redraw();
        assert!(sink.request_redraw());
    }
}
