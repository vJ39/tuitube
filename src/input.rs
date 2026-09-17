//! キー入力の振り分け。Session を触る操作は actions.rs のアクションへ渡す。

use crate::actions::{Session, send_to_player, start_playback, start_search, stop_playback};
use crate::app::{App, AppEvent, Mode};
use crate::mpv::{self, MpvCommand};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
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
    if let Some(command) = playing_command(key.code) {
        send_to_player(app, session, &command).await;
    }
    if key.code == KeyCode::Char('q') {
        app.should_quit = true;
    }
}

/// 再生中のキーと mpv コマンドの対応表。
fn playing_command(code: KeyCode) -> Option<MpvCommand> {
    match code {
        KeyCode::Char(' ') => Some(mpv::cycle_pause()),
        KeyCode::Left => Some(mpv::seek(-5)),
        KeyCode::Right => Some(mpv::seek(5)),
        KeyCode::Up => Some(mpv::add_volume(5)),
        KeyCode::Down => Some(mpv::add_volume(-5)),
        KeyCode::Char('q') | KeyCode::Esc => Some(mpv::quit()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::SearchResult;
    use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
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
        assert_eq!(playing_command(KeyCode::Left), Some(mpv::seek(-5)));
        assert_eq!(playing_command(KeyCode::Right), Some(mpv::seek(5)));
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
