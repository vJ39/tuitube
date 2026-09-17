//! キー・マウス入力の振り分け。Session を触る操作は actions.rs のアクションへ渡す。

use crate::actions::{
    SEEK_STEP_SECS, Session, seek_absolute, seek_relative, send_to_player, start_playback,
    start_search, stop_playback,
};
use crate::app::{App, AppEvent, Mode};
use crate::mpv::{self, MpvCommand};
use crate::seekbar::{MouseAction, MouseInput};
use crate::ui;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use tokio::sync::mpsc::UnboundedSender;

pub async fn handle_key(
    app: &mut App,
    key: KeyEvent,
    tx: &UnboundedSender<AppEvent>,
    session: &mut Session,
) {
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        stop_playback(session).await;
        app.should_quit = true;
        return;
    }

    match app.mode {
        Mode::Input => handle_key_input(app, key, tx, session),
        Mode::Results => handle_key_results(app, key, tx, session).await,
        Mode::Playing => handle_key_playing(app, key, session).await,
    }
}

fn handle_key_input(
    app: &mut App,
    key: KeyEvent,
    tx: &UnboundedSender<AppEvent>,
    session: &mut Session,
) {
    match key.code {
        KeyCode::Enter => start_search(app, tx, session),
        KeyCode::Backspace => {
            app.query.pop();
        }
        KeyCode::Char(c) => {
            app.query.push(c);
            app.error = None;
        }
        KeyCode::Esc => {
            if app.results.is_empty() {
                app.should_quit = true;
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
    match key.code {
        KeyCode::Down => app.select_next(),
        KeyCode::Up => app.select_prev(),
        KeyCode::Enter => start_playback(app, tx, session).await,
        KeyCode::Char('/') | KeyCode::Esc => {
            app.mode = Mode::Input;
            app.error = None;
        }
        KeyCode::Char('q') => app.should_quit = true,
        _ => {}
    }
}

async fn handle_key_playing(app: &mut App, key: KeyEvent, session: &mut Session) {
    if let Some(delta) = seek_step(key.code) {
        seek_relative(app, session, delta, std::time::Instant::now()).await;
    }
    if let Some(command) = playing_command(key.code) {
        send_to_player(app, session, &command).await;
    }
    if key.code == KeyCode::Char('q') {
        app.should_quit = true;
    }
}

/// 再生中のマウス。再生中以外は受け取るだけで捨てる。
pub async fn handle_mouse(app: &mut App, mouse: MouseEvent, session: &mut Session) {
    if app.mode != Mode::Playing {
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
    use crate::app::Playback;
    use crate::search::SearchResult;
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

    /// 80x24 の端末で再生中。バー行は y=21、トラック 65 セルで 1 セル 10 秒。
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

    fn result(id: &str) -> SearchResult {
        SearchResult {
            id: id.to_string(),
            title: format!("title {id}"),
            duration: None,
            uploader: None,
        }
    }

    #[test]
    fn typing_appends_to_the_query_and_clears_the_error() {
        let (tx, _rx) = channel();
        let mut session = Session::default();
        let mut app = App {
            error: Some("boom".to_string()),
            ..App::default()
        };
        handle_key_input(&mut app, key(KeyCode::Char('ラ')), &tx, &mut session);
        handle_key_input(&mut app, key(KeyCode::Char('ー')), &tx, &mut session);

        assert_eq!(app.query, "ラー");
        assert!(app.error.is_none());
    }

    #[test]
    fn backspace_removes_one_character_not_one_byte() {
        let (tx, _rx) = channel();
        let mut session = Session::default();
        let mut app = App {
            query: "ラー".to_string(),
            ..App::default()
        };
        handle_key_input(&mut app, key(KeyCode::Backspace), &tx, &mut session);
        assert_eq!(app.query, "ラ");
        handle_key_input(&mut app, key(KeyCode::Backspace), &tx, &mut session);
        handle_key_input(&mut app, key(KeyCode::Backspace), &tx, &mut session);
        assert!(app.query.is_empty());
    }

    #[test]
    fn esc_quits_only_when_there_is_no_result_list_to_go_back_to() {
        let (tx, _rx) = channel();
        let mut session = Session::default();

        let mut app = App::default();
        handle_key_input(&mut app, key(KeyCode::Esc), &tx, &mut session);
        assert!(app.should_quit);
        assert_eq!(app.mode, Mode::Input);

        let mut app = App {
            results: vec![result("a")],
            ..App::default()
        };
        handle_key_input(&mut app, key(KeyCode::Esc), &tx, &mut session);
        assert!(!app.should_quit);
        assert_eq!(app.mode, Mode::Results);
    }

    #[test]
    fn enter_with_a_blank_query_does_not_start_a_search() {
        let (tx, _rx) = channel();
        let mut session = Session::default();
        let mut app = App {
            query: "   ".to_string(),
            ..App::default()
        };
        handle_key_input(&mut app, key(KeyCode::Enter), &tx, &mut session);

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
    async fn q_quits_from_the_result_list() {
        let (tx, _rx) = channel();
        let mut session = Session::default();
        let mut app = App {
            mode: Mode::Results,
            results: vec![result("a")],
            ..App::default()
        };
        handle_key_results(&mut app, key(KeyCode::Char('q')), &tx, &mut session).await;
        assert!(app.should_quit);
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

    #[tokio::test]
    async fn only_q_quits_the_app_while_playing() {
        let mut session = Session::default();
        let mut app = App {
            mode: Mode::Playing,
            ..App::default()
        };

        // player が無い間もキー処理は進み、送信だけが飛ばされる。
        handle_key_playing(&mut app, key(KeyCode::Esc), &mut session).await;
        assert!(!app.should_quit);
        assert!(app.error.is_none());

        handle_key_playing(&mut app, key(KeyCode::Char('q')), &mut session).await;
        assert!(app.should_quit);
    }

    #[tokio::test]
    async fn mouse_release_on_the_bar_records_an_optimistic_seek() {
        let mut session = Session::default();
        let mut app = playing_app();

        let down = mouse(MouseEventKind::Down(MouseButton::Left), 0, 21);
        handle_mouse(&mut app, down, &mut session).await;
        let up = mouse(MouseEventKind::Up(MouseButton::Left), 13, 21);
        handle_mouse(&mut app, up, &mut session).await;

        assert_eq!(app.playback.time_pos, Some(130.0));
        assert!(app.playback.pending_seek.is_some());
        // player が無い間は送信だけが飛ばされる。
        assert!(app.error.is_none());
        assert_eq!(app.seek_bar.drag, None);
    }

    #[tokio::test]
    async fn a_release_from_another_button_does_not_end_the_drag() {
        let mut session = Session::default();
        let mut app = playing_app();

        let down = mouse(MouseEventKind::Down(MouseButton::Left), 0, 21);
        handle_mouse(&mut app, down, &mut session).await;
        // ドラッグ中の右クリックでシークが飛ばない。左ドラッグはそのまま続く。
        let other = mouse(MouseEventKind::Up(MouseButton::Right), 40, 21);
        handle_mouse(&mut app, other, &mut session).await;
        assert_eq!(app.playback.time_pos, Some(0.0));
        assert_eq!(app.seek_bar.drag, Some(0));

        let up = mouse(MouseEventKind::Up(MouseButton::Left), 13, 21);
        handle_mouse(&mut app, up, &mut session).await;
        assert_eq!(app.playback.time_pos, Some(130.0));
        assert_eq!(app.seek_bar.drag, None);
    }

    #[tokio::test]
    async fn mouse_is_ignored_outside_playing_mode() {
        let mut session = Session::default();
        let mut app = App {
            mode: Mode::Results,
            ..playing_app()
        };

        let down = mouse(MouseEventKind::Down(MouseButton::Left), 0, 21);
        handle_mouse(&mut app, down, &mut session).await;
        let up = mouse(MouseEventKind::Up(MouseButton::Left), 13, 21);
        handle_mouse(&mut app, up, &mut session).await;

        assert_eq!(app.seek_bar, SeekBarState::default());
        assert_eq!(app.playback.time_pos, Some(0.0));
    }

    #[tokio::test]
    async fn arrow_keys_seek_relative_to_the_pending_target_and_clear_hover() {
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

        handle_key_playing(&mut app, key(KeyCode::Right), &mut session).await;
        assert_eq!(app.playback.time_pos, Some(15.0));
        handle_key_playing(&mut app, key(KeyCode::Right), &mut session).await;
        assert_eq!(app.playback.time_pos, Some(20.0));
        handle_key_playing(&mut app, key(KeyCode::Left), &mut session).await;
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
}
