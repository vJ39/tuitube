//! Session を動かすアクション。キー入力もイベント処理もここを通して player を触る。

use crate::app::{App, AppEvent, Mode, Playback};
use crate::cookies::Target;
use crate::display::{self, DisplayMode, LaunchPlan};
use crate::geometry::{cell_size, geometry_for, video_geometry};
use crate::mpv::{self, MpvCommand, MpvController};
use crate::search::{self, RealYtDlp, YtDlp};
use crate::seekbar::{SeekBarState, clamp_target};
use crate::video::VideoSink;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;
use tokio::time::Instant;

/// ドラッグ中は Resize が連続して届くので、落ち着くまで mpv の作り直しを待つ。
const RESIZE_DEBOUNCE: Duration = Duration::from_millis(200);
/// ←→ 1 回あたりのシーク幅。
pub const SEEK_STEP_SECS: f64 = 5.0;

type Sending<'a> = Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;

/// mpv への送信口。テストは送った内容を溜める偽物に差し替える。
pub trait PlayerSink: Send {
    fn send<'a>(&'a mut self, command: &'a MpvCommand) -> Sending<'a>;
}

impl PlayerSink for MpvController {
    fn send<'a>(&'a mut self, command: &'a MpvCommand) -> Sending<'a> {
        Box::pin(MpvController::send(self, command))
    }
}

pub struct Player {
    pub sink: Box<dyn PlayerSink>,
    pub nonce: u64,
}

impl Player {
    /// 1 つでも失敗したらそこで止める。途中まで効いた状態は次の操作で上書きされる。
    pub async fn send_all(&mut self, commands: &[MpvCommand]) -> Result<(), String> {
        for command in commands {
            self.sink.send(command).await?;
        }
        Ok(())
    }
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
        && let Err(e) = p.sink.send(command).await
    {
        app.error = Some(e);
    }
}

/// 毎秒のポーリング。一過性の失敗で再生状況が隠れ続けないよう、成功したらエラーを消す。
pub async fn poll_player(app: &mut App, session: &mut Session) {
    let Some(player) = session.player.as_mut() else {
        return;
    };
    match player.send_all(&mpv::poll_commands()).await {
        Ok(()) => app.error = None,
        Err(e) => app.error = Some(e),
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
    let video = VideoSink::new(video_geometry(app.settings.display.max_pixels()));
    let mut plan = LaunchPlan::new(app.display, video.geometry(), &app.settings);
    // 検索で cookie が効くと確かめた後だけ再生にも渡す (再生側では劣化を検知できない)。
    plan.extra_args
        .extend(app.cookies.for_playback().map(|source| source.mpv_arg()));
    match MpvController::launch(&result.url(), nonce, tx.clone(), video.clone(), &plan).await {
        Ok(controller) => {
            app.playback = Playback {
                title: result.title.clone(),
                ..Playback::default()
            };
            app.seek_bar = SeekBarState::default();
            app.video = Some(video);
            app.mode = Mode::Playing;
            app.error = None;
            session.player = Some(Player {
                sink: Box::new(controller),
                nonce,
            });
        }
        Err(e) => {
            app.video = None;
            app.error = Some(e);
        }
    }
}

/// 埋め込み ⇔ 別ウィンドウ。mpv は再起動せず VO を差し替えるので再生は途切れない。
pub async fn toggle_display_mode(app: &mut App, session: &mut Session) {
    if session.player.is_none() {
        return;
    }
    let next = app.display.toggled();
    let commands = match next {
        DisplayMode::Window => {
            display::to_window_commands(app.settings.fps_cap, &app.settings.window)
        }
        DisplayMode::Embedded => {
            // 別ウィンドウ中の端末リサイズは mpv へ送っていないので、戻るときに現寸法で送り直す。
            let geometry = video_geometry(app.settings.display.max_pixels());
            if let Some(video) = &app.video {
                video.resize(geometry);
            }
            display::to_embedded_commands(geometry, app.settings.fps_cap)
        }
    };
    let Some(player) = session.player.as_mut() else {
        return;
    };
    match player.send_all(&commands).await {
        Ok(()) => {
            app.display = next;
            // kitty VO の後始末が stdout に出ない経路でも画像を残さない。
            if next == DisplayMode::Window {
                session.owe_clear = true;
            }
        }
        Err(e) => app.error = Some(e),
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
    let geometry = geometry_for(cols, rows, cell_size(), app.settings.display.max_pixels());
    if video.geometry() == geometry {
        return;
    }
    video.resize(geometry);
    // 別ウィンドウ中は描き直すものが無い。戻るときに現寸法を送る。
    if app.display != DisplayMode::Embedded {
        return;
    }
    if let Some(p) = session.player.as_mut()
        && let Err(e) = p.send_all(&mpv::resize_video(geometry)).await
    {
        app.error = Some(e);
    }
}

/// quit を送って手放す。届かなくても終了待ちタスクが猶予後に kill するので取り残さない。
pub async fn stop_playback(session: &mut Session) {
    if let Some(mut p) = session.player.take() {
        let _ = p.sink.send(&mpv::quit()).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cookies::{CookieSource, CookieState};
    use crate::display::Quality;
    use crate::search::SearchResult;
    use crate::settings::{DisplaySettings, Settings};
    use crate::video::{CellSize, Geometry, MAX_FRAME_PIXELS};
    use ratatui::layout::Rect;
    use std::process::Output;
    use std::sync::{Arc, Mutex};
    use tokio::sync::mpsc;

    /// 送った内容だけを溜める偽の player。外部プロセスへは届かない。
    struct Recorder {
        sent: Arc<Mutex<Vec<String>>>,
        result: Result<(), String>,
    }

    impl PlayerSink for Recorder {
        fn send<'a>(&'a mut self, command: &'a MpvCommand) -> Sending<'a> {
            let sent = self.sent.clone();
            let result = self.result.clone();
            Box::pin(async move {
                sent.lock().expect("溜め込み先").push(command.to_line());
                result
            })
        }
    }

    fn record(session: &mut Session, result: Result<(), String>) -> Arc<Mutex<Vec<String>>> {
        let sent = Arc::new(Mutex::new(Vec::new()));
        session.player = Some(Player {
            sink: Box::new(Recorder {
                sent: sent.clone(),
                result,
            }),
            nonce: 1,
        });
        sent
    }

    fn lines(sent: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
        sent.lock().expect("溜め込み先").clone()
    }

    fn playing_app() -> App {
        App {
            mode: Mode::Playing,
            ..App::default()
        }
    }

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

        assert_eq!(
            video.geometry(),
            geometry_for(100, 40, cell_size(), MAX_FRAME_PIXELS)
        );
        assert!(session.pending_resize.is_none());
    }

    #[tokio::test]
    async fn toggling_without_a_player_changes_nothing() {
        let mut app = App {
            mode: Mode::Playing,
            video: Some(sink()),
            ..App::default()
        };
        let mut session = Session::default();
        toggle_display_mode(&mut app, &mut session).await;

        assert_eq!(app.display, DisplayMode::Embedded);
        assert!(app.error.is_none());
        assert!(!session.owe_clear);
    }

    #[tokio::test]
    async fn resize_in_window_mode_updates_the_sink_but_owes_no_mpv_commands() {
        let video = sink();
        let mut app = App {
            display: DisplayMode::Window,
            video: Some(video.clone()),
            ..App::default()
        };
        let mut session = Session {
            pending_resize: Some((100, 40)),
            ..Session::default()
        };
        apply_resize(&mut app, &mut session).await;

        assert_eq!(
            video.geometry(),
            geometry_for(100, 40, cell_size(), MAX_FRAME_PIXELS)
        );
        assert!(app.error.is_none());
    }

    #[tokio::test]
    async fn resize_uses_the_configured_pixel_budget() {
        let video = sink();
        let mut app = App {
            video: Some(video.clone()),
            settings: Settings {
                display: DisplaySettings {
                    quality: Quality::Low,
                    ..DisplaySettings::default()
                },
                ..Settings::default()
            },
            ..App::default()
        };
        let mut session = Session {
            pending_resize: Some((100, 40)),
            ..Session::default()
        };
        apply_resize(&mut app, &mut session).await;

        let frame = video.geometry().frame_px;
        let pixels = u64::from(frame.0) * u64::from(frame.1);
        assert!(
            pixels <= u64::from(Quality::Low.max_pixels()),
            "{pixels} px"
        );
    }

    #[tokio::test]
    async fn toggle_to_window_sends_remove_then_vo_and_owes_a_clear() {
        let mut app = playing_app();
        let mut session = Session::default();
        let sent = record(&mut session, Ok(()));
        toggle_display_mode(&mut app, &mut session).await;

        assert_eq!(
            lines(&sent),
            [
                "{\"command\":[\"vf\",\"remove\",\"@tuitube-cap\"]}\n",
                "{\"command\":[\"set_property\",\"vo\",\"\"]}\n",
            ]
        );
        assert_eq!(app.display, DisplayMode::Window);
        assert!(session.owe_clear);
        assert!(app.error.is_none());
    }

    #[tokio::test]
    async fn toggle_back_to_embedded_resizes_the_sink_and_sends_geometry_cap_vo() {
        let video = sink();
        let mut app = App {
            display: DisplayMode::Window,
            video: Some(video.clone()),
            ..playing_app()
        };
        let mut session = Session::default();
        let sent = record(&mut session, Ok(()));
        toggle_display_mode(&mut app, &mut session).await;

        let expected = video_geometry(app.settings.display.max_pixels());
        assert_eq!(video.geometry(), expected);
        assert_eq!(
            lines(&sent),
            display::to_embedded_commands(expected, app.settings.fps_cap)
                .iter()
                .map(|c| c.to_line())
                .collect::<Vec<_>>()
        );
        assert_eq!(app.display, DisplayMode::Embedded);
        // 埋め込みへ戻すときは kitty VO 自身が後始末を出す。
        assert!(!session.owe_clear);
    }

    #[tokio::test]
    async fn toggle_keeps_the_mode_when_sending_fails() {
        let mut app = playing_app();
        let mut session = Session::default();
        let _sent = record(&mut session, Err("パイプが閉じました".to_string()));
        toggle_display_mode(&mut app, &mut session).await;

        assert_eq!(app.display, DisplayMode::Embedded);
        assert_eq!(app.error.as_deref(), Some("パイプが閉じました"));
        assert!(!session.owe_clear);
    }

    #[tokio::test]
    async fn resize_in_window_mode_sends_nothing() {
        let mut app = App {
            display: DisplayMode::Window,
            video: Some(sink()),
            ..playing_app()
        };
        let mut session = Session {
            pending_resize: Some((100, 40)),
            ..Session::default()
        };
        let sent = record(&mut session, Ok(()));
        apply_resize(&mut app, &mut session).await;

        assert!(lines(&sent).is_empty());
    }

    #[tokio::test]
    async fn resize_in_embedded_mode_sends_the_existing_resize_sequence() {
        let mut app = App {
            video: Some(sink()),
            ..playing_app()
        };
        let mut session = Session {
            pending_resize: Some((100, 40)),
            ..Session::default()
        };
        let sent = record(&mut session, Ok(()));
        apply_resize(&mut app, &mut session).await;

        let geometry = geometry_for(100, 40, cell_size(), MAX_FRAME_PIXELS);
        assert_eq!(
            lines(&sent),
            mpv::resize_video(geometry)
                .iter()
                .map(|c| c.to_line())
                .collect::<Vec<_>>()
        );
    }
}
