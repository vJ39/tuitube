use crate::app::{App, Mode, format_time};
use crate::display::DisplayMode;
use crate::geometry::cell_size;
use crate::grid::{self, LayoutMode};
use crate::seekbar::{SeekBar, SeekBarLayout, label_text, label_width};
use crate::video::CellSize;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};

/// 別ウィンドウ再生中に映像領域へ出す案内。
fn window_placeholder(display: DisplayMode) -> String {
    format!("別ウィンドウで再生中  w: {}へ", display.next().label())
}

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

/// 検索画面は [入力, タブ, 結果, ステータス, ヘルプ] の5段。結果に残り全体を渡す。
pub fn search_areas(area: Rect) -> [Rect; 5] {
    Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area)
}

/// 結果ブロックの内側。格子の割り付けと画像の貼り付けが同じ矩形を使う。
pub fn results_inner(area: Rect) -> Rect {
    Block::default()
        .borders(Borders::ALL)
        .inner(search_areas(area)[2])
}

/// 描画と画像の貼り付けが共有する割り付け。格子を組めないときは None。
pub fn grid_layout(app: &App, cell: CellSize) -> Option<grid::Layout> {
    grid_layout_in(app, app.screen, cell)
}

/// 指定の画面寸法での割り付け。リサイズは描き直す前の寸法を渡す。
pub fn grid_layout_in(app: &App, screen: Rect, cell: CellSize) -> Option<grid::Layout> {
    if app.settings.search.layout != LayoutMode::Grid {
        return None;
    }
    grid::layout(results_inner(screen), cell, app.results.len(), app.scroll)
}

/// 入力欄のカーソル位置 (0 始まり)。draw と、画像を貼った後の戻し先が同じ計算を使う。
pub fn input_cursor(screen: Rect, query: &str) -> (u16, u16) {
    let area = search_areas(screen)[0];
    (cursor_x(area, query), area.y + 1)
}

fn draw_search(frame: &mut Frame, app: &App) {
    let areas = search_areas(frame.area());

    let input = Paragraph::new(app.query.as_str())
        .block(Block::default().borders(Borders::ALL).title(" 検索 "));
    frame.render_widget(input, areas[0]);
    draw_tabs(frame, app, areas[1]);

    // app.screen は直前の draw の寸法なので、割り付けは今のフレームで組み直す。
    let layout = if app.settings.search.layout == LayoutMode::Grid {
        grid::layout(
            Block::default().borders(Borders::ALL).inner(areas[2]),
            cell_size(),
            app.results.len(),
            app.scroll,
        )
    } else {
        None
    };
    match &layout {
        Some(layout) => draw_grid(frame, app, areas[2], layout),
        None => draw_list(frame, app, areas[2]),
    }

    draw_footer(frame, app, areas[3], areas[4]);

    if app.mode == Mode::Input {
        frame.set_cursor_position(input_cursor(frame.area(), &app.query));
    }
}

fn draw_tabs(frame: &mut Frame, app: &App, area: Rect) {
    let selected = app.tabs.selected();
    let mut spans = Vec::new();
    for (index, label) in app.tabs.labels().into_iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(" │ ", Style::default().fg(Color::DarkGray)));
        }
        let style = if index == selected {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
        } else {
            Style::default().fg(Color::Gray)
        };
        spans.push(Span::styled(label.to_string(), style));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// 可視範囲と総数。スクロールしても今どこを見ているか分かるようにする。
fn results_title(offset: usize, shown: usize, total: usize) -> String {
    if total == 0 || shown == 0 {
        return " 結果 ".to_string();
    }
    format!(" 結果 {}-{}/{total} ", offset + 1, offset + shown)
}

fn draw_grid(frame: &mut Frame, app: &App, area: Rect, layout: &grid::Layout) {
    let title = results_title(layout.offset, layout.cells.len(), app.results.len());
    frame.render_widget(Block::default().borders(Borders::ALL).title(title), area);

    for (i, cell) in layout.cells.iter().enumerate() {
        let index = layout.offset + i;
        let Some(result) = app.results.get(index) else {
            break;
        };
        // 画像が来ていないセルは枠だけ。来ていれば空けておき、APC が上に載る。
        if app.thumbs.get(&result.id).is_none() {
            frame.render_widget(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::DarkGray)),
                cell.image,
            );
        }
        let title_style = if index == app.selected {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        };
        frame.render_widget(
            Paragraph::new(grid::truncate(&result.title, usize::from(cell.title.width)))
                .style(title_style),
            cell.title,
        );
        let uploader = result.uploader.as_deref().unwrap_or("-");
        let meta = format!("{}  {uploader}", format_time(result.duration));
        frame.render_widget(
            Paragraph::new(grid::truncate(&meta, usize::from(cell.meta.width)))
                .style(Style::default().fg(Color::DarkGray)),
            cell.meta,
        );
    }
}

/// Kitty graphics protocol 非対応の端末と、格子を組めない狭さのときの従来表示。
fn draw_list(frame: &mut Frame, app: &App, area: Rect) {
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
    frame.render_stateful_widget(list, area, &mut state);
}

/// 分岐は網羅する。モードを増やしたときの描き分け漏れをコンパイラに拾わせる。
fn draw_playing(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let video = video_area(area);
    match app.display {
        DisplayMode::Window => draw_window_placeholder(frame, video, app.display),
        DisplayMode::Text => {
            if let Some(sink) = &app.video {
                sink.render_text(video, frame.buffer_mut());
            }
        }
        // 埋め込みの画像は draw の後にメインループが APC で重ねる。
        DisplayMode::Embedded => {}
    }
    draw_seek_bar(frame, app, area);
    draw_footer(frame, app, status_area(area), help_area(area));
}

/// 別ウィンドウ中は映像が来ないので、どこで再生しているかを映像領域に出す。
fn draw_window_placeholder(frame: &mut Frame, area: Rect, display: DisplayMode) {
    if area.height == 0 {
        return;
    }
    let row = Rect {
        y: area.y + area.height / 2,
        height: 1,
        ..area
    };
    frame.render_widget(
        Paragraph::new(window_placeholder(display))
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
        Paragraph::new(help_text(app.mode, app.display, help.width))
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

fn help_text(mode: Mode, display: DisplayMode, width: u16) -> String {
    match mode {
        // 旧版はキーワードを4つ並べて83桁あり、80桁端末では末尾が切れていた。
        Mode::Input => {
            "Enter:検索  Tab:カテゴリ  :yt*:ログイン連動の一覧  Esc:結果へ/終了".to_string()
        }
        Mode::Results => {
            "↑↓←→:選択  Enter:再生  Tab:カテゴリ  r:再取得  /またはEsc:検索へ  q:終了".to_string()
        }
        Mode::Playing => fit_hints(&playing_hints(display), width as usize),
    }
}

/// 再生中の案内。全部で 110 桁ほどあり 80 桁端末には入らないので、
/// 落ちて困らないものを後ろに置く。先頭 7 つは最も幅を食う w:別ウィンドウでも 75 桁に収まる。
fn playing_hints(display: DisplayMode) -> Vec<String> {
    vec![
        "space:一時停止".to_string(),
        "←→:シーク".to_string(),
        "↑↓:音量".to_string(),
        "c:URLコピー".to_string(),
        format!("w:{}", display.next().label()),
        "Esc:停止".to_string(),
        "q:終了".to_string(),
        "s:字幕".to_string(),
        "[ ]:速度±0.1".to_string(),
        "BS:等速".to_string(),
        "クリック:シーク".to_string(),
    ]
}

/// 幅に入るところまでを空白 1 つでつなぐ。help は折り返さないので、
/// 途中で切れた案内を出すより落とす。
fn fit_hints(hints: &[String], width: usize) -> String {
    let mut line = String::new();
    for hint in hints {
        let next = if line.is_empty() {
            hint.clone()
        } else {
            format!("{line} {hint}")
        };
        if grid::display_width(&next) > width {
            break;
        }
        line = next;
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Playback;
    use crate::search::SearchResult;
    use crate::seekbar::SeekBarState;

    fn result(index: usize) -> SearchResult {
        SearchResult {
            id: format!("id{index}"),
            title: format!("title {index}"),
            duration: None,
            uploader: None,
        }
    }

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

    /// 80 桁端末のヘルプ。案内が落ちるかどうかはここで決まる。
    fn help_80(mode: Mode, display: DisplayMode) -> String {
        help_text(mode, display, 80)
    }

    #[test]
    fn results_help_mentions_esc() {
        assert!(help_80(Mode::Results, DisplayMode::Embedded).contains("Esc"));
    }

    #[test]
    fn help_text_names_the_next_display_mode() {
        assert!(help_80(Mode::Playing, DisplayMode::Embedded).contains("w:テキスト"));
        assert!(help_80(Mode::Playing, DisplayMode::Text).contains("w:別ウィンドウ"));
        assert!(help_80(Mode::Playing, DisplayMode::Window).contains("w:埋め込み"));
    }

    #[test]
    fn window_placeholder_names_the_next_mode() {
        assert_eq!(
            window_placeholder(DisplayMode::Window),
            "別ウィンドウで再生中  w: 埋め込みへ"
        );
    }

    #[test]
    fn video_area_layout_is_unchanged_by_the_text_mode() {
        // 文字ブロックは映像と同じ矩形に描くので、割り付けはモードで変わらない。
        let area = Rect::new(0, 0, 80, 24);
        assert_eq!(video_area(area), Rect::new(0, 0, 80, 21));
        assert_eq!(seek_bar_area(area), Rect::new(0, 21, 80, 1));
    }

    #[test]
    fn playing_help_mentions_the_speed_keys() {
        // 80 桁では入らないので、広い端末での案内で見る。
        let help = help_text(Mode::Playing, DisplayMode::Embedded, 200);
        assert!(help.contains("[ ]"), "{help}");
        assert!(help.contains("BS"), "{help}");
        assert!(help.contains("速度"), "{help}");
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
        // 4 つ並べると 83 桁で 80 桁端末に入らないため、":yt*" に畳んである。
        let help = help_80(Mode::Input, DisplayMode::Embedded);
        assert!(help.contains(":yt*"), "{help}");
        assert!(help.contains("Enter:検索"), "{help}");
        assert!(help.contains("Tab:カテゴリ"), "{help}");
        assert!(grid::display_width(&help) <= 80, "{help}");
    }

    #[test]
    fn results_help_mentions_the_grid_and_tab_keys() {
        let help = help_80(Mode::Results, DisplayMode::Embedded);
        for key in ["↑↓←→", "Tab:カテゴリ", "r:再取得", "Enter:再生"] {
            assert!(help.contains(key), "{key} がない: {help}");
        }
        assert!(grid::display_width(&help) <= 80, "{help}");
    }

    #[test]
    fn search_areas_do_not_overlap_and_cover_the_screen() {
        let area = Rect::new(0, 0, 80, 24);
        let areas = search_areas(area);
        assert_eq!(areas[0].y, area.y);
        for pair in areas.windows(2) {
            assert_eq!(pair[0].bottom(), pair[1].y, "{pair:?}");
            assert_eq!(pair[0].width, area.width);
        }
        assert_eq!(areas[4].bottom(), area.bottom());
    }

    #[test]
    fn tab_row_sits_between_the_input_box_and_the_results() {
        let areas = search_areas(Rect::new(0, 0, 80, 24));
        assert_eq!(areas[0], Rect::new(0, 0, 80, 3), "入力ボックスは 3 行");
        assert_eq!(areas[1], Rect::new(0, 3, 80, 1), "タブは 1 行");
        assert_eq!(areas[2].y, 4, "結果はタブの下");
    }

    #[test]
    fn results_area_shrinks_by_one_row_compared_to_the_current_layout() {
        // タブ行が1行増えたぶんだけ結果が狭くなる。ステータス・ヘルプは動かさない。
        let areas = search_areas(Rect::new(0, 0, 80, 24));
        assert_eq!(areas[2], Rect::new(0, 4, 80, 18));
        assert_eq!(areas[3], Rect::new(0, 22, 80, 1));
        assert_eq!(areas[4], Rect::new(0, 23, 80, 1));
        // 結果ブロックの内側は枠のぶんさらに狭い。
        assert_eq!(
            results_inner(Rect::new(0, 0, 80, 24)),
            Rect::new(1, 5, 78, 16)
        );
    }

    #[test]
    fn search_areas_survive_a_terminal_too_short_for_every_row() {
        for height in 0..8 {
            let area = Rect::new(0, 0, 40, height);
            let areas = search_areas(area);
            for rect in areas {
                assert!(rect.bottom() <= area.bottom(), "{rect:?} / {area:?}");
            }
            // 内側を取っても矩形として成立する。
            let inner = results_inner(area);
            assert!(inner.bottom() <= area.bottom(), "{inner:?} / {area:?}");
        }
    }

    #[test]
    fn grid_layout_follows_the_configured_mode() {
        let mut app = App {
            screen: Rect::new(0, 0, 80, 24),
            results: (0..10).map(result).collect(),
            ..App::default()
        };
        let cell = CellSize {
            width_px: 8,
            height_px: 16,
        };
        let layout = grid_layout(&app, cell).expect("格子を組める");
        assert_eq!((layout.columns, layout.rows), (4, 2));
        assert_eq!(layout.image_px, (144, 80));

        app.settings.search.layout = LayoutMode::List;
        assert!(grid_layout(&app, cell).is_none(), "list ではリスト表示");

        // 狭い端末では設定が grid でもリスト表示へ落ちる。
        app.settings.search.layout = LayoutMode::Grid;
        app.screen = Rect::new(0, 0, 80, 10);
        assert!(grid_layout(&app, cell).is_none());
    }

    #[test]
    fn the_results_title_shows_the_visible_range() {
        assert_eq!(results_title(0, 8, 10), " 結果 1-8/10 ");
        assert_eq!(results_title(8, 2, 10), " 結果 9-10/10 ");
        assert_eq!(results_title(0, 0, 0), " 結果 ");
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
    fn playing_help_keeps_the_main_keys_inside_80_columns() {
        for display in [
            DisplayMode::Embedded,
            DisplayMode::Text,
            DisplayMode::Window,
        ] {
            let help = help_80(Mode::Playing, display);
            let width = grid::display_width(&help);
            assert!(width <= 80, "{width} 桁: {help}");
            // 切れた案内を出さない代わりに、押せないと困るキーは必ず入れる。
            for key in [
                "space:一時停止",
                "c:URLコピー",
                &format!("w:{}", display.next().label()),
                "Esc:停止",
                "q:終了",
            ] {
                assert!(help.contains(key), "{key} が落ちた: {help}");
            }
        }
    }

    #[test]
    fn playing_help_drops_whole_hints_when_the_terminal_is_narrow() {
        let wide = help_text(Mode::Playing, DisplayMode::Embedded, 200);
        assert!(wide.contains("クリック:シーク"), "{wide}");

        let narrow = help_text(Mode::Playing, DisplayMode::Embedded, 30);
        assert_eq!(narrow, "space:一時停止 ←→:シーク");
        assert_eq!(help_text(Mode::Playing, DisplayMode::Embedded, 0), "");
    }

    #[test]
    fn playing_help_mentions_the_subtitle_key() {
        // 80 桁では主要キーが先で入らないので、広い端末での案内で見る。
        let wide = help_text(Mode::Playing, DisplayMode::Embedded, 200);
        assert!(wide.contains("s:字幕"), "{wide}");
        // 幅に入らないぶんは丸ごと落ちる。途中で切れた案内は出さない。
        let narrow = help_80(Mode::Playing, DisplayMode::Text);
        assert!(grid::display_width(&narrow) <= 80, "{narrow}");
        assert!(!narrow.contains("s:字"), "{narrow}");
    }

    #[test]
    fn playing_help_mentions_the_mouse() {
        // マウスの案内は幅が余ったときだけ出す。
        assert!(help_text(Mode::Playing, DisplayMode::Embedded, 200).contains("クリック"));
        assert!(help_80(Mode::Playing, DisplayMode::Embedded).contains("シーク"));
    }
}
