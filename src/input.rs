//! キー・マウス入力の振り分け。Session を触る操作は actions.rs のアクションへ渡す。

use crate::actions::{
    SEEK_STEP_SECS, Session, change_speed, cycle_display_mode, move_selection, reload_tab,
    reset_speed, seek_absolute, seek_relative, send_to_player, start_playback, start_search,
    stop_playback, switch_tab,
};
use crate::app::{App, AppEvent, Mode};
use crate::grid::Dir;
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
        KeyCode::Tab => switch_tab(app, tx, session, true),
        KeyCode::BackTab => switch_tab(app, tx, session, false),
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
        KeyCode::Down => move_selection(app, Dir::Down),
        KeyCode::Up => move_selection(app, Dir::Up),
        KeyCode::Right => move_selection(app, Dir::Right),
        KeyCode::Left => move_selection(app, Dir::Left),
        KeyCode::Tab => switch_tab(app, tx, session, true),
        KeyCode::BackTab => switch_tab(app, tx, session, false),
        KeyCode::Char('r') => reload_tab(app, tx, session),
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
        cycle_display_mode(app, session).await;
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

    /// 80x24 の検索画面。格子は 4 列 2 行になる。
    fn grid_app(count: usize) -> App {
        let mut app = App {
            mode: Mode::Results,
            screen: Rect::new(0, 0, 80, 24),
            ..App::default()
        };
        let results: Vec<SearchResult> = (0..count).map(|i| result(&format!("id{i}"))).collect();
        app.set_results(results, &crate::cookies::Target::Search("q".to_string()));
        app
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
        assert_eq!(app.query, "]");
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
        let mut session = Session::default();
        let sent = record(&mut session);
        let mut app = playing_app();

        handle_key_playing(&mut app, key(KeyCode::Char(']')), &mut session).await;
        assert_eq!(
            app.speed,
            crate::speed::Speed::from_tenths(11).expect("1.1x")
        );
        handle_key_playing(&mut app, key(KeyCode::Char('[')), &mut session).await;
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
        let mut session = Session::default();
        let sent = record(&mut session);
        let mut app = App {
            speed: crate::speed::Speed::from_tenths(15).expect("1.5x"),
            ..playing_app()
        };
        handle_key_playing(&mut app, key(KeyCode::Backspace), &mut session).await;

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
        assert_eq!(app.query, "w", "入力モードでは文字として入る");
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

    #[tokio::test]
    async fn tab_switches_the_category_in_the_input_mode() {
        let (tx, _rx) = channel();
        let mut session = Session::default();
        let mut app = App::default();

        handle_key_input(&mut app, key(KeyCode::Tab), &tx, &mut session);
        assert_eq!(app.tabs.selected(), 1);
        take_search(&mut session);

        handle_key_input(&mut app, key(KeyCode::BackTab), &tx, &mut session);
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
            query: "ラーメン".to_string(),
            ..App::default()
        };
        handle_key_input(&mut app, key(KeyCode::Tab), &tx, &mut session);
        take_search(&mut session);
        assert!(!app.tabs.is_all());

        handle_key_input(&mut app, key(KeyCode::Enter), &tx, &mut session);
        assert!(app.tabs.is_all());
        assert!(take_search(&mut session));
    }

    #[tokio::test]
    async fn typing_still_appends_to_the_query_while_a_category_tab_is_selected() {
        let (tx, _rx) = channel();
        let mut session = Session::default();
        let mut app = App::default();
        handle_key_input(&mut app, key(KeyCode::Tab), &tx, &mut session);
        take_search(&mut session);

        handle_key_input(&mut app, key(KeyCode::Char('ラ')), &tx, &mut session);
        handle_key_input(&mut app, key(KeyCode::Char('ー')), &tx, &mut session);
        assert_eq!(app.query, "ラー", "戻れば元の入力が残っている");
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
        app.query = "ラーメン".to_string();
        assert!(app.tabs.state().loaded);

        handle_key_results(&mut app, key(KeyCode::Char('r')), &tx, &mut session).await;
        assert!(!app.tabs.state().loaded);
        assert!(take_search(&mut session));
    }

    #[tokio::test]
    async fn tab_is_ignored_while_playing() {
        let mut session = Session::default();
        let mut app = App {
            mode: Mode::Playing,
            ..playing_app()
        };
        for code in [KeyCode::Tab, KeyCode::BackTab, KeyCode::Char('r')] {
            handle_key_playing(&mut app, key(code), &mut session).await;
        }
        assert_eq!(app.tabs.selected(), 0);
        assert!(session.search_task.is_none());
        assert!(!app.should_quit);
    }
}
