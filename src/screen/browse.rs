//! 検索欄と結果一覧の画面 (Mode::Input/Results/Channel/Playlist)。キーとマウス、検索欄・タブ行・
//! 格子・リストの描画、クリック位置の当たり判定、状態行と案内を持つ。
//! 検索やチャンネル・プレイリストの取得など共有の操作は actions.rs にある。

use crate::actions::{
    Oauth, Session, config_path_from_env, hide_current_channel, hide_selected, leave_background,
    leave_channel, load_more, move_selection, open_channel, reload_channel_tab, reload_tab,
    save_video, select_channel_tab, select_tab, start_playback, start_search, subscribe_channel,
    switch_channel_tab, switch_tab, toggle_search_layout,
};
use crate::app::{App, AppEvent, ChannelView, Mode, format_time};
use crate::badge;
use crate::cookies::ChannelTab;
use crate::display::DisplayMode;
use crate::geometry::cell_size;
use crate::grid::{self, Dir, LayoutMode};
use crate::oauth;
use crate::query::QueryEditor;
use crate::screen::download::open_download;
use crate::screen::playing as playing_screen;
use crate::screen::playlists::{
    self as playlists_screen, is_playlists_key, leave_playlist, open_playlist_by_url,
    open_playlists, reload_playlist,
};
use crate::screen::settings::{is_settings_key, open_settings};
use crate::ui::{draw_footer, grid_layout, search_areas};
use crate::video::CellSize;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use std::path::Path;
use tokio::sync::mpsc::UnboundedSender;

// ---- 状態行 ----

/// 件数と選択中のタイトルの本体。results_status/channel_status で共有する。
fn results_body(app: &App) -> String {
    let count = format!("{} 件", app.view_results().len());
    if app.thumbs.is_fetching() {
        return format!("{count}  |  サムネイル取得中...");
    }
    match app.view_selected_result() {
        Some(result) => format!("{count}  |  {}", result.title),
        None => count,
    }
}

/// 格子のタイトルは 18 桁ほどで切れるので、選択中の完全なタイトルはここに出す。
pub fn results_status(app: &App) -> String {
    format!("{}{}", app.background_marker(), results_body(app))
}

/// チャンネル名とタブは検索欄に出ないので、状態行の先頭に出す。
pub fn channel_status(app: &App) -> String {
    let Some(channel) = &app.channel else {
        return results_status(app);
    };
    format!(
        "{}{} [{}]  |  {}",
        app.background_marker(),
        channel.channel_title,
        channel.tab.label(),
        results_body(app)
    )
}

/// プレイリスト名は検索欄に出ないので、チャンネルと同じく状態行の先頭に出す。
pub fn playlist_status(app: &App) -> String {
    let Some(playlist) = &app.playlist else {
        return results_status(app);
    };
    // URL で開いたプレイリストの名前は中身と一緒に届く。それまでは種類だけ出す。
    let title = if playlist.playlist_title.is_empty() {
        "プレイリスト"
    } else {
        &playlist.playlist_title
    };
    format!(
        "{}{title}  |  {}",
        app.background_marker(),
        results_body(app)
    )
}

// ---- キー ----

pub async fn handle_key_input(
    app: &mut App,
    key: KeyEvent,
    tx: &UnboundedSender<AppEvent>,
    session: &mut Session,
) {
    let extend = key.modifiers.contains(KeyModifiers::SHIFT);
    match key.code {
        KeyCode::Enter => match crate::youtube_url::playlist_id(app.query.text()) {
            Some(id) => open_playlist_by_url(app, tx, session, id),
            None => start_search(app, tx, session),
        },
        KeyCode::Tab => switch_tab(app, tx, session, true),
        KeyCode::BackTab => switch_tab(app, tx, session, false),
        KeyCode::Backspace => app.query.backspace(),
        KeyCode::Left => app.query.move_left(extend),
        KeyCode::Right => app.query.move_right(extend),
        KeyCode::Home => app.query.move_home(extend),
        KeyCode::End => app.query.move_end(extend),
        // 入力欄では大文字の S も検索語なので、設定は Ctrl+S で開く。
        KeyCode::Char(c) if is_settings_key(c, key.modifiers) => open_settings(app, session),
        KeyCode::Char(c) if is_select_all_key(c, key.modifiers) => app.query.select_all(),
        // バックグラウンド中の前面復帰。b は検索語なので Ctrl+B で取る。
        KeyCode::Char(c) if is_leave_background_key(c, key.modifiers) => {
            leave_background(app, session).await;
        }
        // p も検索語なので、プレイリスト一覧は Ctrl+P で開く。
        KeyCode::Char(c) if is_playlists_key(c, key.modifiers) => open_playlists(app, tx, session),
        // 他の Ctrl 付きは検索語に入れない。制御文字が混ざると検索が通らない。
        KeyCode::Char(_) if key.modifiers.contains(KeyModifiers::CONTROL) => {}
        KeyCode::Char(c) => {
            app.query.insert(c);
            app.set_error(None);
        }
        KeyCode::Esc => {
            if app.results.is_empty() {
                app.confirm_quit = true;
            } else {
                app.mode = Mode::Results;
            }
        }
        _ => {}
    }
}

pub async fn handle_key_results(
    app: &mut App,
    key: KeyEvent,
    tx: &UnboundedSender<AppEvent>,
    session: &mut Session,
) {
    let config = config_path_from_env();
    handle_key_results_with(app, key, tx, session, Oauth::real(), config.as_deref()).await;
}

/// oauth の実装先と設定ファイルの置き場を差し替えられる形。テストはここに偽物と
/// 一時ファイルを渡して、実際の通信も利用者の設定の書き換えも起こさない。
async fn handle_key_results_with<B: oauth::Backend + 'static>(
    app: &mut App,
    key: KeyEvent,
    tx: &UnboundedSender<AppEvent>,
    session: &mut Session,
    deps: Oauth<B>,
    config: Option<&Path>,
) {
    match key.code {
        // 末尾で押したときは、もっと見られる (m キーと同じ判定) 状態なら一覧を送らず
        // 読み込みを始める。list 表示は select_next が末尾で先頭へ巻き戻るため、この
        // 判定は move_selection を呼ぶ前に行う (巻き戻った後では末尾にいたと分からない)。
        KeyCode::Down if app.view_is_at_last_result() && app.can_load_more() && !app.searching => {
            load_more(app, tx, session)
        }
        KeyCode::Down => move_selection(app, Dir::Down),
        KeyCode::Up => move_selection(app, Dir::Up),
        KeyCode::Right => move_selection(app, Dir::Right),
        KeyCode::Left => move_selection(app, Dir::Left),
        KeyCode::Tab => switch_tab(app, tx, session, true),
        KeyCode::BackTab => switch_tab(app, tx, session, false),
        KeyCode::Enter => start_playback(app, tx, session).await,
        // 結果一覧では文字を打たないので、S 単独でも開ける。
        KeyCode::Char('S') => open_settings(app, session),
        KeyCode::Char(c) if is_settings_key(c, key.modifiers) => open_settings(app, session),
        // 設定以外の Ctrl 付きは一覧の操作にしない。Ctrl+C のような中断キーで
        // チャンネルへ移ったり取り直したりすると、押した側の意図と食い違う。
        KeyCode::Char(_) if key.modifiers.contains(KeyModifiers::CONTROL) => {}
        KeyCode::Char('r') => reload_tab(app, tx, session),
        KeyCode::Char('c') => open_channel(app, tx, session),
        KeyCode::Char('d') => open_download(app, session),
        KeyCode::Char('a') => save_video(app, tx, session, deps),
        KeyCode::Char('h') => hide_selected(app, std::time::Instant::now()),
        KeyCode::Char('p') => open_playlists(app, tx, session),
        // もっと見られる状態でだけ動く (App::can_load_more で判定し、load_more_with が弾く)。
        KeyCode::Char('m') => load_more(app, tx, session),
        // grid/list の即時切替+自動保存。mpv には触れないので同期のまま呼べる。
        KeyCode::Char('v') => toggle_search_layout(app, config, std::time::Instant::now()),
        // バックグラウンド中でなければ何もしない (leave_background が判定する)。
        KeyCode::Char('b') => leave_background(app, session).await,
        KeyCode::Char('/') | KeyCode::Esc => {
            app.mode = Mode::Input;
            app.set_error(None);
        }
        KeyCode::Char('q') => app.confirm_quit = true,
        _ => {}
    }
}

/// チャンネル一覧。移動と再生は結果一覧と同じで、タブの中身と戻り先だけが違う。
pub async fn handle_key_channel(
    app: &mut App,
    key: KeyEvent,
    tx: &UnboundedSender<AppEvent>,
    session: &mut Session,
) {
    let config = config_path_from_env();
    handle_key_channel_with(app, key, tx, session, Oauth::real(), config.as_deref()).await;
}

/// oauth の実装先と設定ファイルの置き場を差し替えられる形。テストはここに偽物と
/// 一時ファイルを渡して、実際の通信も利用者の設定の書き換えも起こさない。
async fn handle_key_channel_with<B: oauth::Backend + 'static>(
    app: &mut App,
    key: KeyEvent,
    tx: &UnboundedSender<AppEvent>,
    session: &mut Session,
    deps: Oauth<B>,
    config: Option<&Path>,
) {
    match key.code {
        KeyCode::Down => move_selection(app, Dir::Down),
        KeyCode::Up => move_selection(app, Dir::Up),
        KeyCode::Right => move_selection(app, Dir::Right),
        KeyCode::Left => move_selection(app, Dir::Left),
        KeyCode::Tab => switch_channel_tab(app, tx, session, true),
        KeyCode::BackTab => switch_channel_tab(app, tx, session, false),
        KeyCode::Enter => start_playback(app, tx, session).await,
        KeyCode::Char('S') => open_settings(app, session),
        KeyCode::Char(c) if is_settings_key(c, key.modifiers) => open_settings(app, session),
        KeyCode::Char(_) if key.modifiers.contains(KeyModifiers::CONTROL) => {}
        // 取りそこねたタブはここからしか戻せない。タブを送り直しても読み込み済みのまま。
        KeyCode::Char('r') => reload_channel_tab(app, tx, session),
        KeyCode::Char('s') => subscribe_channel(app, tx, session, deps),
        KeyCode::Char('d') => open_download(app, session),
        KeyCode::Char('a') => save_video(app, tx, session, deps),
        KeyCode::Char('h') => hide_current_channel(app, session, std::time::Instant::now()),
        // grid/list の即時切替+自動保存。mpv には触れないので同期のまま呼べる。
        KeyCode::Char('v') => toggle_search_layout(app, config, std::time::Instant::now()),
        // バックグラウンド中でなければ何もしない (leave_background が判定する)。
        KeyCode::Char('b') => leave_background(app, session).await,
        KeyCode::Char('/') | KeyCode::Esc => leave_channel(app, session),
        KeyCode::Char('q') => app.confirm_quit = true,
        _ => {}
    }
}

/// プレイリストの中の動画一覧。チャンネルと同型だが、タブ送りと登録は持たない。
pub async fn handle_key_playlist(
    app: &mut App,
    key: KeyEvent,
    tx: &UnboundedSender<AppEvent>,
    session: &mut Session,
) {
    let config = config_path_from_env();
    handle_key_playlist_with(app, key, tx, session, Oauth::real(), config.as_deref()).await;
}

/// oauth の実装先と設定ファイルの置き場を差し替えられる形。テストはここに偽物と
/// 一時ファイルを渡して、実際の通信も利用者の設定の書き換えも起こさない。
async fn handle_key_playlist_with<B: oauth::Backend + 'static>(
    app: &mut App,
    key: KeyEvent,
    tx: &UnboundedSender<AppEvent>,
    session: &mut Session,
    deps: Oauth<B>,
    config: Option<&Path>,
) {
    match key.code {
        KeyCode::Down => move_selection(app, Dir::Down),
        KeyCode::Up => move_selection(app, Dir::Up),
        KeyCode::Right => move_selection(app, Dir::Right),
        KeyCode::Left => move_selection(app, Dir::Left),
        KeyCode::Enter => start_playback(app, tx, session).await,
        KeyCode::Char('S') => open_settings(app, session),
        KeyCode::Char(c) if is_settings_key(c, key.modifiers) => open_settings(app, session),
        KeyCode::Char(_) if key.modifiers.contains(KeyModifiers::CONTROL) => {}
        KeyCode::Char('r') => reload_playlist(app, tx, session),
        KeyCode::Char('c') => open_channel(app, tx, session),
        KeyCode::Char('d') => open_download(app, session),
        KeyCode::Char('a') => save_video(app, tx, session, deps),
        // プレイリストごと隠す手はないので、チャンネルと違い動画 1 件だけを隠す。
        KeyCode::Char('h') => hide_selected(app, std::time::Instant::now()),
        // grid/list の即時切替+自動保存。mpv には触れないので同期のまま呼べる。
        KeyCode::Char('v') => toggle_search_layout(app, config, std::time::Instant::now()),
        // バックグラウンド中でなければ何もしない (leave_background が判定する)。
        KeyCode::Char('b') => leave_background(app, session).await,
        KeyCode::Char('/') | KeyCode::Esc => leave_playlist(app, session),
        KeyCode::Char('q') => app.confirm_quit = true,
        _ => {}
    }
}

/// バックグラウンド中に前面へ戻すキー。入力欄では b も検索語なので Ctrl 付きだけを見る。
fn is_leave_background_key(c: char, modifiers: KeyModifiers) -> bool {
    modifiers.contains(KeyModifiers::CONTROL) && c.eq_ignore_ascii_case(&'b')
}

/// 検索語を全選択するキー。
fn is_select_all_key(c: char, modifiers: KeyModifiers) -> bool {
    modifiers.contains(KeyModifiers::CONTROL) && c.eq_ignore_ascii_case(&'a')
}

/// 入力モードでのクリックの行き先。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputClick {
    Tab(usize),
    Cursor(usize),
}

/// 押し込みだけを見るので、ドラッグや離した位置では動かない。
/// タブ行と入力欄は重ならないが、既存のタブ選択を先に見る。
fn input_click(app: &App, mouse: MouseEvent) -> Option<InputClick> {
    if !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
        return None;
    }
    if let Some(index) = tab_at_point(app, mouse.column, mouse.row) {
        return Some(InputClick::Tab(index));
    }
    query_index_at_point(app, mouse.column, mouse.row).map(InputClick::Cursor)
}

pub fn handle_mouse_input(
    app: &mut App,
    mouse: MouseEvent,
    tx: &UnboundedSender<AppEvent>,
    session: &mut Session,
) {
    match input_click(app, mouse) {
        Some(InputClick::Tab(index)) => select_tab(app, tx, session, index),
        Some(InputClick::Cursor(index)) => app.query.move_to(index),
        None => {}
    }
}

/// 結果一覧でのクリックの行き先。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResultsClick {
    Tab(usize),
    Play(usize),
}

/// タブ行を先に見る。タブ行は結果の格子と重ならないが、既存の選択操作を優先しておく。
fn results_click(app: &App, cell: CellSize, mouse: MouseEvent) -> Option<ResultsClick> {
    if !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
        return None;
    }
    if let Some(index) = tab_at_point(app, mouse.column, mouse.row) {
        return Some(ResultsClick::Tab(index));
    }
    // 描いた後に結果が入れ替わっていると、同じ座標が別の動画を指す。
    // タブ行と違って再生が走ってしまうので、次の描画まで待つ。
    if !app.results_are_drawn() {
        return None;
    }
    result_at_point(app, cell, mouse.column, mouse.row).map(ResultsClick::Play)
}

pub async fn handle_mouse_results(
    app: &mut App,
    mouse: MouseEvent,
    tx: &UnboundedSender<AppEvent>,
    session: &mut Session,
) {
    match results_click(app, cell_size(), mouse) {
        Some(ResultsClick::Tab(index)) => select_tab(app, tx, session, index),
        Some(ResultsClick::Play(index)) => {
            app.set_view_selected(index);
            start_playback(app, tx, session).await;
        }
        None => {}
    }
}

/// チャンネル一覧のクリック。当たり判定は結果一覧と同じで、タブの行き先だけが違う。
pub async fn handle_mouse_channel(
    app: &mut App,
    mouse: MouseEvent,
    tx: &UnboundedSender<AppEvent>,
    session: &mut Session,
) {
    match results_click(app, cell_size(), mouse) {
        Some(ResultsClick::Tab(index)) => select_channel_tab(app, tx, session, index),
        Some(ResultsClick::Play(index)) => {
            app.set_view_selected(index);
            start_playback(app, tx, session).await;
        }
        None => {}
    }
}

// ---- 描画 ----

/// 入力欄のカーソル位置 (0 始まり)。draw と、画像を貼った後の戻し先が同じ計算を使う。
pub fn input_cursor(screen: Rect, query: &QueryEditor) -> (u16, u16) {
    let area = search_areas(screen)[0];
    let column =
        text_width(query.before_cursor()).saturating_sub(query_scroll(query, input_width(area)));
    (cursor_x(area, column), area.y + 1)
}

/// 入力欄の枠の内側の幅 (桁)。文字が並ぶのも送り幅を決めるのもこの幅の中。
fn input_width(area: Rect) -> usize {
    area.width.saturating_sub(2) as usize
}

/// 1 文字の表示幅。入力欄の桁計算はすべてこれを積む。
/// ratatui は書記素クラスタ単位で桁を進めるため、ZWJ 絵文字のように
/// 複数コードポイントで 1 つの書記素になる文字は対象外。
fn char_width(ch: char) -> usize {
    grid::display_width(&ch.to_string())
}

fn text_width(text: &str) -> usize {
    text.chars().map(char_width).sum()
}

/// 横スクロールの送り幅 (桁)。カーソルが枠の内側に入る最小の幅を、
/// 全角を半分に割らないよう文字境界で求める。
fn query_scroll(query: &QueryEditor, width: usize) -> usize {
    let Some(last) = width.checked_sub(1) else {
        return 0;
    };
    let needed = text_width(query.before_cursor()).saturating_sub(last);
    let mut scrolled = 0;
    for ch in query.text().chars() {
        if scrolled >= needed {
            break;
        }
        scrolled += char_width(ch);
    }
    scrolled
}

/// 画面のこの位置にある検索語の文字。検索欄の外では None。
pub fn query_index_at_point(app: &App, column: u16, row: u16) -> Option<usize> {
    let area = search_areas(app.screen)[0];
    if !area.contains(Position::new(column, row)) {
        return None;
    }
    let width = input_width(area);
    // 文字は枠の内側 (x+1) から並ぶ。枠を押したら内側の端を押したものとして扱う。
    let offset =
        (column.saturating_sub(area.x.saturating_add(1)) as usize).min(width.saturating_sub(1));
    Some(query_index_at_column(
        app.query.text(),
        query_scroll(&app.query, width) + offset,
    ))
}

/// 表示幅を積みながら `column` 桁にある文字を探す。末尾より右は文字数を返す。
fn query_index_at_column(text: &str, column: usize) -> usize {
    let mut x = 0;
    for (index, ch) in text.chars().enumerate() {
        x += char_width(ch);
        if column < x {
            return index;
        }
    }
    text.chars().count()
}

/// 選択範囲だけ反転させた入力行。`from` 文字目より前は横スクロールで隠れている。
/// 入力中でなければ反転は出さない。カーソルも出ない欄に選択だけ残ると、
/// どこを編集しているのか分からなくなる。
fn query_line(query: &QueryEditor, from: usize, editing: bool) -> Line<'_> {
    if !editing {
        return Line::from(query.slice_from(from));
    }
    let (head, selected, tail) = query.slices_from(from);
    Line::from(vec![
        Span::raw(head),
        Span::styled(selected, Style::default().add_modifier(Modifier::REVERSED)),
        Span::raw(tail),
    ])
}

pub fn draw_search(frame: &mut Frame, app: &App) {
    let areas = search_areas(frame.area());

    let scrolled = query_index_at_column(
        app.query.text(),
        query_scroll(&app.query, input_width(areas[0])),
    );
    let input = Paragraph::new(query_line(&app.query, scrolled, app.mode == Mode::Input))
        .block(Block::default().borders(Borders::ALL).title(" 検索 "));
    frame.render_widget(input, areas[0]);
    draw_tabs(frame, app, areas[1]);

    // バックグラウンド中のミニプレイヤーは結果一覧の上に重ねて描くだけにする。
    // 上部 MINI_VIDEO_ROWS 分にしか映らないので、一覧側の幅を全高にわたって
    // 狭めると映像の無い下の行に空白の帯が残り続けて崩れて見える (実機で確認した不具合)。
    let video = playing_screen::video_target_area(app, frame.area());
    let results_area = areas[2];

    if app.mode == Mode::Playlists {
        playlists_screen::draw_playlists(frame, app, results_area);
    } else {
        // app.screen は直前の draw の寸法なので、割り付けは今のフレームで組み直す。
        let layout = if app.settings.search.layout == LayoutMode::Grid {
            grid::layout(
                Block::default().borders(Borders::ALL).inner(results_area),
                cell_size(),
                app.view_results().len(),
                app.view_scroll(),
            )
        } else {
            None
        };
        match &layout {
            Some(layout) => draw_grid(frame, app, results_area, layout),
            None => draw_list(frame, app, results_area),
        }
    }

    // embedded の画像は draw の後にメインループが APC で重ねる。text はここでしか描けない。
    if let Some(video) = video
        && app.display == DisplayMode::Text
        && let Some(sink) = &app.video
    {
        sink.render_text(video, frame.buffer_mut());
    }

    draw_footer(frame, app, areas[3], areas[4]);

    if app.mode == Mode::Input {
        frame.set_cursor_position(input_cursor(frame.area(), &app.query));
    }
}

const TAB_GAP: &str = " │ ";

const TAB_MORE_LEFT: &str = "< ";

const TAB_MORE_RIGHT: &str = " >";

/// タブ行に並べる見出しと選択位置。チャンネル閲覧中はチャンネルのタブへ差し替える。
/// 窓の開始位置はカテゴリタブだけが覚える (チャンネルは 3 つなので常に先頭から数える)。
fn tab_row_source(app: &App) -> (Vec<&str>, usize, usize) {
    match &app.channel {
        Some(channel) => (ChannelView::labels().to_vec(), channel.tab.index(), 0),
        None => (app.tabs.labels(), app.tabs.selected(), app.tabs.window()),
    }
}

fn draw_tabs(frame: &mut Frame, app: &App, area: Rect) {
    let (labels, selected, window) = tab_row_source(app);
    let width = area.width as usize;
    let range = visible_tabs(&labels, selected, width, window);
    if app.channel.is_none() {
        app.tabs.remember_window(range.start);
    }
    let spans = tab_spans(&labels, selected, width, range);
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// `labels[start..end]` を並べたときの行の幅。両端のマーカーぶんも数える。
fn tab_row_width(labels: &[&str], start: usize, end: usize) -> usize {
    let mut width: usize = labels[start..end]
        .iter()
        .map(|label| grid::display_width(label))
        .sum();
    width += grid::display_width(TAB_GAP) * (end - start).saturating_sub(1);
    if start > 0 {
        width += grid::display_width(TAB_MORE_LEFT);
    }
    if end < labels.len() {
        width += grid::display_width(TAB_MORE_RIGHT);
    }
    width
}

/// `start` から右へ詰めたときに入る最後のタブの次。幅が足りなくても 1 つは返す
/// (選択中のタブは切ってでも出す)。
fn tab_window_end(labels: &[&str], start: usize, width: usize) -> usize {
    let mut end = start + 1;
    while end < labels.len() && tab_row_width(labels, start, end + 1) <= width {
        end += 1;
    }
    end
}

/// 幅に入るぶんだけを切り出す窓。前回の窓 `start` から最小限だけ動かすので、
/// 隣のタブへ移っただけでタブ行全体がずれることがない。
fn visible_tabs(
    labels: &[&str],
    selected: usize,
    width: usize,
    start: usize,
) -> std::ops::Range<usize> {
    if labels.is_empty() {
        return 0..0;
    }
    let selected = selected.min(labels.len() - 1);
    // 選択が窓より左にあるなら、そこまで戻す。
    let mut start = start.min(selected);
    // 選択が窓の右から出ているなら、入るまで 1 つずつ送る。
    while tab_window_end(labels, start, width) <= selected {
        start += 1;
    }
    // 端末が広がったぶんは左へ戻す。右端のタブを失わない間だけ動かす。
    while start > 0
        && tab_window_end(labels, start - 1, width) == tab_window_end(labels, start, width)
    {
        start -= 1;
    }
    start..tab_window_end(labels, start, width)
}

/// タブ行に左から並ぶもの。描画とクリックの当たり判定が同じ並びを通るように、
/// 幅の食い方はここだけで決める。
enum TabPiece {
    /// 窓の外にまだタブがあることを示す印。
    Marker(&'static str),
    Gap,
    Label {
        index: usize,
        text: String,
    },
}

impl TabPiece {
    fn text(&self) -> &str {
        match self {
            TabPiece::Marker(text) => text,
            TabPiece::Gap => TAB_GAP,
            TabPiece::Label { text, .. } => text,
        }
    }
}

fn tab_pieces(labels: &[&str], width: usize, range: std::ops::Range<usize>) -> Vec<TabPiece> {
    if range.is_empty() {
        return Vec::new();
    }
    let mut budget = width;
    let mut pieces = Vec::new();
    // マーカーだけで行を埋めない。1 桁も残らないなら出さない。
    if range.start > 0 && grid::display_width(TAB_MORE_LEFT) < budget {
        budget -= grid::display_width(TAB_MORE_LEFT);
        pieces.push(TabPiece::Marker(TAB_MORE_LEFT));
    }
    let tail = range.end < labels.len() && grid::display_width(TAB_MORE_RIGHT) < budget;
    if tail {
        budget -= grid::display_width(TAB_MORE_RIGHT);
    }
    for index in range.clone() {
        if index > range.start {
            if budget <= grid::display_width(TAB_GAP) {
                break;
            }
            budget -= grid::display_width(TAB_GAP);
            pieces.push(TabPiece::Gap);
        }
        // 窓は選択中のタブを必ず残すので、端末より広いラベルが来るのはそれ 1 つのときだけ。
        let text = grid::truncate(labels[index], budget);
        budget -= grid::display_width(&text);
        pieces.push(TabPiece::Label { index, text });
    }
    if tail {
        pieces.push(TabPiece::Marker(TAB_MORE_RIGHT));
    }
    pieces
}

fn tab_spans(
    labels: &[&str],
    selected: usize,
    width: usize,
    range: std::ops::Range<usize>,
) -> Vec<Span<'static>> {
    let dim = Style::default().fg(Color::DarkGray);
    tab_pieces(labels, width, range)
        .into_iter()
        .map(|piece| match piece {
            TabPiece::Marker(text) => Span::styled(text, dim),
            TabPiece::Gap => Span::styled(TAB_GAP, dim),
            TabPiece::Label { index, text } => {
                let style = if index == selected {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
                } else {
                    Style::default().fg(Color::Gray)
                };
                Span::styled(text, style)
            }
        })
        .collect()
}

/// タブ行の `column` 桁にあるタブ。区切り・マーカー・余白の上では None。
fn tab_at_column(
    labels: &[&str],
    width: usize,
    range: std::ops::Range<usize>,
    column: usize,
) -> Option<usize> {
    let mut x = 0;
    for piece in tab_pieces(labels, width, range) {
        let cells = grid::display_width(piece.text());
        if let TabPiece::Label { index, .. } = piece
            && (x..x + cells).contains(&column)
        {
            return Some(index);
        }
        x += cells;
    }
    None
}

/// 画面のこの位置にあるタブ。タブ行の外や、窓の外へ送ったタブの上では None。
pub fn tab_at_point(app: &App, column: u16, row: u16) -> Option<usize> {
    let area = search_areas(app.screen)[1];
    if !area.contains(Position::new(column, row)) {
        return None;
    }
    let (labels, selected, window) = tab_row_source(app);
    let width = area.width as usize;
    // 窓は draw_tabs が覚えたものをそのまま使う。見えている行と判定をずらさない。
    let range = visible_tabs(&labels, selected, width, window);
    tab_at_column(&labels, width, range, (column - area.x) as usize)
}

/// 画面のこの位置にある結果。格子の隙間や、リスト表示では None。
/// 描画と同じ割り付けを通るので、見えているセルと判定がずれない。
pub fn result_at_point(app: &App, cell: CellSize, column: u16, row: u16) -> Option<usize> {
    let layout = grid_layout(app, cell)?;
    let at = Position::new(column, row);
    layout
        .cells
        .iter()
        .position(|cell| {
            cell.image.contains(at) || cell.title.contains(at) || cell.meta.contains(at)
        })
        .map(|i| layout.offset + i)
}

/// 可視範囲と総数。スクロールしても今どこを見ているか分かるようにする。
fn results_title(offset: usize, shown: usize, total: usize) -> String {
    if total == 0 || shown == 0 {
        return " 結果 ".to_string();
    }
    format!(" 結果 {}-{}/{total} ", offset + 1, offset + shown)
}

/// ショートのタブを見ているか。ショートは行ごとには判定できないので、
/// 見ている画面で決める。
pub fn viewing_shorts(app: &App) -> bool {
    app.mode == Mode::Channel
        && app.channel.as_ref().map(|view| view.tab) == Some(ChannelTab::Shorts)
}

fn draw_grid(frame: &mut Frame, app: &App, area: Rect, layout: &grid::Layout) {
    let results = app.view_results();
    let title = results_title(layout.offset, layout.cells.len(), results.len());
    frame.render_widget(Block::default().borders(Borders::ALL).title(title), area);
    let shorts = viewing_shorts(app);

    for (i, cell) in layout.cells.iter().enumerate() {
        let index = layout.offset + i;
        let Some(result) = results.get(index) else {
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
        // サムネイルが来る前でも種別が分かるよう文字でも出す。来たら APC が上に載る。
        if shorts && cell.image.width > 0 && cell.image.height > 0 {
            let at = Rect::new(cell.image.right() - 1, cell.image.y, 1, 1);
            frame.render_widget(
                Paragraph::new(badge::Badge::Shorts.symbol()).style(
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                at,
            );
        }
        let title_style = if index == app.view_selected() {
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
    let shorts = viewing_shorts(app);
    let items: Vec<ListItem> = app
        .view_results()
        .iter()
        .map(|r| {
            let uploader = r.uploader.as_deref().unwrap_or("-");
            // サムネイルを描かない list 表示では present_thumbs のバッジ (#66) が
            // 乗らないので、行の文字に印を足す。
            let marks: String = shorts
                .then_some(badge::Badge::Shorts)
                .into_iter()
                .chain(badge::badges_for(&app.engagement, r))
                .map(|b| b.symbol())
                .collect();
            let prefix = if marks.is_empty() {
                String::new()
            } else {
                format!("{marks} ")
            };
            ListItem::new(format!(
                "{prefix}{}  {}  [{}]",
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
    if !app.view_results().is_empty() {
        state.select(Some(app.view_selected()));
    }
    frame.render_stateful_widget(list, area, &mut state);
}

/// カーソルの桁。`column` は送り幅を引いた後の、枠の内側での位置。
fn cursor_x(input_area: Rect, column: usize) -> u16 {
    let column = column.min(u16::MAX as usize) as u16;
    input_area
        .x
        .saturating_add(1)
        .saturating_add(column)
        .min(input_area.right().saturating_sub(2))
}

/// 検索入力の案内。先頭 5 つで 75 桁ほどになり、80 桁端末にはそこまでが出る。
/// 入力欄では S も検索語なので、設定は Ctrl+S で開く。
/// 後半の編集キーは 80 桁には入らないので、幅のある端末でだけ出る。
/// バックグラウンド中だけ末尾に Ctrl+B (前面へ戻る) を足す。
pub fn input_hints(background: bool) -> Vec<String> {
    let mut hints = vec![
        "Enter:検索".to_string(),
        "Tab:カテゴリ".to_string(),
        ":yt*:ログイン連動の一覧".to_string(),
        "Esc:結果へ/終了".to_string(),
        "Ctrl+S:設定".to_string(),
        "Ctrl+A:全選択".to_string(),
        "Shift+←→:選択".to_string(),
        "Home/End:先頭/末尾".to_string(),
        "クリック:カーソル".to_string(),
        "Ctrl+P:プレイリスト".to_string(),
    ];
    if background {
        hints.push("Ctrl+B:全画面へ".to_string());
    }
    hints
}

/// 結果一覧の案内。S:設定 までちょうど 80 桁で、80 桁端末にはそこまでが出る。
/// h は押さないと気づけないので、矢印と Esc の言葉を削ってでも入れる。
/// もっと見られる間だけ末尾に m を足す (狭い端末では他より先に落ちる)。
/// バックグラウンド中だけ末尾に b (前面へ戻る) を足す。
/// v (grid/list 切替) は 80 桁に入らないので、m/b と同じく幅のある端末でだけ出る。
pub fn results_hints(can_load_more: bool, background: bool) -> Vec<String> {
    let mut hints = vec![
        "↑↓←→".to_string(),
        "Enter:再生".to_string(),
        "c:チャンネル".to_string(),
        "h:隠す".to_string(),
        "Tab:カテゴリ".to_string(),
        "r:再取得".to_string(),
        "Esc:検索".to_string(),
        "q:終了".to_string(),
        "S:設定".to_string(),
        "p:プレイリスト".to_string(),
    ];
    if can_load_more {
        hints.push("m:もっと見る".to_string());
    }
    if background {
        hints.push("b:全画面へ".to_string());
    }
    hints.push("v:表示切替".to_string());
    hints.push("a:保存".to_string());
    hints
}

/// チャンネル一覧の案内。全部で 79 桁で 80 桁端末に収まる。
/// タブの案内が長いので、矢印は結果一覧の案内に任せて落としてある。
/// h はこのチャンネルごと隠す。バックグラウンド中だけ末尾に b (前面へ戻る) を足す。
/// v (grid/list 切替) は 80 桁に入らないので、幅のある端末でだけ出る。
pub fn channel_hints(background: bool) -> Vec<String> {
    let mut hints = vec![
        "Enter:再生".to_string(),
        "Tab:動画/ショート/配信".to_string(),
        "s:登録".to_string(),
        "h:隠す".to_string(),
        "r:再取得".to_string(),
        "Esc:戻る".to_string(),
        "q:終了".to_string(),
        "S:設定".to_string(),
    ];
    if background {
        hints.push("b:全画面へ".to_string());
    }
    hints.push("v:表示切替".to_string());
    hints.push("a:保存".to_string());
    hints
}

/// プレイリストの動画一覧の案内。タブが無いのでカテゴリの案内は出さない。
/// Esc はプレイリスト一覧へ戻る。v (grid/list 切替) は幅のある端末でだけ出る。
pub fn playlist_hints(background: bool) -> Vec<String> {
    let mut hints = vec![
        "↑↓←→".to_string(),
        "Enter:再生".to_string(),
        "c:チャンネル".to_string(),
        "h:隠す".to_string(),
        "r:再取得".to_string(),
        "Esc:一覧へ".to_string(),
        "q:終了".to_string(),
        "S:設定".to_string(),
    ];
    if background {
        hints.push("b:全画面へ".to_string());
    }
    hints.push("v:表示切替".to_string());
    hints.push("a:保存".to_string());
    hints
}

#[cfg(test)]
mod tests {
    // 観点ごとに画面の組み立て方が違うので、小分けにしてそれぞれにヘルパーを持たせる。
    /// 状態行。
    mod state {
        use super::super::{channel_status, playlist_status, results_status};
        use crate::app::{App, ChannelView, Mode, Playback, PlaylistView};
        use crate::cookies::Target;
        use crate::screen::playlists::PlaylistsView;
        use crate::search::{PlaylistEntry, SearchResult};

        fn search_target() -> Target {
            Target::Search("q".to_string())
        }

        fn result(id: &str) -> SearchResult {
            SearchResult {
                id: id.to_string(),
                title: format!("title {id}"),
                duration: None,
                uploader: None,
                channel_id: None,
                is_live: false,
            }
        }

        /// 検索結果を 2 件持ち、そこからチャンネルを開いた App。
        fn channel_app() -> App {
            let mut app = App::default();
            app.set_results(vec![result("a"), result("b")], &search_target());
            app.selected = 1;
            app.scroll = 4;
            app.store_to_tab();
            app.channel = Some(ChannelView::new(
                "UCabc".to_string(),
                "Some Channel".to_string(),
            ));
            app.mode = Mode::Channel;
            app.sync_from_view();
            app
        }

        fn entry(id: &str, title: &str) -> PlaylistEntry {
            PlaylistEntry {
                id: id.to_string(),
                title: title.to_string(),
            }
        }

        /// 検索結果を 2 件持ち、そこからプレイリストの動画一覧まで開いた App。
        /// 選択位置はまだタブへ書き戻していない。取り込みで巻き戻さないことも見たいため。
        fn playlist_app() -> App {
            let mut app = App::default();
            app.set_results(vec![result("a"), result("b")], &search_target());
            app.selected = 1;
            app.scroll = 4;
            app.playlists = Some(PlaylistsView {
                entries: vec![entry("PL1", "作業用BGM"), entry("PL2", "あとで見る")],
                selected: 0,
                loaded: true,
            });
            app.playlist = Some(PlaylistView::new(
                "PL1".to_string(),
                "作業用BGM".to_string(),
            ));
            app.mode = Mode::Playlist;
            app.sync_from_view();
            app
        }

        #[test]
        fn the_results_status_shows_the_full_title_of_the_selection() {
            // 格子ではタイトルが切り詰められるので、完全なタイトルはここでしか読めない。
            let mut app = App {
                mode: Mode::Results,
                results: vec![result("a"), result("b")],
                selected: 1,
                ..App::default()
            };
            assert_eq!(app.status_line(), "2 件  |  title b");

            app.thumbs.set_fetching(true);
            assert_eq!(app.status_line(), "2 件  |  サムネイル取得中...");

            // 0 件なら件数だけ。
            let app = App {
                mode: Mode::Results,
                ..App::default()
            };
            assert_eq!(app.status_line(), "0 件");
        }

        #[test]
        fn background_marker_leads_the_results_and_channel_status() {
            let app = App {
                mode: Mode::Results,
                background: true,
                results: vec![result("a")],
                playback: Playback {
                    title: "song".to_string(),
                    ..Playback::default()
                },
                ..App::default()
            };
            assert_eq!(app.status_line(), "▶ song  |  1 件  |  title a");

            // バックグラウンドでなければ足さない。
            let app = App {
                background: false,
                ..app
            };
            assert_eq!(app.status_line(), "1 件  |  title a");

            let mut channel = ChannelView::new("UCabc".to_string(), "channel".to_string());
            channel.state_mut().results = vec![result("a")];
            let app = App {
                mode: Mode::Channel,
                background: true,
                channel: Some(channel),
                playback: Playback {
                    title: "song".to_string(),
                    ..Playback::default()
                },
                ..App::default()
            };
            assert_eq!(
                app.status_line(),
                "▶ song  |  channel [動画]  |  1 件  |  title a"
            );
        }

        #[test]
        fn the_status_line_names_the_channel_and_the_tab() {
            let mut app = channel_app();
            app.channel.as_mut().expect("channel").state_mut().results = vec![result("v0")];
            let line = app.status_line();
            assert!(line.contains("Some Channel"), "{line}");
            assert!(line.contains("動画"), "{line}");
            assert!(line.contains("1 件"), "{line}");
        }

        #[test]
        fn the_channel_and_playlist_status_fall_back_to_the_results_without_a_view() {
            let mut app = App::default();
            app.set_results(vec![result("a")], &search_target());
            assert_eq!(channel_status(&app), results_status(&app));
            assert_eq!(playlist_status(&app), results_status(&app));
        }

        #[test]
        fn a_playlist_without_a_name_yet_is_called_a_playlist() {
            let mut app = playlist_app();
            app.playlist.as_mut().expect("playlist").playlist_title = String::new();
            assert!(
                playlist_status(&app).starts_with("プレイリスト  |"),
                "{}",
                playlist_status(&app)
            );
        }

        #[test]
        fn the_status_line_names_the_playlist_and_its_count() {
            let mut app = playlist_app();
            app.playlist.as_mut().expect("playlist").state.results = vec![result("v0")];
            let line = app.status_line();
            assert!(line.contains("作業用BGM"), "{line}");
            assert!(line.contains("1 件"), "{line}");
        }
    }

    /// キーとマウス。
    mod keys {
        use super::super::*;
        use crate::app::{ChannelView, Playback, PlaylistView};
        use crate::category::{Category, Tabs};
        use crate::grid::LayoutMode;
        use crate::input::{handle_key, handle_mouse};
        use crate::oauth::fixtures::FakeBackend;
        use crate::query::QueryEditor;
        use crate::screen::playlists::PlaylistsView;
        use crate::search::{PlaylistEntry, SearchResult};
        use crate::seekbar::SeekBarState;
        use crate::ui;
        use crossterm::event::MouseButton;
        use ratatui::layout::Rect;
        use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

        fn key(code: KeyCode) -> KeyEvent {
            KeyEvent::new(code, KeyModifiers::NONE)
        }

        fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
            MouseEvent {
                kind,
                column,
                row,
                modifiers: KeyModifiers::NONE,
            }
        }

        /// 80x24 の端末で再生中。バー行は y=20、トラック 65 セルで 1 セル 10 秒。
        fn playing_app() -> App {
            App {
                mode: Mode::Playing,
                screen: Rect::new(0, 0, 80, 24),
                playback: Playback {
                    time_pos: Some(0.0),
                    duration: Some(650.0),
                    ..Playback::default()
                },
                ..App::default()
            }
        }

        fn channel() -> (UnboundedSender<AppEvent>, UnboundedReceiver<AppEvent>) {
            unbounded_channel()
        }

        /// 実ブラウザにも Google にも届かない依頼先。置き場も存在しないパスを指す。
        fn fake_oauth() -> Oauth<FakeBackend> {
            let dir = std::env::temp_dir().join(format!("tuitube-input-{}", std::process::id()));
            Oauth {
                backend: FakeBackend::new(),
                paths: Some(crate::oauth::Paths {
                    client: dir.join("absent_client.toml"),
                    token: dir.join("absent_token.toml"),
                }),
            }
        }

        /// 表示モードの自動保存の書き先。利用者の設定を書き換えない一時ファイルを渡す。
        fn temp_config(name: &str) -> std::path::PathBuf {
            let dir =
                std::env::temp_dir().join(format!("tuitube-input-{}-{name}", std::process::id()));
            std::fs::create_dir_all(&dir).expect("temp dir");
            dir.join("config.toml")
        }

        fn result(id: &str) -> SearchResult {
            SearchResult {
                id: id.to_string(),
                title: format!("title {id}"),
                duration: None,
                uploader: None,
                channel_id: None,
                is_live: false,
            }
        }

        /// 積まれた検索タスクを、外部プロセスへ届く前に捨てる。
        /// キーの振り分けだけを見るので、実行者の差し替えはアクション側のテストで確かめる。
        fn take_search(session: &mut Session) -> bool {
            match session.search_task.take() {
                Some(task) => {
                    task.abort();
                    true
                }
                None => false,
            }
        }

        /// 積まれたチャンネル引きを、外部プロセスへ届く前に捨てる。
        fn take_channel_lookup(session: &mut Session) -> bool {
            match session.channel_lookup_task.take() {
                Some(task) => {
                    task.abort();
                    true
                }
                None => false,
            }
        }

        /// 割り付けを端末の申告で揺らさないための寸法。
        const CELL: CellSize = CellSize {
            width_px: 8,
            height_px: 16,
        };

        /// 80x24 の検索画面。CELL なら格子は 4 列 2 行になる。
        fn grid_app(count: usize) -> App {
            let mut app = App {
                mode: Mode::Results,
                screen: Rect::new(0, 0, 80, 24),
                ..App::default()
            };
            let results: Vec<SearchResult> =
                (0..count).map(|i| result(&format!("id{i}"))).collect();
            app.set_results(results, &crate::cookies::Target::Search("q".to_string()));
            // 本番では set_results の後に必ず draw が挟まる。
            app.mark_drawn();
            app
        }

        /// チャンネル一覧を見ている状態。
        fn subscribable_app() -> App {
            let mut app = channel_app(2);
            app.mode = Mode::Channel;
            app
        }

        /// タブ行 (80x24 の端末では y=3) の押し込み。
        fn tab_click(column: u16) -> MouseEvent {
            mouse(MouseEventKind::Down(MouseButton::Left), column, 3)
        }

        /// Shift を押しながらのキー。
        fn shift(code: KeyCode) -> KeyEvent {
            KeyEvent::new(code, KeyModifiers::SHIFT)
        }

        /// 検索語の入った 80x24 の入力モード。
        fn query_app(text: &str) -> App {
            App {
                screen: Rect::new(0, 0, 80, 24),
                query: QueryEditor::from(text),
                ..App::default()
            }
        }

        /// 入力欄 (80x24 の端末では y=1) の押し込み。
        fn box_click(column: u16) -> MouseEvent {
            mouse(MouseEventKind::Down(MouseButton::Left), column, 1)
        }

        /// 格子の `index` 番目のセル (画像の左上) の押し込み。
        /// チャンネルへ移れる結果を 4 件持つ 80x24 の検索結果画面。
        fn channel_source_app() -> App {
            let mut app = App {
                mode: Mode::Results,
                screen: Rect::new(0, 0, 80, 24),
                ..App::default()
            };
            let results: Vec<SearchResult> = (0..4)
                .map(|i| SearchResult {
                    uploader: Some(format!("Channel {i}")),
                    channel_id: Some(format!("UC{i}")),
                    ..result(&format!("id{i}"))
                })
                .collect();
            app.set_results(results, &crate::cookies::Target::Search("q".to_string()));
            app.mark_drawn();
            app
        }

        /// チャンネル一覧を開いて、現在タブに `count` 件持たせた画面。
        fn channel_app(count: usize) -> App {
            let mut app = channel_source_app();
            app.store_to_tab();
            app.channel = Some(ChannelView::new("UC0".to_string(), "Channel 0".to_string()));
            app.mode = Mode::Channel;
            let videos: Vec<SearchResult> = (0..count).map(|i| result(&format!("v{i}"))).collect();
            app.set_results(
                videos,
                &crate::cookies::Target::Channel {
                    id: "UC0".to_string(),
                    tab: crate::cookies::ChannelTab::Videos,
                },
            );
            app.mark_drawn();
            app
        }

        /// プレイリスト一覧を開いた画面。検索結果は控えたまま `count` 件の一覧を出す。
        fn playlists_app(count: usize) -> App {
            let mut app = channel_source_app();
            open_playlists_on(&mut app, count);
            app
        }

        fn open_playlists_on(app: &mut App, count: usize) {
            app.store_to_tab();
            app.playlists = Some(PlaylistsView {
                entries: (0..count)
                    .map(|i| PlaylistEntry {
                        id: format!("PL{i}"),
                        title: format!("list {i}"),
                    })
                    .collect(),
                selected: 0,
                loaded: true,
            });
            app.mode = Mode::Playlists;
        }

        /// 一覧の 1 件目を開いて `count` 件の動画を持たせた画面。
        fn playlist_app(count: usize) -> App {
            let mut app = playlists_app(2);
            app.playlist = Some(PlaylistView::new("PL0".to_string(), "list 0".to_string()));
            app.mode = Mode::Playlist;
            let videos: Vec<SearchResult> = (0..count)
                .map(|i| SearchResult {
                    uploader: Some(format!("Channel {i}")),
                    channel_id: Some(format!("UC{i}")),
                    ..result(&format!("v{i}"))
                })
                .collect();
            app.set_results(videos, &crate::cookies::Target::Playlist("PL0".to_string()));
            app.mark_drawn();
            app
        }

        fn cell_click(app: &App, index: usize) -> MouseEvent {
            let layout = ui::grid_layout(app, CELL).expect("格子を組める");
            let image = layout.cells[index].image;
            mouse(MouseEventKind::Down(MouseButton::Left), image.x, image.y)
        }

        /// 検索のやり直しが届いた状態。描き直す前なので、画面は古い一覧のまま。
        fn swap_in_new_results(app: &mut App) {
            let results: Vec<SearchResult> = (0..10).map(|i| result(&format!("new{i}"))).collect();
            app.set_results(results, &crate::cookies::Target::Search("q".to_string()));
        }

        /// Ctrl を押しながらのキー。
        fn ctrl(code: KeyCode) -> KeyEvent {
            KeyEvent::new(code, KeyModifiers::CONTROL)
        }

        /// 利用者の非表示リストを触らないよう、一時ディレクトリを追記先にする。
        fn hiding_app(name: &str) -> App {
            let dir = std::env::temp_dir().join(format!(
                "tuitube-input-hidden-{}-{name}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("temp dir");
            let mut app = App {
                hidden: crate::hidden::load_from(Some(&dir.join("hidden.toml"))),
                ..App::default()
            };
            let results = vec![
                SearchResult {
                    channel_id: Some("UC1".to_string()),
                    ..result("v1")
                },
                result("v2"),
            ];
            app.set_results(results, &crate::cookies::Target::Search("q".to_string()));
            app
        }

        fn hiding_channel_app(name: &str) -> App {
            let mut app = hiding_app(name);
            app.store_to_tab();
            app.channel = Some(ChannelView::new("UC1".to_string(), "One".to_string()));
            app.mode = Mode::Channel;
            app.sync_from_view();
            app
        }

        #[tokio::test]
        async fn o_is_not_a_comment_key_outside_playback() {
            let (tx, _rx) = channel();
            let mut session = Session::default();

            let mut app = grid_app(1);
            handle_key(&mut app, key(KeyCode::Char('o')), &tx, &mut session).await;
            assert!(!app.comments.visible());
            assert!(!take_search(&mut session));

            // 入力欄では検索語の 1 文字。
            let mut app = App::default();
            handle_key(&mut app, key(KeyCode::Char('o')), &tx, &mut session).await;
            assert_eq!(app.query.text(), "o");
            assert!(!app.comments.visible());

            let mut app = App {
                mode: Mode::Settings,
                ..App::default()
            };
            handle_key(&mut app, key(KeyCode::Char('o')), &tx, &mut session).await;
            assert!(!app.comments.visible());
        }

        #[tokio::test]
        async fn typing_appends_to_the_query_and_clears_the_error() {
            let (tx, _rx) = channel();
            let mut session = Session::default();
            let mut app = App {
                error: Some("boom".to_string()),
                ..App::default()
            };
            handle_key_input(&mut app, key(KeyCode::Char('ラ')), &tx, &mut session).await;
            handle_key_input(&mut app, key(KeyCode::Char('ー')), &tx, &mut session).await;

            assert_eq!(app.query.text(), "ラー");
            assert!(app.error.is_none());
        }

        #[tokio::test]
        async fn backspace_removes_one_character_not_one_byte() {
            let (tx, _rx) = channel();
            let mut session = Session::default();
            let mut app = App {
                query: QueryEditor::from("ラー"),
                ..App::default()
            };
            handle_key_input(&mut app, key(KeyCode::Backspace), &tx, &mut session).await;
            assert_eq!(app.query.text(), "ラ");
            handle_key_input(&mut app, key(KeyCode::Backspace), &tx, &mut session).await;
            handle_key_input(&mut app, key(KeyCode::Backspace), &tx, &mut session).await;
            assert!(app.query.text().is_empty());
        }

        #[tokio::test]
        async fn esc_asks_to_confirm_quit_only_when_there_is_no_result_list_to_go_back_to() {
            let (tx, _rx) = channel();
            let mut session = Session::default();

            let mut app = App::default();
            handle_key_input(&mut app, key(KeyCode::Esc), &tx, &mut session).await;
            assert!(!app.should_quit, "即終了ではなく確認を挟む");
            assert!(app.confirm_quit);
            assert_eq!(app.mode, Mode::Input);

            let mut app = App {
                results: vec![result("a")],
                ..App::default()
            };
            handle_key_input(&mut app, key(KeyCode::Esc), &tx, &mut session).await;
            assert!(!app.should_quit);
            assert!(!app.confirm_quit);
            assert_eq!(app.mode, Mode::Results);
        }

        #[tokio::test]
        async fn a_refused_feed_tab_does_not_turn_esc_into_a_quit() {
            let (tx, _rx) = channel();
            let mut session = Session::default();
            let mut app = App {
                mode: Mode::Results,
                results: vec![result("a")],
                tabs: Tabs::with_categories(vec![Category::new("おすすめ", ":ytrec")]),
                ..App::default()
            };

            // cookie 無しなので断られ、そのタブの結果は空のまま。yt-dlp は起動しない。
            handle_key(&mut app, key(KeyCode::Tab), &tx, &mut session).await;
            assert!(!take_search(&mut session));
            assert!(app.results.is_empty());
            assert!(app.error.is_some());
            assert_eq!(app.mode, Mode::Results, "断りでモードを変えない");

            // ヘルプ通りに Esc を押したら検索欄へ戻るだけ。ここで終了しない。
            handle_key(&mut app, key(KeyCode::Esc), &tx, &mut session).await;
            assert!(!app.should_quit, "タブを送っただけでアプリが落ちる");
            assert_eq!(app.mode, Mode::Input);
        }

        #[tokio::test]
        async fn enter_on_a_playlist_url_opens_that_playlist() {
            let (tx, _rx) = channel();
            let mut session = Session::default();
            let mut app = App {
                query: QueryEditor::from(
                    "https://www.youtube.com/playlist?list=PLFgquLnL59alCl_2TQvOiD5Vgm1hCaGSI",
                ),
                ..App::default()
            };

            handle_key_input(&mut app, key(KeyCode::Enter), &tx, &mut session).await;

            assert_eq!(app.mode, Mode::Playlist);
            let playlist = app.playlist.as_ref().expect("中身へ移る");
            assert_eq!(playlist.playlist_id, "PLFgquLnL59alCl_2TQvOiD5Vgm1hCaGSI");
            assert!(take_search(&mut session), "中身を取りに行く");
        }

        #[tokio::test]
        async fn enter_with_a_blank_query_does_not_start_a_search() {
            let (tx, _rx) = channel();
            let mut session = Session::default();
            let mut app = App {
                query: QueryEditor::from("   "),
                ..App::default()
            };
            handle_key_input(&mut app, key(KeyCode::Enter), &tx, &mut session).await;

            assert!(!app.searching);
            assert!(session.search_task.is_none());
        }

        #[tokio::test]
        async fn results_keys_move_the_selection_and_switch_modes() {
            let (tx, _rx) = channel();
            let mut session = Session::default();
            let mut app = App {
                mode: Mode::Results,
                results: vec![result("a"), result("b")],
                error: Some("boom".to_string()),
                ..App::default()
            };

            handle_key_results(&mut app, key(KeyCode::Down), &tx, &mut session).await;
            assert_eq!(app.selected, 1);
            handle_key_results(&mut app, key(KeyCode::Up), &tx, &mut session).await;
            assert_eq!(app.selected, 0);

            handle_key_results(&mut app, key(KeyCode::Char('/')), &tx, &mut session).await;
            assert_eq!(app.mode, Mode::Input);
            assert!(app.error.is_none());
        }

        #[tokio::test]
        async fn q_asks_to_confirm_quit_from_the_result_list() {
            let (tx, _rx) = channel();
            let mut session = Session::default();
            let mut app = App {
                mode: Mode::Results,
                results: vec![result("a")],
                ..App::default()
            };
            handle_key_results(&mut app, key(KeyCode::Char('q')), &tx, &mut session).await;
            assert!(!app.should_quit, "即終了ではなく確認を挟む");
            assert!(app.confirm_quit);
        }

        #[tokio::test]
        async fn b_key_returns_to_playing_only_while_backgrounded() {
            let (tx, _rx) = channel();
            let mut session = Session::default();
            let mut app = App {
                mode: Mode::Results,
                results: vec![result("a")],
                ..App::default()
            };

            // バックグラウンドでなければ何もしない (b は結果一覧の他のキーと衝突しない)。
            handle_key_results(&mut app, key(KeyCode::Char('b')), &tx, &mut session).await;
            assert_eq!(app.mode, Mode::Results);
            assert!(!app.background);

            app.background = true;
            handle_key_results(&mut app, key(KeyCode::Char('b')), &tx, &mut session).await;
            assert_eq!(app.mode, Mode::Playing);
            assert!(!app.background);
        }

        #[tokio::test]
        async fn b_key_returns_to_playing_from_the_channel_list_only_while_backgrounded() {
            let (tx, _rx) = channel();
            let mut session = Session::default();
            let mut app = channel_app(1);

            handle_key_channel(&mut app, key(KeyCode::Char('b')), &tx, &mut session).await;
            assert_eq!(app.mode, Mode::Channel);
            assert!(!app.background);

            app.background = true;
            handle_key_channel(&mut app, key(KeyCode::Char('b')), &tx, &mut session).await;
            assert_eq!(app.mode, Mode::Playing);
            assert!(!app.background);
        }

        #[tokio::test]
        async fn b_key_returns_to_playing_from_a_playlist_only_while_backgrounded() {
            let (tx, _rx) = channel();
            let mut session = Session::default();
            let mut app = playlist_app(2);

            handle_key(&mut app, key(KeyCode::Char('b')), &tx, &mut session).await;
            assert_eq!(app.mode, Mode::Playlist);

            app.background = true;
            handle_key(&mut app, key(KeyCode::Char('b')), &tx, &mut session).await;
            assert_eq!(app.mode, Mode::Playing);
            assert!(!app.background);
        }

        #[tokio::test]
        async fn ctrl_b_returns_to_playing_from_the_input_mode_only_while_backgrounded() {
            let (tx, _rx) = channel();
            let mut session = Session::default();
            let ctrl_b = KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL);

            // 素の b は検索語として入る。
            let mut app = App::default();
            handle_key_input(&mut app, key(KeyCode::Char('b')), &tx, &mut session).await;
            assert_eq!(app.query.text(), "b");

            // Ctrl+B はバックグラウンドでなければ何もしない。
            handle_key_input(&mut app, ctrl_b, &tx, &mut session).await;
            assert_eq!(app.mode, Mode::Input);

            app.background = true;
            handle_key_input(&mut app, ctrl_b, &tx, &mut session).await;
            assert_eq!(app.mode, Mode::Playing);
            assert!(!app.background);
        }

        #[tokio::test]
        async fn speed_keys_change_only_while_playing() {
            let (tx, _rx) = channel();
            let mut session = Session::default();

            let mut app = App {
                mode: Mode::Results,
                results: vec![result("a")],
                ..App::default()
            };
            handle_key(&mut app, key(KeyCode::Char(']')), &tx, &mut session).await;
            assert_eq!(app.speed, crate::speed::Speed::NORMAL);

            // 入力モードでは検索語の文字として入る。
            let mut app = App::default();
            handle_key(&mut app, key(KeyCode::Char(']')), &tx, &mut session).await;
            assert_eq!(app.speed, crate::speed::Speed::NORMAL);
            assert_eq!(app.query.text(), "]");
        }

        #[tokio::test]
        async fn w_toggles_only_while_playing() {
            let (tx, _rx) = channel();
            let mut session = Session::default();

            let mut app = App {
                mode: Mode::Results,
                results: vec![result("a")],
                ..App::default()
            };
            handle_key(&mut app, key(KeyCode::Char('w')), &tx, &mut session).await;
            assert_eq!(app.display, crate::display::DisplayMode::Embedded);

            let mut app = App::default();
            handle_key(&mut app, key(KeyCode::Char('w')), &tx, &mut session).await;
            assert_eq!(app.display, crate::display::DisplayMode::Embedded);
            assert_eq!(app.query.text(), "w", "入力モードでは文字として入る");
        }

        #[tokio::test]
        async fn v_toggles_the_layout_in_the_results_screen() {
            let (tx, _rx) = channel();
            let mut session = Session::default();
            let mut app = grid_app(2);
            let path = temp_config("toggle-layout-results");

            handle_key_results_with(
                &mut app,
                key(KeyCode::Char('v')),
                &tx,
                &mut session,
                fake_oauth(),
                Some(&path),
            )
            .await;

            assert_eq!(app.settings.search.layout, LayoutMode::List);
            let written = std::fs::read_to_string(&path).expect("読める");
            assert!(written.contains("layout = \"list\""), "{written}");
        }

        #[tokio::test]
        async fn v_toggles_the_layout_in_a_channel() {
            let (tx, _rx) = channel();
            let mut session = Session::default();
            let mut app = subscribable_app();
            let path = temp_config("toggle-layout-channel");

            handle_key_channel_with(
                &mut app,
                key(KeyCode::Char('v')),
                &tx,
                &mut session,
                fake_oauth(),
                Some(&path),
            )
            .await;

            assert_eq!(app.settings.search.layout, LayoutMode::List);
            assert!(session.oauth_task.is_none(), "v で登録が走らない");
        }

        #[tokio::test]
        async fn v_toggles_the_layout_in_a_playlist() {
            let (tx, _rx) = channel();
            let mut session = Session::default();
            let mut app = playlist_app(2);
            let path = temp_config("toggle-layout-playlist");

            handle_key_playlist_with(
                &mut app,
                key(KeyCode::Char('v')),
                &tx,
                &mut session,
                fake_oauth(),
                Some(&path),
            )
            .await;

            assert_eq!(app.settings.search.layout, LayoutMode::List);
        }

        #[tokio::test]
        async fn s_on_a_channel_starts_the_subscription() {
            let (tx, _rx) = channel();
            let mut session = Session::default();
            let mut app = subscribable_app();

            handle_key_channel_with(
                &mut app,
                key(KeyCode::Char('s')),
                &tx,
                &mut session,
                fake_oauth(),
                None,
            )
            .await;

            assert_eq!(app.notice.as_deref(), Some(crate::oauth::SUBSCRIBE_NOTICE));
            assert!(session.oauth_task.is_some(), "送信が積まれている");
            assert!(app.notice_until.is_none(), "認可を待つので期限では消さない");
        }

        #[tokio::test]
        async fn capital_s_in_a_channel_still_opens_the_settings() {
            let (tx, _rx) = channel();
            let mut session = Session::default();
            let mut app = subscribable_app();

            handle_key_channel_with(
                &mut app,
                key(KeyCode::Char('S')),
                &tx,
                &mut session,
                fake_oauth(),
                None,
            )
            .await;

            assert_eq!(app.mode, Mode::Settings);
            assert!(session.oauth_task.is_none(), "登録は始めない");
        }

        #[tokio::test]
        async fn control_s_in_a_channel_opens_the_settings_instead_of_subscribing() {
            let (tx, _rx) = channel();
            let mut session = Session::default();
            let mut app = subscribable_app();
            let mut pressed = key(KeyCode::Char('s'));
            pressed.modifiers = KeyModifiers::CONTROL;

            handle_key_channel_with(&mut app, pressed, &tx, &mut session, fake_oauth(), None).await;

            assert_eq!(app.mode, Mode::Settings);
            assert!(session.oauth_task.is_none(), "登録は始めない");
        }

        #[tokio::test]
        async fn the_other_channel_keys_do_not_subscribe() {
            let (tx, _rx) = channel();
            for code in [
                KeyCode::Char('r'),
                KeyCode::Char('c'),
                KeyCode::Char('l'),
                KeyCode::Down,
                KeyCode::Up,
            ] {
                let mut session = Session::default();
                let mut app = subscribable_app();
                handle_key_channel_with(&mut app, key(code), &tx, &mut session, fake_oauth(), None)
                    .await;
                assert!(session.oauth_task.is_none(), "{code:?} で登録が走った");
            }
        }

        #[tokio::test]
        async fn l_is_a_plain_character_outside_of_playback() {
            let (tx, _rx) = channel();
            let mut session = Session::default();

            let mut app = App::default();
            handle_key(&mut app, key(KeyCode::Char('l')), &tx, &mut session).await;
            assert_eq!(app.query.text(), "l");
            assert!(session.oauth_task.is_none());
        }

        #[tokio::test]
        async fn s_is_a_plain_character_outside_of_playback() {
            let (tx, _rx) = channel();
            let mut session = Session::default();

            // 検索入力中は検索語の文字として入る。
            let mut app = App::default();
            handle_key(&mut app, key(KeyCode::Char('s')), &tx, &mut session).await;
            assert_eq!(app.query.text(), "s");
            assert!(app.subtitles.wanted());

            // 結果一覧では何も起きない。
            let mut app = App {
                mode: Mode::Results,
                results: vec![result("a")],
                ..App::default()
            };
            handle_key(&mut app, key(KeyCode::Char('s')), &tx, &mut session).await;
            assert_eq!(app.mode, Mode::Results);
            assert!(app.subtitles.wanted());
            assert!(app.notice.is_none());
        }

        #[tokio::test]
        async fn c_is_typed_into_the_query_in_the_input_mode() {
            let (tx, _rx) = channel();
            let mut session = Session::default();
            let mut app = App::default();
            handle_key(&mut app, key(KeyCode::Char('c')), &tx, &mut session).await;

            assert_eq!(app.query.text(), "c");
            assert!(app.notice.is_none());
        }

        #[tokio::test]
        async fn outside_playing_mode_the_mouse_only_picks_tabs() {
            let (tx, _rx) = channel();
            let mut session = Session::default();
            let mut app = App {
                mode: Mode::Results,
                ..playing_app()
            };

            // シークバーの行を押しても再生位置は動かない。
            let down = mouse(MouseEventKind::Down(MouseButton::Left), 0, 20);
            handle_mouse(&mut app, down, &tx, &mut session).await;
            let up = mouse(MouseEventKind::Up(MouseButton::Left), 13, 20);
            handle_mouse(&mut app, up, &tx, &mut session).await;

            assert_eq!(app.seek_bar, SeekBarState::default());
            assert_eq!(app.playback.time_pos, Some(0.0));
            assert_eq!(app.tabs.selected(), 0);

            // タブ行のクリックだけが通る。
            handle_mouse(&mut app, tab_click(9), &tx, &mut session).await;
            assert_eq!(app.tabs.selected(), 1);
            take_search(&mut session);
            assert_eq!(app.seek_bar, SeekBarState::default());
        }

        #[tokio::test]
        async fn clicking_a_tab_selects_it_in_the_input_mode() {
            let (tx, _rx) = channel();
            let mut session = Session::default();
            let mut app = App {
                screen: Rect::new(0, 0, 80, 24),
                ..App::default()
            };

            // 「すべて」が 0-5 桁、区切りを挟んで「音楽」が 9-12 桁。
            handle_mouse(&mut app, tab_click(9), &tx, &mut session).await;
            assert_eq!(app.tabs.selected(), 1);
            assert!(take_search(&mut session), "未読のタブは検索する");
            assert_eq!(app.mode, Mode::Input, "モードは変えない");

            handle_mouse(&mut app, tab_click(0), &tx, &mut session).await;
            assert!(app.tabs.is_all());
            take_search(&mut session);
        }

        #[tokio::test]
        async fn clicking_a_tab_selects_it_in_the_results_mode() {
            let (tx, _rx) = channel();
            let mut session = Session::default();
            let mut app = grid_app(4);

            handle_mouse(&mut app, tab_click(9), &tx, &mut session).await;
            assert_eq!(app.tabs.selected(), 1);
            assert!(take_search(&mut session), "未読のタブは検索する");

            // 戻れば保持していた結果をそのまま出す。
            handle_mouse(&mut app, tab_click(0), &tx, &mut session).await;
            assert!(app.tabs.is_all());
            assert!(!take_search(&mut session));
            assert_eq!(app.results.len(), 4);
        }

        #[tokio::test]
        async fn clicking_a_separator_or_outside_the_tab_row_changes_nothing() {
            let (tx, _rx) = channel();
            let mut session = Session::default();
            let mut app = grid_app(4);

            // 区切りの上、入力ボックス、結果ブロックの枠、行の右の余白。
            let spots = [
                tab_click(7),
                mouse(MouseEventKind::Down(MouseButton::Left), 9, 1),
                mouse(MouseEventKind::Down(MouseButton::Left), 0, 10),
                tab_click(79),
            ];
            for spot in spots {
                handle_mouse(&mut app, spot, &tx, &mut session).await;
                assert_eq!(app.tabs.selected(), 0, "{spot:?}");
                assert!(!take_search(&mut session), "{spot:?}");
            }
        }

        #[tokio::test]
        async fn only_a_left_press_on_the_tab_row_changes_the_tab() {
            let (tx, _rx) = channel();
            let mut session = Session::default();
            let mut app = grid_app(4);

            let kinds = [
                MouseEventKind::Down(MouseButton::Right),
                MouseEventKind::Drag(MouseButton::Left),
                MouseEventKind::Up(MouseButton::Left),
                MouseEventKind::Moved,
                MouseEventKind::ScrollDown,
            ];
            for kind in kinds {
                handle_mouse(&mut app, mouse(kind, 9, 3), &tx, &mut session).await;
                assert_eq!(app.tabs.selected(), 0, "{kind:?}");
                assert!(!take_search(&mut session), "{kind:?}");
            }
        }

        #[tokio::test]
        async fn arrows_move_the_cursor_and_typing_lands_there() {
            let (tx, _rx) = channel();
            let mut session = Session::default();
            let mut app = query_app("ラーメン");

            handle_key(&mut app, key(KeyCode::Left), &tx, &mut session).await;
            handle_key(&mut app, key(KeyCode::Left), &tx, &mut session).await;
            handle_key(&mut app, key(KeyCode::Char('丼')), &tx, &mut session).await;
            assert_eq!(app.query.text(), "ラー丼メン");

            handle_key(&mut app, key(KeyCode::Right), &tx, &mut session).await;
            handle_key(&mut app, key(KeyCode::Backspace), &tx, &mut session).await;
            assert_eq!(app.query.text(), "ラー丼ン", "カーソルの前を消す");
        }

        #[tokio::test]
        async fn home_and_end_jump_to_the_edges_of_the_query() {
            let (tx, _rx) = channel();
            let mut session = Session::default();
            let mut app = query_app("ラーメン");

            handle_key(&mut app, key(KeyCode::Home), &tx, &mut session).await;
            handle_key(&mut app, key(KeyCode::Char('大')), &tx, &mut session).await;
            assert_eq!(app.query.text(), "大ラーメン");

            handle_key(&mut app, key(KeyCode::End), &tx, &mut session).await;
            handle_key(&mut app, key(KeyCode::Char('丼')), &tx, &mut session).await;
            assert_eq!(app.query.text(), "大ラーメン丼");
        }

        #[tokio::test]
        async fn cursor_keys_on_an_empty_query_change_nothing() {
            let (tx, _rx) = channel();
            let mut session = Session::default();
            let mut app = App::default();

            for code in [KeyCode::Home, KeyCode::End, KeyCode::Left, KeyCode::Right] {
                handle_key(&mut app, key(code), &tx, &mut session).await;
                handle_key(&mut app, shift(code), &tx, &mut session).await;
            }
            assert!(app.query.text().is_empty());
            assert_eq!(app.mode, Mode::Input);
            assert!(!app.should_quit);
        }

        #[tokio::test]
        async fn shift_arrows_select_and_typing_replaces_the_selection() {
            let (tx, _rx) = channel();
            let mut session = Session::default();
            let mut app = query_app("ラーメン");

            handle_key(&mut app, shift(KeyCode::Left), &tx, &mut session).await;
            handle_key(&mut app, shift(KeyCode::Left), &tx, &mut session).await;
            handle_key(&mut app, key(KeyCode::Char('丼')), &tx, &mut session).await;
            assert_eq!(app.query.text(), "ラー丼");
        }

        #[tokio::test]
        async fn shift_home_and_shift_end_select_up_to_the_edges() {
            let (tx, _rx) = channel();
            let mut session = Session::default();

            let mut app = query_app("ラーメン");
            handle_key(&mut app, shift(KeyCode::Home), &tx, &mut session).await;
            handle_key(&mut app, key(KeyCode::Backspace), &tx, &mut session).await;
            assert!(app.query.text().is_empty(), "末尾から先頭まで消える");

            let mut app = query_app("ラーメン");
            handle_key(&mut app, key(KeyCode::Home), &tx, &mut session).await;
            handle_key(&mut app, shift(KeyCode::End), &tx, &mut session).await;
            handle_key(&mut app, key(KeyCode::Backspace), &tx, &mut session).await;
            assert!(app.query.text().is_empty(), "先頭から末尾まで消える");
        }

        #[tokio::test]
        async fn an_arrow_without_shift_drops_the_selection() {
            let (tx, _rx) = channel();
            let mut session = Session::default();
            let mut app = query_app("ラーメン");

            handle_key(&mut app, shift(KeyCode::Left), &tx, &mut session).await;
            handle_key(&mut app, shift(KeyCode::Left), &tx, &mut session).await;
            handle_key(&mut app, key(KeyCode::Left), &tx, &mut session).await;
            handle_key(&mut app, key(KeyCode::Backspace), &tx, &mut session).await;
            assert_eq!(app.query.text(), "ラメン", "選択は消さず 1 文字だけ消える");
        }

        #[tokio::test]
        async fn ctrl_a_selects_everything_without_typing_an_a() {
            let (tx, _rx) = channel();
            let mut session = Session::default();
            let mut app = query_app("ラーメン");

            handle_key(&mut app, ctrl(KeyCode::Char('a')), &tx, &mut session).await;
            assert_eq!(app.query.text(), "ラーメン", "a は検索語に入れない");

            handle_key(&mut app, key(KeyCode::Char('丼')), &tx, &mut session).await;
            assert_eq!(app.query.text(), "丼", "全選択のうえ打てば入れ替わる");
        }

        #[tokio::test]
        async fn backspace_deletes_the_whole_selection() {
            let (tx, _rx) = channel();
            let mut session = Session::default();
            let mut app = query_app("ラーメン");

            handle_key(&mut app, ctrl(KeyCode::Char('a')), &tx, &mut session).await;
            handle_key(&mut app, key(KeyCode::Backspace), &tx, &mut session).await;
            assert!(app.query.text().is_empty());

            handle_key(&mut app, key(KeyCode::Backspace), &tx, &mut session).await;
            assert!(app.query.text().is_empty(), "空でも落ちない");
        }

        #[tokio::test]
        async fn clicking_the_search_box_moves_the_cursor_there() {
            let (tx, _rx) = channel();
            let mut session = Session::default();
            let mut app = query_app("ラーメン");

            // 全角 1 文字が 2 桁。枠の内側 x=1 から数えて x=3 は 2 文字目の頭。
            handle_mouse(&mut app, box_click(3), &tx, &mut session).await;
            handle_key(&mut app, key(KeyCode::Char('丼')), &tx, &mut session).await;

            assert_eq!(app.query.text(), "ラ丼ーメン");
            assert_eq!(app.tabs.selected(), 0, "タブは動かさない");
            assert!(!take_search(&mut session), "検索も走らせない");
        }

        #[tokio::test]
        async fn clicking_past_the_end_of_the_query_puts_the_cursor_at_the_end() {
            let (tx, _rx) = channel();
            let mut session = Session::default();
            let mut app = query_app("ラーメン");

            handle_key(&mut app, key(KeyCode::Home), &tx, &mut session).await;
            handle_mouse(&mut app, box_click(60), &tx, &mut session).await;
            handle_key(&mut app, key(KeyCode::Char('丼')), &tx, &mut session).await;

            assert_eq!(app.query.text(), "ラーメン丼");
        }

        #[tokio::test]
        async fn clicking_the_search_box_drops_the_selection() {
            let (tx, _rx) = channel();
            let mut session = Session::default();
            let mut app = query_app("ラーメン");

            handle_key(&mut app, ctrl(KeyCode::Char('a')), &tx, &mut session).await;
            handle_mouse(&mut app, box_click(1), &tx, &mut session).await;
            handle_key(&mut app, key(KeyCode::Char('丼')), &tx, &mut session).await;

            assert_eq!(app.query.text(), "丼ラーメン", "全置換にはならない");
        }

        #[tokio::test]
        async fn only_a_left_press_in_the_search_box_moves_the_cursor() {
            let (tx, _rx) = channel();
            let mut session = Session::default();
            let kinds = [
                MouseEventKind::Down(MouseButton::Right),
                MouseEventKind::Drag(MouseButton::Left),
                MouseEventKind::Up(MouseButton::Left),
                MouseEventKind::Moved,
                MouseEventKind::ScrollDown,
            ];
            for kind in kinds {
                let mut app = query_app("ラーメン");
                handle_mouse(&mut app, mouse(kind, 1, 1), &tx, &mut session).await;
                handle_key(&mut app, key(KeyCode::Char('丼')), &tx, &mut session).await;
                assert_eq!(app.query.text(), "ラーメン丼", "{kind:?}");
            }
        }

        #[tokio::test]
        async fn c_opens_the_channel_of_the_selected_result() {
            let (tx, _rx) = channel();
            let mut session = Session::default();
            let mut app = channel_source_app();
            app.selected = 2;

            handle_key(&mut app, key(KeyCode::Char('c')), &tx, &mut session).await;

            assert_eq!(app.mode, Mode::Channel);
            let view = app.channel.as_ref().expect("チャンネルへ移る");
            assert_eq!(view.channel_id, "UC2");
            assert_eq!(view.channel_title, "Channel 2");
            assert!(take_search(&mut session), "チャンネルの一覧を取りに行く");
        }

        #[tokio::test]
        async fn c_looks_the_channel_up_when_the_result_has_no_channel_id() {
            let (tx, _rx) = channel();
            let mut session = Session::default();
            let mut app = grid_app(4);
            app.selected = 2;

            handle_key(&mut app, key(KeyCode::Char('c')), &tx, &mut session).await;

            assert!(app.channel.is_none(), "引き終わるまでは移らない");
            assert_eq!(app.mode, Mode::Results, "結果一覧のまま");
            assert_eq!(app.selected, 2, "選択も動かさない");
            assert!(!take_search(&mut session), "一覧の検索は投げ直さない");
            assert!(take_channel_lookup(&mut session), "その 1 本を引きに行く");
        }

        #[tokio::test]
        async fn tab_moves_between_the_channel_tabs() {
            let (tx, _rx) = channel();
            let mut session = Session::default();
            let mut app = channel_app(4);

            handle_key(&mut app, key(KeyCode::Tab), &tx, &mut session).await;
            assert_eq!(
                app.channel.as_ref().expect("channel").tab,
                crate::cookies::ChannelTab::Shorts
            );
            assert!(take_search(&mut session), "未読のタブは取りに行く");
            assert_eq!(app.tabs.selected(), 0, "カテゴリタブは動かさない");

            handle_key(&mut app, key(KeyCode::BackTab), &tx, &mut session).await;
            assert_eq!(
                app.channel.as_ref().expect("channel").tab,
                crate::cookies::ChannelTab::Videos
            );
            assert!(!take_search(&mut session), "読み込み済みは投げ直さない");
        }

        #[tokio::test]
        async fn the_arrow_keys_move_inside_the_channel_list() {
            let (tx, _rx) = channel();
            let mut session = Session::default();
            let mut app = channel_app(6);

            handle_key(&mut app, key(KeyCode::Right), &tx, &mut session).await;
            assert_eq!(app.view_selected(), 1);
            handle_key(&mut app, key(KeyCode::Down), &tx, &mut session).await;
            assert_eq!(app.view_selected(), 5);
            handle_key(&mut app, key(KeyCode::Left), &tx, &mut session).await;
            assert_eq!(app.view_selected(), 4);
            handle_key(&mut app, key(KeyCode::Up), &tx, &mut session).await;
            assert_eq!(app.view_selected(), 0);
            assert_eq!(app.selected, 0, "検索結果側の選択は触らない");
        }

        #[tokio::test]
        async fn esc_and_slash_go_back_to_the_search_results() {
            for code in [KeyCode::Esc, KeyCode::Char('/')] {
                let (tx, _rx) = channel();
                let mut session = Session::default();
                let mut app = channel_app(4);

                handle_key(&mut app, key(code), &tx, &mut session).await;

                assert!(app.channel.is_none(), "{code:?}");
                assert_eq!(app.mode, Mode::Results, "{code:?}");
                assert_eq!(app.view_result_ids(), ["id0", "id1", "id2", "id3"]);
                assert!(!app.should_quit);
            }
        }

        #[tokio::test]
        async fn q_asks_to_confirm_quit_from_the_channel_list() {
            let (tx, _rx) = channel();
            let mut session = Session::default();
            let mut app = channel_app(4);

            handle_key(&mut app, key(KeyCode::Char('q')), &tx, &mut session).await;
            assert!(!app.should_quit, "即終了ではなく確認を挟む");
            assert!(app.confirm_quit);
        }

        #[tokio::test]
        async fn r_takes_the_channel_tab_again() {
            // 取りそこねたタブは読み込み済みのまま残るので、タブを送り直しても取り直せない。
            let (tx, _rx) = channel();
            let mut session = Session::default();
            let mut app = channel_app(4);
            assert!(app.channel.as_ref().expect("channel").state().loaded);

            handle_key(&mut app, key(KeyCode::Char('r')), &tx, &mut session).await;

            assert!(take_search(&mut session), "同じタブを取りに行く");
            assert_eq!(app.mode, Mode::Channel);
            assert_eq!(app.tabs.selected(), 0, "カテゴリタブは動かさない");
        }

        #[tokio::test]
        async fn ctrl_keys_do_not_work_the_channel_list() {
            let (tx, _rx) = channel();
            let mut session = Session::default();
            let mut app = channel_app(4);

            for code in [KeyCode::Char('r'), KeyCode::Char('q')] {
                handle_key_channel(&mut app, ctrl(code), &tx, &mut session).await;
                assert!(!take_search(&mut session), "{code:?}");
                assert!(!app.should_quit, "{code:?}");
            }
        }

        #[tokio::test]
        async fn ctrl_keys_do_not_work_the_results_list() {
            let (tx, _rx) = channel();
            let mut session = Session::default();
            let mut app = channel_source_app();

            // Ctrl+C は handle_key が終了として先に捌くので、ここでは通らないことだけ見る。
            for code in [KeyCode::Char('c'), KeyCode::Char('r'), KeyCode::Char('q')] {
                handle_key_results(&mut app, ctrl(code), &tx, &mut session).await;
                assert!(app.channel.is_none(), "{code:?}");
                assert!(!take_search(&mut session), "{code:?}");
                assert!(!take_channel_lookup(&mut session), "{code:?}");
                assert!(!app.should_quit, "{code:?}");
            }
        }

        #[tokio::test]
        async fn ctrl_s_still_opens_the_settings_from_both_lists() {
            for mut app in [channel_source_app(), channel_app(4)] {
                let (tx, _rx) = channel();
                let mut session = Session::default();
                let back = app.mode;

                handle_key(&mut app, ctrl(KeyCode::Char('s')), &tx, &mut session).await;

                assert_eq!(app.mode, Mode::Settings, "{back:?}");
            }
        }

        #[tokio::test]
        async fn p_opens_the_playlists_from_the_results() {
            let (tx, _rx) = channel();
            let mut session = Session::default();
            let mut app = grid_app(4);
            app.selected = 2;

            handle_key(&mut app, key(KeyCode::Char('p')), &tx, &mut session).await;

            assert_eq!(app.mode, Mode::Playlists);
            assert!(app.playlists.is_some(), "一覧の入れ物を開く");
            assert!(take_search(&mut session), "一覧を取りに行く");
            assert_eq!(app.result_ids(), ["id0", "id1", "id2", "id3"], "結果は残す");
        }

        #[tokio::test]
        async fn ctrl_p_opens_the_playlists_from_the_search_box() {
            let (tx, _rx) = channel();
            let mut session = Session::default();
            let mut app = App::default();

            // 入力欄では素の p は検索語なので、Ctrl+P で開く。
            handle_key(&mut app, key(KeyCode::Char('p')), &tx, &mut session).await;
            assert_eq!(app.mode, Mode::Input);
            assert_eq!(app.query.text(), "p");

            handle_key(&mut app, ctrl(KeyCode::Char('p')), &tx, &mut session).await;

            assert_eq!(app.mode, Mode::Playlists);
            assert_eq!(app.query.text(), "p", "検索語には足さない");
            assert!(take_search(&mut session), "一覧を取りに行く");
        }

        #[tokio::test]
        async fn the_arrow_keys_move_the_selection_inside_a_playlist() {
            let (tx, _rx) = channel();
            let mut session = Session::default();
            let mut app = playlist_app(6);

            handle_key(&mut app, key(KeyCode::Right), &tx, &mut session).await;
            assert_eq!(app.view_selected(), 1);
            handle_key(&mut app, key(KeyCode::Down), &tx, &mut session).await;
            assert_eq!(app.view_selected(), 5);
            handle_key(&mut app, key(KeyCode::Left), &tx, &mut session).await;
            assert_eq!(app.view_selected(), 4);
            handle_key(&mut app, key(KeyCode::Up), &tx, &mut session).await;
            assert_eq!(app.view_selected(), 0);
            assert_eq!(app.selected, 0, "検索結果側の選択は動かさない");
        }

        #[tokio::test]
        async fn esc_and_slash_go_back_to_the_playlists_list() {
            for code in [KeyCode::Esc, KeyCode::Char('/')] {
                let (tx, _rx) = channel();
                let mut session = Session::default();
                let mut app = playlist_app(4);

                handle_key(&mut app, key(code), &tx, &mut session).await;

                assert!(app.playlist.is_none(), "{code:?}");
                assert_eq!(app.mode, Mode::Playlists, "{code:?}");
                assert!(app.playlists.is_some(), "一覧は持ったまま");
                assert!(!take_search(&mut session), "一覧は取り直さない");
            }
        }

        #[tokio::test]
        async fn r_takes_the_playlist_again() {
            let (tx, _rx) = channel();
            let mut session = Session::default();
            let mut app = playlist_app(4);

            handle_key(&mut app, key(KeyCode::Char('r')), &tx, &mut session).await;

            assert!(take_search(&mut session), "同じプレイリストを取りに行く");
            assert_eq!(app.mode, Mode::Playlist);
        }

        #[tokio::test]
        async fn tab_does_nothing_inside_a_playlist() {
            let (tx, _rx) = channel();
            let mut session = Session::default();
            let mut app = playlist_app(4);

            handle_key(&mut app, key(KeyCode::Tab), &tx, &mut session).await;
            handle_key(&mut app, key(KeyCode::BackTab), &tx, &mut session).await;

            assert_eq!(app.mode, Mode::Playlist);
            assert_eq!(app.tabs.selected(), 0, "カテゴリタブは送らない");
            assert!(!take_search(&mut session));
        }

        #[tokio::test]
        async fn c_opens_the_channel_of_the_selected_playlist_video() {
            let (tx, _rx) = channel();
            let mut session = Session::default();
            let mut app = playlist_app(4);
            app.set_view_selected(2);

            handle_key(&mut app, key(KeyCode::Char('c')), &tx, &mut session).await;

            assert_eq!(app.mode, Mode::Channel);
            assert_eq!(app.channel.as_ref().expect("channel").channel_id, "UC2");
            assert!(take_search(&mut session), "チャンネルの一覧を取りに行く");

            handle_key(&mut app, key(KeyCode::Esc), &tx, &mut session).await;
            assert_eq!(app.mode, Mode::Playlist, "Esc でプレイリストへ戻る");
        }

        #[tokio::test]
        async fn d_opens_the_download_screen_from_a_playlist() {
            let (tx, _rx) = channel();
            let mut session = Session::default();
            let mut app = playlist_app(2);

            handle_key(&mut app, key(KeyCode::Char('d')), &tx, &mut session).await;

            assert_eq!(app.mode, Mode::Download);
        }

        #[tokio::test]
        async fn h_hides_the_selected_playlist_video() {
            let (tx, _rx) = channel();
            let mut session = Session::default();
            let mut app = hiding_app("playlist");
            open_playlists_on(&mut app, 1);
            app.playlist = Some(PlaylistView::new("PL0".to_string(), "list 0".to_string()));
            app.mode = Mode::Playlist;
            app.set_results(
                vec![result("v1"), result("v2")],
                &crate::cookies::Target::Playlist("PL0".to_string()),
            );
            app.set_view_selected(1);

            handle_key(&mut app, key(KeyCode::Char('h')), &tx, &mut session).await;

            assert_eq!(app.view_result_ids(), ["v1"], "押した場で消える");
            assert!(app.hidden.videos.contains("v2"));
            assert_eq!(app.mode, Mode::Playlist, "一覧に留まる");
        }

        #[tokio::test]
        async fn ctrl_keys_do_not_work_the_playlist_screens() {
            let (tx, _rx) = channel();
            for mut app in [playlists_app(2), playlist_app(2)] {
                let mut session = Session::default();
                let mode = app.mode;

                for code in [KeyCode::Char('r'), KeyCode::Char('q')] {
                    handle_key(&mut app, ctrl(code), &tx, &mut session).await;
                    assert!(!take_search(&mut session), "{mode:?} {code:?}");
                    assert!(!app.should_quit, "{mode:?} {code:?}");
                    assert!(!app.confirm_quit, "{mode:?} {code:?}");
                }
                assert_eq!(app.mode, mode);
            }
        }

        #[tokio::test]
        async fn q_asks_before_quitting_from_the_playlist_screens() {
            let (tx, _rx) = channel();
            for mut app in [playlists_app(2), playlist_app(2)] {
                let mut session = Session::default();
                let mode = app.mode;

                handle_key(&mut app, key(KeyCode::Char('q')), &tx, &mut session).await;

                assert!(!app.should_quit, "{mode:?}");
                assert!(app.confirm_quit, "{mode:?}");
            }
        }

        #[tokio::test]
        async fn the_settings_open_from_the_playlist_screens() {
            let (tx, _rx) = channel();
            for code in [KeyCode::Char('S'), KeyCode::Char('s')] {
                for mut app in [playlists_app(2), playlist_app(2)] {
                    let mut session = Session::default();
                    let back = app.mode;
                    let pressed = if code == KeyCode::Char('s') {
                        ctrl(code)
                    } else {
                        key(code)
                    };

                    handle_key(&mut app, pressed, &tx, &mut session).await;

                    assert_eq!(app.mode, Mode::Settings, "{back:?} {code:?}");
                }
            }
        }

        #[tokio::test]
        async fn clicking_a_channel_cell_asks_to_play_that_video() {
            let app = channel_app(6);
            let at = cell_click(&app, 2);
            assert_eq!(results_click(&app, CELL, at), Some(ResultsClick::Play(2)));
        }

        #[tokio::test]
        async fn clicking_the_channel_tab_row_switches_the_channel_tab() {
            let (tx, _rx) = channel();
            let mut session = Session::default();
            let mut app = channel_app(4);

            // 「動画 │ ショート」の 2 つめの上。
            handle_mouse(&mut app, tab_click(8), &tx, &mut session).await;

            assert_eq!(
                app.channel.as_ref().expect("channel").tab,
                crate::cookies::ChannelTab::Shorts
            );
            assert_eq!(app.tabs.selected(), 0, "カテゴリタブは動かさない");
            take_search(&mut session);
        }

        #[test]
        fn clicking_a_cell_asks_to_play_that_result() {
            let app = grid_app(10);
            let cells = ui::grid_layout(&app, CELL)
                .expect("格子を組める")
                .cells
                .len();
            assert_eq!(cells, 8, "4 列 2 行");

            for index in 0..cells {
                assert_eq!(
                    results_click(&app, CELL, cell_click(&app, index)),
                    Some(ResultsClick::Play(index)),
                    "{index} 番目のセル"
                );
            }
        }

        #[test]
        fn the_tab_row_wins_over_the_grid() {
            let app = grid_app(10);
            assert_eq!(
                results_click(&app, CELL, tab_click(9)),
                Some(ResultsClick::Tab(1))
            );
        }

        #[test]
        fn only_a_left_press_on_a_cell_asks_for_playback() {
            let app = grid_app(10);
            let at = cell_click(&app, 0);
            let kinds = [
                MouseEventKind::Down(MouseButton::Right),
                MouseEventKind::Drag(MouseButton::Left),
                MouseEventKind::Up(MouseButton::Left),
                MouseEventKind::Moved,
                MouseEventKind::ScrollDown,
            ];
            for kind in kinds {
                assert_eq!(
                    results_click(&app, CELL, mouse(kind, at.column, at.row)),
                    None,
                    "{kind:?}"
                );
            }
        }

        #[test]
        fn clicking_between_the_cells_asks_for_nothing() {
            let app = grid_app(10);
            let layout = ui::grid_layout(&app, CELL).expect("格子を組める");
            let first = layout.cells[0];
            // セルの右の余白、時間の行の下、結果ブロックの枠。
            let spots = [
                (first.image.right(), first.image.y),
                (first.meta.x, first.meta.bottom()),
                (0, first.image.y),
            ];
            for (column, row) in spots {
                let at = mouse(MouseEventKind::Down(MouseButton::Left), column, row);
                assert_eq!(results_click(&app, CELL, at), None, "({column},{row})");
            }
        }

        #[test]
        fn the_list_view_does_not_ask_for_playback() {
            let mut app = grid_app(10);
            let at = cell_click(&app, 0);
            app.settings.search.layout = crate::grid::LayoutMode::List;
            assert_eq!(results_click(&app, CELL, at), None);
        }

        #[test]
        fn a_cell_click_waits_until_the_new_results_are_drawn() {
            let mut app = grid_app(10);
            let at = cell_click(&app, 1);
            swap_in_new_results(&mut app);

            assert_eq!(results_click(&app, CELL, at), None, "描く前のクリック");
            // タブ行は結果の入れ替わりで動かないので、今までどおり通す。
            assert_eq!(
                results_click(&app, CELL, tab_click(9)),
                Some(ResultsClick::Tab(1))
            );

            app.mark_drawn();
            assert_eq!(
                results_click(&app, CELL, at),
                Some(ResultsClick::Play(1)),
                "描き直した後のクリック"
            );
        }

        #[tokio::test]
        async fn a_click_off_the_cells_does_not_start_playback() {
            let (tx, _rx) = channel();
            let mut session = Session::default();
            let mut app = grid_app(10);
            app.selected = 3;

            // 結果ブロックの枠。タブ行にもセルにも当たらない。
            let spot = mouse(MouseEventKind::Down(MouseButton::Left), 0, 10);
            handle_mouse(&mut app, spot, &tx, &mut session).await;

            assert_eq!(app.selected, 3, "選択は動かない");
            assert_eq!(app.mode, Mode::Results);
            assert!(session.player.is_none());
            assert_eq!(session.player_nonce, 0, "再生は始めない");
        }

        #[tokio::test]
        async fn the_list_view_ignores_a_click_on_a_row() {
            let (tx, _rx) = channel();
            let mut session = Session::default();
            let mut app = grid_app(10);
            let at = cell_click(&app, 1);
            app.settings.search.layout = crate::grid::LayoutMode::List;
            app.selected = 3;

            handle_mouse(&mut app, at, &tx, &mut session).await;

            assert_eq!(app.selected, 3);
            assert!(session.player.is_none());
            assert_eq!(session.player_nonce, 0);
        }

        #[tokio::test]
        async fn the_input_screen_still_answers_only_tab_clicks() {
            let (tx, _rx) = channel();
            let mut session = Session::default();
            let mut app = grid_app(10);
            let at = cell_click(&app, 2);
            app.mode = Mode::Input;
            app.selected = 3;

            handle_mouse(&mut app, at, &tx, &mut session).await;
            assert_eq!(app.mode, Mode::Input, "セルを押しても入力欄のまま");
            assert_eq!(app.selected, 3);
            assert_eq!(session.player_nonce, 0);

            handle_mouse(&mut app, tab_click(9), &tx, &mut session).await;
            assert_eq!(app.tabs.selected(), 1, "タブ行は今までどおり");
            take_search(&mut session);
        }

        #[tokio::test]
        async fn a_click_behind_a_finished_search_does_not_start_playback() {
            let (tx, _rx) = channel();
            let mut session = Session::default();
            let mut app = grid_app(10);
            let at = cell_click(&app, 1);
            app.selected = 3;
            swap_in_new_results(&mut app);

            handle_mouse(&mut app, at, &tx, &mut session).await;

            assert_eq!(app.selected, 0, "入れ替えが戻した先から動かない");
            assert!(session.player.is_none());
            assert_eq!(session.player_nonce, 0, "再生は始めない");
        }

        #[tokio::test]
        async fn tab_switches_the_category_in_the_input_mode() {
            let (tx, _rx) = channel();
            let mut session = Session::default();
            let mut app = App::default();

            handle_key_input(&mut app, key(KeyCode::Tab), &tx, &mut session).await;
            assert_eq!(app.tabs.selected(), 1);
            take_search(&mut session);

            handle_key_input(&mut app, key(KeyCode::BackTab), &tx, &mut session).await;
            assert_eq!(app.tabs.selected(), 0);
            take_search(&mut session);
        }

        #[tokio::test]
        async fn tab_switches_the_category_in_the_results_mode() {
            let (tx, _rx) = channel();
            let mut session = Session::default();
            let mut app = grid_app(4);

            handle_key_results(&mut app, key(KeyCode::Tab), &tx, &mut session).await;
            assert_eq!(app.tabs.selected(), 1);
            take_search(&mut session);

            handle_key_results(&mut app, key(KeyCode::BackTab), &tx, &mut session).await;
            assert_eq!(app.tabs.selected(), 0);
            take_search(&mut session);
        }

        #[tokio::test]
        async fn tab_starts_a_search_only_for_a_tab_that_has_not_loaded_yet() {
            let (tx, _rx) = channel();
            let mut session = Session::default();
            let mut app = grid_app(4);

            handle_key_results(&mut app, key(KeyCode::Tab), &tx, &mut session).await;
            assert!(take_search(&mut session), "未読のタブは検索する");

            handle_key_results(&mut app, key(KeyCode::BackTab), &tx, &mut session).await;
            assert!(
                !take_search(&mut session),
                "読み込み済みのタブは保持していた結果を出す"
            );
            assert_eq!(app.results.len(), 4);
        }

        #[tokio::test]
        async fn enter_in_the_input_mode_returns_to_the_all_tab() {
            let (tx, _rx) = channel();
            let mut session = Session::default();
            let mut app = App {
                query: QueryEditor::from("ラーメン"),
                ..App::default()
            };
            handle_key_input(&mut app, key(KeyCode::Tab), &tx, &mut session).await;
            take_search(&mut session);
            assert!(!app.tabs.is_all());

            handle_key_input(&mut app, key(KeyCode::Enter), &tx, &mut session).await;
            assert!(app.tabs.is_all());
            assert!(take_search(&mut session));
        }

        #[tokio::test]
        async fn typing_still_appends_to_the_query_while_a_category_tab_is_selected() {
            let (tx, _rx) = channel();
            let mut session = Session::default();
            let mut app = App::default();
            handle_key_input(&mut app, key(KeyCode::Tab), &tx, &mut session).await;
            take_search(&mut session);

            handle_key_input(&mut app, key(KeyCode::Char('ラ')), &tx, &mut session).await;
            handle_key_input(&mut app, key(KeyCode::Char('ー')), &tx, &mut session).await;
            assert_eq!(app.query.text(), "ラー", "戻れば元の入力が残っている");
            assert!(!app.tabs.is_all());
        }

        #[tokio::test]
        async fn left_and_right_move_inside_the_grid_in_the_results_mode() {
            let (tx, _rx) = channel();
            let mut session = Session::default();
            let mut app = grid_app(10);

            handle_key_results(&mut app, key(KeyCode::Right), &tx, &mut session).await;
            assert_eq!(app.selected, 1);
            handle_key_results(&mut app, key(KeyCode::Down), &tx, &mut session).await;
            assert_eq!(app.selected, 5);
            handle_key_results(&mut app, key(KeyCode::Up), &tx, &mut session).await;
            assert_eq!(app.selected, 1);
            handle_key_results(&mut app, key(KeyCode::Left), &tx, &mut session).await;
            assert_eq!(app.selected, 0);
            // 先頭で ← を押しても巻き戻らない。
            handle_key_results(&mut app, key(KeyCode::Left), &tx, &mut session).await;
            assert_eq!(app.selected, 0);
        }

        #[tokio::test]
        async fn r_reloads_the_current_tab() {
            let (tx, _rx) = channel();
            let mut session = Session::default();
            let mut app = grid_app(4);
            app.query.set("ラーメン");
            assert!(app.tabs.state().loaded);

            handle_key_results(&mut app, key(KeyCode::Char('r')), &tx, &mut session).await;
            assert!(!app.tabs.state().loaded);
            assert!(take_search(&mut session));
        }

        #[tokio::test]
        async fn m_does_nothing_while_the_tab_cannot_load_more() {
            let (tx, _rx) = channel();
            let mut session = Session::default();
            let mut app = grid_app(4);
            app.query.set("ラーメン");
            // requested_limit がまだ立っていない。

            handle_key_results(&mut app, key(KeyCode::Char('m')), &tx, &mut session).await;

            assert!(!take_search(&mut session), "もっと見られない間は動かない");
        }

        #[tokio::test]
        async fn m_loads_more_once_the_tab_can_load_more() {
            let (tx, _rx) = channel();
            let mut session = Session::default();
            let mut app = grid_app(4);
            app.query.set("ラーメン");
            // 前回ちょうど 4 件を要求して満額返ってきた状態。
            app.tabs.state_mut().requested_limit = 4;

            handle_key_results(&mut app, key(KeyCode::Char('m')), &tx, &mut session).await;

            assert!(take_search(&mut session), "もっと見るを取りに行く");
        }

        #[tokio::test]
        async fn down_at_the_last_row_loads_more_instead_of_moving() {
            let (tx, _rx) = channel();
            let mut session = Session::default();
            let mut app = grid_app(8); // 4 列 x 2 行。
            app.query.set("ラーメン");
            app.tabs.state_mut().requested_limit = 8;
            app.selected = 7; // 末尾。

            handle_key_results(&mut app, key(KeyCode::Down), &tx, &mut session).await;

            assert!(take_search(&mut session), "末尾の Down は読み込みを始める");
            assert_eq!(app.selected, 7, "選択はまだ動かさない");
        }

        #[tokio::test]
        async fn down_at_the_last_row_still_moves_when_more_cannot_be_loaded() {
            let (tx, _rx) = channel();
            let mut session = Session::default();
            let mut app = grid_app(8);
            app.query.set("ラーメン");
            // requested_limit を立てていないので can_load_more は false。
            app.selected = 7;

            handle_key_results(&mut app, key(KeyCode::Down), &tx, &mut session).await;

            assert!(
                !take_search(&mut session),
                "もっと見られないので通常の移動のまま"
            );
            assert_eq!(app.selected, 7, "grid は末尾で留まる (今までどおり)");
        }

        #[tokio::test]
        async fn down_before_the_last_row_moves_even_when_more_can_be_loaded() {
            let (tx, _rx) = channel();
            let mut session = Session::default();
            let mut app = grid_app(8);
            app.query.set("ラーメン");
            app.tabs.state_mut().requested_limit = 8;
            app.selected = 0;

            handle_key_results(&mut app, key(KeyCode::Down), &tx, &mut session).await;

            assert!(!take_search(&mut session), "末尾に着くまでは通常の移動");
            assert_eq!(
                app.selected, 4,
                "grid 4 列ぶん進み、2 行目の先頭 (末尾ではない) へ"
            );
        }

        #[tokio::test]
        async fn down_at_the_last_row_does_not_load_more_twice_while_already_searching() {
            let (tx, _rx) = channel();
            let mut session = Session::default();
            let mut app = grid_app(8);
            app.query.set("ラーメン");
            app.tabs.state_mut().requested_limit = 8;
            app.selected = 7;
            app.searching = true; // 直前の Down で既に読み込み中。

            handle_key_results(&mut app, key(KeyCode::Down), &tx, &mut session).await;

            assert!(!take_search(&mut session), "取得中の多重発行を防ぐ");
        }

        #[tokio::test]
        async fn ctrl_s_opens_the_settings_from_both_search_screens() {
            let (tx, _rx) = channel();
            let mut session = Session::default();

            let mut app = App::default();
            handle_key(&mut app, ctrl(KeyCode::Char('s')), &tx, &mut session).await;
            assert_eq!(app.mode, Mode::Settings);
            assert!(app.query.text().is_empty(), "検索語には入れない");

            let mut app = App {
                mode: Mode::Results,
                results: vec![result("a")],
                ..App::default()
            };
            handle_key(&mut app, ctrl(KeyCode::Char('s')), &tx, &mut session).await;
            assert_eq!(app.mode, Mode::Settings);
        }

        #[tokio::test]
        async fn capital_s_is_a_search_character_but_opens_the_settings_from_the_results() {
            let (tx, _rx) = channel();
            let mut session = Session::default();

            // 入力欄では検索語の一部。"SEKIRO" のような語を打てなくなるため拾わない。
            let mut app = App::default();
            for c in "SEKIRO".chars() {
                handle_key(&mut app, key(KeyCode::Char(c)), &tx, &mut session).await;
            }
            assert_eq!(app.query.text(), "SEKIRO");
            assert_eq!(app.mode, Mode::Input);

            // 結果一覧では文字を打たないので、そのまま設定を開く。
            let mut app = App {
                mode: Mode::Results,
                results: vec![result("a")],
                ..App::default()
            };
            handle_key(&mut app, key(KeyCode::Char('S')), &tx, &mut session).await;
            assert_eq!(app.mode, Mode::Settings);
        }

        #[tokio::test]
        async fn other_control_combinations_do_not_reach_the_query() {
            let (tx, _rx) = channel();
            let mut session = Session::default();
            let mut app = App::default();

            for code in [KeyCode::Char('a'), KeyCode::Char('u'), KeyCode::Char('w')] {
                handle_key(&mut app, ctrl(code), &tx, &mut session).await;
            }
            assert!(app.query.text().is_empty(), "制御文字は検索語に入れない");
            assert_eq!(app.mode, Mode::Input);
        }

        #[tokio::test]
        async fn a_saves_the_selected_video_on_the_list_screens() {
            let (tx, _rx) = channel();

            let mut session = Session::default();
            let mut app = grid_app(3);
            app.selected = 1;
            handle_key_results_with(
                &mut app,
                key(KeyCode::Char('a')),
                &tx,
                &mut session,
                fake_oauth(),
                None,
            )
            .await;
            assert_eq!(
                session.oauth_action,
                Some(crate::oauth::Action::Save("id1".to_string()))
            );

            let mut session = Session::default();
            let mut app = channel_app(2);
            handle_key_channel_with(
                &mut app,
                key(KeyCode::Char('a')),
                &tx,
                &mut session,
                fake_oauth(),
                None,
            )
            .await;
            assert!(
                matches!(&session.oauth_action, Some(crate::oauth::Action::Save(_))),
                "{:?}",
                session.oauth_action
            );

            let mut session = Session::default();
            let mut app = playlist_app(2);
            handle_key_playlist_with(
                &mut app,
                key(KeyCode::Char('a')),
                &tx,
                &mut session,
                fake_oauth(),
                None,
            )
            .await;
            assert_eq!(
                session.oauth_action,
                Some(crate::oauth::Action::Save("v0".to_string()))
            );
        }

        #[tokio::test]
        async fn a_on_an_empty_list_saves_nothing() {
            let (tx, _rx) = channel();
            let mut session = Session::default();
            let mut app = App {
                mode: Mode::Results,
                ..App::default()
            };

            handle_key_results_with(
                &mut app,
                key(KeyCode::Char('a')),
                &tx,
                &mut session,
                fake_oauth(),
                None,
            )
            .await;

            assert!(session.oauth_task.is_none());
        }

        #[tokio::test]
        async fn h_hides_the_selected_result() {
            let (tx, _rx) = channel();
            let mut session = Session::default();
            let mut app = hiding_app("results");
            app.selected = 1;

            handle_key_results(&mut app, key(KeyCode::Char('h')), &tx, &mut session).await;

            assert_eq!(app.result_ids(), ["v1"], "押した場で消える");
            assert!(app.hidden.videos.contains("v2"));
            assert_eq!(
                app.notice.as_deref(),
                Some(crate::actions::HIDDEN_VIDEO_NOTICE)
            );
            assert_eq!(app.mode, Mode::Results, "一覧に留まる");
            assert!(!take_search(&mut session), "検索は走らない");
        }

        #[tokio::test]
        async fn h_in_the_results_does_not_open_the_channel() {
            let (tx, _rx) = channel();
            let mut session = Session::default();
            let mut app = hiding_app("results-not-c");

            // c はチャンネルへ移る。h は同じ行に対して非表示にするだけ。
            handle_key_results(&mut app, key(KeyCode::Char('h')), &tx, &mut session).await;

            assert!(app.channel.is_none());
            assert!(!take_channel_lookup(&mut session));
        }

        #[tokio::test]
        async fn h_in_a_channel_hides_it_and_goes_back_to_the_results() {
            let (tx, _rx) = channel();
            let mut session = Session::default();
            let mut app = hiding_channel_app("channel");

            handle_key_channel_with(
                &mut app,
                key(KeyCode::Char('h')),
                &tx,
                &mut session,
                fake_oauth(),
                None,
            )
            .await;

            assert!(app.channel.is_none(), "チャンネルから出る");
            assert_eq!(app.mode, Mode::Results);
            assert!(app.hidden.channels.contains("UC1"));
            assert_eq!(
                app.notice.as_deref(),
                Some(crate::actions::HIDDEN_CHANNEL_NOTICE)
            );
            // 同じチャンネルの動画は検索側の一覧からも消える。
            assert_eq!(app.result_ids(), ["v2"]);
            assert!(session.oauth_task.is_none(), "登録 (s) とは別の操作");
        }

        #[tokio::test]
        async fn d_opens_the_download_screen_from_results() {
            let (tx, _rx) = channel();
            let mut session = Session::default();
            let mut app = grid_app(2);
            app.selected = 1;

            handle_key(&mut app, key(KeyCode::Char('d')), &tx, &mut session).await;

            assert_eq!(app.mode, Mode::Download);
            assert_eq!(app.download.return_mode, Mode::Results);
            assert_eq!(app.download.url, "https://www.youtube.com/watch?v=id1");
        }

        #[tokio::test]
        async fn d_opens_the_download_screen_from_the_channel_view() {
            let (tx, _rx) = channel();
            let mut session = Session::default();
            let mut app = channel_app(2);

            handle_key(&mut app, key(KeyCode::Char('d')), &tx, &mut session).await;

            assert_eq!(app.mode, Mode::Download);
            assert_eq!(app.download.return_mode, Mode::Channel);
        }
    }

    /// 描画、当たり判定、案内。
    mod draw {
        use super::super::*;
        use crate::app::{ChannelView, PlaylistView};
        use crate::badge::Badge;
        use crate::category::Tabs;
        use crate::screen::playlists::PlaylistsView;
        use crate::search::{PlaylistEntry, SearchResult};
        use crate::ui::{draw, grid_layout, help_text, results_inner, search_areas};
        use crate::video::{Geometry, VideoSink};

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

        /// 検索欄を持つ 40x24 の画面。
        fn input_app(text: &str) -> App {
            sized_input_app(text, 40)
        }

        /// 検索欄を持つ幅 `width` の画面。横スクロールの検証に使う。
        fn sized_input_app(text: &str, width: u16) -> App {
            App {
                screen: Rect::new(0, 0, width, 24),
                query: QueryEditor::from(text),
                ..App::default()
            }
        }

        /// 入力欄の行に描かれている文字。
        fn drawn_input_text(app: &App) -> String {
            let width = app.screen.width;
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, 24))
                    .expect("端末");
            terminal.draw(|frame| draw(frame, app)).expect("描ける");
            let buffer = terminal.backend().buffer().clone();
            let mut out = String::new();
            // 両端の枠を除いた内側だけを読む。全角の右半分のセルは空白なので飛ばす。
            let mut skip = false;
            for x in 1..width.saturating_sub(1) {
                if std::mem::take(&mut skip) {
                    continue;
                }
                let symbol = buffer[(x, 1)].symbol();
                skip = grid::display_width(symbol) == 2;
                out.push_str(symbol);
            }
            out.trim_end().to_string()
        }

        /// 入力欄の行で反転表示されている文字。
        fn reversed_input_text(app: &App) -> String {
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 24)).expect("端末");
            terminal.draw(|frame| draw(frame, app)).expect("描ける");
            let buffer = terminal.backend().buffer().clone();
            let mut out = String::new();
            for x in 0..buffer.area.width {
                let cell = &buffer[(x, 1)];
                if cell.modifier.contains(Modifier::REVERSED) {
                    out.push_str(cell.symbol());
                }
            }
            out
        }

        /// 80 桁端末のヘルプ。案内が落ちるかどうかはここで決まる。
        fn help_80(mode: Mode, display: DisplayMode) -> String {
            help_text(mode, display, false, false, false, false, 80)
        }

        /// video_target_area の検証用に、映像を持つ App を組み立てる。
        fn video_app(mode: Mode, display: DisplayMode) -> App {
            App {
                mode,
                display,
                screen: Rect::new(0, 0, 80, 24),
                video: Some(VideoSink::new(Geometry::new(
                    Rect::new(0, 0, 80, 20),
                    crate::video::FALLBACK_CELL,
                    crate::video::MAX_FRAME_PIXELS,
                ))),
                ..App::default()
            }
        }

        /// 指定セルの文字だけを取り出す。rendered() は全角文字で桁がずれるので、
        /// 特定の列を狙うこのテストでは buffer を直接読む。
        fn symbol_at(app: &App, width: u16, height: u16, x: u16, y: u16) -> String {
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height))
                    .expect("端末");
            terminal.draw(|frame| draw(frame, app)).expect("描ける");
            terminal.backend().buffer()[(x, y)].symbol().to_string()
        }

        /// spans をつないだ行。幅と中身をまとめて見る。窓は先頭から開いた状態で始める。
        fn tab_row(labels: &[&str], selected: usize, width: usize) -> String {
            tab_row_from(labels, selected, width, 0).0
        }

        /// 前回の窓を渡す版。行と、次に持ち越す窓の開始位置を返す。
        fn tab_row_from(
            labels: &[&str],
            selected: usize,
            width: usize,
            start: usize,
        ) -> (String, usize) {
            let range = visible_tabs(labels, selected, width, start);
            let next = range.start;
            let row = tab_spans(labels, selected, width, range)
                .iter()
                .map(|span| span.content.as_ref())
                .collect();
            (row, next)
        }

        /// 窓を先頭から開いた状態での当たり判定。
        fn tab_hit(labels: &[&str], selected: usize, width: usize, column: usize) -> Option<usize> {
            let range = visible_tabs(labels, selected, width, 0);
            tab_at_column(labels, width, range, column)
        }

        /// 割り付けを端末の申告で揺らさないための寸法。
        const CELL: CellSize = CellSize {
            width_px: 8,
            height_px: 16,
        };

        /// 80x24 の検索画面。CELL なら格子は 4 列 2 行になる。
        fn grid_app(count: usize) -> App {
            App {
                mode: Mode::Results,
                screen: Rect::new(0, 0, 80, 24),
                results: (0..count).map(result).collect(),
                ..App::default()
            }
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

        /// ショートのタブを見ている 80x24 のチャンネル画面。
        fn shorts_app(count: usize) -> App {
            let mut app = channel_app(count);
            let view = app.channel.as_mut().expect("チャンネル");
            view.tab = ChannelTab::Shorts;
            view.state_mut().results = (0..count).map(result).collect();
            view.state_mut().loaded = true;
            app
        }

        /// 画面の (`x`, `y`) に描かれている 1 文字。
        fn drawn_symbol(app: &App, x: u16, y: u16) -> String {
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).expect("端末");
            terminal.draw(|frame| draw(frame, app)).expect("描ける");
            terminal.backend().buffer()[(x, y)].symbol().to_string()
        }

        /// 画面の `row` 行目に描かれている文字。全角の右半分のセルは空白なので飛ばす。
        fn drawn_row(app: &App, row: u16) -> String {
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).expect("端末");
            terminal.draw(|frame| draw(frame, app)).expect("描ける");
            let buffer = terminal.backend().buffer().clone();
            let mut out = String::new();
            let mut skip = false;
            for x in 0..buffer.area.width {
                if std::mem::take(&mut skip) {
                    continue;
                }
                let symbol = buffer[(x, row)].symbol();
                skip = grid::display_width(symbol) == 2;
                out.push_str(symbol);
            }
            out.trim_end().to_string()
        }

        /// 矩形の左上と右下。両端が同じセルを指すことを確かめるための 2 点。
        fn corners(rect: Rect) -> [(u16, u16); 2] {
            [(rect.x, rect.y), (rect.right() - 1, rect.bottom() - 1)]
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

        /// プレイリスト一覧を開いた 80x24 の画面。検索結果 4 件は控えたまま。
        fn playlists_app(count: usize) -> App {
            let mut app = grid_app(4);
            app.playlists = Some(PlaylistsView {
                entries: (0..count)
                    .map(|i| PlaylistEntry {
                        id: format!("PL{i}"),
                        title: format!("list {i}"),
                    })
                    .collect(),
                selected: 0,
                loaded: true,
            });
            app.mode = Mode::Playlists;
            app
        }

        #[test]
        fn cursor_follows_display_width_not_char_count() {
            let app = input_app("abc");
            assert_eq!(input_cursor(app.screen, &app.query).0, 4);

            // 全角4文字 = 8桁
            let app = input_app("ラーメン");
            assert_eq!(input_cursor(app.screen, &app.query).0, 9);

            let app = input_app("");
            assert_eq!(input_cursor(app.screen, &app.query).0, 1);
        }

        #[test]
        fn cursor_stops_inside_the_border() {
            // 送り幅を入れないと枠に重なる位置でも、枠の内側で止める。
            let area = Rect::new(0, 0, 10, 3);
            assert_eq!(cursor_x(area, 99), 8);
        }

        #[test]
        fn the_cursor_follows_the_editing_position_not_the_end_of_the_text() {
            let mut app = input_app("ラーメン");
            assert_eq!(input_cursor(app.screen, &app.query).0, 9);

            app.query.move_left(false);
            assert_eq!(input_cursor(app.screen, &app.query).0, 7, "全角 3 文字ぶん");

            app.query.move_home(false);
            assert_eq!(input_cursor(app.screen, &app.query).0, 1, "枠の内側の先頭");
        }

        #[test]
        fn the_selection_is_drawn_reversed() {
            let mut app = input_app("ラーメン");
            assert_eq!(reversed_input_text(&app), "", "選択が無ければ反転しない");

            app.query.move_left(true);
            app.query.move_left(true);
            assert_eq!(reversed_input_text(&app), "メン");

            app.query.select_all();
            assert_eq!(reversed_input_text(&app), "ラーメン");
        }

        #[test]
        fn the_selection_is_not_drawn_after_the_focus_leaves_the_search_box() {
            let mut app = input_app("ラーメン");
            app.query.select_all();
            assert_eq!(reversed_input_text(&app), "ラーメン");

            // 結果一覧ではカーソルも出ないので、反転だけ残すと編集中に見える。
            app.mode = Mode::Results;
            assert_eq!(reversed_input_text(&app), "");
        }

        #[test]
        fn the_input_scrolls_to_keep_the_cursor_in_the_box() {
            // 枠の内側 8 桁に対して全角 8 文字 (16 桁)。
            let mut app = sized_input_app("ラーメンラーメン", 10);
            assert_eq!(drawn_input_text(&app), "ーメン", "末尾が見えている");
            assert_eq!(input_cursor(app.screen, &app.query).0, 7, "最後の文字の右");

            app.query.move_home(false);
            assert_eq!(drawn_input_text(&app), "ラーメン", "先頭へ戻れば送りも戻る");
            assert_eq!(input_cursor(app.screen, &app.query).0, 1);
        }

        #[test]
        fn a_query_that_fits_is_not_scrolled() {
            let app = input_app("ラーメン");
            assert_eq!(drawn_input_text(&app), "ラーメン");
            assert_eq!(query_index_at_point(&app, 1, 1), Some(0));
        }

        #[test]
        fn clicking_a_scrolled_box_answers_with_the_character_under_it() {
            let app = sized_input_app("ラーメンラーメン", 10);
            // 送り幅は 10 桁。内側の先頭に出ているのは 6 文字目 (index 5)。
            assert_eq!(query_index_at_point(&app, 1, 1), Some(5));
            assert_eq!(query_index_at_point(&app, 3, 1), Some(6));
            assert_eq!(
                query_index_at_point(&app, 7, 1),
                Some(8),
                "末尾より右は文字数"
            );
        }

        #[test]
        fn clicking_the_right_border_stops_at_the_last_visible_character() {
            let mut app = sized_input_app("ラーメンラーメン", 10);
            app.query.move_home(false);
            // 内側 8 桁には 4 文字しか出ていない。右枠を押しても 5 文字目は指さない。
            assert_eq!(query_index_at_point(&app, 9, 1), Some(3));
            assert_eq!(query_index_at_point(&app, 8, 1), Some(3));
        }

        /// 桁の数え方が 1 つなら、カーソルのいる桁を押すと同じ位置が返る。
        #[test]
        fn the_cursor_column_and_the_click_target_count_the_same_way() {
            let text = "aあiうe";
            let mut app = input_app(text);
            for index in 0..text.chars().count() {
                app.query.move_to(index);
                let x = input_cursor(app.screen, &app.query).0;
                assert_eq!(
                    query_index_at_point(&app, x, 1),
                    Some(index),
                    "{index} 文字目"
                );
            }
        }

        #[test]
        fn clicking_the_search_box_answers_with_the_character_under_it() {
            let app = input_app("ラーメン");
            // 文字は枠の内側 (x=1) から並び、全角 1 文字が 2 桁を占める。
            assert_eq!(query_index_at_point(&app, 1, 1), Some(0));
            assert_eq!(query_index_at_point(&app, 2, 1), Some(0));
            assert_eq!(query_index_at_point(&app, 3, 1), Some(1));
            assert_eq!(query_index_at_point(&app, 8, 1), Some(3));
            assert_eq!(query_index_at_point(&app, 9, 1), Some(4), "文字の先は末尾");
            assert_eq!(query_index_at_point(&app, 30, 1), Some(4));
            assert_eq!(
                query_index_at_point(&app, 0, 1),
                Some(0),
                "左の枠は先頭扱い"
            );
        }

        #[test]
        fn clicking_an_empty_search_box_answers_with_the_head() {
            let app = input_app("");
            assert_eq!(query_index_at_point(&app, 10, 1), Some(0));
        }

        #[test]
        fn the_search_box_hit_test_only_answers_where_it_is_drawn() {
            // 全段が入らない高さでは割り付けが潰れる。どこへ潰れても描いた行の中だけで応じる。
            for height in 0..8u16 {
                let app = App {
                    screen: Rect::new(0, 0, 40, height),
                    ..App::default()
                };
                let area = search_areas(app.screen)[0];
                for row in 0..8u16 {
                    let drawn = row >= area.y && row < area.bottom();
                    let hit = query_index_at_point(&app, 1, row);
                    assert_eq!(
                        hit.is_some(),
                        drawn,
                        "{height} 行 / {row} 行目 (入力欄 {area:?}): {hit:?}"
                    );
                }
            }
        }

        #[test]
        fn input_help_mentions_the_editing_keys() {
            // 既存の案内で 80 桁が埋まっているので、編集キーは幅のある端末でだけ出る。
            let help = help_text(
                Mode::Input,
                DisplayMode::Embedded,
                false,
                false,
                false,
                false,
                140,
            );
            for key in [
                "Ctrl+A:全選択",
                "Shift+←→:選択",
                "Home/End:先頭/末尾",
                "クリック:カーソル",
            ] {
                assert!(help.contains(key), "{key} が無い: {help}");
            }
        }

        #[test]
        fn results_help_mentions_esc() {
            assert!(help_80(Mode::Results, DisplayMode::Embedded).contains("Esc"));
        }

        #[test]
        fn draw_search_does_not_narrow_the_results_block_for_the_mini_player() {
            // ミニプレイヤーは上部 MINI_VIDEO_ROWS 分にしか映らないので、結果ブロックを
            // 全高にわたって狭めると、映像の無い下の行に空白の帯が残り続けて崩れて見える
            // (実機で確認した不具合)。狭めず、ミニプレイヤーは上に重ねて描くだけにする。
            let screen = Rect::new(0, 0, 80, 24);
            let row = search_areas(screen)[2].y;
            let column = screen.width - 1;

            let normal = App {
                mode: Mode::Results,
                screen,
                results: vec![result(0)],
                ..App::default()
            };
            let full_corner = symbol_at(&normal, screen.width, screen.height, column, row);
            assert_ne!(full_corner, " ", "背景無しなら結果ブロックが右端まで届く");

            // バックグラウンド中でも結果ブロックは同じく右端まで届く (映像は上に重なるだけ)。
            let backgrounded = App {
                results: vec![result(0)],
                background: true,
                ..video_app(Mode::Results, DisplayMode::Embedded)
            };
            let corner_while_backgrounded =
                symbol_at(&backgrounded, screen.width, screen.height, column, row);
            assert_eq!(
                corner_while_backgrounded, full_corner,
                "バックグラウンド中も結果ブロックの幅は変えない"
            );
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
        fn a_wide_terminal_shows_every_tab_without_markers() {
            let tabs = Tabs::default();
            let labels = tabs.labels();
            let row = tab_row(&labels, 0, 200);
            for label in &labels {
                assert!(row.contains(label), "{label} が落ちた: {row}");
            }
            assert!(!row.contains('<') && !row.contains('>'), "{row}");
        }

        #[test]
        fn the_tab_row_keeps_the_selected_tab_visible_inside_80_columns() {
            let tabs = Tabs::default();
            let labels = tabs.labels();
            for selected in 0..labels.len() {
                let row = tab_row(&labels, selected, 80);
                assert!(grid::display_width(&row) <= 80, "{selected}: {row}");
                assert!(
                    row.contains(labels[selected]),
                    "選択中の {} が出ていない: {row}",
                    labels[selected]
                );
            }
        }

        #[test]
        fn the_tab_row_marks_the_side_that_is_scrolled_out() {
            let tabs = Tabs::default();
            let labels = tabs.labels();
            // 先頭を選んでいるので右側だけが隠れる。
            let head = tab_row(&labels, 0, 80);
            assert!(head.ends_with('>'), "{head}");
            assert!(!head.starts_with('<'), "{head}");

            // 末尾を選ぶと窓が送られ、左側が隠れる。
            let tail = tab_row(&labels, labels.len() - 1, 80);
            assert!(tail.starts_with('<'), "{tail}");
            assert!(!tail.ends_with('>'), "{tail}");
            assert!(!tail.contains("音楽"), "窓の外は出さない: {tail}");
        }

        #[test]
        fn moving_to_the_next_tab_keeps_the_row_still_while_it_fits() {
            let tabs = Tabs::default();
            let labels = tabs.labels();
            // 80 桁では 5 番目 (アニメ) と 6 番目 (スポーツ) が同じ窓に入る。
            let (row, start) = tab_row_from(&labels, 4, 80, 0);
            let (next, _) = tab_row_from(&labels, 5, 80, start);
            assert_eq!(row, next, "1 つ隣に移っただけでタブ行が動いている");
        }

        #[test]
        fn the_tab_row_scrolls_only_when_the_selection_leaves_the_window() {
            let tabs = Tabs::default();
            let labels = tabs.labels();
            let last = labels.len() - 1;
            let mut range = visible_tabs(&labels, 0, 80, 0);
            // Tab で一周し、BackTab で戻る。
            for selected in (1..=last).chain((0..last).rev()) {
                let next = visible_tabs(&labels, selected, 80, range.start);
                assert!(next.contains(&selected), "{selected} が窓の外: {next:?}");
                if next.start != range.start {
                    assert!(
                        !range.contains(&selected),
                        "窓の中にいるのに動かした: {range:?} -> {next:?}"
                    );
                }
                range = next;
            }
        }

        #[test]
        fn widening_the_terminal_brings_the_left_side_back() {
            let tabs = Tabs::default();
            let labels = tabs.labels();
            let last = labels.len() - 1;
            let (_, start) = tab_row_from(&labels, last, 80, 0);
            assert!(start > 0, "80 桁では左が隠れる");

            let (row, start) = tab_row_from(&labels, last, 200, start);
            assert_eq!(start, 0, "広げたら窓も戻す");
            assert!(row.starts_with("すべて"), "{row}");
        }

        #[test]
        fn the_tab_row_never_overflows_even_on_a_tiny_terminal() {
            let tabs = Tabs::default();
            let labels = tabs.labels();
            for width in 0..60 {
                for selected in 0..labels.len() {
                    let row = tab_row(&labels, selected, width);
                    assert!(
                        grid::display_width(&row) <= width,
                        "{width} 桁 / {selected}: {row}"
                    );
                }
            }
        }

        #[test]
        fn a_single_tab_needs_no_window() {
            assert_eq!(tab_row(&["すべて"], 0, 80), "すべて");
            assert_eq!(tab_row(&[], 0, 80), "");
        }

        #[test]
        fn clicking_a_tab_label_selects_that_tab() {
            let tabs = Tabs::default();
            let labels = tabs.labels();
            // 「すべて」は 6 桁、区切りが 3 桁、「音楽」が 4 桁と並ぶ。
            assert_eq!(tab_hit(&labels, 0, 200, 0), Some(0));
            assert_eq!(tab_hit(&labels, 0, 200, 5), Some(0));
            assert_eq!(tab_hit(&labels, 0, 200, 9), Some(1));
            assert_eq!(tab_hit(&labels, 0, 200, 12), Some(1));
            assert_eq!(tab_hit(&labels, 0, 200, 16), Some(2));
        }

        #[test]
        fn clicking_a_separator_or_the_empty_tail_selects_nothing() {
            let tabs = Tabs::default();
            let labels = tabs.labels();
            for column in 6..9 {
                assert_eq!(tab_hit(&labels, 0, 200, column), None, "{column} は区切り");
            }
            let row = tab_row(&labels, 0, 200);
            let end = grid::display_width(&row);
            assert_eq!(tab_hit(&labels, 0, 200, end), None, "行の右の余白");
            assert_eq!(tab_hit(&labels, 0, 200, 199), None);
        }

        #[test]
        fn clicking_a_scroll_marker_does_not_select_a_tab() {
            let tabs = Tabs::default();
            let labels = tabs.labels();
            let last = labels.len() - 1;

            // 先頭を選んでいるので行末に " >" が出る。
            let head = tab_row(&labels, 0, 80);
            assert!(head.ends_with('>'), "{head}");
            let end = grid::display_width(&head);
            assert_eq!(tab_hit(&labels, 0, 80, end - 1), None);
            assert_eq!(tab_hit(&labels, 0, 80, end - 2), None);

            // 末尾を選ぶと窓が送られ、行頭に "< " が出る。
            let range = visible_tabs(&labels, last, 80, 0);
            assert!(range.start > 0, "80 桁では左が隠れる");
            assert_eq!(tab_at_column(&labels, 80, range.clone(), 0), None);
            assert_eq!(tab_at_column(&labels, 80, range, 1), None);
        }

        #[test]
        fn the_hit_test_agrees_with_the_drawn_row() {
            let tabs = Tabs::default();
            let labels = tabs.labels();
            let dim = Style::default().fg(Color::DarkGray);
            for width in [0usize, 1, 3, 5, 20, 40, 80, 120, 200] {
                for selected in 0..labels.len() {
                    let range = visible_tabs(&labels, selected, width, 0);
                    // 描いた行を左から辿り、各桁がどのタブの上かを並べる。
                    // 区切りとマーカーだけが dim なので、そこでラベルと見分けられる。
                    let mut columns: Vec<Option<usize>> = Vec::new();
                    let mut index = range.start;
                    for span in tab_spans(&labels, selected, width, range.clone()) {
                        let label = span.style != dim;
                        let cells = grid::display_width(span.content.as_ref());
                        columns.extend(std::iter::repeat_n(label.then_some(index), cells));
                        if label {
                            index += 1;
                        }
                    }
                    for column in 0..width + 2 {
                        assert_eq!(
                            tab_at_column(&labels, width, range.clone(), column),
                            columns.get(column).copied().flatten(),
                            "{width} 桁 / 選択 {selected} / {column} 桁目"
                        );
                    }
                }
            }
        }

        #[test]
        fn only_the_tab_row_answers_the_hit_test() {
            let app = App {
                screen: Rect::new(0, 0, 80, 24),
                ..App::default()
            };
            assert_eq!(
                tab_at_point(&app, 0, 3),
                Some(0),
                "タブ行の先頭は「すべて」"
            );
            for row in [0u16, 1, 2, 4, 12, 23] {
                assert_eq!(tab_at_point(&app, 0, row), None, "{row} 行目はタブ行でない");
            }
        }

        #[test]
        fn the_hit_test_follows_the_window_that_was_drawn() {
            let tabs = Tabs::default();
            let labels = tabs.labels();
            let last = labels.len() - 1;
            let range = visible_tabs(&labels, last, 80, 0);
            assert!(range.start > 0, "80 桁では左が隠れる");

            let mut app = App {
                screen: Rect::new(0, 0, 80, 24),
                ..App::default()
            };
            app.tabs.select(last);
            app.tabs.remember_window(range.start);

            // 窓の外のタブは行に出ていないので、どの桁を押しても選べない。
            for column in 0..80u16 {
                let hit = tab_at_point(&app, column, 3);
                assert!(
                    hit.is_none_or(|index| range.contains(&index)),
                    "{column} 桁目で窓の外の {hit:?} を返した"
                );
            }
            // 行頭は "< " なので、窓の先頭のタブはその次から。
            assert_eq!(tab_at_point(&app, 2, 3), Some(range.start));
        }

        #[test]
        fn a_terminal_too_short_for_the_tab_row_answers_only_where_it_is_drawn() {
            // 全段が入らない高さでは割り付けが潰れ、タブ行が y=3 に来るとは限らない。
            // どこへ潰れても、当たり判定は描いた行の中だけで応じる。
            for height in 0..8u16 {
                let app = App {
                    screen: Rect::new(0, 0, 80, height),
                    ..App::default()
                };
                let row_area = search_areas(app.screen)[1];
                for row in 0..8u16 {
                    let drawn = row >= row_area.y && row < row_area.bottom();
                    let hit = tab_at_point(&app, 0, row);
                    assert_eq!(
                        hit.is_some(),
                        drawn,
                        "{height} 行 / {row} 行目 (タブ行 {row_area:?}): {hit:?}"
                    );
                }
            }
        }

        #[test]
        fn the_channel_screen_shows_the_channel_tabs() {
            let app = channel_app(2);
            let row = drawn_row(&app, 3);
            for label in ChannelView::labels() {
                assert!(row.contains(label), "{label} が無い: {row}");
            }
            assert!(!row.contains("すべて"), "カテゴリタブは出さない: {row}");
        }

        #[test]
        fn the_channel_screen_draws_the_channel_results_not_the_search_results() {
            let app = channel_app(2);
            let layout = grid_layout(&app, CELL).expect("格子を組める");
            assert_eq!(layout.cells.len(), 2, "チャンネルの件数で組む");

            let title = drawn_row(&app, layout.cells[0].title.y);
            assert!(title.contains("title 0"), "{title}");
            assert!(!title.contains("title 99"), "{title}");
        }

        #[test]
        fn the_channel_grid_hit_test_uses_the_channel_results() {
            let app = channel_app(10);
            let layout = grid_layout(&app, CELL).expect("格子を組める");
            assert_eq!(layout.cells.len(), 8, "4 列 2 行");
            let first = layout.cells[0].image;
            assert_eq!(result_at_point(&app, CELL, first.x, first.y), Some(0));

            // 検索結果は 1 件しかないので、参照先を取り違えるとここが None になる。
            let last = layout.cells[7].image;
            assert_eq!(result_at_point(&app, CELL, last.x, last.y), Some(7));
        }

        #[test]
        fn a_click_on_the_channel_tab_row_answers_with_the_channel_tab() {
            let app = channel_app(2);
            let area = search_areas(app.screen)[1];
            assert_eq!(tab_at_point(&app, area.x, area.y), Some(0));
            // 「動画」(4 桁) と区切りの後ろは「ショート」。
            assert_eq!(tab_at_point(&app, area.x + 8, area.y), Some(1));
        }

        #[test]
        fn the_channel_help_names_the_way_back() {
            let help = help_text(
                Mode::Channel,
                DisplayMode::Embedded,
                false,
                false,
                false,
                false,
                80,
            );
            for key in ["Enter:再生", "Tab:", "Esc", "q:終了"] {
                assert!(help.contains(key), "{key} が無い: {help}");
            }
        }

        #[test]
        fn the_channel_help_names_the_settings_and_the_reload() {
            // 80 桁端末で出ないと、設定も取り直しも使えることに気づけない。
            let help = help_80(Mode::Channel, DisplayMode::Embedded);
            for key in ["r:再取得", "S:設定"] {
                assert!(help.contains(key), "{key} が無い: {help}");
            }
        }

        #[test]
        fn the_results_help_names_the_hide_key() {
            // 非表示リストを見る画面が無いので、案内に出ないと h に気づけない。
            // h を足すぶん言葉を削ってあるので、他の案内が落ちていないことまで見る。
            let help = help_80(Mode::Results, DisplayMode::Embedded);
            for key in ["h:隠す", "Esc:検索", "q:終了", "S:設定"] {
                assert!(help.contains(key), "{key} が無い: {help}");
            }
            assert!(grid::display_width(&help) <= 80, "{help}");
        }

        #[test]
        fn the_channel_help_names_the_hide_and_subscribe_keys() {
            let help = help_80(Mode::Channel, DisplayMode::Embedded);
            for key in ["s:登録", "h:隠す"] {
                assert!(help.contains(key), "{key} が無い: {help}");
            }
            assert!(grid::display_width(&help) <= 80, "{help}");
        }

        #[test]
        fn the_playlist_help_sends_esc_back_to_the_playlists() {
            // Esc の戻り先がチャンネルと違うので、言葉も「一覧へ」にする。
            let help = help_80(Mode::Playlist, DisplayMode::Embedded);
            for key in [
                "Enter:再生",
                "c:チャンネル",
                "h:隠す",
                "r:再取得",
                "Esc:一覧へ",
                "S:設定",
            ] {
                assert!(help.contains(key), "{key} が無い: {help}");
            }
            assert!(!help.contains("Tab:"), "タブの無い画面: {help}");
            assert!(grid::display_width(&help) <= 80, "{help}");
        }

        #[test]
        fn the_playlist_help_offers_the_way_back_to_the_player_only_while_backgrounded() {
            let back = "b:全画面へ".to_string();
            assert!(!playlist_hints(false).contains(&back));
            assert!(playlist_hints(true).contains(&back));
        }

        #[test]
        fn the_list_help_names_the_save_key_last() {
            for hints in [
                results_hints(false, false),
                channel_hints(false),
                playlist_hints(false),
            ] {
                assert_eq!(
                    hints.last().map(String::as_str),
                    Some("a:保存"),
                    "{hints:?}"
                );
            }
        }

        #[test]
        fn the_results_help_names_the_channel_key() {
            // 80 桁端末で出ないと、チャンネルへ移れること自体に気づけない。
            assert!(help_80(Mode::Results, DisplayMode::Embedded).contains("c:チャンネル"));
        }

        #[test]
        fn the_search_help_names_the_playlists_key() {
            // 80 桁は既存の案内で埋まっているので、幅のある端末でだけ出る。
            let results = help_text(
                Mode::Results,
                DisplayMode::Embedded,
                false,
                false,
                false,
                false,
                200,
            );
            assert!(results.contains("p:プレイリスト"), "{results}");
            let input = help_text(
                Mode::Input,
                DisplayMode::Embedded,
                false,
                false,
                false,
                false,
                200,
            );
            assert!(input.contains("Ctrl+P:プレイリスト"), "{input}");
        }

        #[test]
        fn the_results_help_never_offers_more_when_it_cannot_load_more() {
            let help = help_text(
                Mode::Results,
                DisplayMode::Embedded,
                false,
                false,
                false,
                false,
                200,
            );
            assert!(!help.contains("m:もっと見る"), "{help}");
        }

        #[test]
        fn the_more_hint_shows_up_once_there_is_room_and_more_to_load() {
            let wide = help_text(
                Mode::Results,
                DisplayMode::Embedded,
                false,
                true,
                false,
                false,
                200,
            );
            assert!(wide.contains("m:もっと見る"), "{wide}");

            // 既存の案内だけでちょうど 80 桁が埋まるので、80 桁端末ではまだ出ない。
            let narrow = help_text(
                Mode::Results,
                DisplayMode::Embedded,
                false,
                true,
                false,
                false,
                80,
            );
            assert!(!narrow.contains("m:もっと見る"), "{narrow}");
        }

        #[test]
        fn the_layout_toggle_hint_shows_up_once_there_is_room() {
            let wide = help_text(
                Mode::Results,
                DisplayMode::Embedded,
                false,
                false,
                false,
                false,
                200,
            );
            assert!(wide.contains("v:表示切替"), "{wide}");

            // 既存の案内だけでちょうど 80 桁が埋まるので、80 桁端末ではまだ出ない。
            let narrow = help_80(Mode::Results, DisplayMode::Embedded);
            assert!(!narrow.contains("v:表示切替"), "{narrow}");

            let channel_wide = help_text(
                Mode::Channel,
                DisplayMode::Embedded,
                false,
                false,
                false,
                false,
                200,
            );
            assert!(channel_wide.contains("v:表示切替"), "{channel_wide}");

            let playlist_wide = help_text(
                Mode::Playlist,
                DisplayMode::Embedded,
                false,
                false,
                false,
                false,
                200,
            );
            assert!(playlist_wide.contains("v:表示切替"), "{playlist_wide}");

            // 対象外 (v1): サムネイルを持たないタイトルのみの一覧なので出さない。
            let playlists_wide = help_text(
                Mode::Playlists,
                DisplayMode::Embedded,
                false,
                false,
                false,
                false,
                200,
            );
            assert!(!playlists_wide.contains("v:表示切替"), "{playlists_wide}");
        }

        #[test]
        fn the_leave_background_hint_only_shows_up_while_backgrounded() {
            let hidden = help_text(
                Mode::Results,
                DisplayMode::Embedded,
                false,
                false,
                false,
                false,
                200,
            );
            assert!(!hidden.contains("b:全画面へ"), "{hidden}");

            let shown = help_text(
                Mode::Results,
                DisplayMode::Embedded,
                false,
                false,
                true,
                false,
                200,
            );
            assert!(shown.contains("b:全画面へ"), "{shown}");

            let hidden = help_text(
                Mode::Channel,
                DisplayMode::Embedded,
                false,
                false,
                false,
                false,
                200,
            );
            assert!(!hidden.contains("b:全画面へ"), "{hidden}");

            let shown = help_text(
                Mode::Channel,
                DisplayMode::Embedded,
                false,
                false,
                true,
                false,
                200,
            );
            assert!(shown.contains("b:全画面へ"), "{shown}");

            let hidden = help_text(
                Mode::Input,
                DisplayMode::Embedded,
                false,
                false,
                false,
                false,
                200,
            );
            assert!(!hidden.contains("Ctrl+B"), "{hidden}");

            let shown = help_text(
                Mode::Input,
                DisplayMode::Embedded,
                false,
                false,
                true,
                false,
                200,
            );
            assert!(shown.contains("Ctrl+B:全画面へ"), "{shown}");
        }

        #[test]
        fn clicking_a_cell_answers_with_the_result_behind_it() {
            let app = grid_app(10);
            let layout = grid_layout(&app, CELL).expect("格子を組める");
            assert_eq!(layout.cells.len(), 8, "4 列 2 行");

            for (i, cell) in layout.cells.iter().enumerate() {
                // 画像・タイトル・時間の行はどれも同じ結果を指す。
                for rect in [cell.image, cell.title, cell.meta] {
                    for (column, row) in corners(rect) {
                        assert_eq!(
                            result_at_point(&app, CELL, column, row),
                            Some(layout.offset + i),
                            "{rect:?} の ({column},{row})"
                        );
                    }
                }
            }
        }

        #[test]
        fn the_hit_test_agrees_with_the_drawn_cells() {
            let app = grid_app(10);
            let layout = grid_layout(&app, CELL).expect("格子を組める");
            for row in 0..app.screen.height {
                for column in 0..app.screen.width {
                    let at = Position::new(column, row);
                    let drawn = layout.cells.iter().position(|cell| {
                        cell.image.contains(at) || cell.title.contains(at) || cell.meta.contains(at)
                    });
                    assert_eq!(
                        result_at_point(&app, CELL, column, row),
                        drawn.map(|i| layout.offset + i),
                        "({column},{row})"
                    );
                }
            }
        }

        #[test]
        fn the_margins_around_a_cell_answer_nothing() {
            let app = grid_app(10);
            let layout = grid_layout(&app, CELL).expect("格子を組める");
            let first = layout.cells[0];
            let inner = results_inner(app.screen);

            // セルの右に空けた余白。
            assert_eq!(
                result_at_point(&app, CELL, first.image.right(), first.image.y),
                None
            );
            // 時間の行の下に積んだ下余白。
            assert_eq!(
                result_at_point(&app, CELL, first.meta.x, first.meta.bottom()),
                None
            );
            // 結果ブロックの枠。
            assert_eq!(result_at_point(&app, CELL, inner.x - 1, inner.y), None);
            assert_eq!(result_at_point(&app, CELL, inner.x, inner.y - 1), None);
        }

        #[test]
        fn the_list_view_has_no_clickable_cells() {
            let mut app = grid_app(10);
            let first = grid_layout(&app, CELL).expect("格子を組める").cells[0].image;
            assert_eq!(result_at_point(&app, CELL, first.x, first.y), Some(0));

            app.settings.search.layout = LayoutMode::List;
            for row in 0..app.screen.height {
                for column in 0..app.screen.width {
                    assert_eq!(
                        result_at_point(&app, CELL, column, row),
                        None,
                        "({column},{row})"
                    );
                }
            }
        }

        #[test]
        fn the_list_view_marks_liked_and_subscribed_rows() {
            // list 表示はサムネイルを描かないので、present_thumbs のバッジ (#66) が
            // 乗らない。行のテキストに印を足して、ここでも分かるようにする。
            let mut app = grid_app(2);
            app.settings.search.layout = LayoutMode::List;
            app.results[0].channel_id = Some("UC1".to_string());
            app.engagement
                .remember_like("id0", true, std::time::SystemTime::now());
            app.engagement
                .remember_subscription("UC1", true, std::time::SystemTime::now());

            let text = rendered(&app, 80, 24);
            let line0 = text
                .lines()
                .find(|line| line.contains("title 0"))
                .expect("1 行目がある");
            assert!(line0.contains(Badge::Liked.symbol()), "{line0}");
            assert!(line0.contains(Badge::Subscribed.symbol()), "{line0}");
            // 印の無い行 (id1) には記号を出さない。
            let line1 = text
                .lines()
                .find(|line| line.contains("title 1"))
                .expect("2 行目がある");
            assert!(!line1.contains(Badge::Liked.symbol()), "{line1}");
        }

        #[test]
        fn the_list_view_marks_a_live_row() {
            let mut app = grid_app(2);
            app.settings.search.layout = LayoutMode::List;
            app.results[0].is_live = true;

            let text = rendered(&app, 80, 24);
            let line0 = text
                .lines()
                .find(|line| line.contains("title 0"))
                .expect("1 行目がある");
            assert!(line0.contains(Badge::Live.symbol()), "{line0}");
            let line1 = text
                .lines()
                .find(|line| line.contains("title 1"))
                .expect("2 行目がある");
            assert!(!line1.contains(Badge::Live.symbol()), "{line1}");
        }

        #[test]
        fn the_list_view_marks_every_row_of_the_shorts_tab() {
            // ショートは行では判定できないので、タブを見ている間は全行に印を出す。
            let mut app = shorts_app(2);
            app.settings.search.layout = LayoutMode::List;

            let text = rendered(&app, 80, 24);
            for title in ["title 0", "title 1"] {
                let line = text
                    .lines()
                    .find(|line| line.contains(title))
                    .expect("行がある");
                assert!(line.contains(Badge::Shorts.symbol()), "{line}");
            }

            app.channel.as_mut().expect("チャンネル").tab = ChannelTab::Videos;
            let text = rendered(&app, 80, 24);
            let line0 = text
                .lines()
                .find(|line| line.contains("title 0"))
                .expect("1 行目がある");
            assert!(!line0.contains(Badge::Shorts.symbol()), "{line0}");
        }

        #[test]
        fn the_grid_marks_every_cell_of_the_shorts_tab() {
            let mut app = shorts_app(3);
            let layout = grid_layout(&app, CELL).expect("格子を組める");
            assert_eq!(layout.cells.len(), 3);

            for cell in &layout.cells {
                // いいね/登録の印 (present_thumbs が左上へ重ねる) と被らない右上に置く。
                let mark = drawn_symbol(&app, cell.image.right() - 1, cell.image.y);
                assert_eq!(mark, Badge::Shorts.symbol(), "{:?}", cell.image);
            }

            app.channel.as_mut().expect("チャンネル").tab = ChannelTab::Videos;
            for cell in &layout.cells {
                let mark = drawn_symbol(&app, cell.image.right() - 1, cell.image.y);
                assert_ne!(mark, Badge::Shorts.symbol(), "{:?}", cell.image);
            }
        }

        #[test]
        fn a_terminal_too_small_for_a_grid_has_no_clickable_cells() {
            let mut app = grid_app(10);
            app.screen = Rect::new(0, 0, 80, 10);
            assert!(grid_layout(&app, CELL).is_none(), "格子を組めない");

            for row in 0..app.screen.height {
                for column in 0..app.screen.width {
                    assert_eq!(
                        result_at_point(&app, CELL, column, row),
                        None,
                        "({column},{row})"
                    );
                }
            }
        }

        #[test]
        fn an_empty_result_list_has_no_clickable_cells() {
            let app = grid_app(0);
            for row in 0..app.screen.height {
                for column in 0..app.screen.width {
                    assert_eq!(
                        result_at_point(&app, CELL, column, row),
                        None,
                        "({column},{row})"
                    );
                }
            }
        }

        #[test]
        fn a_scrolled_grid_answers_with_the_scrolled_index() {
            let mut app = grid_app(20);
            app.scroll = 4;
            let layout = grid_layout(&app, CELL).expect("格子を組める");
            assert_eq!(layout.offset, 4);

            let first = layout.cells[0].image;
            assert_eq!(result_at_point(&app, CELL, first.x, first.y), Some(4));
        }

        #[test]
        fn the_last_page_has_nothing_past_the_last_result() {
            let mut app = grid_app(10);
            app.scroll = 8;
            let layout = grid_layout(&app, CELL).expect("格子を組める");
            assert_eq!(layout.cells.len(), 2, "残りは 2 件");

            let image = layout.cells[0].image;
            assert_eq!(result_at_point(&app, CELL, image.x, image.y), Some(8));
            // 3 つめが来ていたはずの桁 (セル 1 つぶん右) には何も無い。
            let pitch = layout.cells[1].image.x - image.x;
            assert_eq!(
                result_at_point(&app, CELL, image.x + 2 * pitch, image.y),
                None
            );
        }

        #[test]
        fn a_cell_without_a_thumbnail_is_still_clickable() {
            let app = grid_app(10);
            assert!(app.thumbs.get("id0").is_none(), "画像はまだ届いていない");

            let first = grid_layout(&app, CELL).expect("格子を組める").cells[0];
            assert_eq!(
                result_at_point(&app, CELL, first.image.x, first.image.y),
                Some(0)
            );
        }

        #[test]
        fn the_results_title_shows_the_visible_range() {
            assert_eq!(results_title(0, 8, 10), " 結果 1-8/10 ");
            assert_eq!(results_title(8, 2, 10), " 結果 9-10/10 ");
            assert_eq!(results_title(0, 0, 0), " 結果 ");
        }

        #[test]
        fn a_playlist_draws_its_videos_like_the_results() {
            let mut app = playlists_app(2);
            let mut view = PlaylistView::new("PL0".to_string(), "list 0".to_string());
            view.state.results = (0..2).map(result).collect();
            view.state.loaded = true;
            app.playlist = Some(view);
            app.mode = Mode::Playlist;

            let grid = rendered(&app, 80, 24);
            assert!(grid.contains("title 0"), "{grid}");
            assert!(!grid.contains("list 1"), "一覧の行は出さない: {grid}");

            app.settings.search.layout = LayoutMode::List;
            let list = rendered(&app, 80, 24);
            assert!(list.contains("結果"), "リスト表示の見出し: {list}");
            assert!(list.contains("title 1"), "{list}");
        }
    }
}
