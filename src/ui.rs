//! 描画の振り分けと、画面をまたいで使う部品 (フッタ、案内の詰め方、検索画面の割り付け)。

use crate::app::{App, Mode};
use crate::display::DisplayMode;
use crate::grid::{self, LayoutMode};
use crate::screen::browse as browse_screen;
use crate::screen::download as download_screen;
use crate::screen::playing as playing_screen;
use crate::screen::playlists as playlists_screen;
use crate::screen::settings as settings_screen;
use crate::video::CellSize;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::widgets::{Block, Borders, Paragraph};

/// 分岐は網羅する。モードを増やしたときの描き分け漏れをコンパイラに拾わせる。
pub fn draw(frame: &mut Frame, app: &App) {
    match app.mode {
        Mode::Playing => playing_screen::draw_playing(frame, app),
        Mode::Settings => settings_screen::draw_settings(frame, app),
        Mode::Download => download_screen::draw_download(frame, app),
        // チャンネルもプレイリストも同じ 5 段の画面。
        // 中身の参照先だけが app.channel / app.playlist へ移る。
        Mode::Input | Mode::Results | Mode::Channel | Mode::Playlists | Mode::Playlist => {
            browse_screen::draw_search(frame, app)
        }
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
    // プレイリスト一覧は結果の格子でなく行のリストを出す。画像を貼ると行に重なる。
    if app.mode == Mode::Playlists {
        return None;
    }
    if app.settings.search.layout != LayoutMode::Grid {
        return None;
    }
    grid::layout(
        results_inner(screen),
        cell,
        app.view_results().len(),
        app.view_scroll(),
    )
}

/// 終了確認 (y/N) の案内。エラーと同じ赤で目立たせる。
const CONFIRM_QUIT_STATUS: &str = "終了しますか？ (y/N)";

fn confirm_quit_hints() -> Vec<String> {
    vec!["y/Enter:終了".to_string(), "n/Esc:キャンセル".to_string()]
}

pub(crate) fn draw_footer(frame: &mut Frame, app: &App, status: Rect, help: Rect) {
    // 終了確認中は、モード別の通知・エラー・ヒントより優先してこちらを出す。
    if app.confirm_quit {
        frame.render_widget(
            Paragraph::new(grid::truncate(CONFIRM_QUIT_STATUS, status.width as usize))
                .style(Style::default().fg(Color::Red)),
            status,
        );
        frame.render_widget(
            Paragraph::new(fit_hints(&confirm_quit_hints(), help.width as usize))
                .style(Style::default().fg(Color::DarkGray)),
            help,
        );
        return;
    }
    let status_style = if app.error.is_some() {
        Style::default().fg(Color::Red)
    } else {
        Style::default().fg(Color::Cyan)
    };
    frame.render_widget(
        Paragraph::new(status_text(app, status.width as usize)).style(status_style),
        status,
    );
    frame.render_widget(
        Paragraph::new(help_line(app, help.width)).style(Style::default().fg(Color::DarkGray)),
        help,
    );
}

/// 今の画面の案内。設定画面で数値を打ち込んでいる間だけ、その操作へ差し替える。
fn help_line(app: &App, width: u16) -> String {
    if app.mode == Mode::Settings {
        return settings_screen::settings_help(app, width as usize);
    }
    help_text(
        app.mode,
        app.display,
        app.comments.visible(),
        app.can_load_more(),
        app.background,
        app.playback.channel_id.is_some(),
        width,
    )
}

/// ステータス行は 1 行で折り返さないので、入らないぶんは "…" にする。
/// 黙って切れると、切れたのか元から短いのかが読み手に分からない。
fn status_text(app: &App, width: usize) -> String {
    grid::truncate(&app.status_line(), width)
}

pub(crate) fn help_text(
    mode: Mode,
    display: DisplayMode,
    comments_open: bool,
    can_load_more: bool,
    background: bool,
    can_subscribe: bool,
    width: u16,
) -> String {
    let hints = match mode {
        Mode::Input => browse_screen::input_hints(background),
        Mode::Results => browse_screen::results_hints(can_load_more, background),
        Mode::Channel => browse_screen::channel_hints(background),
        Mode::Playlists => playlists_screen::playlists_hints(background),
        Mode::Playlist => browse_screen::playlist_hints(background),
        Mode::Playing => playing_screen::playing_hints(display, comments_open, can_subscribe),
        Mode::Settings => settings_screen::settings_hints(),
        Mode::Download => download_screen::download_hints(),
    };
    fit_hints(&hints, width as usize)
}

/// 幅に入るところまでを空白 1 つでつなぐ。help は折り返さないので、
/// 途中で切れた案内を出すより落とす。
pub(crate) fn fit_hints(hints: &[String], width: usize) -> String {
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
    use crate::app::{ChannelView, Playback};
    use crate::search::SearchResult;

    fn result(index: usize) -> SearchResult {
        SearchResult {
            id: format!("id{index}"),
            title: format!("title {index}"),
            duration: None,
            uploader: None,
            channel_id: None,
            is_live: false,
        }
    }

    #[test]
    fn the_status_row_marks_where_it_was_cut() {
        let app = App {
            error: Some("あ".repeat(100)),
            ..App::default()
        };
        let line = status_text(&app, 80);
        // 全角は 2 桁なので、端に 1 桁余ることがある。
        assert!((79..=80).contains(&grid::display_width(&line)), "{line}");
        assert!(line.ends_with('…'), "{line}");
    }

    #[test]
    fn the_cookie_refusal_is_shown_whole_on_an_80_column_terminal() {
        // cookie 未設定でフィードのタブを選ぶと出る行。切れると設定先が読めない。
        for feed in crate::cookies::Feed::ALL {
            let app = App {
                error: Some(crate::cookies::CookieState::Off.refusal(feed)),
                ..App::default()
            };
            let line = status_text(&app, 80);
            assert!(line.contains(feed.label()), "{line}");
            // browser / file どちらの設定先も切れずに出る長さにする。
            assert!(line.contains("[cookies] browser"), "{line}");
            assert!(line.contains("file"), "{line}");
            assert!(!line.contains('…'), "{line}");
        }
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

    /// 80x24 のチャンネル画面。現在タブに `count` 件持たせる。
    fn channel_app(count: usize) -> App {
        let mut view = ChannelView::new("UCabc".to_string(), "Some Channel".to_string());
        view.state_mut().results = (0..count).map(result).collect();
        view.state_mut().loaded = true;
        App {
            mode: Mode::Channel,
            screen: Rect::new(0, 0, 80, 24),
            // 検索結果は残したまま、画面はチャンネルを見ている。
            results: vec![result(99)],
            channel: Some(view),
            ..App::default()
        }
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

    /// TestBackend に 1 フレーム描いて、画面の文字だけを行ごとに取り出す。
    fn rendered(app: &App, width: u16, height: u16) -> String {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height))
                .expect("端末");
        terminal.draw(|frame| draw(frame, app)).expect("描ける");
        let buffer = terminal.backend().buffer().clone();
        (0..buffer.area.height)
            .map(|y| {
                let mut line = String::new();
                // 全角文字は 2 セルを占め、後ろのセルは埋め草なので読み飛ばす。
                let mut skip = 0;
                for x in 0..buffer.area.width {
                    if skip > 0 {
                        skip -= 1;
                        continue;
                    }
                    let symbol = buffer[(x, y)].symbol();
                    skip = grid::display_width(symbol).saturating_sub(1);
                    line.push_str(symbol);
                }
                line
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    // ---- ダウンロード画面 ----

    /// 終了確認中の下段2行。モード別のエラー・通知・ヒントより優先する。
    fn confirm_quit_footer(app: &App) -> (String, String) {
        let screen = rendered(app, 80, 24);
        let mut lines: Vec<&str> = screen.lines().collect();
        let help = lines.pop().expect("help行").trim_end().to_string();
        let status = lines.pop().expect("status行").trim_end().to_string();
        (status, help)
    }

    #[test]
    fn confirm_quit_replaces_the_footer_no_matter_the_mode() {
        let mut playing = App {
            mode: Mode::Playing,
            screen: Rect::new(0, 0, 80, 24),
            playback: Playback {
                title: "song".to_string(),
                time_pos: Some(0.0),
                duration: Some(60.0),
                ..Playback::default()
            },
            ..App::default()
        };
        playing.confirm_quit = true;
        playing.error = Some("boom".to_string());
        playing.notice = Some("notice".to_string());

        let mut results = App {
            mode: Mode::Results,
            results: vec![result(0)],
            ..App::default()
        };
        results.confirm_quit = true;
        results.error = Some("boom".to_string());
        results.notice = Some("notice".to_string());

        let mut channel = channel_app(2);
        channel.confirm_quit = true;
        channel.error = Some("boom".to_string());
        channel.notice = Some("notice".to_string());

        let input = App {
            confirm_quit: true,
            notice: Some("notice".to_string()),
            ..App::default()
        };

        for app in [playing, results, channel, input] {
            let (status, help) = confirm_quit_footer(&app);
            assert!(status.contains("終了しますか"), "{:?}: {status}", app.mode);
            assert!(status.contains("(y/N)"), "{:?}: {status}", app.mode);
            assert!(!status.contains("boom"), "{:?}: {status}", app.mode);
            assert!(!status.contains("notice"), "{:?}: {status}", app.mode);
            assert!(help.contains("y/Enter:終了"), "{:?}: {help}", app.mode);
            assert!(help.contains("n/Esc:キャンセル"), "{:?}: {help}", app.mode);
        }
    }
}
