mod app;
mod mpv;
mod search;
mod ui;
mod video;

use anyhow::Result;
use app::{App, AppEvent, Mode, Playback};
use crossterm::event::{
    self, Event as CrosstermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
};
use mpv::MpvController;
use ratatui::DefaultTerminal;
use ratatui::layout::Rect;
use std::time::Duration;
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio::task::JoinHandle;
use tokio::time::{Instant, timeout};
use video::VideoScreen;

const SEARCH_TIMEOUT: Duration = Duration::from_secs(30);
/// ドラッグ中は Resize が連続して届くので、落ち着くまで mpv の作り直しを待つ。
const RESIZE_DEBOUNCE: Duration = Duration::from_millis(200);

struct Player {
    controller: MpvController,
    nonce: u64,
}

#[derive(Default)]
struct Session {
    player: Option<Player>,
    player_nonce: u64,
    search_nonce: u64,
    search_task: Option<JoinHandle<()>>,
    /// 直近の端末サイズと、それを映像へ反映する時刻。
    pending_resize: Option<(u16, u16)>,
    resize_at: Option<Instant>,
}

#[tokio::main]
async fn main() -> Result<()> {
    mpv::sweep_stale_sockets();
    let mut terminal = ratatui::init();
    let result = run(&mut terminal).await;
    ratatui::restore();
    result
}

async fn run(terminal: &mut DefaultTerminal) -> Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    spawn_input_reader(tx.clone());

    let mut app = App::default();
    let mut session = Session::default();
    let mut ticker = tokio::time::interval(Duration::from_secs(1));

    loop {
        terminal.draw(|frame| ui::draw(frame, &app))?;
        tokio::select! {
            event = rx.recv() => {
                let Some(event) = event else { break };
                handle_event(&mut app, event, &tx, &mut session).await;
            }
            _ = ticker.tick() => {
                if let Some(p) = session.player.as_mut() {
                    match p.controller.poll_properties().await {
                        // 一過性の失敗で再生状況が隠れ続けないよう、成功したらエラーを消す。
                        Ok(()) => app.error = None,
                        Err(e) => app.error = Some(e),
                    }
                }
            }
            _ = wait_until(session.resize_at) => {
                session.resize_at = None;
                apply_resize(&mut app, &mut session).await;
            }
        }
        if app.should_quit {
            break;
        }
    }
    if let Some(task) = session.search_task.take() {
        task.abort();
    }
    stop_playback(&mut session).await;
    Ok(())
}

/// 予定が無いときは永久に待つ (select! の他の枝だけを動かす)。
async fn wait_until(deadline: Option<Instant>) {
    match deadline {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

fn spawn_input_reader(tx: UnboundedSender<AppEvent>) {
    std::thread::spawn(move || {
        loop {
            let sent = match event::read() {
                Ok(CrosstermEvent::Key(key)) if key.kind == KeyEventKind::Press => {
                    tx.send(AppEvent::Key(key))
                }
                Ok(CrosstermEvent::Resize(width, height)) => {
                    tx.send(AppEvent::Resize { width, height })
                }
                Ok(_) => Ok(()),
                Err(_) => break,
            };
            if sent.is_err() {
                break;
            }
        }
    });
}

async fn handle_event(
    app: &mut App,
    event: AppEvent,
    tx: &UnboundedSender<AppEvent>,
    session: &mut Session,
) {
    match event {
        AppEvent::Key(key) => handle_key(app, key, tx, session).await,
        AppEvent::Resize { width, height } => {
            // 連続して届くので、最後の1つだけを少し置いてから反映する。
            session.pending_resize = Some((width, height));
            session.resize_at = Some(Instant::now() + RESIZE_DEBOUNCE);
        }
        AppEvent::SearchDone { nonce, result } => {
            if nonce != session.search_nonce {
                return;
            }
            session.search_task = None;
            app.searching = false;
            match result {
                Ok(results) => app.set_results(results),
                Err(e) => {
                    app.error = Some(e);
                    app.mode = Mode::Input;
                }
            }
        }
        AppEvent::MpvProperty { nonce, id, data } => {
            if session.player.as_ref().is_some_and(|p| p.nonce == nonce) {
                app.apply_property(id, data);
            }
        }
        AppEvent::VideoFrame { nonce } => {
            // このループは1周ごとに描き直すので、要求を受け取るだけで次の描画に乗る。
            if session.player.as_ref().is_some_and(|p| p.nonce == nonce)
                && let Some(video) = &app.video
            {
                video.clear_redraw();
            }
        }
        AppEvent::VideoError { nonce, error } => {
            if session.player.as_ref().is_some_and(|p| p.nonce == nonce) {
                end_playback(app, session, Some(error)).await;
            }
        }
        AppEvent::MpvExited { nonce, error } => {
            if session.player.as_ref().is_some_and(|p| p.nonce == nonce) {
                end_playback(app, session, error).await;
            }
        }
    }
}

async fn end_playback(app: &mut App, session: &mut Session, error: Option<String>) {
    stop_playback(session).await;
    app.playback = Playback::default();
    app.video = None;
    if error.is_some() {
        app.error = error;
    }
    app.mode = if app.results.is_empty() {
        Mode::Input
    } else {
        Mode::Results
    };
}

async fn handle_key(
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
        Mode::Input => match key.code {
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
        },
        Mode::Results => match key.code {
            KeyCode::Down => app.select_next(),
            KeyCode::Up => app.select_prev(),
            KeyCode::Enter => start_playback(app, tx, session).await,
            KeyCode::Char('/') | KeyCode::Esc => {
                app.mode = Mode::Input;
                app.error = None;
            }
            KeyCode::Char('q') => app.should_quit = true,
            _ => {}
        },
        Mode::Playing => {
            let command = match key.code {
                KeyCode::Char(' ') => Some(mpv::cycle_pause()),
                KeyCode::Left => Some(mpv::seek(-5)),
                KeyCode::Right => Some(mpv::seek(5)),
                KeyCode::Up => Some(mpv::add_volume(5)),
                KeyCode::Down => Some(mpv::add_volume(-5)),
                KeyCode::Char('q') | KeyCode::Esc => Some(mpv::quit()),
                _ => None,
            };
            if let (Some(command), Some(p)) = (command, session.player.as_mut())
                && let Err(e) = p.controller.send(&command).await
            {
                app.error = Some(e);
            }
            if key.code == KeyCode::Char('q') {
                app.should_quit = true;
            }
        }
    }
}

fn start_search(app: &mut App, tx: &UnboundedSender<AppEvent>, session: &mut Session) {
    let query = app.query.trim().to_string();
    if query.is_empty() {
        return;
    }
    // 連打しても yt-dlp が並走しないよう、先行の検索は打ち切る (kill_on_drop で子プロセスも落ちる)。
    if let Some(task) = session.search_task.take() {
        task.abort();
    }
    session.search_nonce += 1;
    let nonce = session.search_nonce;
    app.searching = true;
    app.error = None;
    let tx = tx.clone();
    session.search_task = Some(tokio::spawn(async move {
        let result = match timeout(SEARCH_TIMEOUT, search::search(&query)).await {
            Ok(result) => result,
            Err(_) => Err(format!(
                "検索がタイムアウトしました ({} 秒)",
                SEARCH_TIMEOUT.as_secs()
            )),
        };
        let _ = tx.send(AppEvent::SearchDone { nonce, result });
    }));
}

async fn start_playback(app: &mut App, tx: &UnboundedSender<AppEvent>, session: &mut Session) {
    let Some(result) = app.selected_result().cloned() else {
        return;
    };
    stop_playback(session).await;
    session.player_nonce += 1;
    let nonce = session.player_nonce;
    let video = video_screen();
    match MpvController::launch(&result.url(), nonce, tx.clone(), video.clone()).await {
        Ok(controller) => {
            app.playback = Playback {
                title: result.title.clone(),
                ..Playback::default()
            };
            app.video = Some(video);
            app.mode = Mode::Playing;
            app.error = None;
            session.player = Some(Player { controller, nonce });
        }
        Err(e) => {
            app.video = None;
            app.error = Some(e);
        }
    }
}

fn video_screen() -> VideoScreen {
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let (width, height) = video_size(cols, rows);
    VideoScreen::new(width, height)
}

/// mpv に渡す寸法は ratatui の映像領域と一致していなければならない。
fn video_size(cols: u16, rows: u16) -> (u16, u16) {
    let area = ui::video_area(Rect::new(0, 0, cols, rows));
    (area.width.max(1), area.height.max(1))
}

/// 端末サイズが変わったら、仮想画面と mpv の出力寸法を作り直して食い違いを解消する。
async fn apply_resize(app: &mut App, session: &mut Session) {
    let Some((cols, rows)) = session.pending_resize.take() else {
        return;
    };
    let Some(video) = app.video.clone() else {
        return;
    };
    let (width, height) = video_size(cols, rows);
    if video.size() == (width, height) {
        return;
    }
    video.resize(width, height);
    if let Some(p) = session.player.as_mut()
        && let Err(e) = p.controller.resize_video(width, height).await
    {
        app.error = Some(e);
    }
}

async fn stop_playback(session: &mut Session) {
    if let Some(p) = session.player.take() {
        p.controller.shutdown().await;
    }
}
