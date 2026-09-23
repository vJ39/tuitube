//! キー・マウス入力の振り分け。Ctrl+C と終了確認だけをここで受け、残りは各画面へ渡す。

use crate::actions::{Session, remember_playback_position, stop_playback};
use crate::app::{App, AppEvent, Mode};
use crate::screen::browse as browse_screen;
use crate::screen::download as download_screen;
use crate::screen::playing as playing_screen;
use crate::screen::playlists as playlists_screen;
use crate::screen::settings as settings_screen;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent};
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
        Mode::Input => browse_screen::handle_key_input(app, key, tx, session).await,
        Mode::Results => browse_screen::handle_key_results(app, key, tx, session).await,
        Mode::Channel => browse_screen::handle_key_channel(app, key, tx, session).await,
        Mode::Playing => playing_screen::handle_key_playing(app, key, tx, session).await,
        Mode::Settings => settings_screen::handle_key_settings(app, key),
        Mode::Download => download_screen::handle_key_download(app, key, tx, session).await,
        Mode::Playlists => playlists_screen::handle_key_playlists(app, key, tx, session).await,
        Mode::Playlist => browse_screen::handle_key_playlist(app, key, tx, session).await,
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
        Mode::Playing => playing_screen::handle_mouse_playing(app, mouse, tx, session).await,
        Mode::Results => browse_screen::handle_mouse_results(app, mouse, tx, session).await,
        Mode::Channel => browse_screen::handle_mouse_channel(app, mouse, tx, session).await,
        Mode::Input => browse_screen::handle_mouse_input(app, mouse, tx, session),
        Mode::Settings | Mode::Download | Mode::Playlists | Mode::Playlist => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Playback;
    use crate::mpv::{self, MpvCommand};
    use crate::search::SearchResult;
    use crate::seekbar::SeekBarState;
    use crossterm::event::{MouseButton, MouseEventKind};
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

    /// タブ行 (80x24 の端末では y=3) の押し込み。
    fn tab_click(column: u16) -> MouseEvent {
        mouse(MouseEventKind::Down(MouseButton::Left), column, 3)
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

    /// Ctrl を押しながらのキー。
    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
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

    // ---- ダウンロード画面 ----
}
