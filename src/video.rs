use crate::kitty::{ApcParser, FrameAssembler, FrameEvent, GraphicsCommand, VideoFrame};
use ratatui::layout::Rect;
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

    pub fn mpv_args(&self) -> Vec<String> {
        vec![
            format!("--vo-kitty-cols={}", self.area.width.max(1)),
            format!("--vo-kitty-rows={}", self.area.height.max(1)),
            format!("--vo-kitty-width={}", self.frame_px.0),
            format!("--vo-kitty-height={}", self.frame_px.1),
            // 位置決めは tuitube が行う。mpv の自動計算は 1 行ズレるので 1 に固定する。
            "--vo-kitty-left=1".to_string(),
            "--vo-kitty-top=1".to_string(),
            "--vo-kitty-alt-screen=no".to_string(),
            "--vo-kitty-config-clear=no".to_string(),
        ]
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

/// 1 始まり・画面絶対座標の CUP 位置。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Placement {
    pub row: u16,
    pub col: u16,
}

/// 画像が占めるセル数 = ceil(px / cell_px)。領域に収まらなければ None (描かない)。
pub fn placement(area: Rect, cell: CellSize, image_px: (u32, u32)) -> Option<Placement> {
    let cols = image_px.0.div_ceil(u32::from(cell.width_px.max(1)));
    let rows = image_px.1.div_ceil(u32::from(cell.height_px.max(1)));
    if cols == 0 || rows == 0 || cols > u32::from(area.width) || rows > u32::from(area.height) {
        return None;
    }
    let col = u32::from(area.x) + (u32::from(area.width) - cols) / 2 + 1;
    let row = u32::from(area.y) + (u32::from(area.height) - rows) / 2 + 1;
    Some(Placement {
        row: u16::try_from(row).ok()?,
        col: u16::try_from(col).ok()?,
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

/// clear なら削除列を、フレームが領域に収まるなら CUP + APC バイト列を積む。
/// 戻り値はフレームを書いたか。
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
    out.extend_from_slice(&frame.bytes);
    true
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
                assembler: FrameAssembler::default(),
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
        sink.assembler = FrameAssembler::default();
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

    #[test]
    fn placement_centers_the_image_and_refuses_overflow() {
        // 40x11 セルの画像を 80x22 の領域に中央寄せ。余白は 20 桁 / 5 行。
        assert_eq!(
            placement(Rect::new(0, 0, 80, 22), CELL, (320, 176)),
            Some(Placement { row: 6, col: 21 })
        );
        // 領域の原点ぶんだけずれる。
        assert_eq!(
            placement(Rect::new(5, 2, 80, 22), CELL, (320, 176)),
            Some(Placement { row: 8, col: 26 })
        );
        // 領域より大きい画像は描かない。
        assert_eq!(placement(Rect::new(0, 0, 80, 22), CELL, (800, 400)), None);
        // 端数は切り上げでセル数を数える。
        assert_eq!(
            placement(Rect::new(0, 0, 4, 2), CELL, (17, 1)),
            Some(Placement { row: 1, col: 1 })
        );
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
    fn encode_writes_clear_then_cup_then_frame_bytes() {
        let pending = Pending {
            clear: true,
            frame: Some(VideoFrame {
                width_px: 16,
                height_px: 16,
                bytes: b"<apc>".to_vec(),
            }),
        };
        let mut out = Vec::new();
        assert!(encode(&pending, Rect::new(0, 0, 80, 22), CELL, &mut out));
        assert_eq!(out, b"\x1b_Ga=d,q=2;\x1b\\\x1b[11;40H<apc>");

        // 収まらないフレームは捨て、削除だけ書く。
        let oversized = Pending {
            clear: true,
            frame: Some(VideoFrame {
                width_px: 800,
                height_px: 400,
                bytes: b"<apc>".to_vec(),
            }),
        };
        let mut out = Vec::new();
        assert!(!encode(&oversized, Rect::new(0, 0, 80, 22), CELL, &mut out));
        assert_eq!(out, b"\x1b_Ga=d,q=2;\x1b\\");

        // 端末の応答を stdin に流し込まないよう q=2 を必ず付ける。
        let mut out = Vec::new();
        encode_clear(&mut out);
        assert_eq!(out, b"\x1b_Ga=d,q=2;\x1b\\");
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
