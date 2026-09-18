//! 検索結果を格子に割り付ける。すべて整数演算で、端末に触らない。

use crate::video::CellSize;
use ratatui::layout::Rect;
use ratatui::text::Span;

/// 1 列に要る最低の桁数。これで割った数が列数の上限になる。
const MIN_CELL_WIDTH: u16 = 16;
/// 列数の上限。広い端末で際限なく細かくしない。
const MAX_COLUMNS: usize = 6;
/// 画像の最低行数。これを割ると何の動画か分からない。
const MIN_IMAGE_ROWS: u16 = 2;
/// 画像の下に積む タイトル / 時間・投稿者 / 下余白 の 3 行。
const CELL_EXTRA_ROWS: u16 = 3;
/// セルの右に置く余白。
const CELL_RIGHT_MARGIN: u16 = 1;
/// 画像のアスペクト比 (16:9)。
const ASPECT_W: u32 = 16;
const ASPECT_H: u32 = 9;

/// 検索結果の見せ方。Kitty graphics protocol 非対応の端末は list にする。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LayoutMode {
    #[default]
    Grid,
    List,
}

impl LayoutMode {
    pub fn key(self) -> &'static str {
        match self {
            Self::Grid => "grid",
            Self::List => "list",
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        [Self::Grid, Self::List]
            .into_iter()
            .find(|mode| mode.key() == key)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellRect {
    pub image: Rect,
    pub title: Rect,
    pub meta: Rect,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    pub columns: usize,
    pub rows: usize,
    pub offset: usize,
    /// デコード目標。画像矩形のピクセル寸法。
    pub image_px: (u32, u32),
    /// 可視ぶんだけ。cells[i] は結果 offset+i。
    pub cells: Vec<CellRect>,
}

/// 格子を組めない (狭すぎる) ときは None。呼び出し側はリスト表示へ落とす。
pub fn layout(inner: Rect, cell: CellSize, count: usize, scroll: usize) -> Option<Layout> {
    if inner.width == 0 || inner.height == 0 {
        return None;
    }
    let columns = usize::from(inner.width / MIN_CELL_WIDTH).clamp(1, MAX_COLUMNS);
    let cell_width = inner.width / columns as u16;
    if cell_width <= CELL_RIGHT_MARGIN {
        return None;
    }
    let content_width = cell_width - CELL_RIGHT_MARGIN;
    let image_rows = image_rows(content_width, cell);
    let cell_height = image_rows + CELL_EXTRA_ROWS;
    let rows = usize::from(inner.height / cell_height);
    if rows == 0 {
        return None;
    }
    let offset = scroll / columns * columns;
    let mut cells = Vec::new();
    for row in 0..rows {
        for column in 0..columns {
            if offset + row * columns + column >= count {
                break;
            }
            let x = inner.x + column as u16 * cell_width;
            let y = inner.y + row as u16 * cell_height;
            cells.push(CellRect {
                image: Rect::new(x, y, content_width, image_rows),
                title: Rect::new(x, y + image_rows, content_width, 1),
                meta: Rect::new(x, y + image_rows + 1, content_width, 1),
            });
        }
    }
    Some(Layout {
        columns,
        rows,
        offset,
        image_px: (
            u32::from(content_width) * u32::from(cell.width_px),
            u32::from(image_rows) * u32::from(cell.height_px),
        ),
        cells,
    })
}

/// 画像の桁数と端末のセル寸法から、16:9 に最も近い行数を出す。
/// 端末が異常なセル寸法を報告しても cell_height の加算が溢れない値に収める。
fn image_rows(content_width: u16, cell: CellSize) -> u16 {
    let numerator = u32::from(content_width) * u32::from(cell.width_px.max(1)) * ASPECT_H;
    let denominator = ASPECT_W * u32::from(cell.height_px.max(1));
    let rounded = (numerator + denominator / 2) / denominator;
    u16::try_from(rounded)
        .unwrap_or(u16::MAX)
        .clamp(MIN_IMAGE_ROWS, u16::MAX - CELL_EXTRA_ROWS)
}

/// 格子の中の移動。2 次元では戻り先が直感に合わないので巻き戻さない。
pub fn move_selection(selected: usize, count: usize, columns: usize, dir: Dir) -> usize {
    if count == 0 || columns == 0 {
        return selected;
    }
    let last = count - 1;
    match dir {
        Dir::Right => selected.saturating_add(1).min(last),
        Dir::Left => selected.saturating_sub(1),
        Dir::Down => selected.saturating_add(columns).min(last),
        // はみ出す場合は動かない。
        Dir::Up => selected.checked_sub(columns).unwrap_or(selected),
    }
}

/// 選択が可視範囲から出たときだけ先頭表示位置を動かす。戻り値は列数で丸めた位置。
pub fn ensure_visible(selected: usize, columns: usize, rows: usize, scroll: usize) -> usize {
    if columns == 0 || rows == 0 {
        return scroll;
    }
    let offset = scroll / columns * columns;
    let page = rows * columns;
    if selected < offset {
        return selected / columns * columns;
    }
    if selected >= offset + page {
        return (selected / columns + 1 - rows) * columns;
    }
    offset
}

/// 表示幅で切り詰める。切ったときは末尾に "…"。
pub fn truncate(text: &str, width: usize) -> String {
    if display_width(text) <= width {
        return text.to_string();
    }
    if width == 0 {
        return String::new();
    }
    // "…" ぶんの 1 桁を残す。
    let budget = width - 1;
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let w = display_width(&ch.to_string());
        if used + w > budget {
            break;
        }
        out.push(ch);
        used += w;
    }
    out.push('…');
    out
}

/// 全角はセル幅 2。既存の ui::cursor_x と同じ数え方。
pub fn display_width(text: &str) -> usize {
    Span::raw(text).width()
}

#[cfg(test)]
mod tests {
    use super::*;

    const CELL: CellSize = CellSize {
        width_px: 8,
        height_px: 16,
    };

    /// 80x24 の端末の結果ブロックの内側 (枠 1 桁ぶん狭い)。
    fn inner_80x24() -> Rect {
        Rect::new(1, 4, 78, 16)
    }

    #[test]
    fn layout_of_an_80x24_terminal_is_4_columns_by_2_rows() {
        let layout = layout(inner_80x24(), CELL, 10, 0).expect("格子を組める");
        assert_eq!(layout.columns, 4);
        assert_eq!(layout.rows, 2);
        assert_eq!(layout.offset, 0);
        assert_eq!(layout.image_px, (144, 80));
        assert_eq!(layout.cells.len(), 8);

        let first = layout.cells[0];
        assert_eq!(first.image, Rect::new(1, 4, 18, 5));
        assert_eq!(first.title, Rect::new(1, 9, 18, 1));
        assert_eq!(first.meta, Rect::new(1, 10, 18, 1));
        // 2 行目の先頭はセル高 8 行ぶん下。
        assert_eq!(layout.cells[4].image, Rect::new(1, 12, 18, 5));
    }

    #[test]
    fn an_absurd_cell_size_does_not_overflow_the_cell_height() {
        // 端末が異常なピクセル寸法 (xpixel/ypixel) を報告しても割り付けで溢れない。
        let cell = CellSize {
            width_px: 64,
            height_px: 1,
        };
        let layout = layout(Rect::new(0, 0, u16::MAX, u16::MAX), cell, 4, 0).expect("格子を組める");
        assert_eq!(layout.rows, 1);
        for cell in &layout.cells {
            assert!(cell.image.height <= u16::MAX - CELL_EXTRA_ROWS);
            // 行が巻き戻ると下段が画像の上に来る。
            assert!(cell.title.y > cell.image.y);
            assert!(cell.meta.y > cell.title.y);
        }
    }

    #[test]
    fn layout_cells_never_overlap_and_stay_inside_the_inner_area() {
        for (width, height) in [(78, 16), (18, 40), (118, 38), (40, 9), (200, 60)] {
            let inner = Rect::new(2, 3, width, height);
            let Some(layout) = layout(inner, CELL, 60, 0) else {
                continue;
            };
            let mut seen: Vec<Rect> = Vec::new();
            for cell in &layout.cells {
                for rect in [cell.image, cell.title, cell.meta] {
                    assert!(rect.x >= inner.x, "{rect:?} / {inner:?}");
                    assert!(rect.y >= inner.y, "{rect:?} / {inner:?}");
                    assert!(rect.right() <= inner.right(), "{rect:?} / {inner:?}");
                    assert!(rect.bottom() <= inner.bottom(), "{rect:?} / {inner:?}");
                    assert!(
                        seen.iter()
                            .all(|other| other.intersection(rect).area() == 0),
                        "{rect:?} が他のセルと重なっている"
                    );
                    seen.push(rect);
                }
            }
        }
    }

    #[test]
    fn layout_caps_the_column_count_on_a_wide_terminal() {
        let layout = layout(Rect::new(0, 0, 300, 40), CELL, 100, 0).expect("格子を組める");
        assert_eq!(layout.columns, MAX_COLUMNS);
    }

    #[test]
    fn layout_is_none_when_one_cell_row_does_not_fit() {
        assert_eq!(layout(Rect::new(0, 0, 78, 4), CELL, 10, 0), None);
        assert_eq!(layout(Rect::new(0, 0, 0, 16), CELL, 10, 0), None);
        assert_eq!(layout(Rect::new(0, 0, 78, 0), CELL, 10, 0), None);
        // 1 桁では画像も右余白も置けない。
        assert_eq!(layout(Rect::new(0, 0, 1, 16), CELL, 10, 0), None);
    }

    #[test]
    fn layout_image_rows_follow_the_cell_aspect_ratio() {
        let tall = layout(inner_80x24(), CELL, 10, 0).expect("格子を組める");
        let square_cell = CellSize {
            width_px: 8,
            height_px: 8,
        };
        let square = layout(Rect::new(1, 4, 78, 40), square_cell, 10, 0).expect("格子を組める");
        assert_eq!(tall.image_px, (144, 80));
        assert_eq!(square.image_px, (144, 80));
        // セルが正方形に近いほど、同じ縦横比を出すのに行数が要る。
        assert_eq!(tall.cells[0].image.height, 5);
        assert_eq!(square.cells[0].image.height, 10);
    }

    #[test]
    fn layout_offset_is_aligned_to_the_column_count() {
        let layout = layout(inner_80x24(), CELL, 20, 5).expect("格子を組める");
        assert_eq!(layout.columns, 4);
        assert_eq!(layout.offset, 4);
        assert_eq!(layout.cells.len(), 8);
    }

    #[test]
    fn layout_shows_only_the_remaining_items_on_the_last_page() {
        let layout = layout(inner_80x24(), CELL, 10, 8).expect("格子を組める");
        assert_eq!(layout.offset, 8);
        assert_eq!(layout.cells.len(), 2);
    }

    #[test]
    fn right_and_left_move_by_one_and_stop_at_the_ends() {
        assert_eq!(move_selection(0, 10, 4, Dir::Right), 1);
        assert_eq!(move_selection(9, 10, 4, Dir::Right), 9);
        assert_eq!(move_selection(1, 10, 4, Dir::Left), 0);
        assert_eq!(move_selection(0, 10, 4, Dir::Left), 0);
    }

    #[test]
    fn down_and_up_move_by_a_full_row() {
        assert_eq!(move_selection(0, 10, 4, Dir::Down), 4);
        assert_eq!(move_selection(4, 10, 4, Dir::Up), 0);
        // 上にはみ出すときは動かない。
        assert_eq!(move_selection(3, 10, 4, Dir::Up), 3);
    }

    #[test]
    fn down_from_a_partial_last_row_lands_on_the_last_item() {
        // 10 件・4 列。2 行目の末尾 (7) から下は 11 で存在しないので 9 に寄せる。
        assert_eq!(move_selection(7, 10, 4, Dir::Down), 9);
        assert_eq!(move_selection(9, 10, 4, Dir::Down), 9);
    }

    #[test]
    fn move_selection_is_noop_without_results() {
        for dir in [Dir::Left, Dir::Right, Dir::Up, Dir::Down] {
            assert_eq!(move_selection(0, 0, 4, dir), 0, "{dir:?}");
            assert_eq!(move_selection(3, 10, 0, dir), 3, "{dir:?}");
        }
    }

    #[test]
    fn scroll_follows_the_selection_out_of_the_bottom_row() {
        assert_eq!(ensure_visible(8, 4, 2, 0), 4);
        assert_eq!(ensure_visible(12, 4, 2, 0), 8);
    }

    #[test]
    fn scroll_returns_to_zero_when_the_selection_goes_back_up() {
        assert_eq!(ensure_visible(0, 4, 2, 4), 0);
        assert_eq!(ensure_visible(3, 4, 2, 8), 0);
    }

    #[test]
    fn scroll_stays_at_zero_when_everything_fits() {
        for selected in 0..8 {
            assert_eq!(ensure_visible(selected, 4, 2, 0), 0, "{selected}");
        }
        // 列数で丸めた位置を返す。
        assert_eq!(ensure_visible(5, 4, 2, 5), 4);
        assert_eq!(ensure_visible(0, 0, 2, 7), 7);
        assert_eq!(ensure_visible(0, 4, 0, 7), 7);
    }

    #[test]
    fn truncate_counts_display_width_not_chars() {
        assert_eq!(truncate("ラーメン", 5), "ラー…");
        assert_eq!(truncate("abcdef", 4), "abc…");
        // 収まるものはそのまま。
        assert_eq!(truncate("ラーメン", 8), "ラーメン");
        assert_eq!(truncate("abc", 8), "abc");
    }

    #[test]
    fn truncate_never_splits_a_wide_char_in_half() {
        // 幅 4 で "ラ"(2) の次は 2 桁要るので入らない。
        assert_eq!(truncate("ラーメン", 4), "ラ…");
        assert_eq!(display_width(&truncate("ラーメン", 4)), 3);
        assert_eq!(truncate("ラーメン", 1), "…");
        assert_eq!(truncate("ラーメン", 0), "");
    }

    #[test]
    fn layout_mode_round_trips_through_its_key() {
        for mode in [LayoutMode::Grid, LayoutMode::List] {
            assert_eq!(LayoutMode::from_key(mode.key()), Some(mode));
        }
        assert_eq!(LayoutMode::from_key("Grid"), None);
        assert_eq!(LayoutMode::default(), LayoutMode::Grid);
    }
}
