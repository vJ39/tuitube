mod app;
mod kitty;
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
use std::io::Write;
use std::time::Duration;
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio::task::JoinHandle;
use tokio::time::{Instant, timeout};
use video::{Geometry, VideoSink};

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
    /// 再生が終わった後など、sink 越しに出せない画像削除の持ち越し。
    owe_clear: bool,
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
        // draw は必ず MoveTo で始まり SGR を閉じて flush するので、その直後なら割り込まずに書ける。
        let area = ui::video_area(terminal.draw(|frame| ui::draw(frame, &app))?.area);
        present_video(&mut session, &app, area, terminal.backend_mut())?;
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
    // alt screen を抜ければ仕様上は消えるが、端末差を当てにしない。
    let mut out = Vec::new();
    video::encode_clear(&mut out);
    let backend = terminal.backend_mut();
    let _ = backend.write_all(&out);
    let _ = backend.flush();
    Ok(())
}

/// draw の直後に、保留中の画像削除と最新フレームを実端末へ書く。書き手はここだけ。
fn present_video(
    session: &mut Session,
    app: &App,
    area: Rect,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    let mut pending = app
        .video
        .as_ref()
        .and_then(VideoSink::take)
        .unwrap_or_default();
    if std::mem::take(&mut session.owe_clear) {
        pending.clear = true;
    }
    if !pending.clear && pending.frame.is_none() {
        return Ok(());
    }
    let cell = app
        .video
        .as_ref()
        .map(|sink| sink.geometry().cell)
        .unwrap_or(video::FALLBACK_CELL);
    let mut bytes = Vec::new();
    video::encode(&pending, area, cell, &mut bytes);
    out.write_all(&bytes)?;
    out.flush()
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
    // sink を手放した後も残骸は消す。SIGKILL 経路では mpv 自身が消せない。
    session.owe_clear = true;
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
    let video = VideoSink::new(video_geometry());
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

fn video_geometry() -> Geometry {
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    geometry_for(cols, rows)
}

/// mpv に渡す寸法は ratatui の映像領域と一致していなければならない。
fn geometry_for(cols: u16, rows: u16) -> Geometry {
    // ピクセルを報告しない端末では既定のセル寸法で進める (画像が小さめに出るだけ)。
    let cell = crossterm::terminal::window_size()
        .ok()
        .and_then(|size| video::cell_size(size.columns, size.rows, size.width, size.height))
        .unwrap_or(video::FALLBACK_CELL);
    Geometry::new(
        ui::video_area(Rect::new(0, 0, cols, rows)),
        cell,
        video::MAX_FRAME_PIXELS,
    )
}

/// 端末サイズが変わったら、映像の寸法を mpv ごと作り直して食い違いを解消する。
async fn apply_resize(app: &mut App, session: &mut Session) {
    let Some((cols, rows)) = session.pending_resize.take() else {
        return;
    };
    let Some(video) = app.video.clone() else {
        return;
    };
    let geometry = geometry_for(cols, rows);
    if video.geometry() == geometry {
        return;
    }
    video.resize(geometry);
    if let Some(p) = session.player.as_mut()
        && let Err(e) = p.controller.resize_video(geometry).await
    {
        app.error = Some(e);
    }
}

async fn stop_playback(session: &mut Session) {
    if let Some(p) = session.player.take() {
        p.controller.shutdown().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kitty::fixtures::{KITTY_RECONFIG, frame};
    use crate::search::SearchResult;
    use crate::video::{CellSize, MAX_FRAME_PIXELS};

    const CELL: CellSize = CellSize {
        width_px: 8,
        height_px: 16,
    };

    fn area() -> Rect {
        Rect::new(0, 0, 80, 22)
    }

    fn sink() -> VideoSink {
        VideoSink::new(Geometry::new(area(), CELL, MAX_FRAME_PIXELS))
    }

    fn clear_bytes() -> Vec<u8> {
        let mut out = Vec::new();
        video::encode_clear(&mut out);
        out
    }

    fn present(session: &mut Session, app: &App) -> Vec<u8> {
        let mut out = Vec::new();
        present_video(session, app, area(), &mut out).expect("Vec への書き込みは失敗しない");
        out
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
    fn present_writes_nothing_when_there_is_nothing_to_show() {
        let mut session = Session::default();
        assert!(present(&mut session, &App::default()).is_empty());
        // 再生中でも保留が無ければ何も書かない。
        let app = App {
            video: Some(sink()),
            ..App::default()
        };
        assert!(present(&mut session, &app).is_empty());
    }

    #[test]
    fn present_writes_clear_then_cup_then_frame() {
        let video = sink();
        assert!(video.feed(KITTY_RECONFIG));
        assert!(video.feed(&frame(320, 176, b"DATA")));
        let app = App {
            video: Some(video),
            ..App::default()
        };
        let mut session = Session::default();

        let out = present(&mut session, &app);
        // 40x11 セルの画像を 80x22 の領域に中央寄せした CUP が、削除列の直後に来る。
        let mut expected = clear_bytes();
        expected.extend_from_slice(b"\x1b[6;21H");
        assert!(
            out.starts_with(&expected),
            "clear → CUP の順になっていない: {:?}",
            String::from_utf8_lossy(&out)
        );
        assert!(out.ends_with(b"\x1b\\"), "APC が最後まで書かれていない");
        // 取り出し済みなので次の周では何も書かない。
        assert!(present(&mut session, &app).is_empty());
    }

    #[test]
    fn present_consumes_the_owed_clear_without_a_sink() {
        // sink を手放した後の持ち越し。セル寸法は FALLBACK_CELL に退避する。
        let app = App::default();
        assert!(app.video.is_none());
        let mut session = Session {
            owe_clear: true,
            ..Session::default()
        };
        assert_eq!(present(&mut session, &app), clear_bytes());
        assert!(!session.owe_clear);
        assert!(present(&mut session, &app).is_empty());
    }

    #[test]
    fn present_merges_the_owed_clear_with_a_pending_frame() {
        let video = sink();
        assert!(video.feed(&frame(320, 176, b"DATA")));
        let app = App {
            video: Some(video),
            ..App::default()
        };
        let mut session = Session {
            owe_clear: true,
            ..Session::default()
        };
        let out = present(&mut session, &app);
        assert!(out.starts_with(&clear_bytes()));
        assert!(out.ends_with(b"\x1b\\"));
        assert!(!session.owe_clear);
    }

    #[tokio::test]
    async fn end_playback_owes_a_clear_that_the_next_present_writes() {
        let mut app = App {
            mode: Mode::Playing,
            results: vec![result("a")],
            video: Some(sink()),
            playback: Playback {
                title: "song".to_string(),
                ..Playback::default()
            },
            ..App::default()
        };
        let mut session = Session::default();
        end_playback(&mut app, &mut session, Some("boom".to_string())).await;

        assert!(session.owe_clear);
        assert!(app.video.is_none());
        assert_eq!(app.mode, Mode::Results);
        assert_eq!(app.error.as_deref(), Some("boom"));
        assert!(app.playback.title.is_empty());
        assert_eq!(present(&mut session, &app), clear_bytes());
    }

    #[tokio::test]
    async fn end_playback_without_results_returns_to_input() {
        let mut app = App {
            mode: Mode::Playing,
            video: Some(sink()),
            ..App::default()
        };
        let mut session = Session::default();
        end_playback(&mut app, &mut session, None).await;

        assert_eq!(app.mode, Mode::Input);
        assert!(app.error.is_none());
        assert!(session.owe_clear);
    }

    #[test]
    fn geometry_matches_the_video_area_of_the_same_terminal_size() {
        let geometry = geometry_for(80, 24);
        assert_eq!(geometry.area, ui::video_area(Rect::new(0, 0, 80, 24)));
        let pixels = u64::from(geometry.frame_px.0) * u64::from(geometry.frame_px.1);
        assert!(pixels <= u64::from(video::MAX_FRAME_PIXELS), "{pixels} px");
    }
}
