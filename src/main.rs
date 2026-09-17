mod app;
mod mpv;
mod search;
mod ui;

use anyhow::Result;
use app::{App, AppEvent, Mode, Playback};
use crossterm::event::{
    self, Event as CrosstermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
};
use mpv::MpvController;
use ratatui::DefaultTerminal;
use std::time::Duration;
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio::task::JoinHandle;
use tokio::time::timeout;

const SEARCH_TIMEOUT: Duration = Duration::from_secs(30);

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
        }
        if app.should_quit {
            break;
        }
    }
    if let Some(task) = session.search_task.take() {
        task.abort();
    }
    Ok(())
}

fn spawn_input_reader(tx: UnboundedSender<AppEvent>) {
    std::thread::spawn(move || {
        loop {
            match event::read() {
                Ok(CrosstermEvent::Key(key)) if key.kind == KeyEventKind::Press => {
                    if tx.send(AppEvent::Key(key)).is_err() {
                        break;
                    }
                }
                Ok(_) => {}
                Err(_) => break,
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
        AppEvent::MpvExited { nonce, error } => {
            if session.player.as_ref().is_some_and(|p| p.nonce == nonce) {
                session.player = None;
                app.playback = Playback::default();
                if error.is_some() {
                    app.error = error;
                }
                app.mode = if app.results.is_empty() {
                    Mode::Input
                } else {
                    Mode::Results
                };
            }
        }
    }
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
    match MpvController::launch(&result.url(), nonce, tx.clone()).await {
        Ok(controller) => {
            app.playback = Playback {
                title: result.title.clone(),
                ..Playback::default()
            };
            app.mode = Mode::Playing;
            app.error = None;
            session.player = Some(Player { controller, nonce });
        }
        Err(e) => app.error = Some(e),
    }
}

async fn stop_playback(session: &mut Session) {
    if let Some(mut p) = session.player.take() {
        let _ = p.controller.send(&mpv::quit()).await;
    }
}
