//! キー・マウス入力の振り分け。Session を触る操作は actions.rs のアクションへ渡す。

use crate::actions::{
    SEEK_STEP_SECS, Session, adjust_settings_value, change_speed, close_settings, copy_url_with,
    cycle_display_mode, move_selection, move_settings_selection, open_settings, reload_tab,
    reset_speed, save_settings, seek_absolute, seek_relative, send_to_player, start_playback,
    start_search, stop_playback, switch_tab, toggle_subtitles,
};
use crate::app::{App, AppEvent, Mode};
use crate::clipboard::{Clipboard, Pbcopy};
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
        Mode::Settings => handle_key_settings(app, key),
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
        // 入力欄では大文字の S も検索語なので、設定は Ctrl+S で開く。
        KeyCode::Char(c) if is_settings_key(c, key.modifiers) => open_settings(app, session),
        // 他の Ctrl 付きは検索語に入れない。制御文字が混ざると検索が通らない。
        KeyCode::Char(_) if key.modifiers.contains(KeyModifiers::CONTROL) => {}
        KeyCode::Char(c) => {
            app.query.push(c);
            app.set_error(None);
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
        // 結果一覧では文字を打たないので、S 単独でも開ける。
        KeyCode::Char('S') => open_settings(app, session),
        KeyCode::Char(c) if is_settings_key(c, key.modifiers) => open_settings(app, session),
        KeyCode::Char('/') | KeyCode::Esc => {
            app.mode = Mode::Input;
            app.set_error(None);
        }
        KeyCode::Char('q') => app.should_quit = true,
        _ => {}
    }
}

/// 設定画面を開くキー。Ctrl+S はどちらの検索画面でも使える。
fn is_settings_key(c: char, modifiers: KeyModifiers) -> bool {
    modifiers.contains(KeyModifiers::CONTROL) && c.eq_ignore_ascii_case(&'s')
}

/// 設定画面の操作。保存以外は app.settings をその場で書き換えるだけ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettingsAction {
    Move(i32),
    Adjust(i32),
    Save,
    Close,
}

/// 設定画面のキーと操作の対応表。
fn settings_action(code: KeyCode) -> Option<SettingsAction> {
    match code {
        KeyCode::Up => Some(SettingsAction::Move(-1)),
        KeyCode::Down => Some(SettingsAction::Move(1)),
        KeyCode::Left => Some(SettingsAction::Adjust(-1)),
        // Enter / Space は bool の切替に要る。選択肢や数値では → と同じ扱い。
        KeyCode::Right | KeyCode::Enter | KeyCode::Char(' ') => Some(SettingsAction::Adjust(1)),
        KeyCode::Char('s') => Some(SettingsAction::Save),
        KeyCode::Esc | KeyCode::Char('q') => Some(SettingsAction::Close),
        _ => None,
    }
}

fn handle_key_settings(app: &mut App, key: KeyEvent) {
    match settings_action(key.code) {
        Some(SettingsAction::Move(delta)) => move_settings_selection(app, delta),
        Some(SettingsAction::Adjust(delta)) => adjust_settings_value(app, delta),
        Some(SettingsAction::Save) => save_settings(app, std::time::Instant::now()),
        Some(SettingsAction::Close) => close_settings(app),
        None => {}
    }
}

async fn handle_key_playing(app: &mut App, key: KeyEvent, session: &mut Session) {
    handle_key_playing_with(app, key, session, Pbcopy).await;
}

/// クリップボードの書き手を差し替えられる形。テストはここに偽物を渡して pbcopy を起動させない。
async fn handle_key_playing_with<C: Clipboard>(
    app: &mut App,
    key: KeyEvent,
    session: &mut Session,
    clipboard: C,
) {
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
    if key.code == KeyCode::Char('c') {
        copy_url_with(app, clipboard, std::time::Instant::now()).await;
    }
    if key.code == KeyCode::Char('s') {
        toggle_subtitles(app, session, std::time::Instant::now()).await;
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
    use crate::category::{Category, Tabs};
    use crate::clipboard::fixtures::{CopyResult, FakeClipboard};
    use crate::display::DisplayMode;
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
        let mut session = Session::default();
        let mut app = playing_url_app();
        let clipboard = FakeClipboard::new(CopyResult::Ok);
        handle_key_playing_with(
            &mut app,
            key(KeyCode::Char('c')),
            &mut session,
            clipboard.clone(),
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
        let mut session = Session::default();
        let sent = record(&mut session);
        let mut app = playing_app();
        assert!(app.subtitles.wanted(), "[subtitles] enabled の既定は true");

        handle_key_playing(&mut app, key(KeyCode::Char('s')), &mut session).await;
        assert!(!app.subtitles.wanted());
        handle_key_playing(&mut app, key(KeyCode::Char('s')), &mut session).await;
        assert!(app.subtitles.wanted());

        assert_eq!(*sent.lock().expect("溜め込み先"), [SID_NO, SID_AUTO]);
    }

    #[tokio::test]
    async fn s_does_nothing_else_while_playing() {
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
            &mut session,
            clipboard.clone(),
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
            handle_key_playing(&mut app, key(code), &mut session).await;
            assert!(app.subtitles.wanted(), "{code:?} で字幕が動いた");
        }
    }

    #[tokio::test]
    async fn s_is_a_plain_character_outside_of_playback() {
        let (tx, _rx) = channel();
        let mut session = Session::default();

        // 検索入力中は検索語の文字として入る。
        let mut app = App::default();
        handle_key(&mut app, key(KeyCode::Char('s')), &tx, &mut session).await;
        assert_eq!(app.query, "s");
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
            handle_key_playing_with(&mut app, key(code), &mut session, clipboard.clone()).await;
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

        assert_eq!(app.query, "c");
        assert!(app.notice.is_none());
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
        assert!(app.query.is_empty(), "検索語には入れない");

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
        assert_eq!(app.query, "SEKIRO");
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
        assert!(app.query.is_empty(), "制御文字は検索語に入れない");
        assert_eq!(app.mode, Mode::Input);
    }

    #[tokio::test]
    async fn capital_s_is_ignored_while_playing() {
        let mut session = Session::default();
        let sent = record(&mut session);
        let mut app = playing_app();

        handle_key_playing(&mut app, key(KeyCode::Char('S')), &mut session).await;

        assert_eq!(app.mode, Mode::Playing);
        assert!(sent.lock().expect("溜め込み先").is_empty());
        assert!(app.subtitles.wanted(), "小文字 s の字幕とは別のキー");
    }

    #[test]
    fn the_settings_keys_map_to_their_actions() {
        assert_eq!(settings_action(KeyCode::Up), Some(SettingsAction::Move(-1)));
        assert_eq!(
            settings_action(KeyCode::Down),
            Some(SettingsAction::Move(1))
        );
        assert_eq!(
            settings_action(KeyCode::Left),
            Some(SettingsAction::Adjust(-1))
        );
        assert_eq!(
            settings_action(KeyCode::Right),
            Some(SettingsAction::Adjust(1))
        );
        assert_eq!(
            settings_action(KeyCode::Enter),
            Some(SettingsAction::Adjust(1))
        );
        assert_eq!(
            settings_action(KeyCode::Char(' ')),
            Some(SettingsAction::Adjust(1))
        );
        assert_eq!(
            settings_action(KeyCode::Char('s')),
            Some(SettingsAction::Save)
        );
        assert_eq!(settings_action(KeyCode::Esc), Some(SettingsAction::Close));
        assert_eq!(
            settings_action(KeyCode::Char('q')),
            Some(SettingsAction::Close)
        );
        // 開くのに使う S では保存しない。
        assert_eq!(settings_action(KeyCode::Char('S')), None);
        assert_eq!(settings_action(KeyCode::Tab), None);
        assert_eq!(settings_action(KeyCode::Char('x')), None);
    }

    #[tokio::test]
    async fn the_settings_keys_move_the_selection_and_change_the_value() {
        let (tx, _rx) = channel();
        let mut session = Session::default();
        let mut app = App::default();
        handle_key(&mut app, ctrl(KeyCode::Char('s')), &tx, &mut session).await;

        handle_key(&mut app, key(KeyCode::Down), &tx, &mut session).await;
        assert_eq!(app.settings_selected, 1);
        handle_key(&mut app, key(KeyCode::Right), &tx, &mut session).await;
        assert_eq!(
            app.settings.display.quality,
            crate::display::Quality::default().next()
        );
        // ← は前の値へ戻す。
        handle_key(&mut app, key(KeyCode::Left), &tx, &mut session).await;
        assert_eq!(
            app.settings.display.quality,
            crate::display::Quality::default()
        );

        handle_key(&mut app, key(KeyCode::Up), &tx, &mut session).await;
        handle_key(&mut app, key(KeyCode::Enter), &tx, &mut session).await;
        assert_eq!(app.settings.display.mode, DisplayMode::default().next());

        // 設定画面では文字は検索語にならない。
        handle_key(&mut app, key(KeyCode::Char('x')), &tx, &mut session).await;
        assert!(app.query.is_empty());

        handle_key(&mut app, key(KeyCode::Esc), &tx, &mut session).await;
        assert_eq!(app.mode, Mode::Input, "保存せず閉じる");
        assert_eq!(
            app.settings,
            crate::settings::Settings::default(),
            "閉じたら編集前へ戻す"
        );
    }

    #[tokio::test]
    async fn q_closes_the_settings_instead_of_quitting_the_app() {
        let (tx, _rx) = channel();
        let mut session = Session::default();
        let mut app = App {
            mode: Mode::Results,
            results: vec![result("a")],
            ..App::default()
        };
        handle_key(&mut app, key(KeyCode::Char('S')), &tx, &mut session).await;
        handle_key(&mut app, key(KeyCode::Char('q')), &tx, &mut session).await;

        assert!(!app.should_quit);
        assert_eq!(app.mode, Mode::Results);
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
        let mut session = Session::default();
        let mut app = App {
            mode: Mode::Settings,
            ..playing_app()
        };
        let down = mouse(MouseEventKind::Down(MouseButton::Left), 0, 21);
        handle_mouse(&mut app, down, &mut session).await;

        assert_eq!(app.seek_bar, SeekBarState::default());
        assert_eq!(app.settings, crate::settings::Settings::default());
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
