//! Session を動かすアクション。キー入力もイベント処理もここを通して player を触る。

use crate::app::{App, AppEvent, Mode, Playback};
use crate::geometry::{cell_size, geometry_for, video_geometry};
use crate::mpv::{MpvCommand, MpvController};
use crate::search;
use crate::video::VideoSink;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;
use tokio::time::{Instant, timeout};

const SEARCH_TIMEOUT: Duration = Duration::from_secs(30);
/// ドラッグ中は Resize が連続して届くので、落ち着くまで mpv の作り直しを待つ。
const RESIZE_DEBOUNCE: Duration = Duration::from_millis(200);

pub struct Player {
    pub controller: MpvController,
    pub nonce: u64,
}

#[derive(Default)]
pub struct Session {
    pub player: Option<Player>,
    pub player_nonce: u64,
    pub search_nonce: u64,
    pub search_task: Option<JoinHandle<()>>,
    /// 直近の端末サイズと、それを映像へ反映する時刻。
    pub pending_resize: Option<(u16, u16)>,
    pub resize_at: Option<Instant>,
    /// 再生が終わった後など、sink 越しに出せない画像削除の持ち越し。
    pub owe_clear: bool,
}

/// 再生中の player へコマンドを送り、失敗だけを画面に出す。キーもマウスもここを使う。
pub async fn send_to_player(app: &mut App, session: &mut Session, command: &MpvCommand) {
    if let Some(p) = session.player.as_mut()
        && let Err(e) = p.controller.send(command).await
    {
        app.error = Some(e);
    }
}

pub async fn end_playback(app: &mut App, session: &mut Session, error: Option<String>) {
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

pub fn start_search(app: &mut App, tx: &UnboundedSender<AppEvent>, session: &mut Session) {
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

pub async fn start_playback(app: &mut App, tx: &UnboundedSender<AppEvent>, session: &mut Session) {
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

/// 連続して届く Resize は、最後の1つだけを少し置いてから反映する。
pub fn schedule_resize(session: &mut Session, width: u16, height: u16) {
    session.pending_resize = Some((width, height));
    session.resize_at = Some(Instant::now() + RESIZE_DEBOUNCE);
}

/// 端末サイズが変わったら、映像の寸法を mpv ごと作り直して食い違いを解消する。
pub async fn apply_resize(app: &mut App, session: &mut Session) {
    let Some((cols, rows)) = session.pending_resize.take() else {
        return;
    };
    let Some(video) = app.video.clone() else {
        return;
    };
    let geometry = geometry_for(cols, rows, cell_size());
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

pub async fn stop_playback(session: &mut Session) {
    if let Some(p) = session.player.take() {
        p.controller.shutdown().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::SearchResult;
    use crate::video::{CellSize, Geometry, MAX_FRAME_PIXELS};
    use ratatui::layout::Rect;
    use tokio::sync::mpsc;

    const CELL: CellSize = CellSize {
        width_px: 8,
        height_px: 16,
    };

    fn result(id: &str) -> SearchResult {
        SearchResult {
            id: id.to_string(),
            title: format!("title {id}"),
            duration: None,
            uploader: None,
        }
    }

    fn sink() -> VideoSink {
        VideoSink::new(Geometry::new(
            Rect::new(0, 0, 80, 22),
            CELL,
            MAX_FRAME_PIXELS,
        ))
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

    #[tokio::test]
    async fn end_playback_with_results_returns_to_the_list_and_keeps_the_error() {
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

        assert_eq!(app.mode, Mode::Results);
        assert_eq!(app.error.as_deref(), Some("boom"));
        assert!(app.video.is_none());
        assert!(app.playback.title.is_empty());
        assert!(session.owe_clear);
    }

    #[tokio::test]
    async fn end_playback_without_an_error_keeps_the_previous_one() {
        let mut app = App {
            mode: Mode::Playing,
            error: Some("前のエラー".to_string()),
            ..App::default()
        };
        let mut session = Session::default();
        end_playback(&mut app, &mut session, None).await;
        assert_eq!(app.error.as_deref(), Some("前のエラー"));
    }

    #[test]
    fn a_blank_query_does_not_start_a_search() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = App {
            query: "   ".to_string(),
            ..App::default()
        };
        start_search(&mut app, &tx, &mut session);

        assert!(!app.searching);
        assert_eq!(session.search_nonce, 0);
        assert!(session.search_task.is_none());
    }

    #[tokio::test]
    async fn search_bumps_the_nonce_and_clears_the_error() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = App {
            query: "  ラーメン  ".to_string(),
            error: Some("boom".to_string()),
            ..App::default()
        };
        start_search(&mut app, &tx, &mut session);

        assert_eq!(session.search_nonce, 1);
        assert!(app.searching);
        assert!(app.error.is_none());
        // yt-dlp を実際に走らせないよう、一度も polled されないうちに畳む。
        session
            .search_task
            .take()
            .expect("検索タスクが積まれている")
            .abort();
    }

    #[tokio::test]
    async fn resize_is_scheduled_after_the_debounce() {
        let mut session = Session::default();
        let before = Instant::now();
        schedule_resize(&mut session, 100, 40);

        assert_eq!(session.pending_resize, Some((100, 40)));
        let at = session.resize_at.expect("反映時刻が入っている");
        assert!(at >= before + RESIZE_DEBOUNCE);
    }

    #[tokio::test]
    async fn resize_without_a_video_just_consumes_the_request() {
        let mut app = App::default();
        let mut session = Session {
            pending_resize: Some((100, 40)),
            ..Session::default()
        };
        apply_resize(&mut app, &mut session).await;

        assert!(session.pending_resize.is_none());
        assert!(app.error.is_none());
    }

    #[tokio::test]
    async fn resize_updates_the_sink_geometry() {
        let video = sink();
        let mut app = App {
            video: Some(video.clone()),
            ..App::default()
        };
        let mut session = Session {
            pending_resize: Some((100, 40)),
            ..Session::default()
        };
        apply_resize(&mut app, &mut session).await;

        assert_eq!(video.geometry(), geometry_for(100, 40, cell_size()));
        assert!(session.pending_resize.is_none());
    }
}
