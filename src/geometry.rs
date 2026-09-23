//! 端末への寸法問い合わせと、その答えから映像の寸法を決める計算。

use crate::screen::playing;
use crate::video::{self, CellSize, Geometry};
use ratatui::layout::Rect;

/// 端末に今のサイズを聞いて映像寸法を決める。予算は設定の画質から決まる。
pub fn video_geometry(max_pixels: u32) -> Geometry {
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    geometry_for(cols, rows, cell_size(), max_pixels)
}

/// ピクセルを報告しない端末では既定のセル寸法で進める (画像が小さめに出るだけ)。
pub fn cell_size() -> CellSize {
    crossterm::terminal::window_size()
        .ok()
        .and_then(|size| video::cell_size(size.columns, size.rows, size.width, size.height))
        .unwrap_or(video::FALLBACK_CELL)
}

/// mpv に渡す寸法は ratatui の映像領域と一致していなければならない。
pub fn geometry_for(cols: u16, rows: u16, cell: CellSize, max_pixels: u32) -> Geometry {
    Geometry::new(
        playing::video_area(Rect::new(0, 0, cols, rows)),
        cell,
        max_pixels,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const CELL: CellSize = CellSize {
        width_px: 8,
        height_px: 16,
    };

    #[test]
    fn geometry_matches_the_video_area_of_the_same_terminal_size() {
        let geometry = geometry_for(80, 24, CELL, video::MAX_FRAME_PIXELS);
        assert_eq!(geometry.area, playing::video_area(Rect::new(0, 0, 80, 24)));
        assert_eq!(geometry.cell, CELL);
        // 映像領域 80x20 セル × 8x16 px。予算 640*360 に収まるので縮まない。
        assert_eq!(geometry.frame_px, (640, 320));
    }

    #[test]
    fn oversized_terminals_shrink_to_the_pixel_budget() {
        let geometry = geometry_for(200, 60, CELL, video::MAX_FRAME_PIXELS);
        let pixels = u64::from(geometry.frame_px.0) * u64::from(geometry.frame_px.1);
        assert!(pixels <= u64::from(video::MAX_FRAME_PIXELS), "{pixels} px");
        // セル領域は縮めない。縮めるのはフレームのピクセル数だけ。
        assert_eq!(geometry.area, playing::video_area(Rect::new(0, 0, 200, 60)));
    }

    #[test]
    fn a_smaller_budget_shrinks_the_frame_further() {
        let low = geometry_for(80, 24, CELL, crate::display::Quality::Low.max_pixels());
        let pixels = u64::from(low.frame_px.0) * u64::from(low.frame_px.1);
        assert!(
            pixels <= u64::from(crate::display::Quality::Low.max_pixels()),
            "{pixels} px"
        );
        // セル領域は予算で変わらない。
        assert_eq!(low.area, playing::video_area(Rect::new(0, 0, 80, 24)));
    }

    #[test]
    fn a_terminal_too_small_for_the_video_area_still_yields_a_frame() {
        let geometry = geometry_for(1, 1, CELL, video::MAX_FRAME_PIXELS);
        assert!(geometry.frame_px.0 >= 1 && geometry.frame_px.1 >= 1);
    }
}
