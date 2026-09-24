//! 再生画面 (Mode::Playing)。キーとマウス (シークバー・アクション行)、映像・コメント・
//! シークバー・アクション行の描画、状態行と案内を持つ。
//! 画面の割り付け (video_area など) は main.rs・actions.rs も使うのでここから公開する。

use crate::actions::{
    CommentScroll, Oauth, SEEK_STEP_SECS, Session, change_speed, config_path_from_env,
    copy_url_with, cycle_display_mode, enter_background, like_video, reset_speed, save_video,
    scroll_comments, seek_absolute, seek_relative, send_to_player, subscribe_playing_channel,
    toggle_comments, toggle_subtitles,
};
use crate::app::{App, AppEvent, Mode, format_time};
use crate::clipboard::{Clipboard, Pbcopy};
use crate::comments;
use crate::display::DisplayMode;
use crate::grid;
use crate::mpv::{self, MpvCommand};
use crate::oauth;
use crate::screen::download::open_download;
use crate::seekbar::{MouseAction, MouseInput, SeekBar, SeekBarLayout, label_text, label_width};
use crate::ui;
use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};
use std::path::Path;
use std::time::Instant;
use tokio::sync::mpsc::UnboundedSender;

// ---- 割り付け ----

/// 別ウィンドウ再生中に映像領域へ出す案内。
fn window_placeholder(display: DisplayMode) -> String {
    format!("別ウィンドウで再生中  w: {}へ", display.next().label())
}

/// 再生中は [映像, シークバー, アクション, ステータス, ヘルプ] の5段。映像に残り全体を渡す。
fn playing_areas(area: Rect) -> [Rect; 5] {
    Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(1),
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

/// 隅のミニプレイヤーの幅・高さ。設定項目にはしない固定値。
const MINI_VIDEO_COLS: u16 = 32;

const MINI_VIDEO_ROWS: u16 = 10;

/// text は文字セルの数がそのまま解像度になるため、embedded (画像) より広く取らないと
/// 映像に見えない (実機で確認した不具合。#62)。
const TEXT_MINI_VIDEO_COLS: u16 = 64;

const TEXT_MINI_VIDEO_ROWS: u16 = 18;

/// 結果一覧の右上に切り出す、隅のミニプレイヤー用の矩形。
pub fn mini_video_area(screen: Rect, display: DisplayMode) -> Rect {
    let (max_cols, max_rows) = match display {
        DisplayMode::Text => (TEXT_MINI_VIDEO_COLS, TEXT_MINI_VIDEO_ROWS),
        DisplayMode::Embedded | DisplayMode::Window => (MINI_VIDEO_COLS, MINI_VIDEO_ROWS),
    };
    let results = ui::search_areas(screen)[2];
    let cols = max_cols.min(results.width / 2).max(1);
    let rows = max_rows.min(results.height).max(1);
    Rect::new(results.right().saturating_sub(cols), results.y, cols, rows)
}

/// 今のフレームで映像をどこに描くか。無ければ None (window 中・映像が無い)。
pub fn video_target_area(app: &App, screen: Rect) -> Option<Rect> {
    app.video.as_ref()?;
    if app.mode == Mode::Playing {
        return Some(video_area(screen));
    }
    match app.display {
        DisplayMode::Window => None,
        DisplayMode::Embedded | DisplayMode::Text => Some(mini_video_area(screen, app.display)),
    }
}

/// シークバーの行。クリック桁から再生位置を求めるときもこの矩形を使う。
pub fn seek_bar_area(area: Rect) -> Rect {
    playing_areas(area)[1]
}

/// いいね・チャンネル登録の行。クリック位置から操作を求めるときもこの矩形を使う。
pub fn action_area(area: Rect) -> Rect {
    playing_areas(area)[2]
}

/// 再生状態を出す行。
pub fn status_area(area: Rect) -> Rect {
    playing_areas(area)[3]
}

/// 操作説明の行。
pub fn help_area(area: Rect) -> Rect {
    playing_areas(area)[4]
}

/// コメント一覧の枠の内側。描画と送り幅が同じ寸法を数える。
pub fn comments_viewport(screen: Rect) -> Rect {
    comments_block().inner(video_area(screen))
}

/// 描画とヒットテストが共有する割り付け。
pub fn seek_bar_layout(app: &App) -> SeekBarLayout {
    layout_for(app.screen, app.playback.duration)
}

fn layout_for(screen: Rect, duration: Option<f64>) -> SeekBarLayout {
    SeekBarLayout::new(seek_bar_area(screen), label_width(duration))
}

// ---- 状態行 ----

/// 再生中はエラーで再生状況を隠さず、併記する。
pub fn playing_status(app: &App) -> String {
    let line = playback_line(app);
    match &app.error {
        Some(error) => format!("{line}  |  エラー: {error}"),
        None => line,
    }
}

fn playback_line(app: &App) -> String {
    let state = match app.playback.paused {
        Some(true) => "PAUSED",
        Some(false) => "PLAYING",
        None => "状態不明",
    };
    let volume = app
        .playback
        .volume
        .map(|v| format!("  vol {v:.0}"))
        .unwrap_or_default();
    // 狭い端末では末尾から切れるので、字幕の印は行の前方に置く。
    let subtitles = app
        .subtitles
        .marker(&app.settings.subtitles, Instant::now())
        .map(|marker| format!("  {marker}"))
        .unwrap_or_default();
    format!(
        "{state}{subtitles}  {}  {} / {}{volume}  {}  {}",
        app.playback.title,
        format_time(app.playback.time_pos),
        format_time(app.playback.duration),
        app.speed.label(),
        app.display_label()
    )
}

// ---- キー ----

pub async fn handle_key_playing(
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
        KeyCode::Char('a') => save_video(app, tx, session, deps),
        _ => {}
    }
    if key.code == KeyCode::Char('d') {
        open_download(app, session);
    }
    if key.code == KeyCode::Char('b') {
        enter_background(app, session).await;
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

// ---- マウス ----

pub async fn handle_mouse_playing(
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
            ActionKind::Like => like_video(app, tx, session, deps),
            ActionKind::Subscribe => subscribe_playing_channel(app, tx, session, deps),
            ActionKind::Save => save_video(app, tx, session, deps),
        }
        return;
    }
    let Some(input) = mouse_input(mouse.kind) else {
        return;
    };
    // 描画とヒットテストが同じ割り付けを通るので、印とクリック位置が食い違わない。
    let layout = seek_bar_layout(app);
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
fn playing_action_click(app: &App, mouse: MouseEvent) -> Option<ActionKind> {
    if mouse.kind != MouseEventKind::Down(MouseButton::Left) {
        return None;
    }
    action_at_point(app, mouse.column, mouse.row)
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

// ---- 描画 ----

/// 分岐は網羅する。モードを増やしたときの描き分け漏れをコンパイラに拾わせる。
pub fn draw_playing(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let video = video_area(area);
    if app.comments.visible() {
        draw_comments(frame, video, app);
    } else {
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
    }
    draw_seek_bar(frame, app, area);
    draw_actions(frame, app, action_area(area));
    ui::draw_footer(frame, app, status_area(area), help_area(area));
}

fn comments_block() -> Block<'static> {
    Block::default().borders(Borders::ALL).title(" コメント ")
}

/// コメント表示中は映像の代わりに一覧を出す。映像フレームの送出は present_video が止める。
/// 上限 50 件は 1 画面に入らないので、↑↓ の送り幅ぶんだけずらして描く。
fn draw_comments(frame: &mut Frame, area: Rect, app: &App) {
    if area.height == 0 {
        return;
    }
    let block = comments_block();
    let inner = block.inner(area);
    let height = inner.height as usize;
    let lines = comments::display_lines(app.comments.state(), inner.width as usize);
    let offset = app.comments.scroll(lines.len(), height);
    let items: Vec<ListItem> = lines
        .into_iter()
        .skip(offset)
        .take(height)
        .map(ListItem::new)
        .collect();
    frame.render_widget(List::new(items).block(block), area);
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

/// アクション行に出す操作。キーでもクリックでも同じものを指す。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionKind {
    Like,
    Subscribe,
    Save,
}

/// ラベルの区切り。
const ACTION_GAP: &str = "  ";

impl ActionKind {
    pub fn label(self) -> &'static str {
        match self {
            ActionKind::Like => "♥いいね",
            ActionKind::Subscribe => "＋登録",
            ActionKind::Save => "★保存",
        }
    }

    /// 済みのときの色。未は一律 Color::Gray。
    fn done_color(self) -> Color {
        match self {
            ActionKind::Like => Color::Red,
            ActionKind::Subscribe => Color::Green,
            ActionKind::Save => Color::Yellow,
        }
    }
}

/// アクション行に左から並ぶもの。描画とクリックの当たり判定が同じ並びを通るように、
/// 幅の食い方はここだけで決める。
enum ActionPiece {
    Gap,
    Label { kind: ActionKind, done: bool },
}

impl ActionPiece {
    fn text(&self) -> &'static str {
        match self {
            ActionPiece::Gap => ACTION_GAP,
            ActionPiece::Label { kind, .. } => kind.label(),
        }
    }
}

fn action_pieces(app: &App) -> Vec<ActionPiece> {
    let mut pieces = vec![ActionPiece::Label {
        kind: ActionKind::Like,
        done: app.engagement.is_liked(&app.playback.id),
    }];
    // チャンネル ID の無い行から始めた再生では押せないので、登録は並べない。
    if let Some(channel_id) = app.playback.channel_id.as_deref() {
        pieces.push(ActionPiece::Gap);
        pieces.push(ActionPiece::Label {
            kind: ActionKind::Subscribe,
            // 未確認 (None) は未登録と同じ見た目にする。
            done: app.engagement.is_subscribed(channel_id).unwrap_or(false),
        });
    }
    pieces.push(ActionPiece::Gap);
    pieces.push(ActionPiece::Label {
        kind: ActionKind::Save,
        done: app.saved_videos.contains(&app.playback.id),
    });
    pieces
}

fn action_spans(app: &App) -> Vec<Span<'static>> {
    action_pieces(app)
        .into_iter()
        .map(|piece| match piece {
            ActionPiece::Gap => Span::raw(ACTION_GAP),
            ActionPiece::Label { kind, done } => {
                let style = if done {
                    Style::default()
                        .fg(kind.done_color())
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::Gray)
                };
                Span::styled(kind.label(), style)
            }
        })
        .collect()
}

fn draw_actions(frame: &mut Frame, app: &App, area: Rect) {
    frame.render_widget(Paragraph::new(Line::from(action_spans(app))), area);
}

/// アクション行の `column` 桁にある操作。区切りの上では None。
fn action_at_column(app: &App, column: usize) -> Option<ActionKind> {
    let mut x = 0;
    for piece in action_pieces(app) {
        let cells = grid::display_width(piece.text());
        if let ActionPiece::Label { kind, .. } = piece
            && (x..x + cells).contains(&column)
        {
            return Some(kind);
        }
        x += cells;
    }
    None
}

/// 画面のこの位置にある操作。アクション行の外や区切りの上では None。
pub fn action_at_point(app: &App, column: u16, row: u16) -> Option<ActionKind> {
    let area = action_area(app.screen);
    if !area.contains(Position::new(column, row)) {
        return None;
    }
    action_at_column(app, (column - area.x) as usize)
}

/// 再生中の案内。全部で 130 桁ほどあり 80 桁端末には入らないので、
/// 落ちて困らないものを後ろに置く。先頭 7 つは最も幅を食う w:別ウィンドウとコメント表示中でも
/// 77 桁に収まる。
pub fn playing_hints(
    display: DisplayMode,
    comments_open: bool,
    can_subscribe: bool,
) -> Vec<String> {
    let mut hints = vec![
        "space:一時停止".to_string(),
        "←→:シーク".to_string(),
        // コメント表示中の ↑↓ は一覧送りに使う。
        if comments_open {
            "↑↓:行送り".to_string()
        } else {
            "↑↓:音量".to_string()
        },
        "c:URLコピー".to_string(),
        format!("w:{}", display.next().label()),
        // q も Esc と同じく再生を止めるだけで、アプリは終えない。
        "Esc/q:停止".to_string(),
        "♥l:いいね".to_string(),
    ];
    // チャンネル ID の無い再生では押しても何も起きないので、案内も出さない。
    if can_subscribe {
        hints.push("＋u:登録".to_string());
    }
    hints.push("★a:保存".to_string());
    hints.extend([
        "s:字幕".to_string(),
        "o:コメント".to_string(),
        "[ ]:速度±0.1".to_string(),
        "BS:等速".to_string(),
        "クリック:シーク".to_string(),
        "b:検索へ".to_string(),
    ]);
    hints
}

#[cfg(test)]
mod tests {
    // 観点ごとに画面の組み立て方が違うので、小分けにしてそれぞれにヘルパーを持たせる。
    /// 状態行。
    mod state {
        use crate::app::{App, Mode, Playback};
        use crate::speed::Speed;
        use serde_json::{Value, json};
        use std::time::Instant;

        /// 送り返しが要らない取り込み。返り値まで込みで確かめる。
        fn poll(app: &mut App, id: u64, data: Option<Value>) {
            assert_eq!(app.apply_property(id, data), None, "送り返しは出ない");
        }

        fn playing_subtitle_app(title: &str) -> App {
            App {
                mode: Mode::Playing,
                playback: Playback {
                    title: title.to_string(),
                    paused: Some(false),
                    ..Playback::default()
                },
                ..App::default()
            }
        }

        #[test]
        fn the_status_line_names_the_track_that_mpv_chose() {
            // lang="ja-orig,ja" でも ja が選ばれることがある (実測)。印は選ばれた方を出す。
            let mut app = playing_subtitle_app("song");
            poll(&mut app, crate::mpv::REQ_SID, Some(json!(1)));
            poll(&mut app, crate::mpv::REQ_SUB_LANG, Some(json!("ja")));
            assert!(app.status_line().starts_with("PLAYING  字幕ja  song"));
        }

        #[test]
        fn the_status_line_puts_the_subtitle_marker_right_after_the_state() {
            let mut app = playing_subtitle_app("song");
            poll(&mut app, crate::mpv::REQ_SID, Some(json!(1)));
            let line = app.status_line();
            assert!(line.starts_with("PLAYING  字幕ja-orig  song"), "{line}");

            // 消しているときは桁を使わない。
            app.subtitles.set_wanted(false, Instant::now());
            let line = app.status_line();
            assert!(line.starts_with("PLAYING  song"), "{line}");
            assert!(!line.contains("字幕"), "{line}");
        }

        #[test]
        fn the_subtitle_marker_stays_ahead_of_a_long_title() {
            // 狭い端末では行の末尾から切れるので、印は必ずタイトルより前に出す。
            let mut app = playing_subtitle_app(&"長いタイトル".repeat(20));
            poll(&mut app, crate::mpv::REQ_SID, Some(json!(1)));
            let line = app.status_line();
            let marker_at = line.find("字幕").expect("印がある");
            let title_at = line.find("長いタイトル").expect("タイトルがある");
            assert!(marker_at < title_at, "{line}");
            assert!(marker_at < 10, "{line}");
        }

        #[test]
        fn the_status_line_shows_the_speed_after_the_volume() {
            let mut app = App {
                mode: Mode::Playing,
                speed: Speed::from_tenths(15).expect("1.5x"),
                playback: Playback {
                    title: "song".to_string(),
                    paused: Some(false),
                    volume: Some(70.0),
                    ..Playback::default()
                },
                ..App::default()
            };
            let line = app.status_line();
            assert!(line.contains("vol 70  1.5x  ["), "{line}");

            // 等速でも出す。戻ったことが分かるため。
            app.speed = Speed::NORMAL;
            assert!(
                app.status_line().contains("vol 70  1.0x  ["),
                "{}",
                app.status_line()
            );
        }

        #[test]
        fn pause_error_response_is_not_read_as_playing() {
            let mut app = App {
                mode: Mode::Playing,
                ..App::default()
            };
            poll(&mut app, crate::mpv::REQ_PAUSE, Some(json!(true)));
            assert!(app.status_line().starts_with("PAUSED"));
            poll(&mut app, crate::mpv::REQ_PAUSE, None);
            assert_eq!(app.playback.paused, None);
            assert!(!app.status_line().starts_with("PLAYING"));
        }

        #[test]
        fn the_status_line_ends_with_the_display_label_while_playing() {
            let app = App {
                mode: Mode::Playing,
                playback: Playback {
                    title: "song".to_string(),
                    ..Playback::default()
                },
                ..App::default()
            };
            assert!(
                app.status_line().ends_with(&app.display_label()),
                "{}",
                app.status_line()
            );
        }

        #[test]
        fn playing_status_stays_visible_with_error() {
            let app = App {
                mode: Mode::Playing,
                error: Some("boom".to_string()),
                playback: Playback {
                    title: "song".to_string(),
                    paused: Some(true),
                    time_pos: Some(30.0),
                    duration: Some(60.0),
                    volume: Some(70.0),
                    ..Playback::default()
                },
                ..App::default()
            };
            let line = app.status_line();
            assert!(line.starts_with("PAUSED"), "{line}");
            assert!(line.contains("song  00:30 / 01:00  vol 70"), "{line}");
            assert!(line.ends_with("エラー: boom"));
        }

        #[test]
        fn playing_status_without_pause_data_is_marked_unknown() {
            let app = App {
                mode: Mode::Playing,
                ..App::default()
            };
            assert!(app.status_line().starts_with("状態不明"));
        }
    }

    /// キーとマウス。
    mod keys {
        use super::super::*;
        use crate::actions::Session;
        use crate::app::{App, AppEvent, Mode, Playback};
        use crate::clipboard::fixtures::{CopyResult, FakeClipboard};
        use crate::display::DisplayMode;
        use crate::input::{handle_key, handle_mouse};
        use crate::oauth::fixtures::FakeBackend;
        use crate::search::SearchResult;
        use crate::seekbar::SeekBarState;
        use crate::video::CellSize;
        use crossterm::event::{
            KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
        };
        use ratatui::layout::Rect;
        use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

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

        const SID_AUTO: &str = "{\"command\":[\"set_property\",\"sid\",\"auto\"]}\n";

        const SID_NO: &str = "{\"command\":[\"set_property\",\"sid\",\"no\"]}\n";

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

        /// アクション行 (80x24 の端末では y=21) の押し込み。
        fn action_click(column: u16) -> MouseEvent {
            mouse(MouseEventKind::Down(MouseButton::Left), column, 21)
        }

        async fn click_while_playing(app: &mut App, event: MouseEvent, session: &mut Session) {
            let (tx, _rx) = channel();
            handle_mouse_playing_with(app, event, &tx, session, fake_oauth()).await;
        }

        /// タブ行 (80x24 の端末では y=3) の押し込み。
        fn tab_click(column: u16) -> MouseEvent {
            mouse(MouseEventKind::Down(MouseButton::Left), column, 3)
        }

        fn cell_click(app: &App, index: usize) -> MouseEvent {
            let layout = ui::grid_layout(app, CELL).expect("格子を組める");
            let image = layout.cells[index].image;
            mouse(MouseEventKind::Down(MouseButton::Left), image.x, image.y)
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
            let column = crate::grid::display_width(ActionKind::Like.label()) as u16 + 2;

            click_while_playing(&mut app, action_click(column), &mut session).await;

            assert_eq!(
                session.oauth_action,
                Some(crate::oauth::Action::Subscribe("UC1".to_string()))
            );
            assert_eq!(app.playback.time_pos, Some(0.0), "シークは走らない");
        }

        #[tokio::test]
        async fn a_while_playing_saves_the_video() {
            let mut session = Session::default();
            let mut app = playing_url_app();

            press_while_playing(&mut app, KeyCode::Char('a'), &mut session).await;

            assert_eq!(
                session.oauth_action,
                Some(crate::oauth::Action::Save("abc".to_string()))
            );
            assert_eq!(app.notice.as_deref(), Some(crate::oauth::SAVE_NOTICE));
        }

        #[tokio::test]
        async fn clicking_the_save_icon_saves() {
            let mut session = Session::default();
            let mut app = playing_channel_app();
            // いいね・区切り・登録・区切りの右。
            let column = (crate::grid::display_width(ActionKind::Like.label())
                + 2
                + crate::grid::display_width(ActionKind::Subscribe.label())
                + 2) as u16;

            click_while_playing(&mut app, action_click(column), &mut session).await;

            assert_eq!(
                session.oauth_action,
                Some(crate::oauth::Action::Save("abc".to_string()))
            );
            assert_eq!(app.playback.time_pos, Some(0.0), "シークは走らない");
        }

        #[tokio::test]
        async fn the_save_icon_sits_right_after_the_like_without_a_channel() {
            let mut session = Session::default();
            let mut app = playing_url_app();
            let column = crate::grid::display_width(ActionKind::Like.label()) as u16 + 2;

            click_while_playing(&mut app, action_click(column), &mut session).await;

            assert_eq!(
                session.oauth_action,
                Some(crate::oauth::Action::Save("abc".to_string()))
            );
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
        async fn moving_over_the_bar_marks_where_a_click_would_seek() {
            let (tx, _rx) = channel();
            let mut session = Session::default();
            let mut app = playing_app();

            let moved = mouse(MouseEventKind::Moved, 13, 20);
            handle_mouse(&mut app, moved, &tx, &mut session).await;

            assert_eq!(app.seek_bar.hover, Some(13));
            assert_eq!(
                app.playback.time_pos,
                Some(0.0),
                "動かすだけではシークしない"
            );
        }

        #[tokio::test]
        async fn dragging_along_the_bar_follows_the_pointer_until_the_release() {
            let (tx, _rx) = channel();
            let mut session = Session::default();
            let mut app = playing_app();

            let down = mouse(MouseEventKind::Down(MouseButton::Left), 0, 20);
            handle_mouse(&mut app, down, &tx, &mut session).await;
            let drag = mouse(MouseEventKind::Drag(MouseButton::Left), 13, 20);
            handle_mouse(&mut app, drag, &tx, &mut session).await;

            assert_eq!(app.seek_bar.drag, Some(13));
            assert_eq!(app.playback.time_pos, Some(0.0), "離すまではシークしない");
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

    /// 割り付け、描画、案内。
    mod draw {
        use super::super::*;
        use crate::app::{App, Mode, Playback};
        use crate::display::DisplayMode;
        use crate::grid;
        use crate::seekbar::SeekBarState;
        use crate::ui::{draw, help_text, search_areas};
        use crate::video::{Geometry, VideoSink};
        use ratatui::layout::Rect;
        use ratatui::style::{Color, Style};

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

        /// 再生中のアクション行を見るための 80x24 の App。
        fn action_app(channel_id: Option<&str>) -> App {
            App {
                mode: Mode::Playing,
                screen: Rect::new(0, 0, 80, 24),
                playback: Playback {
                    id: "v1".to_string(),
                    channel_id: channel_id.map(str::to_string),
                    ..Playback::default()
                },
                ..App::default()
            }
        }

        /// アクション行で `text` を含むラベルの style。
        fn action_style(app: &App, text: &str) -> Style {
            action_spans(app)
                .into_iter()
                .find(|span| span.content.contains(text))
                .expect("ラベルがある")
                .style
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

        /// コメントを取り終えた再生画面。
        fn commented_app(list: Vec<crate::comments::Comment>) -> App {
            let mut app = App {
                mode: Mode::Playing,
                display: DisplayMode::Embedded,
                screen: Rect::new(0, 0, 80, 24),
                ..App::default()
            };
            app.comments.begin("abc".to_string());
            app.comments.apply("abc", Ok(list));
            app
        }

        fn comment(author: &str, text: &str) -> crate::comments::Comment {
            crate::comments::Comment {
                author: author.to_string(),
                text: text.to_string(),
                like_count: Some(3),
            }
        }

        /// 1 件 2 行なので、80x24 の枠 (内側 18 行) には 9 件と少ししか入らない。
        fn many_comments(count: usize) -> App {
            commented_app(
                (0..count)
                    .map(|i| comment(&format!("author{i}"), &format!("本文{i}")))
                    .collect(),
            )
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
            assert_eq!(video_area(area), Rect::new(0, 0, 80, 20));
            assert_eq!(seek_bar_area(area), Rect::new(0, 20, 80, 1));
        }

        #[test]
        fn playing_help_mentions_the_speed_keys() {
            // 80 桁では入らないので、広い端末での案内で見る。
            let help = help_text(
                Mode::Playing,
                DisplayMode::Embedded,
                false,
                false,
                false,
                false,
                200,
            );
            assert!(help.contains("[ ]"), "{help}");
            assert!(help.contains("BS"), "{help}");
            assert!(help.contains("速度"), "{help}");
        }

        #[test]
        fn video_area_layout_is_unchanged_by_the_display_mode() {
            // プレースホルダは映像と同じ矩形に描くので、割り付けはモードで変わらない。
            let area = Rect::new(0, 0, 80, 24);
            assert_eq!(video_area(area), Rect::new(0, 0, 80, 20));
            assert_eq!(seek_bar_area(area), Rect::new(0, 20, 80, 1));
        }

        #[test]
        fn mini_video_area_sits_at_the_top_right_of_the_results_block() {
            let screen = Rect::new(0, 0, 80, 24);
            let results = search_areas(screen)[2];
            let mini = mini_video_area(screen, DisplayMode::Embedded);
            assert_eq!(mini.y, results.y);
            assert_eq!(mini.right(), results.right());
            assert_eq!(mini.width, 32);
            assert_eq!(mini.height, 10);
        }

        #[test]
        fn mini_video_area_shrinks_to_fit_a_small_results_block() {
            let screen = Rect::new(0, 0, 20, 10);
            let results = search_areas(screen)[2];
            let mini = mini_video_area(screen, DisplayMode::Embedded);
            assert!(
                mini.width <= results.width / 2 && mini.width >= 1,
                "{mini:?}"
            );
            assert!(
                mini.height <= results.height && mini.height >= 1,
                "{mini:?}"
            );
        }

        #[test]
        fn mini_video_area_is_larger_for_text_than_embedded() {
            // text は文字グリッドがそのまま解像度になるため、embedded より広く取る (#62)。
            let screen = Rect::new(0, 0, 80, 24);
            let embedded = mini_video_area(screen, DisplayMode::Embedded);
            let text = mini_video_area(screen, DisplayMode::Text);
            assert!(text.width > embedded.width, "{text:?} vs {embedded:?}");
            assert!(text.height > embedded.height, "{text:?} vs {embedded:?}");
        }

        #[test]
        fn mini_video_area_for_text_shrinks_to_fit_a_small_results_block() {
            let screen = Rect::new(0, 0, 20, 10);
            let results = search_areas(screen)[2];
            let mini = mini_video_area(screen, DisplayMode::Text);
            assert!(
                mini.width <= results.width / 2 && mini.width >= 1,
                "{mini:?}"
            );
            assert!(
                mini.height <= results.height && mini.height >= 1,
                "{mini:?}"
            );
        }

        #[test]
        fn video_target_area_is_none_without_a_video() {
            let app = App {
                mode: Mode::Playing,
                screen: Rect::new(0, 0, 80, 24),
                ..App::default()
            };
            assert_eq!(video_target_area(&app, app.screen), None);
        }

        #[test]
        fn video_target_area_is_the_full_video_area_while_playing() {
            // window であっても Mode::Playing 中は差がない (別ウィンドウでも kitty 用に用意はしてある)。
            for display in [
                DisplayMode::Embedded,
                DisplayMode::Text,
                DisplayMode::Window,
            ] {
                let app = video_app(Mode::Playing, display);
                assert_eq!(
                    video_target_area(&app, app.screen),
                    Some(video_area(app.screen)),
                    "{display:?}"
                );
            }
        }

        #[test]
        fn video_target_area_is_the_mini_area_while_backgrounded() {
            for display in [DisplayMode::Embedded, DisplayMode::Text] {
                let app = video_app(Mode::Results, display);
                assert_eq!(
                    video_target_area(&app, app.screen),
                    Some(mini_video_area(app.screen, display)),
                    "{display:?}"
                );
            }
        }

        #[test]
        fn video_target_area_is_none_in_window_mode_while_backgrounded() {
            let app = video_app(Mode::Results, DisplayMode::Window);
            assert_eq!(video_target_area(&app, app.screen), None);
        }

        #[test]
        fn the_playing_help_always_names_the_background_key() {
            let help = help_text(
                Mode::Playing,
                DisplayMode::Embedded,
                false,
                false,
                false,
                false,
                200,
            );
            assert!(help.contains("b:検索へ"), "{help}");
        }

        #[test]
        fn playing_rows_do_not_overlap_the_video() {
            let area = Rect::new(0, 0, 80, 24);
            assert_eq!(video_area(area), Rect::new(0, 0, 80, 20));
            assert_eq!(seek_bar_area(area), Rect::new(0, 20, 80, 1));
            assert_eq!(action_area(area), Rect::new(0, 21, 80, 1));
            assert_eq!(status_area(area), Rect::new(0, 22, 80, 1));
            assert_eq!(help_area(area), Rect::new(0, 23, 80, 1));
        }

        #[test]
        fn the_action_row_shows_the_like_and_the_subscribe_label() {
            let app = action_app(Some("UC1"));
            let row = drawn_row(&app, action_area(app.screen).y);
            assert!(row.contains("いいね"), "{row}");
            assert!(row.contains("登録"), "{row}");
        }

        #[test]
        fn the_action_row_hides_the_subscribe_label_without_a_channel_id() {
            // チャンネル ID の無い行から始めた再生では押せないので、印も出さない。
            let app = action_app(None);
            let row = drawn_row(&app, action_area(app.screen).y);
            assert!(row.contains("いいね"), "{row}");
            assert!(!row.contains("登録"), "{row}");
        }

        #[test]
        fn the_action_row_shows_the_save_label_colored_once_saved() {
            let gray = Style::default().fg(Color::Gray);
            let mut app = action_app(None);
            let row = drawn_row(&app, action_area(app.screen).y);
            assert!(row.contains("★保存"), "{row}");
            assert_eq!(action_style(&app, "保存"), gray);

            app.saved_videos.insert("v1".to_string());
            assert_ne!(action_style(&app, "保存"), gray, "保存した動画は色を付ける");
        }

        #[test]
        fn a_done_action_is_colored_and_an_undone_one_is_gray() {
            let gray = Style::default().fg(Color::Gray);
            let now = std::time::SystemTime::UNIX_EPOCH;
            let mut app = action_app(Some("UC1"));
            assert_eq!(action_style(&app, "いいね"), gray);
            assert_eq!(
                action_style(&app, "登録"),
                gray,
                "未確認は未登録と同じ見た目"
            );

            app.engagement.remember_subscription("UC1", false, now);
            assert_eq!(action_style(&app, "登録"), gray);

            app.engagement.remember_like("v1", true, now);
            app.engagement.remember_subscription("UC1", true, now);
            assert_ne!(action_style(&app, "いいね"), gray);
            assert_ne!(action_style(&app, "登録"), gray);
        }

        #[test]
        fn the_action_hit_test_agrees_with_the_drawn_row() {
            for channel_id in [None, Some("UC1")] {
                let app = action_app(channel_id);
                // 描いた行を左から辿り、各桁がどの操作の上かを並べる。
                let mut columns: Vec<Option<ActionKind>> = Vec::new();
                for piece in action_pieces(&app) {
                    let cells = grid::display_width(piece.text());
                    let kind = match piece {
                        ActionPiece::Label { kind, .. } => Some(kind),
                        ActionPiece::Gap => None,
                    };
                    columns.extend(std::iter::repeat_n(kind, cells));
                }
                for column in 0..columns.len() + 2 {
                    assert_eq!(
                        action_at_column(&app, column),
                        columns.get(column).copied().flatten(),
                        "{channel_id:?} / {column} 桁目"
                    );
                }
            }
        }

        #[test]
        fn only_the_action_row_answers_the_action_hit_test() {
            let app = action_app(Some("UC1"));
            let row = action_area(app.screen).y;
            assert_eq!(action_at_point(&app, 0, row), Some(ActionKind::Like));
            let subscribe_x = grid::display_width(ActionKind::Like.label()) as u16 + 2;
            assert_eq!(
                action_at_point(&app, subscribe_x, row),
                Some(ActionKind::Subscribe)
            );
            for other in [0u16, 1, row - 1, row + 1, 23] {
                assert_eq!(
                    action_at_point(&app, 0, other),
                    None,
                    "{other} 行目はアクション行でない"
                );
            }
        }

        #[test]
        fn playing_help_mentions_the_like_and_the_subscribe_key() {
            // 80 桁では主要キーが先で入らないので、広い端末での案内で見る。
            let wide = help_text(
                Mode::Playing,
                DisplayMode::Embedded,
                false,
                false,
                false,
                true,
                200,
            );
            assert!(wide.contains("l:いいね"), "{wide}");
            assert!(wide.contains("u:登録"), "{wide}");

            let no_channel = help_text(
                Mode::Playing,
                DisplayMode::Embedded,
                false,
                false,
                false,
                false,
                200,
            );
            assert!(no_channel.contains("l:いいね"), "{no_channel}");
            assert!(!no_channel.contains("u:登録"), "{no_channel}");
        }

        #[test]
        fn playing_help_mentions_the_save_key() {
            let wide = help_text(
                Mode::Playing,
                DisplayMode::Embedded,
                false,
                false,
                false,
                false,
                200,
            );
            assert!(wide.contains("a:保存"), "{wide}");
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
                    "Esc/q:停止",
                ] {
                    assert!(help.contains(key), "{key} が落ちた: {help}");
                }
                // q はアプリを終えず、Esc と同じく再生を止めて一覧へ戻る。
                assert!(!help.contains("q:終了"), "{help}");
            }
        }

        #[test]
        fn playing_help_drops_whole_hints_when_the_terminal_is_narrow() {
            let wide = help_text(
                Mode::Playing,
                DisplayMode::Embedded,
                false,
                false,
                false,
                false,
                200,
            );
            assert!(wide.contains("クリック:シーク"), "{wide}");

            let narrow = help_text(
                Mode::Playing,
                DisplayMode::Embedded,
                false,
                false,
                false,
                false,
                30,
            );
            assert_eq!(narrow, "space:一時停止 ←→:シーク");
            assert_eq!(
                help_text(
                    Mode::Playing,
                    DisplayMode::Embedded,
                    false,
                    false,
                    false,
                    false,
                    0
                ),
                ""
            );
        }

        #[test]
        fn playing_help_mentions_the_subtitle_key() {
            // 80 桁では主要キーが先で入らないので、広い端末での案内で見る。
            let wide = help_text(
                Mode::Playing,
                DisplayMode::Embedded,
                false,
                false,
                false,
                false,
                200,
            );
            assert!(wide.contains("s:字幕"), "{wide}");
            // 幅に入らないぶんは丸ごと落ちる。途中で切れた案内は出さない。
            let narrow = help_80(Mode::Playing, DisplayMode::Text);
            assert!(grid::display_width(&narrow) <= 80, "{narrow}");
            assert!(!narrow.contains("s:字"), "{narrow}");
        }

        #[test]
        fn the_comment_list_takes_over_the_video_area() {
            let mut app = commented_app(vec![comment("alice", "おもしろい")]);
            app.comments.toggle();
            let screen = rendered(&app, 80, 24);

            assert!(screen.contains("alice (+3)"), "{screen}");
            assert!(screen.contains("おもしろい"), "{screen}");
            // 映像領域を置き換えるだけで、下 3 段はそのまま。
            assert!(screen.contains("space:一時停止"), "{screen}");
        }

        #[test]
        fn a_video_with_comments_disabled_says_so_instead_of_looking_broken() {
            let mut app = commented_app(Vec::new());
            app.comments.toggle();
            assert!(
                rendered(&app, 80, 24).contains(crate::comments::NO_COMMENTS),
                "コメント無効の動画はエラーにしない"
            );
        }

        #[test]
        fn the_video_comes_back_when_the_comment_list_is_closed() {
            let mut app = commented_app(vec![comment("alice", "おもしろい")]);
            app.display = DisplayMode::Window;
            app.comments.toggle();
            assert!(rendered(&app, 80, 24).contains("alice"));

            app.comments.toggle();
            let screen = rendered(&app, 80, 24);
            assert!(!screen.contains("alice"), "{screen}");
            assert!(screen.contains("別ウィンドウで再生中"), "{screen}");
        }

        #[test]
        fn playing_help_mentions_the_comment_key() {
            // 80 桁では主要キーが先で入らないので、広い端末での案内で見る。
            let wide = help_text(
                Mode::Playing,
                DisplayMode::Embedded,
                false,
                false,
                false,
                false,
                200,
            );
            assert!(wide.contains("o:コメント"), "{wide}");
        }

        #[test]
        fn the_arrow_hint_switches_to_the_comment_list_while_it_is_open() {
            let open = help_text(
                Mode::Playing,
                DisplayMode::Embedded,
                true,
                false,
                false,
                false,
                200,
            );
            assert!(open.contains("↑↓:行送り"), "{open}");
            assert!(!open.contains("↑↓:音量"), "{open}");

            let closed = help_text(
                Mode::Playing,
                DisplayMode::Embedded,
                false,
                false,
                false,
                false,
                200,
            );
            assert!(closed.contains("↑↓:音量"), "{closed}");
        }

        #[test]
        fn the_comment_list_scrolls_to_the_entries_that_do_not_fit() {
            let mut app = many_comments(comments::COMMENT_LIMIT);
            app.comments.toggle();
            let top = rendered(&app, 80, 24);
            assert!(top.contains("author0"), "{top}");
            assert!(!top.contains("author20"), "{top}");

            let view = comments_viewport(app.screen);
            let lines = comments::display_lines(app.comments.state(), view.width as usize).len();
            app.comments.scroll_by(40, lines, view.height as usize);
            let scrolled = rendered(&app, 80, 24);
            assert!(scrolled.contains("author20"), "{scrolled}");
            assert!(!scrolled.contains("author0 "), "{scrolled}");

            // 最後の 1 件までは送れる。
            app.comments
                .scroll_by(lines as isize, lines, view.height as usize);
            let bottom = rendered(&app, 80, 24);
            assert!(
                bottom.contains(&format!("author{}", comments::COMMENT_LIMIT - 1)),
                "{bottom}"
            );
        }

        #[test]
        fn the_comment_viewport_is_the_inside_of_the_frame() {
            let screen = Rect::new(0, 0, 80, 24);
            let view = comments_viewport(screen);
            assert_eq!(view, Rect::new(1, 1, 78, 18));
        }

        #[test]
        fn playing_help_mentions_the_mouse() {
            // マウスの案内は幅が余ったときだけ出す。
            assert!(
                help_text(
                    Mode::Playing,
                    DisplayMode::Embedded,
                    false,
                    false,
                    false,
                    false,
                    200
                )
                .contains("クリック")
            );
            assert!(help_80(Mode::Playing, DisplayMode::Embedded).contains("シーク"));
        }
    }
}
