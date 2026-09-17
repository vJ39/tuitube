use crate::app::{App, Mode, format_time};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};

/// 再生中は [映像, ステータス, ヘルプ] の3段。映像に残り全体を渡す。
fn playing_areas(area: Rect) -> [Rect; 3] {
    Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area)
}

/// mpv に渡す `--vo-kitty-*` は描画先と同じ寸法でなければならない。
pub fn video_area(area: Rect) -> Rect {
    playing_areas(area)[0]
}

pub fn draw(frame: &mut Frame, app: &App) {
    if app.mode == Mode::Playing {
        draw_playing(frame, app);
        return;
    }

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

/// 映像領域には何も描かない。画像は draw の後にメインループが APC で重ねる。
fn draw_playing(frame: &mut Frame, app: &App) {
    let [_video, status, help] = playing_areas(frame.area());
    draw_footer(frame, app, status, help);
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
        Paragraph::new(help_text(app.mode)).style(Style::default().fg(Color::DarkGray)),
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

fn help_text(mode: Mode) -> &'static str {
    match mode {
        Mode::Input => "Enter:検索  Esc:結果へ/終了",
        Mode::Results => "↑↓:選択  Enter:再生  /またはEsc:検索入力へ  q:終了",
        Mode::Playing => "space:一時停止  ←→:5秒シーク  ↑↓:音量±5  Esc:停止  q:終了",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(help_text(Mode::Results).contains("Esc"));
    }
}
