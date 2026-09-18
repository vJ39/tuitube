use crate::app::{App, Mode, format_time};
use crate::display::DisplayMode;
use crate::seekbar::{SeekBar, SeekBarLayout, label_text, label_width};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};

/// 別ウィンドウ再生中に映像領域へ出す案内。
const WINDOW_PLACEHOLDER: &str = "別ウィンドウで再生中  w: 埋め込みに戻す";

/// 再生中は [映像, シークバー, ステータス, ヘルプ] の4段。映像に残り全体を渡す。
fn playing_areas(area: Rect) -> [Rect; 4] {
    Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area)
}

/// mpv に渡す `--vo-kitty-*` は描画先と同じ寸法でなければならない。
pub fn video_area(area: Rect) -> Rect {
    playing_areas(area)[0]
}

/// シークバーの行。クリック桁から再生位置を求めるときもこの矩形を使う。
pub fn seek_bar_area(area: Rect) -> Rect {
    playing_areas(area)[1]
}

/// 再生状態を出す行。
pub fn status_area(area: Rect) -> Rect {
    playing_areas(area)[2]
}

/// 操作説明の行。
pub fn help_area(area: Rect) -> Rect {
    playing_areas(area)[3]
}

/// 描画とヒットテストが共有する割り付け。
pub fn seek_bar_layout(app: &App) -> SeekBarLayout {
    layout_for(app.screen, app.playback.duration)
}

fn layout_for(screen: Rect, duration: Option<f64>) -> SeekBarLayout {
    SeekBarLayout::new(seek_bar_area(screen), label_width(duration))
}

/// 分岐は網羅する。モードを増やしたときの描き分け漏れをコンパイラに拾わせる。
pub fn draw(frame: &mut Frame, app: &App) {
    match app.mode {
        Mode::Playing => draw_playing(frame, app),
        Mode::Input | Mode::Results => draw_search(frame, app),
    }
}

fn draw_search(frame: &mut Frame, app: &App) {
    let areas = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(frame.area());

    let input = Paragraph::new(app.query.as_str())
        .block(Block::default().borders(Borders::ALL).title(" 検索 "));
    frame.render_widget(input, areas[0]);

    let items: Vec<ListItem> = app
        .results
        .iter()
        .map(|r| {
            let uploader = r.uploader.as_deref().unwrap_or("-");
            ListItem::new(format!(
                "{}  {}  [{}]",
                format_time(r.duration),
                r.title,
                uploader
            ))
        })
        .collect();
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(" 結果 "))
        .highlight_symbol("> ")
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    let mut state = ListState::default();
    if !app.results.is_empty() {
        state.select(Some(app.selected));
    }
    frame.render_stateful_widget(list, areas[1], &mut state);

    draw_footer(frame, app, areas[2], areas[3]);

    if app.mode == Mode::Input {
        frame.set_cursor_position((cursor_x(areas[0], &app.query), areas[0].y + 1));
    }
}

/// 埋め込み中の映像領域には何も描かない。画像は draw の後にメインループが APC で重ねる。
fn draw_playing(frame: &mut Frame, app: &App) {
    let area = frame.area();
    if app.display == DisplayMode::Window {
        draw_window_placeholder(frame, video_area(area));
    }
    draw_seek_bar(frame, app, area);
    draw_footer(frame, app, status_area(area), help_area(area));
}

/// 別ウィンドウ中は映像が来ないので、どこで再生しているかを映像領域に出す。
fn draw_window_placeholder(frame: &mut Frame, area: Rect) {
    if area.height == 0 {
        return;
    }
    let row = Rect {
        y: area.y + area.height / 2,
        height: 1,
        ..area
    };
    frame.render_widget(
        Paragraph::new(WINDOW_PLACEHOLDER)
            .style(Style::default().fg(Color::DarkGray))
            .centered(),
        row,
    );
}

/// ポインタが指す列と時刻。duration が無いとシークできないので、印もラベルも出さない。
fn seek_pointer(app: &App, layout: &SeekBarLayout) -> Option<(u16, f64)> {
    let (column, duration) = app.seek_bar.shown_column().zip(app.playback.duration)?;
    Some((column, layout.seconds_at(column, duration)))
}

fn draw_seek_bar(frame: &mut Frame, app: &App, area: Rect) {
    let layout = layout_for(area, app.playback.duration);
    let pointer = seek_pointer(app, &layout);
    let label = label_text(
        pointer
            .map(|(_, seconds)| seconds)
            .or(app.playback.time_pos),
        app.playback.duration,
    );
    let bar = SeekBar {
        layout,
        filled: layout.filled_cells(app.playback.time_pos, app.playback.duration),
        marker: pointer.map(|(column, _)| column),
        label: &label,
        highlighted: pointer.is_some(),
    };
    frame.render_widget(bar, seek_bar_area(area));
}

fn draw_footer(frame: &mut Frame, app: &App, status: Rect, help: Rect) {
    let status_style = if app.error.is_some() {
        Style::default().fg(Color::Red)
    } else {
        Style::default().fg(Color::Cyan)
    };
    frame.render_widget(
        Paragraph::new(app.status_line()).style(status_style),
        status,
    );
    frame.render_widget(
        Paragraph::new(help_text(app.mode, app.display))
            .style(Style::default().fg(Color::DarkGray)),
        help,
    );
}

/// 全角文字はセル幅2で描画されるため、文字数ではなく表示幅で桁を数える。
fn cursor_x(input_area: Rect, query: &str) -> u16 {
    let width = Span::raw(query).width().min(u16::MAX as usize) as u16;
    input_area
        .x
        .saturating_add(1)
        .saturating_add(width)
        .min(input_area.right().saturating_sub(2))
}

fn help_text(mode: Mode, display: DisplayMode) -> &'static str {
    match mode {
        Mode::Input => {
            "Enter:検索  :ytrec/:ythis/:ytsubs/:ytwatchlater:ログイン連動の一覧  Esc:結果へ/終了"
        }
        Mode::Results => "↑↓:選択  Enter:再生  /またはEsc:検索入力へ  q:終了",
        Mode::Playing => match display {
            DisplayMode::Embedded => {
                "space:一時停止  ←→:5秒シーク  クリック/ドラッグ:シーク  ↑↓:音量±5  w:別ウィンドウ  Esc:停止  q:終了"
            }
            DisplayMode::Window => {
                "space:一時停止  ←→:5秒シーク  クリック/ドラッグ:シーク  ↑↓:音量±5  w:埋め込みへ  Esc:停止  q:終了"
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Playback;
    use crate::seekbar::SeekBarState;

    #[test]
    fn a_video_without_duration_gets_no_marker_to_seek_with() {
        let mut app = App {
            mode: Mode::Playing,
            screen: Rect::new(0, 0, 80, 24),
            playback: Playback {
                time_pos: Some(10.0),
                duration: None,
                ..Playback::default()
            },
            seek_bar: SeekBarState {
                hover: Some(10),
                drag: None,
            },
            ..App::default()
        };
        assert_eq!(seek_pointer(&app, &seek_bar_layout(&app)), None);

        // duration が届けば同じホバー列に印と時刻が出る (トラック 65 セルで 1 セル 10 秒)。
        app.playback.duration = Some(650.0);
        assert_eq!(
            seek_pointer(&app, &seek_bar_layout(&app)),
            Some((10, 100.0))
        );
    }

    #[test]
    fn cursor_follows_display_width_not_char_count() {
        let area = Rect::new(0, 0, 40, 3);
        assert_eq!(cursor_x(area, ""), 1);
        assert_eq!(cursor_x(area, "abc"), 4);
        // 全角4文字 = 8桁
        assert_eq!(cursor_x(area, "ラーメン"), 9);
    }

    #[test]
    fn cursor_stops_inside_the_border() {
        let area = Rect::new(0, 0, 10, 3);
        assert_eq!(cursor_x(area, "ラーメンラーメン"), 8);
    }

    #[test]
    fn results_help_mentions_esc() {
        assert!(help_text(Mode::Results, DisplayMode::Embedded).contains("Esc"));
    }

    #[test]
    fn help_text_names_the_other_display_mode() {
        assert!(help_text(Mode::Playing, DisplayMode::Embedded).contains("w:別ウィンドウ"));
        assert!(help_text(Mode::Playing, DisplayMode::Window).contains("w:埋め込みへ"));
    }

    #[test]
    fn video_area_layout_is_unchanged_by_the_display_mode() {
        // プレースホルダは映像と同じ矩形に描くので、割り付けはモードで変わらない。
        let area = Rect::new(0, 0, 80, 24);
        assert_eq!(video_area(area), Rect::new(0, 0, 80, 21));
        assert_eq!(seek_bar_area(area), Rect::new(0, 21, 80, 1));
    }

    #[test]
    fn input_help_mentions_feed_keywords() {
        let help = help_text(Mode::Input, DisplayMode::Embedded);
        for keyword in [":ytrec", ":ythis", ":ytsubs", ":ytwatchlater"] {
            assert!(help.contains(keyword), "{keyword} がない: {help}");
        }
        assert!(help.contains("Enter:検索"));
    }

    #[test]
    fn playing_rows_do_not_overlap_the_video() {
        let area = Rect::new(0, 0, 80, 24);
        assert_eq!(video_area(area), Rect::new(0, 0, 80, 21));
        assert_eq!(seek_bar_area(area), Rect::new(0, 21, 80, 1));
        assert_eq!(status_area(area), Rect::new(0, 22, 80, 1));
        assert_eq!(help_area(area), Rect::new(0, 23, 80, 1));
    }

    #[test]
    fn playing_help_mentions_the_mouse() {
        assert!(help_text(Mode::Playing, DisplayMode::Embedded).contains("シーク"));
    }
}
