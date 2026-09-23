//! キー・マウス入力の振り分け。Session を触る操作は actions.rs のアクションへ渡す。

use crate::actions::{
    CommentScroll, Oauth, SEEK_STEP_SECS, Session, change_speed, config_path_from_env,
    copy_url_with, cycle_display_mode, enter_background, hide_current_channel, hide_selected,
    leave_background, leave_channel, like_video, load_more, move_selection, open_channel,
    reload_channel_tab, reload_tab, remember_playback_position, reset_speed, scroll_comments,
    seek_absolute, seek_relative, select_channel_tab, select_tab, send_to_player, start_playback,
    start_search, stop_playback, subscribe_channel, subscribe_playing_channel, switch_channel_tab,
    switch_tab, toggle_comments, toggle_search_layout, toggle_subtitles,
};
use crate::app::{App, AppEvent, Mode};
use crate::clipboard::{Clipboard, Pbcopy};
use crate::geometry::cell_size;
use crate::grid::Dir;
use crate::mpv::{self, MpvCommand};
use crate::oauth;
use crate::screen::download::{self as download_screen, open_download};
use crate::screen::playlists::{
    self as playlists_screen, is_playlists_key, leave_playlist, open_playlists, reload_playlist,
};
use crate::screen::settings::{self as settings_screen, is_settings_key, open_settings};
use crate::seekbar::{MouseAction, MouseInput};
use crate::ui;
use crate::video::CellSize;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use std::path::Path;
use tokio::sync::mpsc::UnboundedSender;

pub async fn handle_key(
    app: &mut App,
    key: KeyEvent,
    tx: &UnboundedSender<AppEvent>,
    session: &mut Session,
) {
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        remember_playback_position(app);
        stop_playback(session).await;
        app.should_quit = true;
        return;
    }

    // 終了確認中は他のキーを一切無視し、Y/N の返事だけを見る。
    // 他のモードの処理と同じく、Ctrl 付きの文字は答えとして扱わない。
    if app.confirm_quit {
        if !key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                    // Ctrl+C の即終了と同じく、バックグラウンド中の再生を持ったまま終了しない。
                    remember_playback_position(app);
                    stop_playback(session).await;
                    app.should_quit = true;
                    app.confirm_quit = false;
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    app.confirm_quit = false;
                }
                _ => {}
            }
        }
        return;
    }

    match app.mode {
        Mode::Input => handle_key_input(app, key, tx, session).await,
        Mode::Results => handle_key_results(app, key, tx, session).await,
        Mode::Channel => handle_key_channel(app, key, tx, session).await,
        Mode::Playing => handle_key_playing(app, key, tx, session).await,
        Mode::Settings => settings_screen::handle_key_settings(app, key),
        Mode::Download => download_screen::handle_key_download(app, key, tx, session).await,
        Mode::Playlists => playlists_screen::handle_key_playlists(app, key, tx, session).await,
        Mode::Playlist => handle_key_playlist(app, key, tx, session).await,
    }
}

async fn handle_key_input(
    app: &mut App,
    key: KeyEvent,
    tx: &UnboundedSender<AppEvent>,
    session: &mut Session,
) {
    let extend = key.modifiers.contains(KeyModifiers::SHIFT);
    match key.code {
        KeyCode::Enter => start_search(app, tx, session),
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

async fn handle_key_results(
    app: &mut App,
    key: KeyEvent,
    tx: &UnboundedSender<AppEvent>,
    session: &mut Session,
) {
    let config = config_path_from_env();
    handle_key_results_with(app, key, tx, session, config.as_deref()).await;
}

/// 設定ファイルの置き場を差し替えられる形。テストはここに一時ファイルを渡して
/// 利用者の設定を書き換えない。
async fn handle_key_results_with(
    app: &mut App,
    key: KeyEvent,
    tx: &UnboundedSender<AppEvent>,
    session: &mut Session,
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
async fn handle_key_channel(
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
async fn handle_key_playlist(
    app: &mut App,
    key: KeyEvent,
    tx: &UnboundedSender<AppEvent>,
    session: &mut Session,
) {
    let config = config_path_from_env();
    handle_key_playlist_with(app, key, tx, session, config.as_deref()).await;
}

/// 設定ファイルの置き場を差し替えられる形。テストはここに一時ファイルを渡して
/// 利用者の設定を書き換えない。
async fn handle_key_playlist_with(
    app: &mut App,
    key: KeyEvent,
    tx: &UnboundedSender<AppEvent>,
    session: &mut Session,
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

async fn handle_key_playing(
    app: &mut App,
    key: KeyEvent,
    tx: &UnboundedSender<AppEvent>,
    session: &mut Session,
) {
    let config = config_path_from_env();
    handle_key_playing_with(
        app,
        key,
        tx,
        session,
        Pbcopy,
        Oauth::real(),
        config.as_deref(),
    )
    .await;
}

/// クリップボードの書き手と設定ファイルの置き場を差し替えられる形。テストはここに
/// 偽物と一時ファイルを渡して pbcopy を起動させず、利用者の設定も書き換えない。
async fn handle_key_playing_with<C: Clipboard, B: oauth::Backend + 'static>(
    app: &mut App,
    key: KeyEvent,
    tx: &UnboundedSender<AppEvent>,
    session: &mut Session,
    clipboard: C,
    deps: Oauth<B>,
    config: Option<&Path>,
) {
    // コメントを読んでいる間の ↑↓ は一覧送り。音量は閉じてから。
    if app.comments.visible()
        && let Some(step) = comment_scroll_step(key.code)
    {
        scroll_comments(app, step);
        return;
    }
    if let Some(delta) = seek_step(key.code) {
        seek_relative(app, session, delta, std::time::Instant::now()).await;
    }
    if let Some(steps) = speed_step(key.code) {
        change_speed(app, session, steps).await;
    }
    if key.code == KeyCode::Backspace {
        reset_speed(app, session).await;
    }
    if let Some(command) = playing_command(key.code) {
        send_to_player(app, session, &command).await;
    }
    // 複数コマンドと App の状態更新を伴うので playing_command には入れない。
    if key.code == KeyCode::Char('w') {
        cycle_display_mode(app, session, config, std::time::Instant::now()).await;
    }
    if key.code == KeyCode::Char('c') {
        copy_url_with(app, clipboard, std::time::Instant::now()).await;
    }
    if key.code == KeyCode::Char('s') {
        toggle_subtitles(app, session, std::time::Instant::now()).await;
    }
    if key.code == KeyCode::Char('o') {
        toggle_comments(app, session);
    }
    // deps は一度しか渡せないので、OAuth を使う操作は 1 つの match にまとめる。
    match key.code {
        KeyCode::Char('l') => like_video(app, tx, session, deps),
        KeyCode::Char('u') => subscribe_playing_channel(app, tx, session, deps),
        _ => {}
    }
    if key.code == KeyCode::Char('d') {
        open_download(app, session);
    }
    if key.code == KeyCode::Char('b') {
        enter_background(app, session).await;
    }
}

/// マウス。再生中はシーク、入力欄はタブと検索欄のカーソル移動、
/// 結果一覧はタブと格子のクリックを見る。
pub async fn handle_mouse(
    app: &mut App,
    mouse: MouseEvent,
    tx: &UnboundedSender<AppEvent>,
    session: &mut Session,
) {
    // 終了確認中はクリックもドラッグも見ない。
    if app.confirm_quit {
        return;
    }
    match app.mode {
        Mode::Playing => handle_mouse_playing(app, mouse, tx, session).await,
        Mode::Results => handle_mouse_results(app, mouse, tx, session).await,
        Mode::Channel => handle_mouse_channel(app, mouse, tx, session).await,
        Mode::Input => handle_mouse_input(app, mouse, tx, session),
        Mode::Settings | Mode::Download | Mode::Playlists | Mode::Playlist => {}
    }
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
    if let Some(index) = ui::tab_at_point(app, mouse.column, mouse.row) {
        return Some(InputClick::Tab(index));
    }
    ui::query_index_at_point(app, mouse.column, mouse.row).map(InputClick::Cursor)
}

fn handle_mouse_input(
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
    if let Some(index) = ui::tab_at_point(app, mouse.column, mouse.row) {
        return Some(ResultsClick::Tab(index));
    }
    // 描いた後に結果が入れ替わっていると、同じ座標が別の動画を指す。
    // タブ行と違って再生が走ってしまうので、次の描画まで待つ。
    if !app.results_are_drawn() {
        return None;
    }
    ui::result_at_point(app, cell, mouse.column, mouse.row).map(ResultsClick::Play)
}

async fn handle_mouse_results(
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
async fn handle_mouse_channel(
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

async fn handle_mouse_playing(
    app: &mut App,
    mouse: MouseEvent,
    tx: &UnboundedSender<AppEvent>,
    session: &mut Session,
) {
    handle_mouse_playing_with(app, mouse, tx, session, Oauth::real()).await;
}

async fn handle_mouse_playing_with<B: oauth::Backend + 'static>(
    app: &mut App,
    mouse: MouseEvent,
    tx: &UnboundedSender<AppEvent>,
    session: &mut Session,
    deps: Oauth<B>,
) {
    // アイコンの上での押し込みはシークにしない。外れたら下のシーク処理へ渡す。
    if let Some(kind) = playing_action_click(app, mouse) {
        match kind {
            ui::ActionKind::Like => like_video(app, tx, session, deps),
            ui::ActionKind::Subscribe => subscribe_playing_channel(app, tx, session, deps),
        }
        return;
    }
    let Some(input) = mouse_input(mouse.kind) else {
        return;
    };
    // 描画とヒットテストが同じ割り付けを通るので、印とクリック位置が食い違わない。
    let layout = ui::seek_bar_layout(app);
    let action = app
        .seek_bar
        .on_mouse(input, mouse.column, mouse.row, &layout);
    let Some(MouseAction::Seek { column }) = action else {
        return;
    };
    // duration が取れない動画 (ライブ等) では列を秒に直せない。
    let Some(duration) = app.playback.duration else {
        return;
    };
    let target = layout.seconds_at(column, duration);
    seek_absolute(app, session, target, std::time::Instant::now()).await;
}

/// アクション行を押し込んだときの操作。押し込み以外は None
/// (移動はシークバーの hover に渡す必要がある)。
fn playing_action_click(app: &App, mouse: MouseEvent) -> Option<ui::ActionKind> {
    if mouse.kind != MouseEventKind::Down(MouseButton::Left) {
        return None;
    }
    ui::action_at_point(app, mouse.column, mouse.row)
}

/// 左ボタンと移動だけ。それ以外は None。
fn mouse_input(kind: MouseEventKind) -> Option<MouseInput> {
    match kind {
        MouseEventKind::Moved => Some(MouseInput::Move),
        MouseEventKind::Down(MouseButton::Left) => Some(MouseInput::Press),
        MouseEventKind::Drag(MouseButton::Left) => Some(MouseInput::Drag),
        // 種別を報告しない端末の Up も crossterm は Left として返すので、Left だけで足りる。
        // 全種別を受けると、ドラッグ中の右クリックがその場でシークを確定させてしまう。
        MouseEventKind::Up(MouseButton::Left) => Some(MouseInput::Release),
        _ => None,
    }
}

/// ←→ のシーク幅。シークは先行更新を伴うので playing_command とは別経路。
fn seek_step(code: KeyCode) -> Option<f64> {
    match code {
        KeyCode::Left => Some(-SEEK_STEP_SECS),
        KeyCode::Right => Some(SEEK_STEP_SECS),
        _ => None,
    }
}

/// コメント表示中の送り幅。上限 50 件は 1 画面に入らないので、行送りと画面送りを用意する。
fn comment_scroll_step(code: KeyCode) -> Option<CommentScroll> {
    match code {
        KeyCode::Up => Some(CommentScroll::Line(-1)),
        KeyCode::Down => Some(CommentScroll::Line(1)),
        KeyCode::PageUp => Some(CommentScroll::Page(-1)),
        KeyCode::PageDown => Some(CommentScroll::Page(1)),
        _ => None,
    }
}

/// 速度の刻み。mpv 既定の `[` `]` と同じ位置に置く (mpv は × 0.9 / × 1.1 で刻みだけ違う)。
fn speed_step(code: KeyCode) -> Option<i8> {
    match code {
        KeyCode::Char('[') => Some(-1),
        KeyCode::Char(']') => Some(1),
        _ => None,
    }
}

/// 再生中のキーと mpv コマンドの対応表。
fn playing_command(code: KeyCode) -> Option<MpvCommand> {
    match code {
        KeyCode::Char(' ') => Some(mpv::cycle_pause()),
        KeyCode::Up => Some(mpv::add_volume(5)),
        KeyCode::Down => Some(mpv::add_volume(-5)),
        KeyCode::Char('q') | KeyCode::Esc => Some(mpv::quit()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{ChannelView, Playback, PlaylistView};
    use crate::category::{Category, Tabs};
    use crate::clipboard::fixtures::{CopyResult, FakeClipboard};
    use crate::display::DisplayMode;
    use crate::grid::LayoutMode;
    use crate::oauth::fixtures::FakeBackend;
    use crate::query::QueryEditor;
    use crate::screen::playlists::PlaylistsView;
    use crate::search::{PlaylistEntry, SearchResult};
    use crate::seekbar::SeekBarState;
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
        let dir = std::env::temp_dir().join(format!("tuitube-input-{}-{name}", std::process::id()));
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
        let results: Vec<SearchResult> = (0..count).map(|i| result(&format!("id{i}"))).collect();
        app.set_results(results, &crate::cookies::Target::Search("q".to_string()));
        // 本番では set_results の後に必ず draw が挟まる。
        app.mark_drawn();
        app
    }

    #[tokio::test]
    async fn o_toggles_the_comment_list_while_playing() {
        let (tx, _rx) = channel();
        let mut session = Session::default();
        let mut app = playing_app();

        handle_key(&mut app, key(KeyCode::Char('o')), &tx, &mut session).await;
        assert!(app.comments.visible());
        assert!(session.owe_clear, "貼ってある映像を剥がす");

        handle_key(&mut app, key(KeyCode::Char('o')), &tx, &mut session).await;
        assert!(!app.comments.visible());
    }

    /// 上限まで取れた再生画面。1 件 2 行なので 80x24 の枠 (内側 18 行) には収まらない。
    fn commented_app() -> App {
        let mut app = playing_app();
        app.comments.begin("abc".to_string());
        let list = (0..crate::comments::COMMENT_LIMIT)
            .map(|i| crate::comments::Comment {
                author: format!("author{i}"),
                text: format!("本文{i}"),
                like_count: None,
            })
            .collect();
        app.comments.apply("abc", Ok(list));
        app
    }

    #[tokio::test]
    async fn the_arrows_scroll_the_comment_list_while_it_is_open() {
        let (tx, _rx) = channel();
        let mut session = Session::default();
        let mut app = commented_app();
        // 50 件 = 100 行を 18 行の枠で見る。
        let lines = 2 * crate::comments::COMMENT_LIMIT;
        let height = 18;

        // 閉じている間の ↑↓ は音量のままで、一覧は動かない。
        handle_key(&mut app, key(KeyCode::Down), &tx, &mut session).await;
        assert_eq!(app.comments.scroll(lines, height), 0);

        app.comments.toggle();
        handle_key(&mut app, key(KeyCode::Down), &tx, &mut session).await;
        assert_eq!(app.comments.scroll(lines, height), 1);
        handle_key(&mut app, key(KeyCode::PageDown), &tx, &mut session).await;
        assert_eq!(app.comments.scroll(lines, height), 1 + height);
        handle_key(&mut app, key(KeyCode::Up), &tx, &mut session).await;
        assert_eq!(app.comments.scroll(lines, height), height);
        handle_key(&mut app, key(KeyCode::PageUp), &tx, &mut session).await;
        assert_eq!(app.comments.scroll(lines, height), 0);
    }

    #[tokio::test]
    async fn scrolling_does_not_take_over_the_other_playback_keys() {
        let (tx, _rx) = channel();
        let mut session = Session::default();
        let mut app = commented_app();
        app.comments.toggle();

        // ←→ のシークも o の開閉もそのまま効く。
        handle_key(&mut app, key(KeyCode::Right), &tx, &mut session).await;
        assert_eq!(app.playback.time_pos, Some(SEEK_STEP_SECS));
        handle_key(&mut app, key(KeyCode::Char('o')), &tx, &mut session).await;
        assert!(!app.comments.visible());
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
    async fn b_key_backgrounds_playback_and_returns_to_the_result_list() {
        let (tx, _rx) = channel();
        let mut session = Session::default();
        let mut app = App {
            results: vec![result("a")],
            ..playing_app()
        };

        handle_key_playing(&mut app, key(KeyCode::Char('b')), &tx, &mut session).await;

        assert!(app.background);
        assert_eq!(app.mode, Mode::Results);
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
    async fn confirm_quit_stops_playback_before_quitting() {
        let (tx, _rx) = channel();
        let mut session = Session::default();
        let sent = record(&mut session);
        let mut app = App {
            confirm_quit: true,
            ..App::default()
        };

        handle_key(&mut app, key(KeyCode::Char('y')), &tx, &mut session).await;

        assert!(app.should_quit);
        assert!(!app.confirm_quit);
        assert_eq!(*sent.lock().expect("溜め込み先"), [mpv::quit().to_line()]);
    }

    #[tokio::test]
    async fn confirm_quit_remembers_the_playback_position_before_stopping() {
        let (tx, _rx) = channel();
        let mut session = Session::default();
        let dir = std::env::temp_dir().join(format!(
            "tuitube-input-confirm-quit-resume-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let mut app = App {
            confirm_quit: true,
            resume: crate::resume::load_from(Some(&dir.join("resume.toml"))),
            playback: Playback {
                id: "v1".to_string(),
                time_pos: Some(120.0),
                duration: Some(600.0),
                ..Playback::default()
            },
            ..App::default()
        };

        handle_key(&mut app, key(KeyCode::Char('y')), &tx, &mut session).await;

        assert!(app.should_quit);
        assert_eq!(app.resume.lookup("v1"), Some(120.0));
    }

    #[test]
    fn playing_keys_map_to_mpv_commands() {
        assert_eq!(
            playing_command(KeyCode::Char(' ')),
            Some(mpv::cycle_pause())
        );
        // シークは先行更新を伴うので playing_command からは外れている。
        assert_eq!(playing_command(KeyCode::Left), None);
        assert_eq!(playing_command(KeyCode::Right), None);
        assert_eq!(seek_step(KeyCode::Left), Some(-5.0));
        assert_eq!(seek_step(KeyCode::Right), Some(5.0));
        assert_eq!(seek_step(KeyCode::Char('x')), None);
        assert_eq!(playing_command(KeyCode::Up), Some(mpv::add_volume(5)));
        assert_eq!(playing_command(KeyCode::Down), Some(mpv::add_volume(-5)));
        assert_eq!(playing_command(KeyCode::Esc), Some(mpv::quit()));
        assert_eq!(playing_command(KeyCode::Char('q')), Some(mpv::quit()));
        assert_eq!(playing_command(KeyCode::Char('x')), None);
        assert_eq!(playing_command(KeyCode::Enter), None);
    }

    #[test]
    fn w_is_not_a_plain_mpv_command() {
        // 表示モードの切替は複数コマンドなので、シークと同じく別経路。
        assert_eq!(playing_command(KeyCode::Char('w')), None);
    }

    #[test]
    fn bracket_keys_step_the_speed_and_are_not_plain_mpv_commands() {
        assert_eq!(speed_step(KeyCode::Char('[')), Some(-1));
        assert_eq!(speed_step(KeyCode::Char(']')), Some(1));
        assert_eq!(speed_step(KeyCode::Char('x')), None);
        assert_eq!(speed_step(KeyCode::Backspace), None);
        // 送信の要否を tuitube 側で決めるので、対応表には載せない。
        assert_eq!(playing_command(KeyCode::Char('[')), None);
        assert_eq!(playing_command(KeyCode::Char(']')), None);
        assert_eq!(playing_command(KeyCode::Backspace), None);
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

    /// 送った内容だけを溜める偽の player。外部プロセスへは届かない。
    struct Recorder(std::sync::Arc<std::sync::Mutex<Vec<String>>>);

    impl crate::actions::PlayerSink for Recorder {
        fn send<'a>(&'a mut self, command: &'a MpvCommand) -> crate::actions::Sending<'a> {
            let sent = self.0.clone();
            Box::pin(async move {
                sent.lock().expect("溜め込み先").push(command.to_line());
                Ok(())
            })
        }
    }

    fn record(session: &mut Session) -> std::sync::Arc<std::sync::Mutex<Vec<String>>> {
        let sent = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        session.player = Some(crate::actions::Player {
            sink: Box::new(Recorder(sent.clone())),
            nonce: 1,
        });
        sent
    }

    #[tokio::test]
    async fn bracket_keys_send_the_speed_while_playing() {
        let (tx, _rx) = channel();
        let mut session = Session::default();
        let sent = record(&mut session);
        let mut app = playing_app();

        handle_key_playing(&mut app, key(KeyCode::Char(']')), &tx, &mut session).await;
        assert_eq!(
            app.speed,
            crate::speed::Speed::from_tenths(11).expect("1.1x")
        );
        handle_key_playing(&mut app, key(KeyCode::Char('[')), &tx, &mut session).await;
        assert_eq!(app.speed, crate::speed::Speed::NORMAL);

        assert_eq!(
            *sent.lock().expect("溜め込み先"),
            [
                "{\"command\":[\"set_property\",\"speed\",1.1]}\n",
                "{\"command\":[\"set_property\",\"speed\",1.0]}\n",
            ]
        );
    }

    #[tokio::test]
    async fn backspace_resets_the_speed_while_playing() {
        let (tx, _rx) = channel();
        let mut session = Session::default();
        let sent = record(&mut session);
        let mut app = App {
            speed: crate::speed::Speed::from_tenths(15).expect("1.5x"),
            ..playing_app()
        };
        handle_key_playing(&mut app, key(KeyCode::Backspace), &tx, &mut session).await;

        assert_eq!(app.speed, crate::speed::Speed::NORMAL);
        assert_eq!(
            *sent.lock().expect("溜め込み先"),
            ["{\"command\":[\"set_property\",\"speed\",1.0]}\n"]
        );
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
            Some(&path),
        )
        .await;

        assert_eq!(app.settings.search.layout, LayoutMode::List);
    }

    const URL: &str = "https://www.youtube.com/watch?v=abc";

    fn playing_url_app() -> App {
        App {
            playback: Playback {
                url: URL.to_string(),
                time_pos: Some(0.0),
                duration: Some(650.0),
                ..Playback::default()
            },
            ..playing_app()
        }
    }

    #[tokio::test]
    async fn c_copies_the_url_while_playing() {
        let (tx, _rx) = channel();
        let mut session = Session::default();
        let mut app = playing_url_app();
        let clipboard = FakeClipboard::new(CopyResult::Ok);
        handle_key_playing_with(
            &mut app,
            key(KeyCode::Char('c')),
            &tx,
            &mut session,
            clipboard.clone(),
            fake_oauth(),
            Some(&temp_config("scratch")),
        )
        .await;

        assert_eq!(clipboard.copied(), [URL]);
        assert_eq!(
            app.notice.as_deref(),
            Some(crate::actions::COPIED_NOTICE),
            "コピーできたことを伝える"
        );
    }

    const SID_AUTO: &str = "{\"command\":[\"set_property\",\"sid\",\"auto\"]}\n";
    const SID_NO: &str = "{\"command\":[\"set_property\",\"sid\",\"no\"]}\n";

    #[tokio::test]
    async fn s_toggles_the_subtitle_while_playing() {
        let (tx, _rx) = channel();
        let mut session = Session::default();
        let sent = record(&mut session);
        let mut app = playing_app();
        assert!(app.subtitles.wanted(), "[subtitles] enabled の既定は true");

        handle_key_playing(&mut app, key(KeyCode::Char('s')), &tx, &mut session).await;
        assert!(!app.subtitles.wanted());
        handle_key_playing(&mut app, key(KeyCode::Char('s')), &tx, &mut session).await;
        assert!(app.subtitles.wanted());

        assert_eq!(*sent.lock().expect("溜め込み先"), [SID_NO, SID_AUTO]);
    }

    #[tokio::test]
    async fn s_does_nothing_else_while_playing() {
        let (tx, _rx) = channel();
        // s は既存のキーと重なっていない。
        assert_eq!(playing_command(KeyCode::Char('s')), None);
        assert_eq!(seek_step(KeyCode::Char('s')), None);
        assert_eq!(speed_step(KeyCode::Char('s')), None);

        let mut session = Session::default();
        let sent = record(&mut session);
        let mut app = playing_app();
        let clipboard = FakeClipboard::new(CopyResult::Ok);
        handle_key_playing_with(
            &mut app,
            key(KeyCode::Char('s')),
            &tx,
            &mut session,
            clipboard.clone(),
            fake_oauth(),
            Some(&temp_config("scratch")),
        )
        .await;

        assert_eq!(*sent.lock().expect("溜め込み先"), [SID_NO]);
        assert_eq!(app.speed, crate::speed::Speed::NORMAL);
        assert_eq!(app.display, DisplayMode::Embedded);
        assert!(clipboard.copied().is_empty());
        assert!(!app.should_quit);
        assert_eq!(app.mode, Mode::Playing);
    }

    #[tokio::test]
    async fn the_other_playing_keys_do_not_toggle_the_subtitle() {
        let (tx, _rx) = channel();
        let mut session = Session::default();
        let _sent = record(&mut session);
        let mut app = playing_app();
        for code in [
            KeyCode::Char(' '),
            KeyCode::Char('w'),
            KeyCode::Char('['),
            KeyCode::Char(']'),
            KeyCode::Backspace,
            KeyCode::Left,
            KeyCode::Right,
            KeyCode::Up,
            KeyCode::Down,
        ] {
            handle_key_playing_with(
                &mut app,
                key(code),
                &tx,
                &mut session,
                FakeClipboard::new(CopyResult::Ok),
                fake_oauth(),
                Some(&temp_config("scratch")),
            )
            .await;
            assert!(app.subtitles.wanted(), "{code:?} で字幕が動いた");
        }
    }

    #[tokio::test]
    async fn w_writes_the_new_display_mode_to_the_config_file() {
        let (tx, _rx) = channel();
        let mut session = Session::default();
        let _sent = record(&mut session);
        let mut app = playing_app();
        let path = temp_config("w-autosave");

        handle_key_playing_with(
            &mut app,
            key(KeyCode::Char('w')),
            &tx,
            &mut session,
            FakeClipboard::new(CopyResult::Ok),
            fake_oauth(),
            Some(&path),
        )
        .await;

        assert_eq!(app.display, DisplayMode::Text);
        let written = std::fs::read_to_string(&path).expect("読める");
        assert!(written.contains("mode = \"text\""), "{written}");
    }

    /// チャンネル一覧を見ている状態。
    fn subscribable_app() -> App {
        let mut app = channel_app(2);
        app.mode = Mode::Channel;
        app
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
    async fn l_while_playing_starts_the_like() {
        let (tx, _rx) = channel();
        let mut session = Session::default();
        let mut app = playing_url_app();
        let clipboard = FakeClipboard::new(CopyResult::Ok);

        handle_key_playing_with(
            &mut app,
            key(KeyCode::Char('l')),
            &tx,
            &mut session,
            clipboard,
            fake_oauth(),
            Some(&temp_config("scratch")),
        )
        .await;

        assert_eq!(app.notice.as_deref(), Some(crate::oauth::LIKE_NOTICE));
        assert!(session.oauth_task.is_some(), "送信が積まれている");
    }

    #[tokio::test]
    async fn l_does_nothing_without_a_video_url() {
        let (tx, _rx) = channel();
        let mut session = Session::default();
        // playing_app は URL を持たない。
        let mut app = playing_app();
        let clipboard = FakeClipboard::new(CopyResult::Ok);

        handle_key_playing_with(
            &mut app,
            key(KeyCode::Char('l')),
            &tx,
            &mut session,
            clipboard,
            fake_oauth(),
            Some(&temp_config("scratch")),
        )
        .await;

        assert!(session.oauth_task.is_none());
        assert!(app.notice.is_none());
    }

    #[tokio::test]
    async fn the_other_playing_keys_do_not_like() {
        let (tx, _rx) = channel();
        for code in [
            KeyCode::Char(' '),
            KeyCode::Char('c'),
            KeyCode::Char('s'),
            KeyCode::Char('o'),
            KeyCode::Char('w'),
        ] {
            let mut session = Session::default();
            let mut app = playing_url_app();
            handle_key_playing_with(
                &mut app,
                key(code),
                &tx,
                &mut session,
                FakeClipboard::new(CopyResult::Ok),
                fake_oauth(),
                Some(&temp_config("scratch")),
            )
            .await;
            assert!(session.oauth_task.is_none(), "{code:?} でいいねが走った");
        }
    }

    /// チャンネル ID を引き継いだ再生。アクション行に登録ラベルが出る状態。
    fn playing_channel_app() -> App {
        let mut app = playing_url_app();
        app.playback.channel_id = Some("UC1".to_string());
        app
    }

    async fn press_while_playing(app: &mut App, code: KeyCode, session: &mut Session) {
        let (tx, _rx) = channel();
        handle_key_playing_with(
            app,
            key(code),
            &tx,
            session,
            FakeClipboard::new(CopyResult::Ok),
            fake_oauth(),
            Some(&temp_config("scratch")),
        )
        .await;
    }

    #[tokio::test]
    async fn u_while_playing_subscribes_to_the_channel_of_the_video() {
        let mut session = Session::default();
        let mut app = playing_channel_app();

        press_while_playing(&mut app, KeyCode::Char('u'), &mut session).await;

        assert_eq!(app.notice.as_deref(), Some(crate::oauth::SUBSCRIBE_NOTICE));
        assert_eq!(
            session.oauth_action,
            Some(crate::oauth::Action::Subscribe("UC1".to_string()))
        );
    }

    #[tokio::test]
    async fn u_does_nothing_without_a_channel_id() {
        let mut session = Session::default();
        // playing_url_app はチャンネル ID を持たない。
        let mut app = playing_url_app();

        press_while_playing(&mut app, KeyCode::Char('u'), &mut session).await;

        assert!(session.oauth_task.is_none());
        assert!(app.notice.is_none());
    }

    #[tokio::test]
    async fn l_and_u_ask_for_different_actions() {
        let mut session = Session::default();
        let mut app = playing_channel_app();

        press_while_playing(&mut app, KeyCode::Char('l'), &mut session).await;
        assert_eq!(
            session.oauth_action,
            Some(crate::oauth::Action::Like("abc".to_string()))
        );

        press_while_playing(&mut app, KeyCode::Char('u'), &mut session).await;
        assert_eq!(
            session.oauth_action,
            Some(crate::oauth::Action::Subscribe("UC1".to_string()))
        );
    }

    /// アクション行 (80x24 の端末では y=21) の押し込み。
    fn action_click(column: u16) -> MouseEvent {
        mouse(MouseEventKind::Down(MouseButton::Left), column, 21)
    }

    async fn click_while_playing(app: &mut App, event: MouseEvent, session: &mut Session) {
        let (tx, _rx) = channel();
        handle_mouse_playing_with(app, event, &tx, session, fake_oauth()).await;
    }

    #[tokio::test]
    async fn clicking_the_like_icon_likes_instead_of_seeking() {
        let mut session = Session::default();
        let mut app = playing_channel_app();

        click_while_playing(&mut app, action_click(0), &mut session).await;

        assert_eq!(
            session.oauth_action,
            Some(crate::oauth::Action::Like("abc".to_string()))
        );
        assert_eq!(app.playback.time_pos, Some(0.0), "シークは走らない");
        assert_eq!(app.seek_bar.drag, None);
    }

    #[tokio::test]
    async fn clicking_the_subscribe_icon_subscribes() {
        let mut session = Session::default();
        let mut app = playing_channel_app();
        // いいねラベルと区切りの右。
        let column = crate::grid::display_width(ui::ActionKind::Like.label()) as u16 + 2;

        click_while_playing(&mut app, action_click(column), &mut session).await;

        assert_eq!(
            session.oauth_action,
            Some(crate::oauth::Action::Subscribe("UC1".to_string()))
        );
        assert_eq!(app.playback.time_pos, Some(0.0), "シークは走らない");
    }

    #[tokio::test]
    async fn a_click_next_to_the_icons_does_nothing() {
        let mut session = Session::default();
        let mut app = playing_channel_app();

        click_while_playing(&mut app, action_click(70), &mut session).await;

        assert!(session.oauth_action.is_none());
        assert_eq!(app.playback.time_pos, Some(0.0));
    }

    #[tokio::test]
    async fn the_seek_bar_still_answers_clicks_with_the_action_row_in_place() {
        let mut session = Session::default();
        let mut app = playing_channel_app();

        let down = mouse(MouseEventKind::Down(MouseButton::Left), 0, 20);
        click_while_playing(&mut app, down, &mut session).await;
        let up = mouse(MouseEventKind::Up(MouseButton::Left), 13, 20);
        click_while_playing(&mut app, up, &mut session).await;

        assert_eq!(app.playback.time_pos, Some(130.0));
        assert!(session.oauth_action.is_none(), "いいねは走らない");
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
    async fn the_other_playing_keys_do_not_copy() {
        let (tx, _rx) = channel();
        // c は既存のキーと重なっていない。
        assert_eq!(playing_command(KeyCode::Char('c')), None);
        assert_eq!(seek_step(KeyCode::Char('c')), None);
        assert_eq!(speed_step(KeyCode::Char('c')), None);

        let mut session = Session::default();
        let mut app = playing_url_app();
        let clipboard = FakeClipboard::new(CopyResult::Ok);
        for code in [
            KeyCode::Char(' '),
            KeyCode::Char('w'),
            KeyCode::Char('['),
            KeyCode::Char(']'),
            KeyCode::Backspace,
            KeyCode::Left,
            KeyCode::Right,
            KeyCode::Up,
            KeyCode::Down,
            KeyCode::Esc,
        ] {
            handle_key_playing_with(
                &mut app,
                key(code),
                &tx,
                &mut session,
                clipboard.clone(),
                fake_oauth(),
                Some(&temp_config("scratch")),
            )
            .await;
        }
        assert!(clipboard.copied().is_empty());
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
    async fn q_stops_playback_without_quitting_the_app() {
        let (tx, _rx) = channel();
        let mut session = Session::default();
        let mut app = App {
            mode: Mode::Playing,
            ..App::default()
        };

        // player が無い間もキー処理は進み、送信だけが飛ばされる。
        // q は Esc と同じく再生を止めるだけ。アプリごと終了しない。
        handle_key_playing(&mut app, key(KeyCode::Esc), &tx, &mut session).await;
        assert!(!app.should_quit);
        assert!(app.error.is_none());

        handle_key_playing(&mut app, key(KeyCode::Char('q')), &tx, &mut session).await;
        assert!(!app.should_quit);
        assert!(app.error.is_none());
    }

    #[tokio::test]
    async fn mouse_release_on_the_bar_records_an_optimistic_seek() {
        let (tx, _rx) = channel();
        let mut session = Session::default();
        let mut app = playing_app();

        let down = mouse(MouseEventKind::Down(MouseButton::Left), 0, 20);
        handle_mouse(&mut app, down, &tx, &mut session).await;
        let up = mouse(MouseEventKind::Up(MouseButton::Left), 13, 20);
        handle_mouse(&mut app, up, &tx, &mut session).await;

        assert_eq!(app.playback.time_pos, Some(130.0));
        assert!(app.playback.pending_seek.is_some());
        // player が無い間は送信だけが飛ばされる。
        assert!(app.error.is_none());
        assert_eq!(app.seek_bar.drag, None);
    }

    #[tokio::test]
    async fn a_release_from_another_button_does_not_end_the_drag() {
        let (tx, _rx) = channel();
        let mut session = Session::default();
        let mut app = playing_app();

        let down = mouse(MouseEventKind::Down(MouseButton::Left), 0, 20);
        handle_mouse(&mut app, down, &tx, &mut session).await;
        // ドラッグ中の右クリックでシークが飛ばない。左ドラッグはそのまま続く。
        let other = mouse(MouseEventKind::Up(MouseButton::Right), 40, 20);
        handle_mouse(&mut app, other, &tx, &mut session).await;
        assert_eq!(app.playback.time_pos, Some(0.0));
        assert_eq!(app.seek_bar.drag, Some(0));

        let up = mouse(MouseEventKind::Up(MouseButton::Left), 13, 20);
        handle_mouse(&mut app, up, &tx, &mut session).await;
        assert_eq!(app.playback.time_pos, Some(130.0));
        assert_eq!(app.seek_bar.drag, None);
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

    /// タブ行 (80x24 の端末では y=3) の押し込み。
    fn tab_click(column: u16) -> MouseEvent {
        mouse(MouseEventKind::Down(MouseButton::Left), column, 3)
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
    async fn y_or_enter_confirms_the_quit() {
        let (tx, _rx) = channel();
        for code in [KeyCode::Char('y'), KeyCode::Char('Y'), KeyCode::Enter] {
            let mut session = Session::default();
            let mut app = App {
                confirm_quit: true,
                ..App::default()
            };
            handle_key(&mut app, key(code), &tx, &mut session).await;
            assert!(app.should_quit, "{code:?}");
            assert!(!app.confirm_quit, "{code:?}");
        }
    }

    #[tokio::test]
    async fn n_or_esc_cancels_the_quit() {
        let (tx, _rx) = channel();
        for code in [KeyCode::Char('n'), KeyCode::Char('N'), KeyCode::Esc] {
            let mut session = Session::default();
            let mut app = App {
                confirm_quit: true,
                ..App::default()
            };
            handle_key(&mut app, key(code), &tx, &mut session).await;
            assert!(!app.should_quit, "{code:?}");
            assert!(!app.confirm_quit, "{code:?}");
        }
    }

    #[tokio::test]
    async fn other_keys_are_ignored_while_confirming_quit() {
        let (tx, _rx) = channel();
        let mut session = Session::default();
        // 入力モードなら本来は検索語になるキーも、確認中は一切通さない。
        let mut app = App {
            confirm_quit: true,
            ..App::default()
        };
        handle_key(&mut app, key(KeyCode::Char('a')), &tx, &mut session).await;
        assert!(app.confirm_quit, "確認状態のまま");
        assert!(!app.should_quit);
        assert!(app.query.text().is_empty(), "検索語に入ってはいけない");

        handle_key(&mut app, key(KeyCode::Down), &tx, &mut session).await;
        assert!(app.confirm_quit, "他のキーで解除されない");

        // Tab や Ctrl+S のようなモード遷移キーも通さない。
        handle_key(&mut app, key(KeyCode::Tab), &tx, &mut session).await;
        assert_eq!(app.mode, Mode::Input, "確認中はタブ切替も起きない");
        handle_key(&mut app, ctrl(KeyCode::Char('s')), &tx, &mut session).await;
        assert_ne!(app.mode, Mode::Settings, "確認中は設定も開かない");
        assert!(app.confirm_quit);
    }

    #[tokio::test]
    async fn ctrl_y_and_ctrl_n_do_not_answer_the_quit_confirmation() {
        let (tx, _rx) = channel();
        for code in [KeyCode::Char('y'), KeyCode::Char('n')] {
            let mut session = Session::default();
            let mut app = App {
                confirm_quit: true,
                ..App::default()
            };
            handle_key(&mut app, ctrl(code), &tx, &mut session).await;
            assert!(app.confirm_quit, "{code:?} は修飾キー付きなので無視する");
            assert!(!app.should_quit, "{code:?}");
        }
    }

    #[tokio::test]
    async fn ctrl_c_still_quits_immediately_while_confirming_quit() {
        let (tx, _rx) = channel();
        let mut session = Session::default();
        let mut app = App {
            confirm_quit: true,
            ..App::default()
        };
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        handle_key(&mut app, ctrl_c, &tx, &mut session).await;
        assert!(app.should_quit, "確認中でも Ctrl+C は逃げ道として残す");
    }

    #[tokio::test]
    async fn the_mouse_does_nothing_while_confirming_quit() {
        let (tx, _rx) = channel();
        let mut session = Session::default();
        let mut app = grid_app(1);
        app.confirm_quit = true;

        handle_mouse(
            &mut app,
            mouse(MouseEventKind::Down(MouseButton::Left), 0, 0),
            &tx,
            &mut session,
        )
        .await;

        assert!(app.confirm_quit, "確認状態のまま");
        assert!(!app.should_quit);
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

    fn cell_click(app: &App, index: usize) -> MouseEvent {
        let layout = ui::grid_layout(app, CELL).expect("格子を組める");
        let image = layout.cells[index].image;
        mouse(MouseEventKind::Down(MouseButton::Left), image.x, image.y)
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

    /// 検索のやり直しが届いた状態。描き直す前なので、画面は古い一覧のまま。
    fn swap_in_new_results(app: &mut App) {
        let results: Vec<SearchResult> = (0..10).map(|i| result(&format!("new{i}"))).collect();
        app.set_results(results, &crate::cookies::Target::Search("q".to_string()));
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
    async fn clicking_a_cell_while_playing_does_not_start_another_playback() {
        let (tx, _rx) = channel();
        let mut session = Session::default();
        let mut app = grid_app(10);
        let at = cell_click(&app, 1);
        app.mode = Mode::Playing;
        app.selected = 3;

        handle_mouse(&mut app, at, &tx, &mut session).await;

        assert_eq!(app.selected, 3);
        assert_eq!(session.player_nonce, 0);
    }

    #[tokio::test]
    async fn clicking_the_tab_row_while_playing_does_not_switch_tabs() {
        let (tx, _rx) = channel();
        let mut session = Session::default();
        let mut app = playing_app();

        handle_mouse(&mut app, tab_click(9), &tx, &mut session).await;
        assert_eq!(app.tabs.selected(), 0, "再生中はシークだけ");
        assert!(!take_search(&mut session));
        assert_eq!(app.seek_bar, SeekBarState::default());
    }

    #[tokio::test]
    async fn arrow_keys_seek_relative_to_the_pending_target_and_clear_hover() {
        let (tx, _rx) = channel();
        let mut session = Session::default();
        let mut app = App {
            playback: Playback {
                time_pos: Some(10.0),
                duration: Some(650.0),
                ..Playback::default()
            },
            seek_bar: SeekBarState {
                hover: Some(3),
                drag: None,
            },
            ..playing_app()
        };

        handle_key_playing(&mut app, key(KeyCode::Right), &tx, &mut session).await;
        assert_eq!(app.playback.time_pos, Some(15.0));
        handle_key_playing(&mut app, key(KeyCode::Right), &tx, &mut session).await;
        assert_eq!(app.playback.time_pos, Some(20.0));
        handle_key_playing(&mut app, key(KeyCode::Left), &tx, &mut session).await;
        assert_eq!(app.playback.time_pos, Some(15.0));
        assert_eq!(app.seek_bar.hover, None);
    }

    #[tokio::test]
    async fn ctrl_c_quits_while_playing() {
        let (tx, _rx) = channel();
        let mut session = Session::default();
        let mut app = App {
            mode: Mode::Playing,
            ..App::default()
        };
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        handle_key(&mut app, ctrl_c, &tx, &mut session).await;

        assert!(app.should_quit);
    }

    #[tokio::test]
    async fn ctrl_c_remembers_the_playback_position_before_quitting() {
        let (tx, _rx) = channel();
        let mut session = Session::default();
        let dir = std::env::temp_dir().join(format!(
            "tuitube-input-ctrl-c-resume-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let mut app = App {
            mode: Mode::Playing,
            resume: crate::resume::load_from(Some(&dir.join("resume.toml"))),
            playback: Playback {
                id: "v1".to_string(),
                time_pos: Some(120.0),
                duration: Some(600.0),
                ..Playback::default()
            },
            ..App::default()
        };
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);

        handle_key(&mut app, ctrl_c, &tx, &mut session).await;

        assert!(app.should_quit);
        assert_eq!(app.resume.lookup("v1"), Some(120.0));
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

    /// Ctrl を押しながらのキー。
    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
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
    async fn capital_s_is_ignored_while_playing() {
        let (tx, _rx) = channel();
        let mut session = Session::default();
        let sent = record(&mut session);
        let mut app = playing_app();

        handle_key_playing(&mut app, key(KeyCode::Char('S')), &tx, &mut session).await;

        assert_eq!(app.mode, Mode::Playing);
        assert!(sent.lock().expect("溜め込み先").is_empty());
        assert!(app.subtitles.wanted(), "小文字 s の字幕とは別のキー");
    }

    #[tokio::test]
    async fn ctrl_c_still_quits_from_the_settings() {
        let (tx, _rx) = channel();
        let mut session = Session::default();
        let mut app = App {
            mode: Mode::Settings,
            ..App::default()
        };
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        handle_key(&mut app, ctrl_c, &tx, &mut session).await;

        assert!(app.should_quit);
    }

    #[tokio::test]
    async fn the_mouse_is_ignored_on_the_settings_screen() {
        let (tx, _rx) = channel();
        let mut session = Session::default();
        let mut app = App {
            mode: Mode::Settings,
            ..playing_app()
        };
        let down = mouse(MouseEventKind::Down(MouseButton::Left), 0, 20);
        handle_mouse(&mut app, down, &tx, &mut session).await;
        // 設定画面ではタブ行の桁も設定一覧の一部なので、クリックでタブを移さない。
        handle_mouse(&mut app, tab_click(9), &tx, &mut session).await;

        assert_eq!(app.seek_bar, SeekBarState::default());
        assert_eq!(app.settings, crate::settings::Settings::default());
        assert_eq!(app.tabs.selected(), 0);
        assert!(!take_search(&mut session));
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
    async fn h_is_not_taken_by_the_other_screens() {
        let (tx, _rx) = channel();
        let mut session = Session::default();
        // 再生中の h は今までどおり何もしない。
        let mut app = playing_app();
        handle_key_playing(&mut app, key(KeyCode::Char('h')), &tx, &mut session).await;

        assert_eq!(app.mode, Mode::Playing);
        assert!(app.notice.is_none());
        assert!(app.hidden.videos.is_empty());
    }

    #[tokio::test]
    async fn tab_is_ignored_while_playing() {
        let (tx, _rx) = channel();
        let mut session = Session::default();
        let mut app = App {
            mode: Mode::Playing,
            ..playing_app()
        };
        for code in [KeyCode::Tab, KeyCode::BackTab, KeyCode::Char('r')] {
            handle_key_playing(&mut app, key(code), &tx, &mut session).await;
        }
        assert_eq!(app.tabs.selected(), 0);
        assert!(session.search_task.is_none());
        assert!(!app.should_quit);
    }

    // ---- ダウンロード画面 ----

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

    #[tokio::test]
    async fn d_opens_the_download_screen_while_playing() {
        let (tx, _rx) = channel();
        let mut session = Session::default();
        let mut app = App {
            playback: Playback {
                title: "曲".to_string(),
                url: "https://www.youtube.com/watch?v=song1".to_string(),
                ..Playback::default()
            },
            ..playing_app()
        };

        handle_key(&mut app, key(KeyCode::Char('d')), &tx, &mut session).await;

        assert_eq!(app.mode, Mode::Download);
        assert_eq!(app.download.return_mode, Mode::Playing);
        assert_eq!(app.download.url, "https://www.youtube.com/watch?v=song1");
        assert_eq!(app.download.filename.text(), "曲");
    }
}
