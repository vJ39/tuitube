//! シークバーの純粋な部分。割り付け・当たり判定・列⇄秒・状態機械・描画。
//! crossterm / tokio / mpv には依存しない。

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::widgets::Widget;

pub const FILLED: &str = "█";
pub const EMPTY: &str = "░";
pub const MARKER: &str = "┃";
/// トラックとラベルの間の空白。
pub const LABEL_GAP: u16 = 2;
/// duration ちょうどへ飛ぶと mpv が終了するため、末尾に残す秒数。
pub const SEEK_END_MARGIN_SECS: f64 = 1.0;

/// バー 1 行の割り付け。左がトラック、右が固定幅ラベル。幅が足りなければトラック幅 0。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SeekBarLayout {
    pub track: Rect,
    pub label: Rect,
}

impl SeekBarLayout {
    /// row は screen::playing::seek_bar_area() の 1 行。label_width は label_width(duration)。
    pub fn new(row: Rect, label_width: u16) -> Self {
        let reserved = label_width.saturating_add(LABEL_GAP);
        if row.width <= reserved {
            // ラベルすら入らない幅ではトラックを諦め、ラベルを右端で切る。
            return Self {
                track: Rect { width: 0, ..row },
                label: row,
            };
        }
        let track_width = row.width - reserved;
        Self {
            track: Rect {
                width: track_width,
                ..row
            },
            label: Rect {
                x: row.x + track_width + LABEL_GAP,
                width: label_width,
                ..row
            },
        }
    }

    /// (column, row) がトラック上なら Some(column)。ラベル・空白・他の行は None。
    pub fn hit(&self, column: u16, row: u16) -> Option<u16> {
        let on_track = self.track.width > 0
            && row >= self.track.y
            && row < self.track.bottom()
            && column >= self.track.x
            && column < self.track.right();
        on_track.then_some(column)
    }

    /// ドラッグ中にトラック外へ出た列を端に吸着させる。トラック幅 0 なら track.x。
    pub fn clamp_column(&self, column: u16) -> u16 {
        if self.track.width == 0 {
            return self.track.x;
        }
        column.clamp(self.track.x, self.track.right() - 1)
    }

    /// 列の左端に対応する秒。幅 0 なら 0.0。
    /// duration 到着やリサイズで割り付けが変わると、保持していたホバー/ドラッグ列が
    /// トラック外に残るので、換算前にトラック内へ吸着させる。
    pub fn seconds_at(&self, column: u16, duration: f64) -> f64 {
        if self.track.width == 0 {
            return 0.0;
        }
        let offset = f64::from(self.clamp_column(column) - self.track.x);
        // 乗算を先にすると、割り切れる位置の秒が f64 でも正確に出る。
        offset * duration / f64::from(self.track.width)
    }

    /// 充填セル数。どちらかが None、または duration <= 0 なら 0。
    pub fn filled_cells(&self, time_pos: Option<f64>, duration: Option<f64>) -> u16 {
        let (Some(time_pos), Some(duration)) = (time_pos, duration) else {
            return 0;
        };
        if duration <= 0.0 || self.track.width == 0 {
            return 0;
        }
        if time_pos >= duration {
            return self.track.width;
        }
        let width = f64::from(self.track.width);
        (time_pos.max(0.0) / duration * width).floor() as u16
    }
}

/// 送信直前の丸め。
pub fn clamp_target(target: f64, duration: Option<f64>) -> f64 {
    match duration {
        Some(duration) => target.clamp(0.0, (duration - SEEK_END_MARGIN_SECS).max(0.0)),
        None => target.max(0.0),
    }
}

/// ラベル幅。duration だけで決まり、再生中に変わらない。
pub fn label_width(duration: Option<f64>) -> u16 {
    let width = time_text(duration, has_hours(duration)).chars().count();
    (width * 2 + 3).min(u16::MAX as usize) as u16
}

/// "01:23 / 34:05"。shown は duration と同じ桁形式に揃える。
pub fn label_text(shown: Option<f64>, duration: Option<f64>) -> String {
    let hours = has_hours(duration);
    let right = time_text(duration, hours);
    // 位置が未取得のときは duration と同じ桁数の伏せ字にする ("1:01:01" なら "-:--:--")。
    let left = match shown {
        Some(shown) => time_text(Some(shown), hours),
        None => right
            .chars()
            .map(|c| if c.is_ascii_digit() { '-' } else { c })
            .collect(),
    };
    let pad = right.chars().count().saturating_sub(left.chars().count());
    format!("{:pad$}{left} / {right}", "")
}

/// duration が不明な動画 (ライブ・音声のみ) は再生位置が 1 時間を越えうるので、
/// 時間桁ぶんを確保しておく。
fn has_hours(duration: Option<f64>) -> bool {
    match duration {
        Some(duration) => duration.max(0.0).round() as u64 >= 3600,
        None => true,
    }
}

/// duration に時間桁があれば、再生位置も "0:01:15" の形に揃えて幅を固定する。
/// 時間桁が無いときは、末尾で再生位置が 1 時間を越えても分へ繰り上げて "60:00" と出す。
/// 桁を繰り上げるとラベルが予約幅からはみ出し、右端の duration が切れる。
fn time_text(seconds: Option<f64>, hours: bool) -> String {
    let Some(seconds) = seconds else {
        return if hours { "--:--:--" } else { "--:--" }.to_string();
    };
    let total = seconds.max(0.0).round() as u64;
    if hours {
        format!(
            "{}:{:02}:{:02}",
            total / 3600,
            (total % 3600) / 60,
            total % 60
        )
    } else {
        format!("{:02}:{:02}", total / 60, total % 60)
    }
}

/// crossterm の MouseEventKind から、この機能が見る分だけを取り出したもの。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseInput {
    Move,
    Press,
    Drag,
    Release,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseAction {
    Seek { column: u16 },
}

/// ホバー列とドラッグ列。どちらも画面絶対列。
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
pub struct SeekBarState {
    pub hover: Option<u16>,
    pub drag: Option<u16>,
}

impl SeekBarState {
    /// 状態遷移。シークすべきときだけ Some。
    pub fn on_mouse(
        &mut self,
        input: MouseInput,
        column: u16,
        row: u16,
        layout: &SeekBarLayout,
    ) -> Option<MouseAction> {
        match input {
            MouseInput::Move => {
                // ボタンが離れているのに Up が届かなかった (ウィンドウ外で離した)。
                // どこで離したか分からないのでシークせず取り消す。
                self.drag = None;
                self.hover = layout.hit(column, row);
                None
            }
            MouseInput::Press => {
                if let Some(column) = layout.hit(column, row) {
                    self.drag = Some(column);
                }
                None
            }
            MouseInput::Drag => {
                // 行から外れても続ける。列だけトラック端へ吸着させる。
                if self.drag.is_some() {
                    self.drag = Some(layout.clamp_column(column));
                }
                None
            }
            MouseInput::Release => {
                self.drag.take()?;
                self.hover = layout.hit(column, row);
                Some(MouseAction::Seek {
                    column: layout.clamp_column(column),
                })
            }
        }
    }

    /// ラベルと印に使う列。drag があれば drag、無ければ hover。
    pub fn shown_column(&self) -> Option<u16> {
        self.drag.or(self.hover)
    }

    /// キーでシークしたときに呼ぶ。drag は触らない。
    pub fn clear_hover(&mut self) {
        self.hover = None;
    }
}

/// 描画。screen/playing.rs が App から組み立てる。
pub struct SeekBar<'a> {
    pub layout: SeekBarLayout,
    pub filled: u16,
    pub marker: Option<u16>,
    pub label: &'a str,
    /// ホバー/ドラッグ中はラベルの色を変える。
    pub highlighted: bool,
}

impl Widget for SeekBar<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let track = self.layout.track.intersection(area);
        for x in track.x..track.right() {
            let symbol = if Some(x) == self.marker {
                MARKER
            } else if x < track.x.saturating_add(self.filled) {
                FILLED
            } else {
                EMPTY
            };
            let color = if Some(x) == self.marker {
                Color::Yellow
            } else {
                Color::Cyan
            };
            if let Some(cell) = buf.cell_mut((x, track.y)) {
                cell.set_symbol(symbol)
                    .set_style(Style::default().fg(color));
            }
        }

        let label = self.layout.label.intersection(area);
        let color = if self.highlighted {
            Color::Yellow
        } else {
            Color::Cyan
        };
        buf.set_stringn(
            label.x,
            label.y,
            self.label,
            label.width as usize,
            Style::default().fg(color),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 80x24 の端末でバー行は y=21、duration 650 秒。トラック 65 セル = 1 セル 10 秒。
    fn layout() -> SeekBarLayout {
        SeekBarLayout::new(Rect::new(0, 21, 80, 1), 13)
    }

    fn row_symbols(buf: &Buffer, y: u16) -> String {
        (buf.area.x..buf.area.right())
            .map(|x| buf.cell((x, y)).expect("バッファ内").symbol())
            .collect()
    }

    #[test]
    fn seek_bar_layout_reserves_a_fixed_label_on_the_right() {
        let layout = layout();
        assert_eq!(layout.track, Rect::new(0, 21, 65, 1));
        assert_eq!(layout.label, Rect::new(67, 21, 13, 1));

        // ラベルすら入らない幅ではトラックを諦める。
        let narrow = SeekBarLayout::new(Rect::new(0, 0, 10, 1), 13);
        assert_eq!(narrow.track.width, 0);
        assert_eq!(narrow.label, Rect::new(0, 0, 10, 1));
    }

    #[test]
    fn columns_on_the_track_map_to_seconds_from_the_left_edge() {
        let layout = layout();
        assert_eq!(layout.hit(0, 21), Some(0));
        assert_eq!(layout.hit(64, 21), Some(64));
        assert_eq!(layout.hit(65, 21), None);
        assert_eq!(layout.hit(70, 21), None);
        assert_eq!(layout.hit(10, 20), None);

        assert_eq!(layout.seconds_at(0, 650.0), 0.0);
        assert_eq!(layout.seconds_at(13, 650.0), 130.0);
        assert_eq!(layout.seconds_at(64, 650.0), 640.0);

        assert_eq!(layout.clamp_column(0), 0);
        assert_eq!(layout.clamp_column(70), 64);
        assert_eq!(layout.clamp_column(200), 64);
    }

    #[test]
    fn seconds_never_run_past_the_track_of_the_current_layout() {
        // duration が届くとラベルが 13→17 桁になりトラックが 4 セル縮む。
        // 縮む前の列を持ったままでも duration を越える時刻は出さない。
        let narrowed = SeekBarLayout::new(Rect::new(0, 21, 80, 1), 17);
        assert_eq!(narrowed.track, Rect::new(0, 21, 61, 1));
        assert_eq!(
            narrowed.seconds_at(64, 4000.0),
            narrowed.seconds_at(60, 4000.0)
        );
        assert!(narrowed.seconds_at(64, 4000.0) < 4000.0);

        let layout = layout();
        assert_eq!(layout.seconds_at(200, 650.0), 640.0);
        assert_eq!(
            SeekBarLayout::new(Rect::new(0, 0, 10, 1), 13).seconds_at(5, 650.0),
            0.0
        );
    }

    #[test]
    fn filled_cells_follow_the_ratio_and_saturate() {
        let layout = layout();
        assert_eq!(layout.filled_cells(Some(0.0), Some(650.0)), 0);
        assert_eq!(layout.filled_cells(Some(325.0), Some(650.0)), 32);
        assert_eq!(layout.filled_cells(Some(650.0), Some(650.0)), 65);
        assert_eq!(layout.filled_cells(Some(700.0), Some(650.0)), 65);
        assert_eq!(layout.filled_cells(None, Some(650.0)), 0);
        assert_eq!(layout.filled_cells(Some(10.0), None), 0);
        assert_eq!(layout.filled_cells(Some(10.0), Some(0.0)), 0);
    }

    #[test]
    fn clamp_target_keeps_the_seek_inside_the_file() {
        // 負値は mpv では「末尾から」の意味になり、duration ちょうどでは mpv が終了する。
        assert_eq!(clamp_target(-3.0, Some(100.0)), 0.0);
        assert_eq!(clamp_target(99.9, Some(100.0)), 99.0);
        assert_eq!(clamp_target(50.0, Some(100.0)), 50.0);
        assert_eq!(clamp_target(0.3, Some(0.5)), 0.0);
        assert_eq!(clamp_target(50.0, None), 50.0);
        assert_eq!(clamp_target(-1.0, None), 0.0);
    }

    #[test]
    fn hover_follows_the_pointer_only_on_the_track() {
        let layout = layout();
        let mut state = SeekBarState::default();

        assert_eq!(state.on_mouse(MouseInput::Move, 10, 21, &layout), None);
        assert_eq!(state.hover, Some(10));
        assert_eq!(state.shown_column(), Some(10));

        assert_eq!(state.on_mouse(MouseInput::Move, 10, 5, &layout), None);
        assert_eq!(state.hover, None);

        assert_eq!(state.on_mouse(MouseInput::Move, 70, 21, &layout), None);
        assert_eq!(state.hover, None);
        assert_eq!(state.shown_column(), None);
    }

    #[test]
    fn drag_moves_the_marker_and_seeks_once_on_release() {
        let layout = layout();
        let mut state = SeekBarState::default();

        assert_eq!(state.on_mouse(MouseInput::Press, 10, 21, &layout), None);
        assert_eq!(state.drag, Some(10));

        assert_eq!(state.on_mouse(MouseInput::Drag, 20, 21, &layout), None);
        assert_eq!(state.drag, Some(20));

        // 行から外れても続き、列だけトラック端へ吸着する。
        assert_eq!(state.on_mouse(MouseInput::Drag, 200, 3, &layout), None);
        assert_eq!(state.drag, Some(64));

        assert_eq!(
            state.on_mouse(MouseInput::Release, 30, 21, &layout),
            Some(MouseAction::Seek { column: 30 })
        );
        assert_eq!(state.drag, None);
        assert_eq!(state.hover, Some(30));
    }

    #[test]
    fn a_press_outside_the_track_never_starts_a_drag() {
        let layout = layout();
        let mut state = SeekBarState::default();

        assert_eq!(state.on_mouse(MouseInput::Press, 10, 5, &layout), None);
        assert_eq!(state.on_mouse(MouseInput::Drag, 20, 21, &layout), None);
        assert_eq!(state.on_mouse(MouseInput::Release, 30, 21, &layout), None);
        assert_eq!(state.drag, None);

        // ウィンドウ外で離すと Up が来ず Moved が届く。どこで離したか分からないので取り消す。
        let mut state = SeekBarState::default();
        state.on_mouse(MouseInput::Press, 10, 21, &layout);
        assert_eq!(state.on_mouse(MouseInput::Move, 12, 21, &layout), None);
        assert_eq!(state.drag, None);
        assert_eq!(state.hover, Some(12));
    }

    #[test]
    fn label_keeps_the_same_width_while_playing() {
        assert_eq!(label_width(Some(300.0)), 13);
        assert_eq!(label_width(Some(3661.0)), 17);
        // duration が不明なら再生位置が 1 時間を越えうるので時間桁ぶん確保する。
        assert_eq!(label_width(None), 19);

        assert_eq!(label_text(Some(75.0), Some(300.0)), "01:15 / 05:00");
        assert_eq!(label_text(Some(75.0), Some(3661.0)), "0:01:15 / 1:01:01");
        assert_eq!(label_text(None, Some(300.0)), "--:-- / 05:00");
        assert_eq!(label_text(None, Some(3661.0)), "-:--:-- / 1:01:01");
        assert_eq!(label_text(None, None), "--:--:-- / --:--:--");
    }

    #[test]
    fn label_fits_the_reserved_width_even_when_the_position_outgrows_the_duration() {
        // 予約幅を越えると set_stringn が右端の duration を削る。
        let fits = |shown, duration| {
            label_text(shown, duration).chars().count() <= label_width(duration) as usize
        };

        // duration 不明のまま 1 時間を越えた (ライブ・音声のみ)。
        assert_eq!(label_text(Some(3700.0), None), " 1:01:40 / --:--:--");
        assert!(fits(Some(3700.0), None));
        // duration が 3600 未満なのに再生位置が 3600 以上になる末尾付近。
        assert_eq!(label_text(Some(3600.0), Some(3599.0)), "60:00 / 59:59");
        assert!(fits(Some(3600.0), Some(3599.0)));

        // 再生位置は duration 付近までしか進まないので、その範囲で幅が保たれれば足りる。
        let cases: [(Option<f64>, &[f64]); 5] = [
            (None, &[0.0, 3700.0, 359_999.0]),
            (Some(0.0), &[0.0, 5.0]),
            (Some(300.0), &[0.0, 299.0, 305.0]),
            (Some(3599.0), &[0.0, 3599.0, 3600.0, 3660.0]),
            (Some(3661.0), &[0.0, 3661.0, 3700.0]),
        ];
        for (duration, positions) in cases {
            assert!(fits(None, duration), "--:-- / {duration:?}");
            for &shown in positions {
                assert!(fits(Some(shown), duration), "{shown} / {duration:?}");
            }
        }
    }

    #[test]
    fn seek_bar_renders_fill_marker_and_label() {
        let area = Rect::new(0, 0, 30, 1);
        let layout = SeekBarLayout::new(area, 13);
        let mut buf = Buffer::empty(area);
        SeekBar {
            layout,
            filled: 5,
            marker: Some(8),
            label: "01:23 / 34:05",
            highlighted: true,
        }
        .render(area, &mut buf);

        assert_eq!(row_symbols(&buf, 0), "█████░░░┃░░░░░░  01:23 / 34:05");
        assert_eq!(buf.cell((17, 0)).expect("ラベル先頭").fg, Color::Yellow);
    }

    #[test]
    fn a_row_too_narrow_for_a_track_clamps_every_column_to_its_left_edge() {
        let layout = SeekBarLayout::new(Rect::new(5, 20, 4, 1), 8);
        assert_eq!(layout.track.width, 0);
        assert_eq!(layout.clamp_column(40), 5);
        assert_eq!(layout.clamp_column(0), 5);
    }
}
