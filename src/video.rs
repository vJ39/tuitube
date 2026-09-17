use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

const ESC: u8 = 0x1b;
/// これを超えるパラメータ列は mpv の出力ではないので、書き換えずそのまま流す。
const MAX_PARAMS: usize = 32;

/// mpv の `--vo=tct` が吐く ANSI 列を解釈する仮想端末。
/// 実端末には一切書き込まず、解釈済みのセルグリッドだけを ratatui に渡す
/// (素通しすると alternate screen 切り替えや絶対座標指定で TUI が壊れる)。
#[derive(Clone)]
pub struct VideoScreen {
    vt: Arc<Mutex<Vt>>,
    redraw: Arc<AtomicBool>,
}

struct Vt {
    parser: vt100::Parser,
    rewriter: TctRewriter,
    scratch: Vec<u8>,
    /// 完成したフレームだけを持つ表バッファ。描画はこちらからしか行わない。
    frame: Option<Buffer>,
    /// フレームの区切りを1度でも見たか。見ていない間だけ生の画面を描く。
    saw_frame: bool,
}

impl Vt {
    /// 映像として見せる範囲。最下行は改行の逃がし場所なので含めない。
    fn visible(&self) -> Rect {
        let (rows, cols) = self.parser.screen().size();
        Rect::new(0, 0, cols, rows.saturating_sub(1))
    }
}

/// mpv は各フレームの末尾に `ESC[0m` と改行を出す。最下行に映像を描いた直後だと
/// この改行で画面が1行スクロールし、最上行が消えるので、逃がし用の行を1行余分に持つ。
fn new_parser(width: u16, height: u16) -> vt100::Parser {
    vt100::Parser::new(height.saturating_add(1), width, 0)
}

impl VideoScreen {
    pub fn new(width: u16, height: u16) -> Self {
        Self {
            vt: Arc::new(Mutex::new(Vt {
                parser: new_parser(width, height),
                rewriter: TctRewriter::default(),
                scratch: Vec::new(),
                frame: None,
                saw_frame: false,
            })),
            redraw: Arc::new(AtomicBool::new(false)),
        }
    }

    /// (桁数, 行数)。mpv に渡す `--vo-tct-width/height` の出どころでもある。
    pub fn size(&self) -> (u16, u16) {
        let visible = self.lock().visible();
        (visible.width, visible.height)
    }

    /// フレームが1枚以上完成したら true。描画はこの区切りでしか更新しない。
    pub fn feed(&self, bytes: &[u8]) -> bool {
        let mut vt = self.lock();
        let visible = vt.visible();
        let Vt {
            parser,
            rewriter,
            scratch,
            frame,
            saw_frame,
        } = &mut *vt;
        let mut rest = bytes;
        let mut completed = false;
        while !rest.is_empty() {
            scratch.clear();
            let (used, frame_end) = rewriter.rewrite(rest, scratch);
            parser.process(scratch);
            rest = &rest[used..];
            if frame_end {
                capture_frame(parser.screen(), visible, frame);
                *saw_frame = true;
                completed = true;
            }
        }
        completed
    }

    /// 端末リサイズ後の寸法に作り直す。mpv 側も映像を作り直すので途中状態は捨てる。
    pub fn resize(&self, width: u16, height: u16) {
        let mut vt = self.lock();
        if vt.visible() == Rect::new(0, 0, width, height) {
            return;
        }
        vt.parser = new_parser(width, height);
        vt.frame = None;
    }

    /// 未処理の再描画要求が無いときだけ true。30fps の feed で通知を溜めないための畳み込み。
    pub fn request_redraw(&self) -> bool {
        !self.redraw.swap(true, Ordering::AcqRel)
    }

    pub fn clear_redraw(&self) {
        self.redraw.store(false, Ordering::Release);
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        let vt = self.lock();
        let visible = vt.visible();
        match &vt.frame {
            Some(frame) => copy_frame(frame, area, buf),
            // 区切りを出さない mpv でも映像が消えないよう、1枚目が来るまでは生の画面を描く。
            // リサイズ直後 (frame を捨てた後) は描かない。作り直し中の映像が混ざるため。
            None if !vt.saw_frame => {
                let area = Rect {
                    height: area.height.min(visible.height),
                    ..area
                };
                render_screen(vt.parser.screen(), area, buf);
            }
            None => {}
        }
    }

    /// 描画側が panic しても映像を止める理由はないので、poison は無視して中身を使う。
    fn lock(&self) -> MutexGuard<'_, Vt> {
        self.vt.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// 完成した画面を表バッファへ退避する。
fn capture_frame(screen: &vt100::Screen, visible: Rect, frame: &mut Option<Buffer>) {
    if !matches!(frame, Some(buf) if buf.area == visible) {
        *frame = Some(Buffer::empty(visible));
    }
    let buf = frame.as_mut().expect("just filled");
    buf.reset();
    render_screen(screen, visible, buf);
}

fn copy_frame(frame: &Buffer, area: Rect, buf: &mut Buffer) {
    let height = area.height.min(frame.area.height);
    let width = area.width.min(frame.area.width);
    for row in 0..height {
        for col in 0..width {
            let Some(cell) = frame.cell((col, row)) else {
                continue;
            };
            if let Some(target) = buf.cell_mut((area.x + col, area.y + row)) {
                *target = cell.clone();
            }
        }
    }
}

/// 仮想端末のセルグリッドを ratatui の Buffer へ転写する。
pub fn render_screen(screen: &vt100::Screen, area: Rect, buf: &mut Buffer) {
    let (rows, cols) = screen.size();
    let height = area.height.min(rows);
    let width = area.width.min(cols);
    for row in 0..height {
        for col in 0..width {
            let Some(cell) = screen.cell(row, col) else {
                continue;
            };
            if cell.is_wide_continuation() {
                continue;
            }
            let Some(target) = buf.cell_mut((area.x + col, area.y + row)) else {
                continue;
            };
            let contents = cell.contents();
            target.set_symbol(if contents.is_empty() { " " } else { &contents });
            target.set_fg(convert_color(cell.fgcolor()));
            target.set_bg(convert_color(cell.bgcolor()));
        }
    }
}

fn convert_color(color: vt100::Color) -> Color {
    match color {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(i) => Color::Indexed(i),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

#[derive(Default, Clone, Copy)]
enum CsiState {
    #[default]
    Text,
    Escape,
    Csi,
}

/// mpv の tct 出力を vt100 が読める形へ直す。状態を持つのは CSI が read の境界で分断されるため。
/// - HVP (`ESC [ row ; col f`) を同義の CUP (`… H`) へ。vt100 は CUP しか解釈しない。
/// - mpv は行・桁を 0 始まりで載せてくるが ECMA-48 のパラメータは 1 始まりなので +1 する
///   (そのままだと行0と行1が重なり、最下行が空白のまま残る)。
/// - synchronized output の終端 `ESC [ ? 2026 l` をフレームの区切りとして報告する。
#[derive(Default)]
struct TctRewriter {
    state: CsiState,
    params: Vec<u8>,
    /// パラメータが長すぎて書き換えを諦めた CSI。そのまま素通しする。
    raw: bool,
}

impl TctRewriter {
    /// input を書き換えながら out へ積む。フレーム終端に達したらそこで打ち切る。
    /// 戻り値は (消費したバイト数, フレームが完成したか)。
    fn rewrite(&mut self, input: &[u8], out: &mut Vec<u8>) -> (usize, bool) {
        for (i, &byte) in input.iter().enumerate() {
            match self.state {
                CsiState::Text => {
                    out.push(byte);
                    if byte == ESC {
                        self.state = CsiState::Escape;
                    }
                }
                CsiState::Escape => {
                    out.push(byte);
                    self.state = match byte {
                        b'[' => CsiState::Csi,
                        ESC => CsiState::Escape,
                        _ => CsiState::Text,
                    };
                }
                // パラメータバイトと中間バイト。終端バイトまで貯めてから書き換える。
                CsiState::Csi if (0x20..=0x3f).contains(&byte) => {
                    if !self.raw && self.params.len() >= MAX_PARAMS {
                        out.append(&mut self.params);
                        self.raw = true;
                    }
                    if self.raw {
                        out.push(byte);
                    } else {
                        self.params.push(byte);
                    }
                }
                // 終端バイト、または CSI を中断する制御文字。
                CsiState::Csi => {
                    let frame_end = !self.raw && byte == b'l' && self.params == b"?2026";
                    if self.raw {
                        out.push(byte);
                    } else if byte == b'f' {
                        push_incremented(&self.params, out);
                        out.push(b'H');
                    } else {
                        out.extend_from_slice(&self.params);
                        out.push(byte);
                    }
                    self.params.clear();
                    self.raw = false;
                    self.state = if byte == ESC {
                        CsiState::Escape
                    } else {
                        CsiState::Text
                    };
                    if frame_end {
                        return (i + 1, true);
                    }
                }
            }
        }
        (input.len(), false)
    }
}

/// 数値パラメータを1つずつ +1 する。数値以外が混ざる列は触らない。
fn push_incremented(params: &[u8], out: &mut Vec<u8>) {
    if !params.iter().all(|b| b.is_ascii_digit() || *b == b';') {
        out.extend_from_slice(params);
        return;
    }
    for (i, part) in params.split(|b| *b == b';').enumerate() {
        if i > 0 {
            out.push(b';');
        }
        match std::str::from_utf8(part)
            .ok()
            .and_then(|s| s.parse::<u32>().ok())
        {
            Some(n) => out.extend_from_slice(n.saturating_add(1).to_string().as_bytes()),
            // 省略されたパラメータは既定値のままにする。
            None => out.extend_from_slice(part),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// tct が毎フレーム先頭に出す制御列 (カーソル非表示 / マウス追跡 / alternate screen / フレーム開始)。
    const TCT_PROLOGUE: &[u8] = b"\x1b[?25l\x1b[?1003h\x1b[?1049h\x1b[2J\x1b[?2026h";
    /// mpv が1フレームの終わりに出す synchronized output の終端。
    const FRAME_END: &[u8] = b"\x1b[?2026l";
    /// 映像トラックを入れ直したときに mpv が出す VO の終了と再初期化 (実測)。
    const TCT_RESTART: &[u8] =
        b"\x1b[?25h\x1b[?1003l\x1b[?1049l\x1b[?25l\x1b[?1003h\x1b[?1049h\x1b[2J";

    fn render(bytes: &[u8], width: u16, height: u16, area: Rect) -> Buffer {
        let screen = VideoScreen::new(width, height);
        screen.feed(bytes);
        let mut buf = Buffer::empty(Rect::new(0, 0, 20, 8));
        screen.render(area, &mut buf);
        buf
    }

    fn rewritten(chunks: &[&[u8]]) -> Vec<u8> {
        let mut rewriter = TctRewriter::default();
        let mut out = Vec::new();
        for chunk in chunks {
            let mut rest = *chunk;
            while !rest.is_empty() {
                let (used, _) = rewriter.rewrite(rest, &mut out);
                rest = &rest[used..];
            }
        }
        out
    }

    #[test]
    fn transfers_true_color_half_blocks() {
        let buf = render(
            b"\x1b[48;2;255;0;0m\x1b[38;2;0;0;255m\xe2\x96\x84",
            4,
            2,
            Rect::new(0, 0, 4, 2),
        );
        let cell = &buf[(0, 0)];
        assert_eq!(cell.symbol(), "▄");
        assert_eq!(cell.bg, Color::Rgb(255, 0, 0));
        assert_eq!(cell.fg, Color::Rgb(0, 0, 255));
    }

    #[test]
    fn control_sequences_do_not_leak_into_cells() {
        let buf = render(
            &[TCT_PROLOGUE, b"\x1b[48;2;1;2;3mA"].concat(),
            4,
            2,
            Rect::new(0, 0, 4, 2),
        );
        assert_eq!(buf[(0, 0)].symbol(), "A");
        assert_eq!(buf[(0, 0)].bg, Color::Rgb(1, 2, 3));
        assert_eq!(buf[(1, 0)].symbol(), " ");
    }

    #[test]
    fn mpv_row_addressing_keeps_every_row() {
        // mpv が実際に出す形: 0 始まりの座標を 1 始まりの HVP に載せてくる。
        let buf = render(
            &[TCT_PROLOGUE, b"\x1b[0;0fA\x1b[1;0fB\x1b[2;0fC"].concat(),
            6,
            3,
            Rect::new(0, 0, 6, 3),
        );
        // +1 して解釈するので、最上行から順に並び、行が重ならない。
        assert_eq!(buf[(0, 0)].symbol(), "A");
        assert_eq!(buf[(0, 1)].symbol(), "B");
        assert_eq!(buf[(0, 2)].symbol(), "C");
    }

    #[test]
    fn every_row_survives_the_newline_mpv_puts_at_the_end_of_a_frame() {
        // 実測 (mpv 0.41.0 --vo-tct-height=4) の形: 0 始まりの行を4行出し、最後に SGR リセットと改行。
        let bytes = [
            TCT_PROLOGUE,
            b"\x1b[0;0fA\x1b[1;0fB\x1b[2;0fC\x1b[3;0fD\x1b[0m\n",
            FRAME_END,
        ]
        .concat();
        let buf = render(&bytes, 4, 4, Rect::new(0, 0, 4, 4));
        // 最下行まで描かれ、かつ末尾の改行でスクロールして最上行が消えたりしない。
        assert_eq!(buf[(0, 0)].symbol(), "A");
        assert_eq!(buf[(0, 1)].symbol(), "B");
        assert_eq!(buf[(0, 2)].symbol(), "C");
        assert_eq!(buf[(0, 3)].symbol(), "D");
    }

    #[test]
    fn frames_after_the_first_are_not_shifted_by_the_trailing_newline() {
        let frame = |c: u8| {
            let mut bytes = b"\x1b[?2026h".to_vec();
            for row in 0..3u8 {
                bytes.extend_from_slice(format!("\x1b[{row};0f").as_bytes());
                bytes.push(c);
            }
            bytes.extend_from_slice(b"\x1b[0m\n");
            bytes.extend_from_slice(FRAME_END);
            bytes
        };
        let screen = VideoScreen::new(2, 3);
        screen.feed(TCT_PROLOGUE);
        for c in [b'A', b'B', b'C'] {
            assert!(screen.feed(&frame(c)));
        }
        let mut buf = Buffer::empty(Rect::new(0, 0, 2, 3));
        screen.render(Rect::new(0, 0, 2, 3), &mut buf);
        for row in 0..3 {
            assert_eq!(buf[(0, row)].symbol(), "C", "row {row}");
        }
    }

    #[test]
    fn horizontal_offset_is_not_shifted_left() {
        // 映像が横方向に中央寄せされると mpv は 0 始まりの桁 (実測で 2) を送ってくる。
        let buf = render(
            &[TCT_PROLOGUE, b"\x1b[0;2fX"].concat(),
            6,
            2,
            Rect::new(0, 0, 6, 2),
        );
        assert_eq!(buf[(2, 0)].symbol(), "X");
        assert_eq!(buf[(1, 0)].symbol(), " ");
    }

    #[test]
    fn video_is_placed_at_the_area_origin() {
        let buf = render(
            &[TCT_PROLOGUE, b"\x1b[0;1fX\x1b[2;3fY"].concat(),
            6,
            4,
            Rect::new(5, 2, 6, 4),
        );
        assert_eq!(buf[(6, 2)].symbol(), "X");
        assert_eq!(buf[(8, 4)].symbol(), "Y");
        // 領域の外は触らない。
        assert_eq!(buf[(0, 0)].symbol(), " ");
        assert_eq!(buf[(0, 0)].bg, Color::Reset);
    }

    #[test]
    fn clips_to_the_smaller_of_area_and_screen() {
        let buf = render(b"\x1b[41mABCDEFGH", 8, 1, Rect::new(0, 0, 3, 1));
        assert_eq!(buf[(0, 0)].symbol(), "A");
        assert_eq!(buf[(2, 0)].symbol(), "C");
        // 4 桁目以降は転写されないので Buffer の初期値のまま。
        assert_eq!(buf[(3, 0)].bg, Color::Reset);
    }

    #[test]
    fn maps_every_color_kind() {
        assert_eq!(convert_color(vt100::Color::Default), Color::Reset);
        assert_eq!(convert_color(vt100::Color::Idx(9)), Color::Indexed(9));
        assert_eq!(
            convert_color(vt100::Color::Rgb(10, 20, 30)),
            Color::Rgb(10, 20, 30)
        );
    }

    #[test]
    fn indexed_colors_survive_the_round_trip() {
        let buf = render(b"\x1b[31;46mZ", 2, 1, Rect::new(0, 0, 2, 1));
        assert_eq!(buf[(0, 0)].fg, Color::Indexed(1));
        assert_eq!(buf[(0, 0)].bg, Color::Indexed(6));
    }

    #[test]
    fn size_reports_columns_then_rows() {
        assert_eq!(VideoScreen::new(64, 20).size(), (64, 20));
    }

    #[test]
    fn hvp_becomes_cup_with_one_based_parameters() {
        assert_eq!(rewritten(&[b"\x1b[12;34f"]), b"\x1b[13;35H");
        assert_eq!(
            rewritten(&[b"\x1b[0;4fx\x1b[1;4fy"]),
            b"\x1b[1;5Hx\x1b[2;5Hy"
        );
        // 桁上がりで長さが変わっても壊れない。
        assert_eq!(rewritten(&[b"\x1b[9;99f"]), b"\x1b[10;100H");
        // 省略されたパラメータは既定値のまま。
        assert_eq!(rewritten(&[b"\x1b[f"]), b"\x1b[H");
        // 本文中の f、SGR、プライベートモードは素通し。
        assert_eq!(
            rewritten(&[b"off\x1b[38;2;1;2;3m\x1b[?1049h"]).as_slice(),
            b"off\x1b[38;2;1;2;3m\x1b[?1049h"
        );
    }

    #[test]
    fn hvp_split_across_reads_is_still_rewritten() {
        assert_eq!(rewritten(&[b"\x1b[12;", b"34f"]), b"\x1b[13;35H");
        assert_eq!(rewritten(&[b"\x1b", b"[1;1f"]), b"\x1b[2;2H");
        assert_eq!(rewritten(&[b"\x1b[1;1", b"f"]), b"\x1b[2;2H");
    }

    #[test]
    fn overlong_parameters_are_passed_through_untouched() {
        let long = b"\x1b[1;2;3;4;5;6;7;8;9;10;11;12;13;14;15;16;17;18f";
        assert_eq!(rewritten(&[long]), long);
    }

    #[test]
    fn rewrite_stops_at_the_frame_boundary() {
        let mut rewriter = TctRewriter::default();
        let mut out = Vec::new();
        let input = b"A\x1b[?2026lB";
        let (used, frame_end) = rewriter.rewrite(input, &mut out);
        assert!(frame_end);
        assert_eq!(used, input.len() - 1);
        assert_eq!(out, b"A\x1b[?2026l");
        out.clear();
        let (used, frame_end) = rewriter.rewrite(&input[used..], &mut out);
        assert!(!frame_end);
        assert_eq!((used, out.as_slice()), (1, b"B".as_slice()));
    }

    #[test]
    fn feed_reports_only_completed_frames() {
        let screen = VideoScreen::new(4, 1);
        assert!(!screen.feed(&[TCT_PROLOGUE, b"\x1b[0;0fAB"].concat()));
        assert!(screen.feed(FRAME_END));
    }

    #[test]
    fn half_written_frames_are_not_rendered() {
        let screen = VideoScreen::new(4, 1);
        let mut buf = Buffer::empty(Rect::new(0, 0, 4, 1));
        // 1枚目が完成するまでは生画面を描く (synchronized output 非対応の mpv 向けの退避路)。
        screen.feed(&[TCT_PROLOGUE, b"\x1b[0;0fAAAA", FRAME_END].concat());
        screen.render(Rect::new(0, 0, 4, 1), &mut buf);
        assert_eq!(buf[(0, 0)].symbol(), "A");

        // 2枚目を途中まで流しても、描かれるのは完成済みの1枚目のまま。
        screen.feed(b"\x1b[?2026h\x1b[0;0fBB");
        screen.render(Rect::new(0, 0, 4, 1), &mut buf);
        assert_eq!(buf[(0, 0)].symbol(), "A");
        assert_eq!(buf[(3, 0)].symbol(), "A");

        screen.feed(&[b"BB", FRAME_END].concat());
        screen.render(Rect::new(0, 0, 4, 1), &mut buf);
        assert_eq!(buf[(0, 0)].symbol(), "B");
        assert_eq!(buf[(3, 0)].symbol(), "B");
    }

    #[test]
    fn several_frames_in_one_read_are_all_parsed() {
        let screen = VideoScreen::new(4, 1);
        let bytes = [
            TCT_PROLOGUE,
            b"\x1b[0;0fAAAA",
            FRAME_END,
            b"\x1b[?2026h\x1b[0;0fCCCC",
            FRAME_END,
        ]
        .concat();
        assert!(screen.feed(&bytes));
        let mut buf = Buffer::empty(Rect::new(0, 0, 4, 1));
        screen.render(Rect::new(0, 0, 4, 1), &mut buf);
        assert_eq!(buf[(0, 0)].symbol(), "C");
    }

    #[test]
    fn redraw_requests_are_collapsed_until_cleared() {
        let screen = VideoScreen::new(4, 1);
        assert!(screen.request_redraw());
        assert!(!screen.request_redraw());
        screen.clear_redraw();
        assert!(screen.request_redraw());
    }

    #[test]
    fn resize_changes_the_grid_and_drops_the_stale_frame() {
        let screen = VideoScreen::new(4, 2);
        screen.feed(&[TCT_PROLOGUE, b"\x1b[0;0fAAAA", FRAME_END].concat());
        screen.resize(8, 3);
        assert_eq!(screen.size(), (8, 3));

        // 作り直しの途中は、古い寸法のまま届くフレームを描かない。
        screen.feed(b"\x1b[?2026h\x1b[0;0fBBBB");
        let mut buf = Buffer::empty(Rect::new(0, 0, 8, 3));
        buf.set_string(0, 0, "........", ratatui::style::Style::default());
        screen.render(Rect::new(0, 0, 8, 3), &mut buf);
        assert_eq!(buf[(0, 0)].symbol(), ".");

        // mpv は VO を作り直すとき alternate screen をやり直して画面を消す (実測)。
        screen.feed(&[TCT_RESTART, b"\x1b[?2026h\x1b[2;7fZ", FRAME_END].concat());
        screen.render(Rect::new(0, 0, 8, 3), &mut buf);
        assert_eq!(buf[(7, 2)].symbol(), "Z");
        assert_eq!(buf[(0, 0)].symbol(), " ");
    }

    #[test]
    fn resize_to_the_same_size_keeps_the_current_frame() {
        let screen = VideoScreen::new(4, 1);
        screen.feed(&[TCT_PROLOGUE, b"\x1b[0;0fAAAA", FRAME_END].concat());
        screen.resize(4, 1);
        let mut buf = Buffer::empty(Rect::new(0, 0, 4, 1));
        screen.render(Rect::new(0, 0, 4, 1), &mut buf);
        assert_eq!(buf[(0, 0)].symbol(), "A");
    }

    #[test]
    fn feeding_in_chunks_matches_a_single_feed() {
        let bytes = b"\x1b[1;1f\x1b[48;2;9;8;7m\xe2\x96\x84\x1b[48;2;1;1;1m\xe2\x96\x84";
        let whole = render(bytes, 4, 2, Rect::new(0, 0, 4, 2));
        let split = {
            let screen = VideoScreen::new(4, 2);
            // ANSI 列と UTF-8 の途中で切れても状態が壊れないこと。
            let (head, tail) = bytes.split_at(13);
            screen.feed(head);
            screen.feed(tail);
            let mut buf = Buffer::empty(Rect::new(0, 0, 20, 8));
            screen.render(Rect::new(0, 0, 4, 2), &mut buf);
            buf
        };
        assert_eq!(whole[(1, 1)].bg, Color::Rgb(9, 8, 7));
        assert_eq!(split[(1, 1)], whole[(1, 1)]);
        assert_eq!(split[(2, 1)], whole[(2, 1)]);
    }
}
