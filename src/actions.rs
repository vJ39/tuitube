//! Session を動かすアクション。キー入力もイベント処理もここを通して player を触る。

use crate::app::{App, AppEvent, Mode, Playback};
use crate::cookies::Target;
use crate::geometry::{cell_size, geometry_for, video_geometry};
use crate::mpv::{self, MpvCommand, MpvController};
use crate::search::{self, RealYtDlp, YtDlp};
use crate::seekbar::{SeekBarState, clamp_target};
use crate::video::VideoSink;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;
use tokio::time::Instant;

/// ドラッグ中は Resize が連続して届くので、落ち着くまで mpv の作り直しを待つ。
const RESIZE_DEBOUNCE: Duration = Duration::from_millis(200);
/// ←→ 1 回あたりのシーク幅。
pub const SEEK_STEP_SECS: f64 = 5.0;

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

/// ←→ のシーク。mpv へは相対で送り、表示だけ目標値へ先行させる。
pub async fn seek_relative(
    app: &mut App,
    session: &mut Session,
    delta: f64,
    now: std::time::Instant,
) {
    send_to_player(app, session, &mpv::seek(delta.round() as i64)).await;
    if let Some(base) = app.playback.seek_base() {
        let target = clamp_target(base + delta, app.playback.duration);
        app.playback.begin_seek(target, now);
    }
    // キー操作が来たらホバー表示は用済み。
    app.seek_bar.clear_hover();
}

/// バーのクリック・ドラッグのシーク。送る値は必ずファイル内へ丸める。
pub async fn seek_absolute(
    app: &mut App,
    session: &mut Session,
    target: f64,
    now: std::time::Instant,
) {
    let target = clamp_target(target, app.playback.duration);
    send_to_player(app, session, &mpv::seek_absolute(target)).await;
    app.playback.begin_seek(target, now);
}

pub async fn end_playback(app: &mut App, session: &mut Session, error: Option<String>) {
    stop_playback(session).await;
    app.playback = Playback::default();
    app.seek_bar = SeekBarState::default();
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
    start_search_with(app, tx, session, RealYtDlp);
}

/// yt-dlp の実行者を差し替えられる形。テストはここに偽物を渡して外部プロセスへ届かせない。
pub fn start_search_with<R>(
    app: &mut App,
    tx: &UnboundedSender<AppEvent>,
    session: &mut Session,
    runner: R,
) where
    R: YtDlp + Send + Sync + 'static,
{
    let query = app.query.trim().to_string();
    if query.is_empty() {
        return;
    }
    let target = Target::for_query(&query);
    // 断る場合も先に打ち切る。生き残った先行検索の結果が後から画面を塗り替えないため。
    cancel_search(app, session);
    // cookie が無いフィードは yt-dlp を 10 秒待たせても結果が出ないので、その場で断る。
    if target.requires_login()
        && app.cookies.for_search().is_none()
        && let Target::Feed(feed) = &target
    {
        app.error = Some(app.cookies.refusal(*feed));
        app.mode = Mode::Input;
        return;
    }
    let nonce = session.search_nonce;
    app.searching = true;
    app.error = None;
    let cookies = app.cookies.for_search().cloned();
    let tx = tx.clone();
    session.search_task = Some(tokio::spawn(async move {
        let report = search::run_search(&runner, &target, cookies.as_ref()).await;
        let _ = tx.send(AppEvent::SearchDone {
            nonce,
            target,
            report,
        });
    }));
}

/// 先行の検索を打ち切る。nonce を進めるので、届いてしまった結果は捨てられる
/// (kill_on_drop で子プロセスも落ちる)。
fn cancel_search(app: &mut App, session: &mut Session) {
    if let Some(task) = session.search_task.take() {
        task.abort();
    }
    session.search_nonce += 1;
    app.searching = false;
    app.notice = None;
}

pub async fn start_playback(app: &mut App, tx: &UnboundedSender<AppEvent>, session: &mut Session) {
    let Some(result) = app.selected_result().cloned() else {
        return;
    };
    stop_playback(session).await;
    session.player_nonce += 1;
    let nonce = session.player_nonce;
    let video = VideoSink::new(video_geometry());
    // 検索で cookie が効くと確かめた後だけ再生にも渡す (再生側では劣化を検知できない)。
    let extra: Vec<String> = app
        .cookies
        .for_playback()
        .map(|source| vec![source.mpv_arg()])
        .unwrap_or_default();
    match MpvController::launch(
        &result.url(),
        nonce,
        tx.clone(),
        video.clone(),
        app.fps_limit,
        &extra,
    )
    .await
    {
        Ok(controller) => {
            app.playback = Playback {
                title: result.title.clone(),
                ..Playback::default()
            };
            app.seek_bar = SeekBarState::default();
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

/// ソースの fps が届いたら、上限を超えるときだけ mpv に fps フィルタを足す。
/// 読み込み前は値が返らないので、取れないうちは次のポーリングに任せる。
pub async fn apply_fps_limit(app: &mut App, session: &mut Session, source_fps: Option<f64>) {
    let Some(source_fps) = source_fps.filter(|fps| fps.is_finite() && *fps > 0.0) else {
        return;
    };
    if let Some(p) = session.player.as_mut()
        && let Err(e) = p.controller.limit_fps(source_fps).await
    {
        app.error = Some(e);
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
    use crate::cookies::{CookieSource, CookieState};
    use crate::search::SearchResult;
    use crate::video::{CellSize, Geometry, MAX_FRAME_PIXELS};
    use ratatui::layout::Rect;
    use std::future::Future;
    use std::process::Output;
    use tokio::sync::mpsc;

    const CELL: CellSize = CellSize {
        width_px: 8,
        height_px: 16,
    };

    /// 応答を返さない偽の yt-dlp。タスクは積まれるが外部プロセスは起動しない。
    struct StubYtDlp;

    impl YtDlp for StubYtDlp {
        fn run(&self, _args: Vec<String>) -> impl Future<Output = std::io::Result<Output>> + Send {
            std::future::pending()
        }
    }

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
        start_search_with(&mut app, &tx, &mut session, StubYtDlp);
        // 積んだタスクを一度動かす。偽のランナーなので外部プロセスには届かない。
        tokio::task::yield_now().await;

        assert_eq!(session.search_nonce, 1);
        assert!(app.searching);
        assert!(app.error.is_none());
        assert!(session.search_task.is_some());
    }

    #[tokio::test]
    async fn feed_requiring_login_is_refused_before_spawning_when_cookies_are_off() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = App {
            query: ":ytrec".to_string(),
            ..App::default()
        };
        start_search_with(&mut app, &tx, &mut session, StubYtDlp);

        assert!(session.search_task.is_none(), "yt-dlp を起動しない");
        // 断った試行も世代を1つ進める (先行の結果を無効にするため)。
        assert_eq!(session.search_nonce, 1);
        assert!(!app.searching);
        let error = app.error.expect("理由を出す");
        assert!(error.contains(crate::cookies::ENV_VAR), "{error}");
    }

    #[tokio::test]
    async fn a_refused_feed_cancels_the_search_in_flight() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = App {
            query: "ラーメン".to_string(),
            notice: Some("前の検索の知らせ".to_string()),
            ..App::default()
        };
        start_search_with(&mut app, &tx, &mut session, StubYtDlp);
        tokio::task::yield_now().await;
        assert!(app.searching);

        // 検索中に cookie 無しのフィードを要求する。
        app.query = ":ytrec".to_string();
        start_search_with(&mut app, &tx, &mut session, StubYtDlp);

        // 先行タスクは残さず、その結果が後から採用されないよう nonce も進める。
        assert!(session.search_task.is_none());
        assert_eq!(session.search_nonce, 2);
        assert!(!app.searching);
        assert!(app.notice.is_none());
        assert_eq!(app.mode, Mode::Input);
        let error = app.error.expect("理由を出す");
        assert!(error.contains(crate::cookies::ENV_VAR), "{error}");
    }

    #[tokio::test]
    async fn feed_requiring_login_is_refused_when_suspended() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = App {
            query: ":ythis".to_string(),
            cookies: CookieState::Suspended {
                source: CookieSource::from_env_value(Some("chrome")).expect("spec"),
                reason: "cookie を読めませんでした".to_string(),
            },
            ..App::default()
        };
        start_search_with(&mut app, &tx, &mut session, StubYtDlp);

        assert!(session.search_task.is_none());
        let error = app.error.expect("理由を出す");
        assert!(error.contains("停止中"), "{error}");
        assert!(error.contains("cookie を読めませんでした"), "{error}");
    }

    #[tokio::test]
    async fn start_search_clears_the_notice() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = App {
            query: "ラーメン".to_string(),
            notice: Some("前の検索の知らせ".to_string()),
            ..App::default()
        };
        start_search_with(&mut app, &tx, &mut session, StubYtDlp);

        assert!(app.notice.is_none());
        assert!(session.search_task.is_some());
    }

    #[tokio::test]
    async fn an_unknown_source_fps_leaves_the_filter_for_the_next_poll() {
        // container-fps は読み込み前だと返らない。値が無いうちは何もしない。
        let mut app = App::default();
        let mut session = Session::default();
        apply_fps_limit(&mut app, &mut session, None).await;
        apply_fps_limit(&mut app, &mut session, Some(f64::NAN)).await;
        apply_fps_limit(&mut app, &mut session, Some(0.0)).await;
        // 再生していなければ送り先も無い。
        apply_fps_limit(&mut app, &mut session, Some(30.0)).await;
        assert!(app.error.is_none());
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
