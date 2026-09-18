mod actions;
mod app;
mod geometry;
mod input;
mod kitty;
mod mpv;
mod search;
mod seekbar;
mod ui;
mod video;

use actions::{
    Session, apply_fps_limit, apply_resize, end_playback, schedule_resize, stop_playback,
};
use anyhow::Result;
use app::{App, AppEvent, Mode};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event as CrosstermEvent, KeyEventKind,
};
use crossterm::execute;
use input::{handle_key, handle_mouse};
use ratatui::DefaultTerminal;
use ratatui::layout::Rect;
use std::io::Write;
use std::time::Duration;
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio::time::Instant;
use video::VideoSink;

/// ホバーの Moved は1セルごとに届く。溜まった分をまとめて捌いてから描き直す。
const EVENT_DRAIN_LIMIT: usize = 64;

#[tokio::main]
async fn main() -> Result<()> {
    mpv::sweep_stale_sockets();
    // マウス追跡は alt screen に入った後で有効化し、抜ける前に解除する。
    let mut terminal = ratatui::init();
    let result = match enable_mouse_capture() {
        Ok(()) => {
            install_mouse_panic_hook();
            run(&mut terminal).await
        }
        Err(e) => Err(e),
    };
    disable_mouse_capture();
    ratatui::restore();
    result
}

fn enable_mouse_capture() -> Result<()> {
    execute!(std::io::stdout(), EnableMouseCapture)?;
    Ok(())
}

fn disable_mouse_capture() {
    let _ = execute!(std::io::stdout(), DisableMouseCapture);
}

/// ratatui の hook (restore) より先に追跡を止める。残すとシェルに戻った後もゴミが出る。
fn install_mouse_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        disable_mouse_capture();
        previous(info);
    }));
}

async fn run(terminal: &mut DefaultTerminal) -> Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    spawn_input_reader(tx.clone());

    // 設定は起動時に一度だけ読む。読み替えたときは notice がステータス行に出る。
    let fps = mpv::FpsLimit::from_env();
    let mut app = App {
        fps_limit: fps.limit,
        notice: fps.notice,
        ..App::default()
    };
    let mut session = Session::default();
    let mut ticker = tokio::time::interval(Duration::from_secs(1));

    loop {
        // draw は必ず MoveTo で始まり SGR を閉じて flush するので、その直後なら割り込まずに書ける。
        // マウスの当たり判定は「ユーザーが今見ている画面」で行うので、描いた寸法を控える。
        app.screen = terminal.draw(|frame| ui::draw(frame, &app))?.area;
        let area = ui::video_area(app.screen);
        present_video(&mut session, &app, area, terminal.backend_mut())?;
        tokio::select! {
            event = rx.recv() => {
                let Some(event) = event else { break };
                handle_event(&mut app, event, &tx, &mut session).await;
                for _ in 0..EVENT_DRAIN_LIMIT {
                    if app.should_quit {
                        break;
                    }
                    let Ok(event) = rx.try_recv() else { break };
                    handle_event(&mut app, event, &tx, &mut session).await;
                }
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
                // 種別の選り分けは input::mouse_input に寄せる。
                Ok(CrosstermEvent::Mouse(mouse)) => tx.send(AppEvent::Mouse(mouse)),
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
        AppEvent::Mouse(mouse) => handle_mouse(app, mouse, session).await,
        AppEvent::Resize { width, height } => schedule_resize(session, width, height),
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
                // fps 上限は表示でなく mpv への指示なので、App でなく player へ渡す。
                if id == mpv::REQ_CONTAINER_FPS {
                    apply_fps_limit(app, session, data.and_then(|v| v.as_f64())).await;
                } else {
                    app.apply_property(id, data);
                }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Playback;
    use crate::kitty::fixtures::{KITTY_RECONFIG, frame};
    use crate::search::SearchResult;
    use crate::video::{CellSize, Geometry, MAX_FRAME_PIXELS};

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
        // 320x176 px は 80x22 の領域と同じ比率なので、原点から領域いっぱいに広がる。
        let mut expected = clear_bytes();
        expected.extend_from_slice(b"\x1b[1;1H");
        assert!(
            out.starts_with(&expected),
            "clear → CUP の順になっていない: {:?}",
            String::from_utf8_lossy(&out)
        );
        let scale_keys = b",c=80,r=22";
        assert!(
            out.windows(scale_keys.len()).any(|w| w == scale_keys),
            "表示セル数が入っていない: {:?}",
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
}
