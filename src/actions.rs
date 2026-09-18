//! Session を動かすアクション。キー入力もイベント処理もここを通して player を触る。

use crate::app::{App, AppEvent, Mode, Playback};
use crate::clipboard::{Clipboard, MISSING_PBCOPY};
use crate::cookies::Target;
use crate::display::{self, DisplayMode, LaunchPlan};
use crate::fetch::{Fetcher, RealCurl};
use crate::geometry::{cell_size, geometry_for, video_geometry};
use crate::grid::{self, Dir};
use crate::mpv::{self, MpvCommand, MpvController};
use crate::search::{self, RealYtDlp, YtDlp};
use crate::seekbar::{SeekBarState, clamp_target};
use crate::speed::Speed;
use crate::thumbs;
use crate::ui;
use crate::video::{DecoderKind, VideoSink};
use ratatui::layout::Rect;
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
/// コピーできたことを伝える文言。
pub const COPIED_NOTICE: &str = "URL をコピーしました";

pub type Sending<'a> = Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;

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
    /// サムネイルの取得・デコード。nonce は search_nonce を共用する。
    pub thumbs_task: Option<JoinHandle<()>>,
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
        app.set_error(Some(e));
    }
}

/// ポーリングの応答を取り込む。mpv へ送り返すものがあれば続けて送る。
pub async fn apply_property(
    app: &mut App,
    session: &mut Session,
    id: u64,
    data: Option<serde_json::Value>,
) {
    if let Some(command) = app.apply_property(id, data) {
        send_to_player(app, session, &command).await;
    }
}

/// 毎秒の手入れ。期限切れの知らせとエラーを片付けてから再生状況を問い合わせる。
pub async fn on_tick(app: &mut App, session: &mut Session, now: std::time::Instant) {
    app.expire_notice(now);
    app.expire_error(now);
    poll_player(app, session, now).await;
}

/// 毎秒のポーリング。一過性の失敗で再生状況が隠れ続けないよう、成功したらエラーを消す。
/// ただし操作が失敗した理由は、押した本人が読む前に消えないよう期限まで残す。
pub async fn poll_player(app: &mut App, session: &mut Session, now: std::time::Instant) {
    let Some(player) = session.player.as_mut() else {
        return;
    };
    match player.send_all(&mpv::poll_commands()).await {
        Ok(()) => app.clear_polled_error(now),
        Err(e) => app.set_error(Some(e)),
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
    // mpv の a=d でサムネイルも消えているので、結果へ戻ったら貼り直す。
    app.thumbs.mark_dirty();
    if error.is_some() {
        app.set_error(error);
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
    // 検索ボックスの文字列は「すべて」タブのもの。対応を 1 対 1 に保つ。
    if !app.tabs.is_all() {
        app.store_to_tab();
        app.tabs.select_all();
        app.sync_from_tab();
    }
    spawn_search(app, tx, session, Target::for_query(&query), runner);
}

/// 今のタブのクエリで検索する。「すべて」タブでは検索ボックスの文字列を使う。
pub fn start_tab_search_with<R>(
    app: &mut App,
    tx: &UnboundedSender<AppEvent>,
    session: &mut Session,
    runner: R,
) where
    R: YtDlp + Send + Sync + 'static,
{
    let Some(target) = app.tabs.target(&app.query) else {
        // 検索できないタブでも先行検索は打ち切る。残すと結果がこのタブへ流れ込む。
        cancel_search(app, session);
        return;
    };
    spawn_search(app, tx, session, target, runner);
}

fn spawn_search<R>(
    app: &mut App,
    tx: &UnboundedSender<AppEvent>,
    session: &mut Session,
    target: Target,
    runner: R,
) where
    R: YtDlp + Send + Sync + 'static,
{
    // 断る場合も先に打ち切る。生き残った先行検索の結果が後から画面を塗り替えないため。
    cancel_search(app, session);
    // cookie が無いフィードは yt-dlp を 10 秒待たせても結果が出ないので、その場で断る。
    if target.requires_login()
        && app.cookies.for_search().is_none()
        && let Target::Feed(feed) = &target
    {
        app.set_error(Some(app.cookies.refusal(*feed)));
        app.mode = Mode::Input;
        return;
    }
    let nonce = session.search_nonce;
    let limit = app.settings.search.limit;
    app.searching = true;
    app.set_error(None);
    let cookies = app.cookies.for_search().cloned();
    let tx = tx.clone();
    session.search_task = Some(tokio::spawn(async move {
        let report = search::run_search(&runner, &target, cookies.as_ref(), limit).await;
        let _ = tx.send(AppEvent::SearchDone {
            nonce,
            target,
            report,
        });
    }));
}

pub fn switch_tab(
    app: &mut App,
    tx: &UnboundedSender<AppEvent>,
    session: &mut Session,
    forward: bool,
) {
    switch_tab_with(app, tx, session, forward, RealYtDlp);
}

/// タブを移り、そのタブの状態を画面へ写す。まだ読んでいないタブだけ検索する。
pub fn switch_tab_with<R>(
    app: &mut App,
    tx: &UnboundedSender<AppEvent>,
    session: &mut Session,
    forward: bool,
    runner: R,
) where
    R: YtDlp + Send + Sync + 'static,
{
    // 移る前に必ず打ち切る。走らせたままにすると、その結果が移った先のタブへ書き込まれる。
    cancel_search(app, session);
    app.store_to_tab();
    if forward {
        app.tabs.next();
    } else {
        app.tabs.prev();
    }
    app.sync_from_tab();
    app.set_error(None);
    if app.tabs.state().loaded {
        return;
    }
    start_tab_search_with(app, tx, session, runner);
}

pub fn reload_tab(app: &mut App, tx: &UnboundedSender<AppEvent>, session: &mut Session) {
    reload_tab_with(app, tx, session, RealYtDlp);
}

pub fn reload_tab_with<R>(
    app: &mut App,
    tx: &UnboundedSender<AppEvent>,
    session: &mut Session,
    runner: R,
) where
    R: YtDlp + Send + Sync + 'static,
{
    app.tabs.state_mut().loaded = false;
    start_tab_search_with(app, tx, session, runner);
}

/// 格子の中の移動。リスト表示に落ちているときは既存の巻き戻る移動を使う。
pub fn move_selection(app: &mut App, dir: Dir) {
    let Some(layout) = ui::grid_layout(app, cell_size()) else {
        match dir {
            Dir::Down => app.select_next(),
            Dir::Up => app.select_prev(),
            Dir::Left | Dir::Right => {}
        }
        return;
    };
    app.selected = grid::move_selection(app.selected, app.results.len(), layout.columns, dir);
    let scroll = grid::ensure_visible(app.selected, layout.columns, layout.rows, app.scroll);
    // 選択の強調は画像の外に描くので、可視範囲が動いたときだけ貼り直す。
    if scroll != app.scroll {
        app.scroll = scroll;
        app.thumbs.mark_dirty();
    }
}

pub fn start_thumbnails(app: &mut App, tx: &UnboundedSender<AppEvent>, session: &mut Session) {
    start_thumbnails_with(app, tx, session, RealCurl);
}

/// 未取得のサムネイルを 1 タスクで取り、デコードして目標寸法へ縮める。
pub fn start_thumbnails_with<F>(
    app: &mut App,
    tx: &UnboundedSender<AppEvent>,
    session: &mut Session,
    fetcher: F,
) where
    F: Fetcher + Send + Sync + 'static,
{
    if let Some(task) = session.thumbs_task.take() {
        task.abort();
    }
    app.thumbs.set_fetching(false);
    if !app.settings.thumbnails.enabled || app.thumbs.disabled().is_some() {
        return;
    }
    let Some(layout) = ui::grid_layout(app, cell_size()) else {
        return;
    };
    let target_px = layout.image_px;
    let ids = app.thumbs.wanted(&app.result_ids(), target_px);
    if ids.is_empty() {
        return;
    }
    let nonce = session.search_nonce;
    let cache = app.settings.thumbnails.dir();
    let timeout = app.settings.thumbnails.timeout;
    let tx = tx.clone();
    app.thumbs.set_fetching(true);
    session.thumbs_task = Some(tokio::spawn(async move {
        let outcome = thumbs::fetch_thumbnails(&fetcher, ids, target_px, cache, timeout).await;
        let _ = tx.send(AppEvent::ThumbsReady {
            nonce,
            target_px,
            images: outcome.images,
            notice: outcome.notice,
            disable: outcome.disable,
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
    app.set_notice(None);
}

/// 再生開始時の映像スロットと起動計画。mpv を起動せずに検証できるよう切り出してある。
fn playback_plan(app: &App) -> (VideoSink, LaunchPlan) {
    // 別ウィンドウで始めても、端末内へ戻ったときのために kitty で用意しておく。
    let kind = app.display.decoder_kind().unwrap_or(DecoderKind::Kitty);
    let video = VideoSink::with_kind(kind, video_geometry(app.settings.display.max_pixels()));
    let mut plan = LaunchPlan::new(app.display, video.geometry(), &app.settings);
    plan.speed = app.speed;
    // 検索で cookie が効くと確かめた後だけ再生にも渡す (再生側では劣化を検知できない)。
    plan.extra_args
        .extend(app.cookies.for_playback().map(|source| source.mpv_arg()));
    (video, plan)
}

/// 起動できた後の画面側の状態。mpv を起動せずに検証できるよう切り出してある。
pub fn enter_playback(
    app: &mut App,
    session: &mut Session,
    title: String,
    url: String,
    video: VideoSink,
) {
    app.playback = Playback {
        title,
        url,
        ..Playback::default()
    };
    app.seek_bar = SeekBarState::default();
    app.video = Some(video);
    app.mode = Mode::Playing;
    app.set_error(None);
    // 貼ってあるサムネイルは ratatui の差分描画では消えない。end_playback と対称に剥がす。
    session.owe_clear = true;
}

pub async fn start_playback(app: &mut App, tx: &UnboundedSender<AppEvent>, session: &mut Session) {
    let Some(result) = app.selected_result().cloned() else {
        return;
    };
    stop_playback(session).await;
    session.player_nonce += 1;
    let nonce = session.player_nonce;
    let (video, plan) = playback_plan(app);
    let url = result.url();
    match MpvController::launch(&url, nonce, tx.clone(), video.clone(), &plan).await {
        Ok(controller) => {
            enter_playback(app, session, result.title.clone(), url, video);
            session.player = Some(Player {
                sink: Box::new(controller),
                nonce,
            });
        }
        Err(e) => {
            app.video = None;
            app.set_error(Some(e));
        }
    }
}

/// 速度を steps × 0.1 動かす。境界では止まる。
pub async fn change_speed(app: &mut App, session: &mut Session, steps: i8) {
    set_speed(app, session, app.speed.stepped(steps)).await;
}

pub async fn reset_speed(app: &mut App, session: &mut Session) {
    set_speed(app, session, Speed::NORMAL).await;
}

/// 値が変わらないときは送らない (境界で押し続けても mpv を叩かない)。
async fn set_speed(app: &mut App, session: &mut Session, next: Speed) {
    if next == app.speed {
        return;
    }
    let Some(player) = session.player.as_mut() else {
        return;
    };
    match player.sink.send(&next.command()).await {
        // 送信前に発行されたポーリングが古い値を返しても、ここで送った値を保つ。
        Ok(()) => app.set_speed_sent(next, std::time::Instant::now()),
        Err(e) => app.set_error(Some(e)),
    }
}

/// 再生中の URL をクリップボードへ渡す。
/// 書き手を差し替えられる形。テストはここに偽物を渡して pbcopy を起動させない。
/// 成否どちらも期限つきで出す。素の app.error だと次のポーリングの成功が消してしまう。
pub async fn copy_url_with<C: Clipboard>(app: &mut App, clipboard: C, now: std::time::Instant) {
    let url = app.playback.url.clone();
    if url.is_empty() {
        return;
    }
    match clipboard.copy(url).await {
        Ok(()) => app.set_temporary_notice(COPIED_NOTICE.to_string(), now),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            app.set_temporary_error(MISSING_PBCOPY.to_string(), now)
        }
        Err(e) => app.set_temporary_error(format!("URL をコピーできませんでした: {e}"), now),
    }
}

/// 埋め込み → テキスト → 別ウィンドウ → 埋め込み。
/// mpv は再起動せず VO を差し替えるので再生は途切れない。
pub async fn cycle_display_mode(app: &mut App, session: &mut Session) {
    let from = app.display;
    let to = from.next();
    // 別ウィンドウ中の端末リサイズは mpv へ送っていないので、端末内へ戻るときに現寸法で送り直す。
    let geometry = video_geometry(app.settings.display.max_pixels());
    let commands = display::switch_commands(
        from,
        to,
        geometry,
        app.settings.fps_cap,
        &app.settings.window,
    );
    let Some(player) = session.player.as_mut() else {
        return;
    };
    match player.send_all(&commands).await {
        Ok(()) => {
            // 送れてからデコーダを替える。失敗したときに app.display と食い違わせない。
            // 新しい VO の先頭バイトが旧デコーダへ入ることはあるが、reset が捨てる。
            if let (Some(kind), Some(video)) = (to.decoder_kind(), &app.video) {
                video.reset(kind, geometry);
            }
            app.display = to;
            // kitty VO の後始末が stdout に出ない経路でも画像を残さない。
            if from == DisplayMode::Embedded {
                session.owe_clear = true;
            }
        }
        Err(e) => app.set_error(Some(e)),
    }
}

/// 連続して届く Resize は、最後の1つだけを少し置いてから反映する。
pub fn schedule_resize(session: &mut Session, width: u16, height: u16) {
    session.pending_resize = Some((width, height));
    session.resize_at = Some(Instant::now() + RESIZE_DEBOUNCE);
}

/// 端末サイズが変わったら、映像の寸法を mpv ごと作り直して食い違いを解消する。
pub async fn apply_resize(app: &mut App, session: &mut Session, tx: &UnboundedSender<AppEvent>) {
    apply_resize_with(app, session, tx, RealCurl).await;
}

/// サムネイルの取得者を差し替えられる形。テストはここに偽物を渡して curl を起動させない。
pub async fn apply_resize_with<F>(
    app: &mut App,
    session: &mut Session,
    tx: &UnboundedSender<AppEvent>,
    fetcher: F,
) where
    F: Fetcher + Send + Sync + 'static,
{
    let Some((cols, rows)) = session.pending_resize.take() else {
        return;
    };
    let Some(video) = app.video.clone() else {
        // 非再生中。ratatui の 2J で画像が消えているので、新しい寸法で貼り直す。
        // 列数・行数が変わると選択が可視範囲の外へ出るので、新しい割り付けで追い直す。
        if let Some(layout) = ui::grid_layout_in(app, Rect::new(0, 0, cols, rows), cell_size()) {
            app.scroll =
                grid::ensure_visible(app.selected, layout.columns, layout.rows, app.scroll);
        }
        app.thumbs.mark_dirty();
        start_thumbnails_with(app, tx, session, fetcher);
        return;
    };
    let geometry = geometry_for(cols, rows, cell_size(), app.settings.display.max_pixels());
    if video.geometry() == geometry {
        return;
    }
    video.resize(geometry);
    let Some(p) = session.player.as_mut() else {
        return;
    };
    let sent = match app.display {
        DisplayMode::Embedded => p.send_all(&mpv::resize_video(geometry)).await,
        DisplayMode::Text => p.send_all(&mpv::resize_text_video(geometry)).await,
        // 別ウィンドウ中は描き直すものが無い。戻るときに現寸法を送る。
        DisplayMode::Window => return,
    };
    if let Err(e) = sent {
        app.set_error(Some(e));
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
    use crate::clipboard::fixtures::{CopyResult, FakeClipboard};
    use crate::cookies::{CookieSource, CookieState};
    use crate::display::Quality;
    use crate::fetch::fixtures::{CurlResult, FakeCurl};
    use crate::rgb::RgbImage;
    use crate::search::SearchResult;
    use crate::settings::{DisplaySettings, Settings};
    use crate::speed::Speed;
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

    /// 送り先を気にしないリサイズ。サムネイル取得の要求だけが捨てられる。
    async fn resize(app: &mut App, session: &mut Session) {
        let (tx, _rx) = mpsc::unbounded_channel();
        apply_resize_with(app, session, &tx, FakeCurl::new(CurlResult::Failed, b"")).await;
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
                source: CookieSource::from_spec(Some("chrome")).expect("spec"),
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
        resize(&mut app, &mut session).await;

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
        resize(&mut app, &mut session).await;

        assert_eq!(
            video.geometry(),
            geometry_for(100, 40, cell_size(), MAX_FRAME_PIXELS)
        );
        assert!(session.pending_resize.is_none());
    }

    #[tokio::test]
    async fn cycling_without_a_player_changes_nothing() {
        let video = sink();
        let mut app = App {
            mode: Mode::Playing,
            video: Some(video.clone()),
            ..App::default()
        };
        let mut session = Session::default();
        cycle_display_mode(&mut app, &mut session).await;

        assert_eq!(app.display, DisplayMode::Embedded);
        assert_eq!(video.kind(), DecoderKind::Kitty);
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
        resize(&mut app, &mut session).await;

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
        resize(&mut app, &mut session).await;

        let frame = video.geometry().frame_px;
        let pixels = u64::from(frame.0) * u64::from(frame.1);
        assert!(
            pixels <= u64::from(Quality::Low.max_pixels()),
            "{pixels} px"
        );
    }

    #[tokio::test]
    async fn cycle_from_embedded_to_text_resets_the_sink_and_owes_a_clear() {
        let video = sink();
        let mut app = App {
            video: Some(video.clone()),
            ..playing_app()
        };
        let mut session = Session::default();
        let sent = record(&mut session, Ok(()));
        cycle_display_mode(&mut app, &mut session).await;

        let geometry = video_geometry(app.settings.display.max_pixels());
        assert_eq!(
            lines(&sent),
            display::switch_commands(
                DisplayMode::Embedded,
                DisplayMode::Text,
                geometry,
                app.settings.fps_cap,
                &app.settings.window,
            )
            .iter()
            .map(|c| c.to_line())
            .collect::<Vec<_>>()
        );
        assert_eq!(app.display, DisplayMode::Text);
        assert_eq!(video.kind(), DecoderKind::Text);
        // 端末に残った kitty の画像は tuitube が消す。
        assert!(session.owe_clear);
        assert!(app.error.is_none());
    }

    #[tokio::test]
    async fn cycle_from_text_to_the_window_removes_the_cap_and_owes_no_clear() {
        let video = sink();
        video.reset(DecoderKind::Text, video.geometry());
        let mut app = App {
            display: DisplayMode::Text,
            video: Some(video.clone()),
            ..playing_app()
        };
        let mut session = Session::default();
        let sent = record(&mut session, Ok(()));
        cycle_display_mode(&mut app, &mut session).await;

        assert_eq!(
            lines(&sent),
            [
                "{\"command\":[\"vf\",\"remove\",\"@tuitube-cap\"]}\n",
                "{\"command\":[\"set_property\",\"vo\",\"\"]}\n",
            ]
        );
        assert_eq!(app.display, DisplayMode::Window);
        // 別ウィンドウでは映像が stdout に来ないので、デコーダは触らない。
        assert_eq!(video.kind(), DecoderKind::Text);
        // 文字ブロックは ratatui が上書きするので消す画像は無い。
        assert!(!session.owe_clear);
    }

    #[tokio::test]
    async fn cycle_from_the_window_to_embedded_matches_the_current_behaviour() {
        let video = sink();
        let mut app = App {
            display: DisplayMode::Window,
            video: Some(video.clone()),
            ..playing_app()
        };
        let mut session = Session::default();
        let sent = record(&mut session, Ok(()));
        cycle_display_mode(&mut app, &mut session).await;

        let expected = video_geometry(app.settings.display.max_pixels());
        assert_eq!(video.geometry(), expected);
        assert_eq!(
            lines(&sent),
            display::switch_commands(
                DisplayMode::Window,
                DisplayMode::Embedded,
                expected,
                app.settings.fps_cap,
                &app.settings.window,
            )
            .iter()
            .map(|c| c.to_line())
            .collect::<Vec<_>>()
        );
        assert_eq!(app.display, DisplayMode::Embedded);
        assert_eq!(video.kind(), DecoderKind::Kitty);
        // 埋め込みへ戻すときは kitty VO 自身が後始末を出す。
        assert!(!session.owe_clear);
    }

    #[tokio::test]
    async fn cycle_keeps_the_mode_when_sending_fails() {
        let mut app = playing_app();
        let mut session = Session::default();
        let _sent = record(&mut session, Err("パイプが閉じました".to_string()));
        cycle_display_mode(&mut app, &mut session).await;

        assert_eq!(app.display, DisplayMode::Embedded);
        assert_eq!(app.error.as_deref(), Some("パイプが閉じました"));
        assert!(!session.owe_clear);
    }

    #[tokio::test]
    async fn cycle_keeps_the_decoder_when_sending_fails() {
        // デコーダだけ先に替えると、表示は埋め込みのままなのに映像が来なくなる。
        let video = sink();
        let mut app = App {
            video: Some(video.clone()),
            ..playing_app()
        };
        let mut session = Session::default();
        let _sent = record(&mut session, Err("パイプが閉じました".to_string()));
        cycle_display_mode(&mut app, &mut session).await;

        assert_eq!(app.display, DisplayMode::Embedded);
        assert_eq!(video.kind(), DecoderKind::Kitty);
        assert_eq!(app.error.as_deref(), Some("パイプが閉じました"));
    }

    #[tokio::test]
    async fn resize_in_text_mode_sends_the_tct_resize_sequence() {
        let mut app = App {
            display: DisplayMode::Text,
            video: Some(sink()),
            ..playing_app()
        };
        let mut session = Session {
            pending_resize: Some((100, 40)),
            ..Session::default()
        };
        let sent = record(&mut session, Ok(()));
        resize(&mut app, &mut session).await;

        let geometry = geometry_for(100, 40, cell_size(), MAX_FRAME_PIXELS);
        assert_eq!(
            lines(&sent),
            mpv::resize_text_video(geometry)
                .iter()
                .map(|c| c.to_line())
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn start_playback_picks_the_decoder_from_the_display_mode() {
        for (display, kind) in [
            (DisplayMode::Embedded, DecoderKind::Kitty),
            (DisplayMode::Text, DecoderKind::Text),
            // 別ウィンドウから端末内へ戻れるよう kitty で用意しておく。
            (DisplayMode::Window, DecoderKind::Kitty),
        ] {
            let app = App {
                display,
                ..App::default()
            };
            let (video, plan) = playback_plan(&app);
            assert_eq!(video.kind(), kind, "{display:?}");
            assert_eq!(plan.mode, display);
        }
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
        resize(&mut app, &mut session).await;

        assert!(lines(&sent).is_empty());
    }

    fn faster() -> Speed {
        Speed::from_tenths(15).expect("1.5x")
    }

    #[tokio::test]
    async fn change_speed_sends_set_property_and_updates_the_app() {
        let mut app = playing_app();
        let mut session = Session::default();
        let sent = record(&mut session, Ok(()));
        change_speed(&mut app, &mut session, 1).await;

        assert_eq!(
            lines(&sent),
            ["{\"command\":[\"set_property\",\"speed\",1.1]}\n"]
        );
        assert_eq!(app.speed, Speed::from_tenths(11).expect("1.1x"));
        assert!(app.error.is_none());
    }

    #[tokio::test]
    async fn change_speed_at_the_bound_sends_nothing() {
        let mut app = App {
            speed: Speed::MAX,
            ..playing_app()
        };
        let mut session = Session::default();
        let sent = record(&mut session, Ok(()));
        change_speed(&mut app, &mut session, 1).await;

        assert!(lines(&sent).is_empty());
        assert_eq!(app.speed, Speed::MAX);

        let mut app = App {
            speed: Speed::MIN,
            ..playing_app()
        };
        change_speed(&mut app, &mut session, -1).await;
        assert!(lines(&sent).is_empty());
        assert_eq!(app.speed, Speed::MIN);
    }

    #[tokio::test]
    async fn reset_speed_returns_to_normal_and_is_idempotent() {
        let mut app = App {
            speed: faster(),
            ..playing_app()
        };
        let mut session = Session::default();
        let sent = record(&mut session, Ok(()));
        reset_speed(&mut app, &mut session).await;

        assert_eq!(
            lines(&sent),
            ["{\"command\":[\"set_property\",\"speed\",1.0]}\n"]
        );
        assert_eq!(app.speed, Speed::NORMAL);

        // 既に等速なら送らない。
        reset_speed(&mut app, &mut session).await;
        assert_eq!(lines(&sent).len(), 1);
    }

    #[tokio::test]
    async fn change_speed_keeps_the_value_when_sending_fails() {
        let mut app = playing_app();
        let mut session = Session::default();
        let _sent = record(&mut session, Err("パイプが閉じました".to_string()));
        change_speed(&mut app, &mut session, 1).await;

        assert_eq!(app.speed, Speed::NORMAL);
        assert_eq!(app.error.as_deref(), Some("パイプが閉じました"));
    }

    #[tokio::test]
    async fn change_speed_without_a_player_changes_nothing() {
        let mut app = playing_app();
        let mut session = Session::default();
        change_speed(&mut app, &mut session, 1).await;

        assert_eq!(app.speed, Speed::NORMAL);
        assert!(app.speed_sent_at.is_none());
        assert!(app.error.is_none());
    }

    #[tokio::test]
    async fn a_poll_that_arrives_after_a_key_press_does_not_roll_the_speed_back() {
        // ticker が 1.0 を問い合わせた直後に ] を押すと、応答は 1.0 のまま届く。
        let mut app = playing_app();
        let mut session = Session::default();
        let sent = record(&mut session, Ok(()));
        change_speed(&mut app, &mut session, 1).await;
        assert_eq!(app.speed, Speed::from_tenths(11).expect("1.1x"));

        apply_property(
            &mut app,
            &mut session,
            mpv::REQ_SPEED,
            Some(serde_json::json!(1.0)),
        )
        .await;

        // 巻き戻さない。巻き戻すと次の ] が 1.1 を再送するだけになり、押下が 1 回消える。
        assert_eq!(app.speed, Speed::from_tenths(11).expect("1.1x"));
        change_speed(&mut app, &mut session, 1).await;
        assert_eq!(app.speed, Speed::from_tenths(12).expect("1.2x"));
        assert_eq!(
            lines(&sent),
            [
                "{\"command\":[\"set_property\",\"speed\",1.1]}\n",
                "{\"command\":[\"set_property\",\"speed\",1.2]}\n",
            ]
        );
    }

    #[tokio::test]
    async fn a_speed_outside_the_range_is_sent_back_to_mpv() {
        // mpv ウィンドウ側の ] で 8.0 まで上がった状態。表示だけ 4.0x にすると実再生と食い違う。
        let mut app = playing_app();
        let mut session = Session::default();
        let sent = record(&mut session, Ok(()));
        apply_property(
            &mut app,
            &mut session,
            mpv::REQ_SPEED,
            Some(serde_json::json!(8.0)),
        )
        .await;

        assert_eq!(app.speed, Speed::MAX);
        assert_eq!(
            lines(&sent),
            ["{\"command\":[\"set_property\",\"speed\",4.0]}\n"]
        );
    }

    #[tokio::test]
    async fn a_speed_inside_the_range_is_taken_as_is_without_sending_anything() {
        let mut app = playing_app();
        let mut session = Session::default();
        let sent = record(&mut session, Ok(()));
        apply_property(
            &mut app,
            &mut session,
            mpv::REQ_SPEED,
            Some(serde_json::json!(2.75)),
        )
        .await;

        assert_eq!(app.speed, Speed::from_tenths(28).expect("2.8x"));
        assert!(lines(&sent).is_empty());
    }

    #[test]
    fn start_playback_passes_the_current_speed_to_the_plan() {
        let app = App {
            speed: faster(),
            ..playing_app()
        };
        let (_video, plan) = playback_plan(&app);
        assert_eq!(plan.speed, faster());
        assert!(plan.args().contains(&"--speed=1.5".to_string()));

        // 既定は等速で、起動引数に出さない。
        let (_video, plan) = playback_plan(&App::default());
        assert_eq!(plan.speed, Speed::NORMAL);
        assert!(!plan.args().iter().any(|a| a.starts_with("--speed")));
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
        resize(&mut app, &mut session).await;

        let geometry = geometry_for(100, 40, cell_size(), MAX_FRAME_PIXELS);
        assert_eq!(
            lines(&sent),
            mpv::resize_video(geometry)
                .iter()
                .map(|c| c.to_line())
                .collect::<Vec<_>>()
        );
    }

    const TINY_8X4: &[u8] = include_bytes!("testdata/tiny8x4.jpg");

    /// 80x24 の検索画面。格子は 4 列 2 行になる。
    fn grid_app(count: usize) -> App {
        let mut app = App {
            mode: Mode::Results,
            screen: Rect::new(0, 0, 80, 24),
            ..App::default()
        };
        let results: Vec<SearchResult> = (0..count).map(|i| result(&format!("id{i}"))).collect();
        app.set_results(results, &Target::Search("q".to_string()));
        app.thumbs.take_dirty();
        app
    }

    fn thumb_dir(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("tuitube-actions-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    /// ThumbsReady を 1 件受け取る。届かなければ None。
    async fn next_thumbs(
        rx: &mut mpsc::UnboundedReceiver<AppEvent>,
        session: &mut Session,
    ) -> Option<AppEvent> {
        let task = session.thumbs_task.take()?;
        task.await.expect("タスクは panic しない");
        rx.try_recv().ok()
    }

    fn thumbs_ids(event: &AppEvent) -> Vec<String> {
        let AppEvent::ThumbsReady { images, .. } = event else {
            panic!("ThumbsReady のはず");
        };
        images.iter().map(|(id, _)| id.clone()).collect()
    }

    #[tokio::test]
    async fn switch_tab_searches_only_a_tab_that_has_not_loaded_yet() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = App {
            query: "ラーメン".to_string(),
            ..App::default()
        };
        switch_tab_with(&mut app, &tx, &mut session, true, StubYtDlp);

        assert_eq!(app.tabs.selected(), 1);
        assert!(session.search_task.is_some(), "未読のタブは検索する");
        assert!(app.searching);

        // 読み込み済みにして戻り、また来ても検索し直さない。
        app.set_results(vec![result("a")], &Target::Search("音楽".to_string()));
        session.search_task = None;
        app.searching = false;
        switch_tab_with(&mut app, &tx, &mut session, false, StubYtDlp);
        assert!(app.tabs.is_all());
        // 「すべて」タブは未読なので検索が積まれる。それを片付けてから戻る。
        session.search_task = None;
        switch_tab_with(&mut app, &tx, &mut session, true, StubYtDlp);
        assert!(
            session.search_task.is_none(),
            "保持していた結果をそのまま出す"
        );
        assert_eq!(app.results.len(), 1);
    }

    #[tokio::test]
    async fn switching_to_a_loaded_tab_drops_the_running_search() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        // 「すべて」タブに結果を持たせてから、隣のタブで検索を走らせる。
        let mut app = grid_app(1);
        switch_tab_with(&mut app, &tx, &mut session, true, StubYtDlp);
        let running = session.search_nonce;
        assert!(session.search_task.is_some());
        assert!(app.searching);

        switch_tab_with(&mut app, &tx, &mut session, false, StubYtDlp);
        assert!(app.tabs.is_all());
        assert!(session.search_task.is_none(), "走らせたままにしない");
        assert!(!app.searching, "検索中の表示が残らない");
        assert_ne!(
            session.search_nonce, running,
            "先に走っていた検索の結果を移った先のタブへ書き込ませない"
        );
        assert_eq!(app.result_ids(), ["id0"], "保持していた結果のまま");
    }

    #[tokio::test]
    async fn a_reload_that_cannot_search_still_drops_the_running_one() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = grid_app(1);
        app.query = "ラーメン".to_string();
        reload_tab_with(&mut app, &tx, &mut session, StubYtDlp);
        let running = session.search_nonce;
        assert!(session.search_task.is_some());

        // 検索ボックスが空の「すべて」タブは検索を作れない。それでも先行分は止める。
        app.query.clear();
        reload_tab_with(&mut app, &tx, &mut session, StubYtDlp);
        assert!(session.search_task.is_none());
        assert!(!app.searching);
        assert_ne!(session.search_nonce, running);
    }

    const URL: &str = "https://www.youtube.com/watch?v=id0";

    fn playing_url_app() -> App {
        App {
            playback: Playback {
                url: URL.to_string(),
                ..Playback::default()
            },
            ..playing_app()
        }
    }

    #[tokio::test]
    async fn copy_url_hands_the_playing_url_to_the_clipboard() {
        let mut app = playing_url_app();
        let clipboard = FakeClipboard::new(CopyResult::Ok);
        copy_url_with(&mut app, clipboard.clone(), std::time::Instant::now()).await;

        assert_eq!(clipboard.copied(), [URL]);
        assert_eq!(app.notice.as_deref(), Some(COPIED_NOTICE));
        assert!(app.notice_until.is_some(), "しばらくしたら消える知らせ");
        assert!(app.error.is_none());
    }

    #[tokio::test]
    async fn copy_url_reports_a_missing_pbcopy() {
        let mut app = playing_url_app();
        copy_url_with(
            &mut app,
            FakeClipboard::new(CopyResult::Missing),
            std::time::Instant::now(),
        )
        .await;

        assert_eq!(app.error.as_deref(), Some(crate::clipboard::MISSING_PBCOPY));
        assert!(app.error_until.is_some(), "しばらくしたら消えるエラー");
        assert!(app.notice.is_none(), "失敗をコピー成功と見せない");
    }

    #[tokio::test]
    async fn copy_url_reports_a_failed_copy() {
        let mut app = playing_url_app();
        copy_url_with(
            &mut app,
            FakeClipboard::new(CopyResult::Failed),
            std::time::Instant::now(),
        )
        .await;

        let error = app.error.expect("理由を出す");
        assert!(error.contains("コピーできませんでした"), "{error}");
        assert!(app.notice.is_none());
    }

    #[tokio::test]
    async fn copy_url_without_a_url_starts_nothing() {
        let mut app = playing_app();
        let clipboard = FakeClipboard::new(CopyResult::Ok);
        copy_url_with(&mut app, clipboard.clone(), std::time::Instant::now()).await;

        assert!(clipboard.copied().is_empty());
        assert!(app.notice.is_none());
        assert!(app.error.is_none());
    }

    #[tokio::test]
    async fn the_tick_drops_a_temporary_notice_once_its_time_is_up() {
        let t0 = std::time::Instant::now();
        let mut app = playing_url_app();
        let mut session = Session::default();
        app.set_temporary_notice(COPIED_NOTICE.to_string(), t0);

        on_tick(&mut app, &mut session, t0 + crate::app::NOTICE_TTL / 2).await;
        assert!(app.notice.is_some(), "期限前は消さない");

        on_tick(&mut app, &mut session, t0 + crate::app::NOTICE_TTL).await;
        assert!(app.notice.is_none());
    }

    #[tokio::test]
    async fn a_copy_failure_survives_the_polling_that_follows() {
        let t0 = std::time::Instant::now();
        let mut app = playing_url_app();
        let mut session = Session::default();
        // ポーリングは成功する。何もしなければ 1 秒後のティックがエラーを消してしまう。
        record(&mut session, Ok(()));
        copy_url_with(&mut app, FakeClipboard::new(CopyResult::Missing), t0).await;

        on_tick(&mut app, &mut session, t0 + crate::app::NOTICE_TTL / 2).await;
        assert_eq!(
            app.error.as_deref(),
            Some(crate::clipboard::MISSING_PBCOPY),
            "読む前に消さない"
        );

        on_tick(&mut app, &mut session, t0 + crate::app::NOTICE_TTL).await;
        assert!(app.error.is_none(), "期限が来たら消す");
        assert!(app.error_until.is_none());
    }

    #[tokio::test]
    async fn polling_still_clears_an_error_of_its_own() {
        let t0 = std::time::Instant::now();
        let mut app = playing_url_app();
        let mut session = Session::default();
        record(&mut session, Err("パイプが閉じました".to_string()));
        poll_player(&mut app, &mut session, t0).await;
        assert_eq!(app.error.as_deref(), Some("パイプが閉じました"));

        session.player = None;
        record(&mut session, Ok(()));
        poll_player(&mut app, &mut session, t0).await;
        assert!(app.error.is_none());
    }

    #[test]
    fn entering_playback_keeps_the_url_of_the_video() {
        let mut app = grid_app(4);
        let mut session = Session::default();
        enter_playback(
            &mut app,
            &mut session,
            "song".to_string(),
            URL.to_string(),
            sink(),
        );
        assert_eq!(app.playback.url, URL);
    }

    #[test]
    fn entering_playback_owes_a_clear_for_the_thumbnails() {
        let mut app = grid_app(4);
        let mut session = Session::default();
        enter_playback(
            &mut app,
            &mut session,
            "song".to_string(),
            URL.to_string(),
            sink(),
        );

        assert_eq!(app.mode, Mode::Playing);
        assert!(app.video.is_some());
        assert_eq!(app.playback.title, "song");
        // 貼ってあるサムネイルは 2J でしか消えない。終了側と同じく持ち越して消す。
        assert!(session.owe_clear, "再生画面の上にサムネイルを残さない");
    }

    #[tokio::test]
    async fn switching_tabs_carries_the_selection_of_each_tab() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = grid_app(10);
        app.selected = 6;
        app.scroll = 4;

        switch_tab_with(&mut app, &tx, &mut session, true, StubYtDlp);
        assert_eq!(app.selected, 0);
        assert!(app.results.is_empty());

        switch_tab_with(&mut app, &tx, &mut session, false, StubYtDlp);
        assert_eq!(app.selected, 6);
        assert_eq!(app.scroll, 4);
        assert_eq!(app.results.len(), 10);
    }

    #[tokio::test]
    async fn reload_tab_searches_the_current_tab_again() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = grid_app(3);
        assert!(app.tabs.state().loaded);
        // query が空だと「すべて」タブは検索できないので、語を入れておく。
        app.query = "ラーメン".to_string();

        reload_tab_with(&mut app, &tx, &mut session, StubYtDlp);
        assert!(session.search_task.is_some());
        assert!(!app.tabs.state().loaded);
    }

    #[tokio::test]
    async fn a_search_from_the_input_box_returns_to_the_all_tab() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = App {
            query: "ラーメン".to_string(),
            ..App::default()
        };
        app.tabs.next();
        assert!(!app.tabs.is_all());

        start_search_with(&mut app, &tx, &mut session, StubYtDlp);
        assert!(app.tabs.is_all(), "ボックスとタブの対応を 1 対 1 に保つ");
        assert!(session.search_task.is_some());
    }

    #[tokio::test]
    async fn a_finished_search_starts_a_thumbnail_fetch_for_its_ids() {
        let dir = thumb_dir("fetch");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = grid_app(3);
        app.settings.thumbnails.cache_dir = Some(dir.clone());
        let curl = FakeCurl::new(CurlResult::Wrote, TINY_8X4);

        start_thumbnails_with(&mut app, &tx, &mut session, curl);
        assert!(app.thumbs.is_fetching());
        let event = next_thumbs(&mut rx, &mut session).await.expect("届く");
        assert_eq!(thumbs_ids(&event), ["id0", "id1", "id2"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn thumbnails_are_not_fetched_when_the_setting_is_off() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = grid_app(3);
        app.settings.thumbnails.enabled = false;
        start_thumbnails_with(
            &mut app,
            &tx,
            &mut session,
            FakeCurl::new(CurlResult::Wrote, b""),
        );

        assert!(session.thumbs_task.is_none());
        assert!(!app.thumbs.is_fetching());
    }

    #[tokio::test]
    async fn thumbnails_are_not_fetched_once_disabled_or_in_list_mode() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();

        let mut app = grid_app(3);
        app.thumbs.disable(crate::thumbs::MISSING_CURL.to_string());
        start_thumbnails_with(
            &mut app,
            &tx,
            &mut session,
            FakeCurl::new(CurlResult::Wrote, b""),
        );
        assert!(session.thumbs_task.is_none());

        let mut app = grid_app(3);
        app.settings.search.layout = crate::grid::LayoutMode::List;
        start_thumbnails_with(
            &mut app,
            &tx,
            &mut session,
            FakeCurl::new(CurlResult::Wrote, b""),
        );
        assert!(
            session.thumbs_task.is_none(),
            "リスト表示では画像を使わない"
        );
    }

    #[tokio::test]
    async fn a_resize_while_not_playing_repaints_and_refetches_from_the_cache() {
        let dir = thumb_dir("resize");
        std::fs::write(dir.join("id0.jpg"), TINY_8X4).expect("書ける");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut app = grid_app(1);
        app.settings.thumbnails.cache_dir = Some(dir.clone());
        let mut session = Session {
            pending_resize: Some((100, 40)),
            ..Session::default()
        };
        // キャッシュにあるので、落ちる偽 curl を渡しても読めている。
        let curl = FakeCurl::new(CurlResult::Failed, b"");
        apply_resize_with(&mut app, &mut session, &tx, curl).await;

        // 端末リサイズでは ratatui の 2J で画像が消えるので貼り直しを予約する。
        assert!(app.thumbs.take_dirty());
        assert!(session.pending_resize.is_none());
        let event = next_thumbs(&mut rx, &mut session).await.expect("届く");
        assert_eq!(thumbs_ids(&event), ["id0"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_resize_downloads_through_the_given_fetcher() {
        let dir = thumb_dir("resize-fetcher");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut app = grid_app(1);
        app.settings.thumbnails.cache_dir = Some(dir.clone());
        let mut session = Session {
            pending_resize: Some((100, 40)),
            ..Session::default()
        };
        // キャッシュが空なので取りに行く。渡した偽物が使われる限り curl は起動しない。
        let curl = FakeCurl::new(CurlResult::Wrote, TINY_8X4);
        apply_resize_with(&mut app, &mut session, &tx, curl).await;

        let event = next_thumbs(&mut rx, &mut session).await.expect("届く");
        let AppEvent::ThumbsReady { images, .. } = &event else {
            panic!("ThumbsReady のはず");
        };
        assert_eq!(images.len(), 1);
        assert!(images[0].1.is_ok(), "偽物が書いた画像を読めている");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_resize_follows_the_selection_into_the_new_grid() {
        let dir = thumb_dir("resize-follow");
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = grid_app(40);
        app.settings.thumbnails.cache_dir = Some(dir.clone());
        // 80x24 は 4 列 2 行。末尾を選んで最終ページを出している状態。
        app.selected = 39;
        app.scroll = 32;
        let mut session = Session {
            pending_resize: Some((128, 24)),
            ..Session::default()
        };
        let curl = FakeCurl::new(CurlResult::Failed, b"");
        apply_resize_with(&mut app, &mut session, &tx, curl).await;

        // 128x24 は 6 列 1 行。39 番は 30..36 の外へ出るので追い直す。
        assert_eq!(app.scroll, 36, "選択が可視範囲の外に取り残される");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn move_selection_walks_the_grid_and_scrolls_only_when_needed() {
        let mut app = grid_app(10);
        move_selection(&mut app, Dir::Right);
        assert_eq!(app.selected, 1);
        move_selection(&mut app, Dir::Down);
        assert_eq!(app.selected, 5);
        move_selection(&mut app, Dir::Up);
        assert_eq!(app.selected, 1);
        move_selection(&mut app, Dir::Left);
        assert_eq!(app.selected, 0);
        // 可視範囲の中で動くだけなら貼り直さない。
        assert_eq!(app.scroll, 0);
        assert!(!app.thumbs.take_dirty());
    }

    #[test]
    fn move_selection_falls_back_to_the_wrapping_list_move_without_a_grid() {
        let mut app = App {
            mode: Mode::Results,
            results: vec![result("a"), result("b")],
            // 画面寸法が無いので格子は組めない。
            ..App::default()
        };
        move_selection(&mut app, Dir::Down);
        assert_eq!(app.selected, 1);
        move_selection(&mut app, Dir::Down);
        assert_eq!(app.selected, 0, "リストでは巻き戻る");
        move_selection(&mut app, Dir::Right);
        assert_eq!(app.selected, 0, "←→ はリストでは効かない");
    }

    #[test]
    fn end_playback_asks_for_a_thumbnail_repaint() {
        // mpv の a=d でサムネイルも消えている。
        let mut app = grid_app(3);
        app.mode = Mode::Playing;
        let mut session = Session::default();
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime")
            .block_on(end_playback(&mut app, &mut session, None));
        assert!(app.thumbs.take_dirty());
    }

    #[test]
    fn a_ready_thumbnail_is_not_fetched_again() {
        let mut app = grid_app(2);
        let image = RgbImage::new(2, 2, vec![0; 12]).expect("長さは合っている");
        app.thumbs
            .apply(vec![("id0".to_string(), Ok(image))], (144, 80));
        assert_eq!(app.thumbs.wanted(&app.result_ids(), (144, 80)), ["id1"]);
    }
}
