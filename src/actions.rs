//! Session を動かすアクション。キー入力もイベント処理もここを通して player を触る。

use crate::app::{App, AppEvent, ChannelView, DownloadField, Mode, Playback, SETTINGS_ITEMS};
use crate::clipboard::{Clipboard, MISSING_PBCOPY};
use crate::comments;
use crate::cookies::Target;
use crate::display::{self, DisplayMode, LaunchPlan};
use crate::download;
use crate::fetch::{Fetcher, RealCurl};
use crate::geometry::{cell_size, geometry_for, video_geometry};
use crate::grid::{self, Dir};
use crate::mpv::{self, MpvCommand, MpvController};
use crate::oauth;
use crate::query::QueryEditor;
use crate::search::{self, RealYtDlp, SearchResult, YtDlp};
use crate::seekbar::{SeekBarState, clamp_target};
use crate::settings;
use crate::settings::MAX_SEARCH_LIMIT;
use crate::speed::Speed;
use crate::subtitles::{self, SubtitleLaunch, SubtitleStatus};
use crate::thumbs;
use crate::ui;
use crate::video::{CellSize, DecoderKind, Geometry, VideoSink};
use ratatui::layout::Rect;
use std::collections::HashMap;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;
use tokio::time::Instant;

/// ドラッグ中は Resize が連続して届くので、落ち着くまで mpv の作り直しを待つ。
const RESIZE_DEBOUNCE: Duration = Duration::from_millis(200);
/// 控えておく検索の数。既定のタブ 10 枚を cookie の有無ぶん持っても余る幅にしてある。
/// 1 件あたり最大 search.limit 件を抱えるので、使い捨ての検索語で青天井にはしない。
const CACHE_CAPACITY: usize = 64;
/// ←→ 1 回あたりのシーク幅。
pub const SEEK_STEP_SECS: f64 = 5.0;
/// コピーできたことを伝える文言。
pub const COPIED_NOTICE: &str = "URL をコピーしました";
/// チャンネル引きの待ち時間を伝える文言。
pub const CHANNEL_LOOKUP_NOTICE: &str = "チャンネル情報を取得中…";
/// 引いても channel_id が無かったときの文言。
pub const NO_CHANNEL_NOTICE: &str = "このチャンネルへは移動できません";
/// 非表示にしたことを伝える文言。
pub const HIDDEN_VIDEO_NOTICE: &str = "この動画を非表示にしました";
pub const HIDDEN_CHANNEL_NOTICE: &str = "このチャンネルを非表示にしました";
/// 設定ファイルの置き場を決められないときの理由。$HOME も $XDG_CONFIG_HOME も無い環境。
pub const NO_CONFIG_PATH: &str = "設定ファイルの置き場が分かりません ($HOME を設定してください)";

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
    /// 再生中の動画のコメント取得。
    pub comments_task: Option<JoinHandle<()>>,
    /// 再生ごとに進む世代。前の動画のコメントが遅れて届いても混ざらない。
    pub comments_nonce: u64,
    /// 選択中の 1 本から channel_id を引く取得。
    pub channel_lookup_task: Option<JoinHandle<()>>,
    /// 打ち切りごとに進む世代。前の行ぶんが遅れて届いても混ざらない。
    pub channel_lookup_nonce: u64,
    /// チャンネル登録・いいねの認証と送信。
    pub oauth_task: Option<JoinHandle<()>>,
    /// 打ち切りごとに進む世代。前の操作ぶんが遅れて届いても混ざらない。
    pub oauth_nonce: u64,
    /// 送信中の操作。成功したら控えへ写すので、送った内容をここへ置いておく。
    pub oauth_action: Option<oauth::Action>,
    /// いいね済み/登録済みの問い合わせ。nonce は search_nonce を共用する。
    pub engagement_task: Option<JoinHandle<()>>,
    /// 動画/音声ファイルのダウンロード。同時に走らせるのは1件まで。
    pub download_task: Option<JoinHandle<()>>,
    /// 打ち切りごとに進む世代。前のダウンロードぶんが遅れて届いても混ざらない。
    pub download_nonce: u64,
    /// 直近の端末サイズと、それを映像へ反映する時刻。
    pub pending_resize: Option<(u16, u16)>,
    pub resize_at: Option<Instant>,
    /// 再生が終わった後など、sink 越しに出せない画像削除の持ち越し。
    pub owe_clear: bool,
    /// 成功した検索の控え。ディスクへは残さないのでアプリを終えると消える。
    pub search_cache: HashMap<String, CacheEntry>,
    /// 走らせた検索の鍵。結果を反映すると cookie の状態が動くので、
    /// 取りに行った時点の鍵をここへ置いておく。
    pub pending_cache_key: Option<String>,
}

/// 検索 1 回ぶんの控え。
pub struct CacheEntry {
    pub results: Vec<SearchResult>,
    pub fetched_at: Instant,
}

/// 同じ検索を指す鍵。cookie の有無で結果が変わるので、使ったかどうかも混ぜる。
pub fn cache_key(target: &Target, limit: usize, used_cookies: bool) -> String {
    format!("{target:?}|{limit}|{used_cookies}")
}

/// 期限内の控えだけを返す。
fn cached_results(session: &Session, key: &str, ttl: Duration) -> Option<Vec<SearchResult>> {
    let entry = session.search_cache.get(key)?;
    (entry.fetched_at.elapsed() < ttl).then(|| entry.results.clone())
}

/// 成功した検索を控える。失敗・控えから返した分・設定が off の間は鍵が無いので何もしない。
pub fn remember_search(session: &mut Session, results: &[SearchResult], ttl: Duration) {
    let Some(key) = session.pending_cache_key.take() else {
        return;
    };
    // 上書きぶんは枠を取り合わない。
    session.search_cache.remove(&key);
    // 期限切れは読まれないまま残るので、書くついでに落とす。
    session
        .search_cache
        .retain(|_, entry| entry.fetched_at.elapsed() < ttl);
    // 検索語ごとに鍵が増えるので、それでも溢れるぶんは取ったのが古い順に捨てる。
    while session.search_cache.len() >= CACHE_CAPACITY {
        let Some(oldest) = session
            .search_cache
            .iter()
            .min_by_key(|(_, entry)| entry.fetched_at)
            .map(|(key, _)| key.clone())
        else {
            break;
        };
        session.search_cache.remove(&oldest);
    }
    session.search_cache.insert(
        key,
        CacheEntry {
            results: results.to_vec(),
            fetched_at: Instant::now(),
        },
    );
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

/// 入れ替わる/消える直前の再生位置を記憶する。まだ何も再生していない
/// (id が空、または位置・長さが未知) ときは何もしない。書き込みの失敗は無視する
/// (視聴の続きを覚えられるだけの機能で、失敗を利用者へ伝える強さは無い)。
pub fn remember_playback_position(app: &mut App) {
    if app.playback.id.is_empty() {
        return;
    }
    if let (Some(position), Some(duration)) = (app.playback.time_pos, app.playback.duration) {
        let _ = app.resume.remember(&app.playback.id, position, duration);
    }
}

pub async fn end_playback(app: &mut App, session: &mut Session, error: Option<String>) {
    stop_playback(session).await;
    cancel_comments(session);
    app.comments.end();
    remember_playback_position(app);
    app.playback = Playback::default();
    app.seek_bar = SeekBarState::default();
    app.video = None;
    // バックグラウンド中に自然終了・エラーで閉じても、退避状態を持ち越さない。
    app.background = false;
    // sink を手放した後も残骸は消す。SIGKILL 経路では mpv 自身が消せない。
    session.owe_clear = true;
    // mpv の a=d でサムネイルも消えているので、結果へ戻ったら貼り直す。
    app.thumbs.mark_dirty();
    if error.is_some() {
        app.set_error(error);
    }
    app.enter_search_mode(background_return_mode(app));
}

/// end_playback / enter_background が検索側のどのモードへ戻すかの共通基準。
fn background_return_mode(app: &App) -> Mode {
    if app.channel.is_some() {
        Mode::Channel
    } else if app.results.is_empty() {
        Mode::Input
    } else {
        Mode::Results
    }
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
    let query = app.query.text().trim().to_string();
    if query.is_empty() {
        return;
    }
    // 検索ボックスの文字列は「すべて」タブのもの。対応を 1 対 1 に保つ。
    if !app.tabs.is_all() {
        app.store_to_tab();
        app.tabs.select_all();
        app.sync_from_tab();
    }
    spawn_search(
        app,
        tx,
        session,
        Target::for_query(&query),
        app.settings.search.limit,
        runner,
    );
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
    let limit = app.settings.search.limit;
    start_tab_search_with_limit(app, tx, session, limit, runner);
}

/// 要求件数を指定してタブのクエリで検索する。`start_tab_search_with` の中身。
fn start_tab_search_with_limit<R>(
    app: &mut App,
    tx: &UnboundedSender<AppEvent>,
    session: &mut Session,
    limit: usize,
    runner: R,
) where
    R: YtDlp + Send + Sync + 'static,
{
    let Some(target) = app.tabs.target(app.query.text()) else {
        // 検索できないタブでも先行検索は打ち切る。残すと結果がこのタブへ流れ込む。
        cancel_search(app, session);
        return;
    };
    spawn_search(app, tx, session, target, limit, runner);
}

fn spawn_search<R>(
    app: &mut App,
    tx: &UnboundedSender<AppEvent>,
    session: &mut Session,
    target: Target,
    limit: usize,
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
        // モードは動かさない。結果が空の Mode::Input では Esc がアプリの終了になるので、
        // タブを送っただけの利用者がヘルプ通りに Esc を押すと落ちてしまう。
        app.set_error(Some(app.cookies.refusal(*feed)));
        return;
    }
    let nonce = session.search_nonce;
    let timeout = app.settings.search.timeout;
    let cookies = app.cookies.for_search().cloned();
    // off の間は鍵を作らない。読み出しも控えも鍵の有無で決まるので、溜まりもしない。
    let key = app
        .settings
        .search
        .cache_enabled
        .then(|| cache_key(&target, limit, cookies.is_some()));
    // r での取り直しは「今の中身を見たい」意思表示。読み込めるまで控えは出さない。
    if !app.view_state().reload
        && let Some(key) = &key
        && let Some(results) = cached_results(session, key, app.settings.search.cache_ttl)
    {
        // 取りに行っていないので cookie の状態は確かめない。
        app.set_error(None);
        app.set_results(results, &target);
        // AppEvent::SearchDone を経由しないこの経路でも、requested_limit の更新は
        // apply_search_done と同じ条件で行う。ここを飛ばすと、「もっと見る」で
        // 増やした後に同じ語を検索し直した際、件数の少ないキャッシュへ古い
        // requested_limit が残って can_load_more の判定がずれる。
        if matches!(target, Target::Search(_)) {
            app.view_state_mut().requested_limit = limit;
        }
        start_thumbnails(app, tx, session);
        return;
    }
    session.pending_cache_key = key;
    app.searching = true;
    app.set_error(None);
    let tx = tx.clone();
    session.search_task = Some(tokio::spawn(async move {
        let report = search::run_search(&runner, &target, cookies.as_ref(), limit, timeout).await;
        let _ = tx.send(AppEvent::SearchDone {
            nonce,
            target,
            report,
            requested_limit: limit,
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

pub fn open_channel(app: &mut App, tx: &UnboundedSender<AppEvent>, session: &mut Session) {
    open_channel_with(app, tx, session, RealYtDlp);
}

/// 選択中の結果のチャンネルへ移る。行が channel_id を持たないフィード系では、
/// その 1 本だけを取りに行ってから移る。
pub fn open_channel_with<R>(
    app: &mut App,
    tx: &UnboundedSender<AppEvent>,
    session: &mut Session,
    runner: R,
) where
    R: YtDlp + Send + Sync + 'static,
{
    let Some(result) = app.view_selected_result() else {
        return;
    };
    let video_id = result.id.clone();
    let url = result.url();
    let uploader = result.uploader.clone();
    match result.channel_id.clone() {
        Some(channel_id) => {
            let title = uploader.unwrap_or_else(|| channel_id.clone());
            enter_channel_with(app, tx, session, channel_id, title, runner);
        }
        None => start_channel_lookup_with(app, tx, session, video_id, url, runner),
    }
}

/// チャンネル一覧へ移って動画タブを取りに行く。直に移る経路と、
/// channel_id を引き終えてから移る経路の両方がここを通る。
pub fn enter_channel_with<R>(
    app: &mut App,
    tx: &UnboundedSender<AppEvent>,
    session: &mut Session,
    channel_id: String,
    title: String,
    runner: R,
) where
    R: YtDlp + Send + Sync + 'static,
{
    // 走らせたままにすると、その結果がチャンネルのタブへ書き込まれる。
    cancel_search(app, session);
    // 戻ったときに同じ位置から続けられるよう、検索側の選択を控える。
    app.store_to_tab();
    app.channel = Some(ChannelView::new(channel_id, title));
    app.mode = Mode::Channel;
    app.set_error(None);
    app.sync_from_view();
    start_channel_search_with(app, tx, session, runner);
}

/// channel_id を持たない行のために、その 1 本だけを非 flat で取りに行く。
/// 走っている検索は止めない (結果の行き先が変わらないため)。
fn start_channel_lookup_with<R>(
    app: &mut App,
    tx: &UnboundedSender<AppEvent>,
    session: &mut Session,
    video_id: String,
    url: String,
    runner: R,
) where
    R: YtDlp + Send + Sync + 'static,
{
    cancel_channel_lookup(session);
    let nonce = session.channel_lookup_nonce;
    // 引きは実測 3〜4 秒・上限 15 秒で、知らせの寿命 (3 秒) より長い。
    // 期限では消さず、結果が届いた時点で畳む。
    app.set_notice(Some(CHANNEL_LOOKUP_NOTICE.to_string()));
    let tx = tx.clone();
    session.channel_lookup_task = Some(tokio::spawn(async move {
        let result = search::fetch_channel(&runner, &url).await;
        let _ = tx.send(AppEvent::ChannelLookupDone {
            nonce,
            video_id,
            result,
        });
    }));
}

/// 先行のチャンネル引きを打ち切る。nonce を進めるので、送信済みの結果は捨てられる。
fn cancel_channel_lookup(session: &mut Session) {
    if let Some(task) = session.channel_lookup_task.take() {
        task.abort();
    }
    session.channel_lookup_nonce += 1;
}

/// 認証が要る操作の呼び先。テストは偽の Backend と一時ディレクトリを渡す。
pub struct Oauth<B> {
    pub backend: B,
    pub paths: Option<oauth::Paths>,
}

impl Oauth<oauth::RealBackend> {
    pub fn real() -> Self {
        Self {
            backend: oauth::RealBackend::default(),
            paths: oauth::Paths::from_env(),
        }
    }
}

/// 渡したチャンネルを登録する。
pub fn subscribe_to<B>(
    app: &mut App,
    tx: &UnboundedSender<AppEvent>,
    session: &mut Session,
    channel_id: String,
    deps: Oauth<B>,
) where
    B: oauth::Backend + 'static,
{
    start_oauth(app, tx, session, oauth::Action::Subscribe(channel_id), deps);
}

/// 表示中のチャンネルを登録する。チャンネルを見ていなければ何もしない。
pub fn subscribe_channel<B>(
    app: &mut App,
    tx: &UnboundedSender<AppEvent>,
    session: &mut Session,
    deps: Oauth<B>,
) where
    B: oauth::Backend + 'static,
{
    let Some(channel_id) = app.channel.as_ref().map(|c| c.channel_id.clone()) else {
        return;
    };
    subscribe_to(app, tx, session, channel_id, deps);
}

/// 再生中の動画のチャンネルを登録する。チャンネル ID を持たない行から始めた再生では何もしない。
pub fn subscribe_playing_channel<B>(
    app: &mut App,
    tx: &UnboundedSender<AppEvent>,
    session: &mut Session,
    deps: Oauth<B>,
) where
    B: oauth::Backend + 'static,
{
    let Some(channel_id) = app.playback.channel_id.clone() else {
        return;
    };
    subscribe_to(app, tx, session, channel_id, deps);
}

/// 再生中の動画にいいねする。動画 ID を取れなければ何もしない。
pub fn like_video<B>(
    app: &mut App,
    tx: &UnboundedSender<AppEvent>,
    session: &mut Session,
    deps: Oauth<B>,
) where
    B: oauth::Backend + 'static,
{
    let Some(video_id) = oauth::video_id_from_url(&app.playback.url) else {
        return;
    };
    start_oauth(app, tx, session, oauth::Action::Like(video_id), deps);
}

/// 認証から送信までを 1 タスクで進める。ブラウザでの認可を挟むので待ち時間は読めない。
/// 知らせは期限では消さず、結果が届いた時点で畳む。
pub fn start_oauth<B>(
    app: &mut App,
    tx: &UnboundedSender<AppEvent>,
    session: &mut Session,
    action: oauth::Action,
    deps: Oauth<B>,
) where
    B: oauth::Backend + 'static,
{
    let Some(paths) = deps.paths else {
        app.set_error(Some(oauth::NO_CONFIG_PATH.to_string()));
        return;
    };
    cancel_oauth(session);
    let nonce = session.oauth_nonce;
    app.set_notice(Some(action.notice().to_string()));
    session.oauth_action = Some(action.clone());
    let backend = deps.backend;
    let tx = tx.clone();
    session.oauth_task = Some(tokio::spawn(async move {
        let result = oauth::run(&backend, &paths, &action).await;
        let _ = tx.send(AppEvent::OauthDone { nonce, result });
    }));
}

/// 先行の認証・送信を打ち切る。nonce を進めるので、送信済みの結果は捨てられる。
fn cancel_oauth(session: &mut Session) {
    if let Some(task) = session.oauth_task.take() {
        task.abort();
    }
    session.oauth_action = None;
    session.oauth_nonce += 1;
}

pub fn start_engagement(app: &mut App, tx: &UnboundedSender<AppEvent>, session: &mut Session) {
    start_engagement_with(
        app,
        tx,
        session,
        Oauth::real(),
        std::time::SystemTime::now(),
    );
}

/// いいね済み一覧と、一覧に出ているチャンネルの登録有無を背景で確かめる。
/// 印は飾りなので、置き場が無い・認証がまだ・失敗したときは何も出さずに黙って終える。
pub fn start_engagement_with<B>(
    app: &mut App,
    tx: &UnboundedSender<AppEvent>,
    session: &mut Session,
    deps: Oauth<B>,
    now: std::time::SystemTime,
) where
    B: oauth::Backend + 'static,
{
    if let Some(task) = session.engagement_task.take() {
        task.abort();
    }
    if !app.settings.engagement.enabled {
        return;
    }
    let Some(paths) = deps.paths else {
        return;
    };
    let ttl = app.settings.engagement.ttl;
    let liked_wanted = app.engagement.liked_needs_refresh(now, ttl);
    let channels = app
        .engagement
        .channels_needing_refresh(&app.view_channel_ids(), now, ttl);
    if !liked_wanted && channels.is_empty() {
        return;
    }
    let nonce = session.search_nonce;
    let max_concurrent = app.settings.engagement.max_concurrent_requests;
    let backend = std::sync::Arc::new(deps.backend);
    let tx = tx.clone();
    session.engagement_task = Some(tokio::spawn(async move {
        let Ok(Some(token)) = oauth::access_token_for_refresh(backend.as_ref(), &paths).await
        else {
            return;
        };
        let liked_videos = if liked_wanted {
            oauth::refresh_liked_videos(backend.as_ref(), &token)
                .await
                .ok()
        } else {
            None
        };
        // 一部だけ反映すると、返らなかった ID を未登録として確定させてしまう。
        // 失敗したチャンクがあれば、チャンネル側は丸ごと諦める。
        let subscribed = if channels.is_empty() {
            Some(Vec::new())
        } else {
            oauth::refresh_subscriptions(
                std::sync::Arc::clone(&backend),
                &token,
                &channels,
                max_concurrent,
            )
            .await
            .ok()
        };
        let (asked_channels, subscribed_channels) = match subscribed {
            Some(found) => (channels, found),
            None => (Vec::new(), Vec::new()),
        };
        if liked_videos.is_none() && asked_channels.is_empty() {
            return;
        }
        let _ = tx.send(AppEvent::EngagementReady {
            nonce,
            liked_videos,
            asked_channels,
            subscribed_channels,
        });
    }));
}

/// チャンネル一覧を抜けて検索結果へ戻る。
/// 選択中の動画を非表示にする。書けた分だけ今の一覧からも消える。
pub fn hide_selected(app: &mut App, now: std::time::Instant) {
    let Some(result) = app.view_selected_result() else {
        return;
    };
    let (id, title) = (result.id.clone(), result.title.clone());
    match app.hidden.add_video(&id, &title) {
        Ok(()) => {
            app.drop_hidden();
            app.set_temporary_notice(HIDDEN_VIDEO_NOTICE.to_string(), now);
            // 最後の 1 件を隠したら見るものが無い。空の格子に留めず検索欄へ戻す。
            if app.channel.is_none() && app.results.is_empty() {
                app.enter_search_mode(Mode::Input);
            }
        }
        Err(e) => app.set_temporary_error(hide_failed(&e), now),
    }
}

/// 表示中のチャンネルを非表示にして検索側へ戻る。そのチャンネルの動画は全部消えるため。
pub fn hide_current_channel(app: &mut App, session: &mut Session, now: std::time::Instant) {
    let Some(channel) = app.channel.as_ref() else {
        return;
    };
    let (id, title) = (channel.channel_id.clone(), channel.channel_title.clone());
    match app.hidden.add_channel(&id, &title) {
        Ok(()) => {
            // 戻り先の判断は絞り込んだ後の件数で決める。
            app.drop_hidden();
            leave_channel(app, session);
            app.set_temporary_notice(HIDDEN_CHANNEL_NOTICE.to_string(), now);
        }
        Err(e) => app.set_temporary_error(hide_failed(&e), now),
    }
}

fn hide_failed(reason: &str) -> String {
    format!("非表示リストを保存できません: {reason}")
}

pub fn leave_channel(app: &mut App, session: &mut Session) {
    if app.channel.is_none() {
        return;
    }
    // 結果が検索側のタブへ流れ込まないよう、先に打ち切る。
    cancel_search(app, session);
    app.channel = None;
    app.set_error(None);
    app.sync_from_view();
    app.mode = if app.results.is_empty() {
        Mode::Input
    } else {
        Mode::Results
    };
}

pub fn switch_channel_tab(
    app: &mut App,
    tx: &UnboundedSender<AppEvent>,
    session: &mut Session,
    forward: bool,
) {
    switch_channel_tab_with(app, tx, session, forward, RealYtDlp);
}

/// チャンネル内のタブを移る。骨格は switch_tab_with と同じ。
pub fn switch_channel_tab_with<R>(
    app: &mut App,
    tx: &UnboundedSender<AppEvent>,
    session: &mut Session,
    forward: bool,
    runner: R,
) where
    R: YtDlp + Send + Sync + 'static,
{
    if app.channel.is_none() {
        return;
    }
    cancel_search(app, session);
    if let Some(channel) = app.channel.as_mut() {
        if forward {
            channel.next_tab();
        } else {
            channel.prev_tab();
        }
    }
    app.sync_from_view();
    app.set_error(None);
    if app.channel.as_ref().is_some_and(|c| c.state().loaded) {
        return;
    }
    start_channel_search_with(app, tx, session, runner);
}

pub fn select_channel_tab(
    app: &mut App,
    tx: &UnboundedSender<AppEvent>,
    session: &mut Session,
    index: usize,
) {
    select_channel_tab_with(app, tx, session, index, RealYtDlp);
}

/// 位置を指してチャンネル内のタブへ移る。骨格は select_tab_with と同じ。
pub fn select_channel_tab_with<R>(
    app: &mut App,
    tx: &UnboundedSender<AppEvent>,
    session: &mut Session,
    index: usize,
    runner: R,
) where
    R: YtDlp + Send + Sync + 'static,
{
    let Some(channel) = app.channel.as_ref() else {
        return;
    };
    // 押し直し。まだ読めていないタブだけ取り直す。
    if channel.tab.index() == index {
        if !channel.state().loaded && !app.searching {
            app.set_error(None);
            start_channel_search_with(app, tx, session, runner);
        }
        return;
    }
    // 範囲外は ChannelView::select_tab が弾く。移れてから打ち切る。
    if !app
        .channel
        .as_mut()
        .is_some_and(|channel| channel.select_tab(index))
    {
        return;
    }
    cancel_search(app, session);
    app.sync_from_view();
    app.set_error(None);
    if app.channel.as_ref().is_some_and(|c| c.state().loaded) {
        return;
    }
    start_channel_search_with(app, tx, session, runner);
}

/// 今のチャンネルタブを取りに行く。
fn start_channel_search_with<R>(
    app: &mut App,
    tx: &UnboundedSender<AppEvent>,
    session: &mut Session,
    runner: R,
) where
    R: YtDlp + Send + Sync + 'static,
{
    let Some(target) = app.channel.as_ref().map(ChannelView::target) else {
        return;
    };
    spawn_search(app, tx, session, target, app.settings.search.limit, runner);
}

pub fn reload_channel_tab(app: &mut App, tx: &UnboundedSender<AppEvent>, session: &mut Session) {
    reload_channel_tab_with(app, tx, session, RealYtDlp);
}

/// 今のチャンネルタブを取り直す。骨格は reload_tab_with と同じ。
/// 失敗したタブは読み込み済みのまま残るので、ここが取り直しの入口になる。
pub fn reload_channel_tab_with<R>(
    app: &mut App,
    tx: &UnboundedSender<AppEvent>,
    session: &mut Session,
    runner: R,
) where
    R: YtDlp + Send + Sync + 'static,
{
    if app.channel.is_none() {
        return;
    }
    request_reload(app);
    start_channel_search_with(app, tx, session, runner);
}

/// 取り直しを頼む。打ち切られても印は残るので、次の入口も控えを出さずに取りに行く。
fn request_reload(app: &mut App) {
    let state = app.view_state_mut();
    state.loaded = false;
    state.reload = true;
}

pub fn select_tab(
    app: &mut App,
    tx: &UnboundedSender<AppEvent>,
    session: &mut Session,
    index: usize,
) {
    select_tab_with(app, tx, session, index, RealYtDlp);
}

/// 位置を指してタブへ移る。中身は switch_tab_with と同じで、選び方だけが違う。
pub fn select_tab_with<R>(
    app: &mut App,
    tx: &UnboundedSender<AppEvent>,
    session: &mut Session,
    index: usize,
    runner: R,
) where
    R: YtDlp + Send + Sync + 'static,
{
    // 押し直し。cookie 無しで断られた等、まだ読めていないタブだけ取り直す。
    // Input モードでは r が検索語になるので、ここがクリックからの唯一の取り直し口になる。
    if index == app.tabs.selected() {
        if !app.tabs.state().loaded && !app.searching {
            app.set_error(None);
            start_tab_search_with(app, tx, session, runner);
        }
        return;
    }
    app.store_to_tab();
    // 範囲外は Tabs::select が弾く。走っている検索を巻き込まないよう、移れてから打ち切る。
    if !app.tabs.select(index) {
        return;
    }
    // 走らせたままにすると、その結果が移った先のタブへ書き込まれる。
    cancel_search(app, session);
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
    request_reload(app);
    // 「もっと見る」で伸ばした件数を、取り直し(r)でも維持する。r は「今の中身を
    // 見たい」意思表示であって、件数を初期値へ戻す意図ではないため。Feed/Channel の
    // タブは requested_limit を立てないままなので settings.search.limit のままになる。
    let limit = app
        .view_state()
        .requested_limit
        .max(app.settings.search.limit);
    start_tab_search_with_limit(app, tx, session, limit, runner);
}

pub fn load_more(app: &mut App, tx: &UnboundedSender<AppEvent>, session: &mut Session) {
    load_more_with(app, tx, session, RealYtDlp);
}

/// 「もっと見る」。今表示中の件数 + search.limit を新しい要求件数とし (MAX_SEARCH_LIMIT で
/// 丸める)、一覧を丸ごと取り直す。差分追記ではない。Target::Search のタブでだけ動く。
pub fn load_more_with<R>(
    app: &mut App,
    tx: &UnboundedSender<AppEvent>,
    session: &mut Session,
    runner: R,
) where
    R: YtDlp + Send + Sync + 'static,
{
    if !app.can_load_more() {
        return;
    }
    let Some(target) = app.tabs.target(app.query.text()) else {
        return;
    };
    if !matches!(target, Target::Search(_)) {
        return;
    }
    let limit = (app.view_results().len() + app.settings.search.limit).min(MAX_SEARCH_LIMIT);
    spawn_search(app, tx, session, target, limit, runner);
}

/// 格子の中の移動。リスト表示に落ちているときは既存の巻き戻る移動を使う。
pub fn move_selection(app: &mut App, dir: Dir) {
    move_selection_with(app, cell_size(), dir);
}

/// 指定のセル寸法での移動。端末に聞いた寸法を使わない呼び手はこちら。
pub fn move_selection_with(app: &mut App, cell: CellSize, dir: Dir) {
    let Some(layout) = ui::grid_layout(app, cell) else {
        match dir {
            Dir::Down => app.select_next(),
            Dir::Up => app.select_prev(),
            Dir::Left | Dir::Right => {}
        }
        return;
    };
    let selected = grid::move_selection(
        app.view_selected(),
        app.view_results().len(),
        layout.columns,
        dir,
    );
    app.set_view_selected(selected);
    let scroll = grid::ensure_visible(selected, layout.columns, layout.rows, app.view_scroll());
    // 選択の強調は画像の外に描くので、可視範囲が動いたときだけ貼り直す。
    if scroll != app.view_scroll() {
        app.set_view_scroll(scroll);
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
    let ids = app.thumbs.wanted(&app.view_result_ids(), target_px);
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

pub fn start_comments(
    app: &mut App,
    tx: &UnboundedSender<AppEvent>,
    session: &mut Session,
    video_id: String,
    url: String,
) {
    start_comments_with(app, tx, session, video_id, url, RealYtDlp);
}

/// 再生中の動画のコメントを背景で取る。再生はブロックしない。
/// yt-dlp の実行者を差し替えられる形。テストはここに偽物を渡して外部プロセスへ届かせない。
pub fn start_comments_with<R>(
    app: &mut App,
    tx: &UnboundedSender<AppEvent>,
    session: &mut Session,
    video_id: String,
    url: String,
    runner: R,
) where
    R: YtDlp + Send + Sync + 'static,
{
    cancel_comments(session);
    let nonce = session.comments_nonce;
    app.comments.begin(video_id.clone());
    let tx = tx.clone();
    session.comments_task = Some(tokio::spawn(async move {
        let comments = comments::fetch_comments(&runner, &url).await;
        let _ = tx.send(AppEvent::CommentsReady {
            nonce,
            video_id,
            comments,
        });
    }));
}

/// 先行の取得を打ち切る。nonce を進めるので、送信済みの結果は捨てられる。
fn cancel_comments(session: &mut Session) {
    if let Some(task) = session.comments_task.take() {
        task.abort();
    }
    session.comments_nonce += 1;
}

/// o のコメント表示トグル。貼ってある映像は ratatui の差分描画では消えないので、
/// 出すときに剥がす (present_video が以後のフレームを止める)。
pub fn toggle_comments(app: &mut App, session: &mut Session) {
    if app.comments.toggle() {
        session.owe_clear = true;
    }
}

/// コメント一覧の送り。行数も送れる幅も、描画と同じ枠の内側で数える。
pub fn scroll_comments(app: &mut App, step: CommentScroll) {
    let view = ui::comments_viewport(app.screen);
    let height = view.height as usize;
    let total = comments::display_lines(app.comments.state(), view.width as usize).len();
    let delta = match step {
        CommentScroll::Line(lines) => lines,
        // 1 画面ぶん。高さ 0 でも止まらないよう最低 1 行は動かす。
        CommentScroll::Page(pages) => pages * height.max(1) as isize,
    };
    app.comments.scroll_by(delta, total, height);
}

/// コメント一覧の送り幅。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommentScroll {
    Line(isize),
    Page(isize),
}

/// 先行の検索を打ち切る。nonce を進めるので、届いてしまった結果は捨てられる
/// (kill_on_drop で子プロセスも落ちる)。
fn cancel_search(app: &mut App, session: &mut Session) {
    if let Some(task) = session.search_task.take() {
        task.abort();
    }
    session.search_nonce += 1;
    // 打ち切った検索の結果はもう採用しないので、その控え先も捨てる。
    session.pending_cache_key = None;
    app.searching = false;
    app.set_notice(None);
    // 一覧が入れ替わる操作では、引いている最中のチャンネルも用済み。
    cancel_channel_lookup(session);
    // 知らせを畳む以上、認証待ちも残さない。残すと待っていることが見えないまま
    // ブラウザの認可だけ生き、127.0.0.1 のポートも掴んだままになる。
    cancel_oauth(session);
}

/// 再生開始時の映像スロットと起動計画。mpv を起動せずに検証できるよう切り出してある。
fn playback_plan(app: &App) -> (VideoSink, LaunchPlan) {
    // 別ウィンドウで始めても、端末内へ戻ったときのために kitty で用意しておく。
    let kind = app.display.decoder_kind().unwrap_or(DecoderKind::Kitty);
    let video = VideoSink::with_kind(kind, video_geometry(app.settings.display.max_pixels()));
    let mut plan = LaunchPlan::new(app.display, video.geometry(), &app.settings);
    plan.speed = app.speed;
    // 非表示で始めてもトラックは用意される。再生中に s で出せる。
    plan.subtitles = SubtitleLaunch::new(&app.settings.subtitles, app.subtitles.wanted());
    // 完了していない続きがあれば、そこから再開する。
    plan.resume_at = app
        .view_selected_result()
        .and_then(|result| app.resume.lookup(&result.id));
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
    id: String,
    channel_id: Option<String>,
    video: VideoSink,
) {
    // バックグラウンド再生中に別の動画へ差し替える経路もここを通るので、
    // 入れ替わる前の位置をここで記憶する。
    remember_playback_position(app);
    app.playback = Playback {
        title,
        url,
        id,
        channel_id,
        ..Playback::default()
    };
    app.seek_bar = SeekBarState::default();
    app.video = Some(video);
    app.mode = Mode::Playing;
    // バックグラウンド中に別の動画へ差し替えても、前面 (Mode::Playing) に確実に戻す。
    app.background = false;
    app.set_error(None);
    // 表示の希望は持ち越し、前の動画の選択だけ捨てる。
    app.subtitles.begin_playback(std::time::Instant::now());
    // 貼ってあるサムネイルは ratatui の差分描画では消えない。end_playback と対称に剥がす。
    session.owe_clear = true;
}

pub async fn start_playback(app: &mut App, tx: &UnboundedSender<AppEvent>, session: &mut Session) {
    let Some(result) = app.view_selected_result().cloned() else {
        return;
    };
    stop_playback(session).await;
    session.player_nonce += 1;
    let nonce = session.player_nonce;
    let (video, plan) = playback_plan(app);
    let url = result.url();
    match MpvController::launch(&url, nonce, tx.clone(), video.clone(), &plan).await {
        Ok(controller) => {
            enter_playback(
                app,
                session,
                result.title.clone(),
                url.clone(),
                result.id.clone(),
                result.channel_id.clone(),
                video,
            );
            session.player = Some(Player {
                sink: Box::new(controller),
                nonce,
            });
            start_comments(app, tx, session, result.id.clone(), url);
        }
        Err(e) => {
            app.video = None;
            app.set_error(Some(e));
        }
    }
}

/// Mode::Playing から検索側へ退避する (b)。session.player/app.video/app.playback は
/// 生かしたまま、映像の置き場所だけ隅のミニプレイヤーへ動かす。
pub async fn enter_background(app: &mut App, session: &mut Session) {
    app.background = true;
    app.mode = background_return_mode(app);
    apply_video_placement(app, session).await;
}

/// 検索側から前面 (Mode::Playing) へ戻る。バックグラウンド中でなければ何もしない。
pub async fn leave_background(app: &mut App, session: &mut Session) {
    if !app.background {
        return;
    }
    app.background = false;
    app.mode = Mode::Playing;
    apply_video_placement(app, session).await;
}

/// 新しい置き場所 (ui::video_target_area) へ映像の寸法を合わせる。
/// バックグラウンドのどちら向きの遷移でも手順は同じ。
async fn apply_video_placement(app: &mut App, session: &mut Session) {
    if let Some(area) = ui::video_target_area(app, app.screen) {
        let geometry = Geometry::new(area, cell_size(), app.settings.display.max_pixels());
        if let Some(video) = &app.video {
            video.resize(geometry);
        }
        send_resize_commands(app, session, geometry).await;
    }
    // 古い置き場所の残像は消す。mpv の a=d でサムネイルも巻き込まれるので貼り直す。
    session.owe_clear = true;
    app.thumbs.mark_dirty();
}

/// apply_resize_with と同じ分岐。window 中は描き直すものが無いので何も送らない。
async fn send_resize_commands(app: &mut App, session: &mut Session, geometry: Geometry) {
    let Some(p) = session.player.as_mut() else {
        return;
    };
    let sent = match app.display {
        DisplayMode::Embedded => p.send_all(&mpv::resize_video(geometry)).await,
        DisplayMode::Text => p.send_all(&mpv::resize_text_video(geometry)).await,
        DisplayMode::Window => return,
    };
    if let Err(e) = sent {
        app.set_error(Some(e));
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

/// s の字幕トグル。mpv は再起動せず sid を auto / no で入れ替える。
/// 数値の sid は無いトラックでも success が返るので使わない。
pub async fn toggle_subtitles(app: &mut App, session: &mut Session, now: std::time::Instant) {
    let settings = app.settings.subtitles.clone();
    // 字幕なしは推定なので、押されたら止めずに送る。取り違えても次のポーリングで直る。
    // 止めると、無い動画で消せないまま次の動画へ要求を持ち越してしまう。
    let missing = app.subtitles.status(&settings, now) == SubtitleStatus::Missing;
    let (wanted, command) = match app.subtitles.toggle_command(&settings) {
        Ok(next) => next,
        Err(reason) => {
            app.set_temporary_notice(reason, now);
            return;
        }
    };
    let Some(player) = session.player.as_mut() else {
        return;
    };
    match player.sink.send(&command).await {
        Ok(()) => {
            app.subtitles.set_wanted(wanted, now);
            // tct では mpv が字幕を描かない (実測)。押しても何も起きない画面にしない。
            let notice = if wanted && app.display == DisplayMode::Text {
                subtitles::text_mode_notice()
            } else if missing {
                // 出ないまま消したので、出なかった理由を返す。
                subtitles::missing_notice(&settings.lang)
            } else {
                subtitles::toggle_notice(wanted, &settings.lang)
            };
            app.set_temporary_notice(notice, now);
        }
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
/// 切り替えられたら設定ファイルへも残す。保存先を差し替えられる形にしてあり、
/// テストはここに一時ファイルを渡して利用者の設定を書き換えない。
pub async fn cycle_display_mode(
    app: &mut App,
    session: &mut Session,
    config: Option<&Path>,
    now: std::time::Instant,
) {
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
            // 切り替え自体は済んでいるので、保存に失敗しても再生は続ける。
            // 理由は save_display_mode が期限つきで出す。
            save_display_mode(app, config, to, now);
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
            let scroll = grid::ensure_visible(
                app.view_selected(),
                layout.columns,
                layout.rows,
                app.view_scroll(),
            );
            app.set_view_scroll(scroll);
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

/// 設定画面へ入る。開いた元のモードは close_settings が戻り先に使う。
pub fn open_settings(app: &mut App, session: &mut Session) {
    app.settings_return = app.mode;
    app.mode = Mode::Settings;
    app.settings_selected = app.settings_selected.min(SETTINGS_ITEMS.len() - 1);
    // 打ち込みかけの数字を持ち越さない。残ると開いた直後から打ち込み中になる。
    app.settings_edit = None;
    // Esc の戻り先。保存していない編集は、この値で捨てる。
    app.settings_backup = app.settings.clone();
    // 貼ってあるサムネイルは ratatui の差分描画では消えないので、設定の行に重ならないよう剥がす。
    session.owe_clear = true;
}

/// 保存せず閉じる。編集した値は開いた時点 (保存していればその時点) の値へ戻す。
/// search.limit や search.layout はセッション中に読み直されるので、残すと
/// 「保存しなければ何も変わらない」と食い違う。
pub fn close_settings(app: &mut App) {
    app.settings = app.settings_backup.clone();
    app.settings_edit = None;
    app.mode = app.settings_return;
    // 開くときに剥がしたサムネイルを貼り直す。
    app.thumbs.mark_dirty();
}

/// ダウンロード画面へ入る。Results/Channel は選択中の行、Playing は再生中の動画が対象。
/// 対象を選べない (一覧が空) ときは何もしない。
pub fn open_download(app: &mut App, session: &mut Session) {
    let (title, url) = match app.mode {
        Mode::Playing => (app.playback.title.clone(), app.playback.url.clone()),
        _ => match app.view_selected_result() {
            Some(result) => (result.title.clone(), result.url()),
            None => return,
        },
    };
    app.download_return = app.mode;
    app.mode = Mode::Download;
    app.download_url = url;
    let home = std::env::var_os("HOME");
    let dir = download::default_dir(app.settings.download.dir.as_deref(), home.as_deref());
    let dir_text = dir.map(|d| d.display().to_string()).unwrap_or_default();
    app.download_dir = QueryEditor::from(dir_text.as_str());
    app.download_filename = QueryEditor::from(download::sanitize_filename(&title).as_str());
    app.download_audio_only = false;
    app.download_focus = DownloadField::Dir;
    // 貼ってあるサムネイル/映像は差分描画では消えないので、剥がしてから画面を出す。
    session.owe_clear = true;
}

/// 何もせず閉じる。ダウンロードを始めていれば背景で進んだまま。
pub fn close_download(app: &mut App) {
    app.mode = app.download_return;
    // 開くときに剥がしたサムネイル/映像を貼り直す。
    app.thumbs.mark_dirty();
}

/// ↑↓ のフォーカス移動。3 行しか無いので端では巻き戻す。
pub fn move_download_focus(app: &mut App, delta: i32) {
    app.download_focus = if delta < 0 {
        app.download_focus.prev()
    } else {
        app.download_focus.next()
    };
}

/// Enter での開始。保存先とファイル名がどちらも入っていることを確かめてから、
/// 背景でダウンロードを始めて画面を閉じる。
pub fn start_download<D>(
    app: &mut App,
    tx: &UnboundedSender<AppEvent>,
    session: &mut Session,
    downloader: D,
) where
    D: download::Downloader + Send + Sync + 'static,
{
    let dir_text = app.download_dir.text().trim().to_string();
    // 初期値は open_download でサニタイズ済みだが、その後の自由入力で `/` `\` を
    // 打ち直されるとディレクトリを飛び出せてしまうため、開始直前にも掛け直す。
    let filename_text = download::sanitize_filename(app.download_filename.text().trim());
    if dir_text.is_empty() || filename_text.is_empty() {
        app.set_temporary_error(
            "保存先とファイル名を入力してください".to_string(),
            std::time::Instant::now(),
        );
        return;
    }
    cancel_download(session);
    let nonce = session.download_nonce;
    let audio_only = app.download_audio_only;
    let url = app.download_url.clone();
    let debug_log_path = download_debug_log_path(app.settings.download.debug);
    // 期限では消さず、終わった時点 (DownloadDone) で畳む。
    app.set_notice(Some(format!(
        "{}{filename_text}",
        download::DOWNLOADING_PREFIX
    )));
    let tx = tx.clone();
    session.download_task = Some(tokio::spawn(async move {
        let notice = download::run(
            &downloader,
            &dir_text,
            &filename_text,
            audio_only,
            &url,
            debug_log_path.as_deref(),
        )
        .await;
        let _ = tx.send(AppEvent::DownloadDone { nonce, notice });
    }));
    close_download(app);
}

/// `[download] debug` が有効なときだけログの置き場を返す。無効なら None
/// (= download::run へ渡さず、今までと同じくログを書かない)。
fn download_debug_log_path(enabled: bool) -> Option<std::path::PathBuf> {
    if !enabled {
        return None;
    }
    let xdg = std::env::var_os("XDG_CONFIG_HOME");
    let home = std::env::var_os("HOME");
    download::debug_log_path(xdg.as_deref(), home.as_deref())
}

/// 先行のダウンロードを打ち切る。nonce を進めるので、届いてしまった結果は捨てられる
/// (kill_on_drop で子プロセスも落ちる)。
fn cancel_download(session: &mut Session) {
    if let Some(task) = session.download_task.take() {
        task.abort();
    }
    session.download_nonce += 1;
}

/// ↑↓ の選択移動。行数が少ないので端では巻き戻す。
pub fn move_settings_selection(app: &mut App, delta: i32) {
    let count = SETTINGS_ITEMS.len();
    let current = app.settings_selected.min(count - 1);
    app.settings_selected = if delta < 0 {
        (current + count - 1) % count
    } else {
        (current + 1) % count
    };
}

/// ←→ (と Enter / Space) の値変更。変えるのは選択中の行だけ。
pub fn adjust_settings_value(app: &mut App, delta: i32) {
    app.settings_item().adjust(&mut app.settings, delta);
}

/// s での保存。設定ファイルの場所は起動時と同じ規則で決める。
pub fn save_settings(app: &mut App, now: std::time::Instant) {
    save_settings_to(app, config_path_from_env().as_deref(), now);
}

/// 設定ファイルの置き場。起動時と同じ規則で決める。
pub fn config_path_from_env() -> Option<std::path::PathBuf> {
    let xdg = std::env::var_os("XDG_CONFIG_HOME");
    let home = std::env::var_os("HOME");
    settings::config_path(xdg.as_deref(), home.as_deref())
}

/// 保存先を差し替えられる形。テストはここに一時ファイルを渡して利用者の設定を書き換えない。
/// 成否どちらも期限つきで出す。設定画面にはポーリングが無く、素の error は消す機会が無い。
pub fn save_settings_to(app: &mut App, path: Option<&Path>, now: std::time::Instant) {
    let Some(path) = path else {
        app.set_temporary_error(NO_CONFIG_PATH.to_string(), now);
        return;
    };
    // 環境変数が効いている項目はファイル側の値のまま書く。一時的な指定を焼き付けない。
    let to_write = app.env_overridden.restore(&app.settings);
    // display.mode は app.display (今の実行中の値) と別物で、次回起動でしか動かない。
    // 設定画面での編集だけでは今の画面に何も起きないので、保存の知らせに添えて伝える。
    let display_mode_deferred = app.settings.display.mode != app.display;
    match settings::save_to(path, &to_write) {
        Ok(()) => {
            // 保存した内容が Esc の戻り先になる。
            app.settings_backup = app.settings.clone();
            app.set_temporary_notice(
                saved_notice(path, &app.env_overridden, display_mode_deferred),
                now,
            );
        }
        Err(e) => app.set_temporary_error(format!("設定を保存できません: {e}"), now),
    }
}

/// w での自動保存。設定画面の s と違い保存を意図した操作ではないので、
/// display.mode の行だけを差し替え、成功したときは何も出さない。
/// settings へ入れるのは保存できた値だけ。食い違うと後の s が古い値を焼き付ける。
fn save_display_mode(
    app: &mut App,
    path: Option<&Path>,
    mode: DisplayMode,
    now: std::time::Instant,
) {
    let Some(path) = path else {
        app.set_temporary_error(NO_CONFIG_PATH.to_string(), now);
        return;
    };
    match settings::save_display_mode_to(path, mode) {
        Ok(()) => {
            app.settings.display.mode = mode;
            // 保存した値が Esc の戻り先になる。他の項目は開いた時点の値のまま残す。
            app.settings_backup.display.mode = mode;
        }
        Err(e) => app.set_temporary_error(format!("設定を保存できません: {e}"), now),
    }
}

/// 保存できた旨。書き換えなかった項目があれば、その名前も出す。
fn saved_notice(
    path: &Path,
    overridden: &settings::EnvOverridden,
    display_mode_deferred: bool,
) -> String {
    let saved = format!("{} に保存しました", path.display());
    let mut notes = Vec::new();
    if !overridden.is_empty() {
        notes.push(format!(
            "{} は環境変数の指定中で書き換えません",
            overridden.keys().join("、")
        ));
    }
    if display_mode_deferred {
        notes.push("display.mode は次回起動から反映されます".to_string());
    }
    if notes.is_empty() {
        saved
    } else {
        format!("{saved} ({})", notes.join("。"))
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
    use crate::comments::CommentState;
    use crate::cookies::{ChannelTab, CookieSource, CookieState, Feed};
    use crate::display::Quality;
    use crate::fetch::fixtures::{CurlResult, FakeCurl};
    use crate::oauth::fixtures::FakeBackend;
    use crate::query::QueryEditor;
    use crate::rgb::RgbImage;
    use crate::search::fixtures::{FakeYtDlp, Step, done};
    use crate::search::{ChannelRef, SearchResult};
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

    /// 自動保存の書き先を一時ファイルにした w 切替。利用者の設定を書き換えない。
    async fn cycle_to(app: &mut App, session: &mut Session, path: &Path) {
        cycle_display_mode(app, session, Some(path), std::time::Instant::now()).await;
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
            channel_id: None,
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

    #[tokio::test]
    async fn end_playback_resets_background() {
        let mut app = App {
            mode: Mode::Playing,
            background: true,
            video: Some(sink()),
            ..App::default()
        };
        let mut session = Session::default();
        end_playback(&mut app, &mut session, None).await;
        assert!(!app.background);
    }

    fn resume_temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tuitube-actions-resume-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[tokio::test]
    async fn end_playback_remembers_the_position_of_the_video_it_leaves() {
        let dir = resume_temp_dir("end-playback");
        let mut app = App {
            mode: Mode::Playing,
            resume: crate::resume::load_from(Some(&dir.join("resume.toml"))),
            playback: Playback {
                id: "v1".to_string(),
                time_pos: Some(120.0),
                duration: Some(600.0),
                ..Playback::default()
            },
            ..App::default()
        };
        let mut session = Session::default();

        end_playback(&mut app, &mut session, None).await;

        assert_eq!(app.resume.lookup("v1"), Some(120.0));
    }

    #[test]
    fn enter_playback_resets_background() {
        // バックグラウンド中 (Mode::Results 等) に別の動画で Enter を押した経路を再現する。
        let mut app = App {
            mode: Mode::Results,
            background: true,
            ..App::default()
        };
        let mut session = Session::default();
        enter_playback(
            &mut app,
            &mut session,
            "title".to_string(),
            "https://example.com/watch".to_string(),
            "id0".to_string(),
            None,
            sink(),
        );
        assert_eq!(app.mode, Mode::Playing);
        assert!(!app.background);
    }

    #[test]
    fn enter_playback_remembers_the_position_of_the_video_it_replaces() {
        // バックグラウンド再生中に別の動画を選んだ経路 (design: enter_playback も保存箇所)。
        let dir = resume_temp_dir("enter-playback");
        let mut app = App {
            mode: Mode::Results,
            background: true,
            resume: crate::resume::load_from(Some(&dir.join("resume.toml"))),
            playback: Playback {
                id: "v1".to_string(),
                time_pos: Some(120.0),
                duration: Some(600.0),
                ..Playback::default()
            },
            ..App::default()
        };
        let mut session = Session::default();

        enter_playback(
            &mut app,
            &mut session,
            "title".to_string(),
            "https://example.com/watch".to_string(),
            "v2".to_string(),
            None,
            sink(),
        );

        assert_eq!(
            app.resume.lookup("v1"),
            Some(120.0),
            "前の動画の位置を覚える"
        );
        assert_eq!(app.playback.id, "v2");
    }

    #[test]
    fn enter_playback_remembers_the_channel_of_the_video() {
        // 再生中のチャンネル登録 (u) はここで覚えた channel_id だけを見る。
        let mut app = App::default();
        let mut session = Session::default();

        enter_playback(
            &mut app,
            &mut session,
            "title".to_string(),
            "https://example.com/watch".to_string(),
            "v1".to_string(),
            Some("UC1".to_string()),
            sink(),
        );

        assert_eq!(app.playback.channel_id.as_deref(), Some("UC1"));
    }

    #[test]
    fn remember_playback_position_does_nothing_without_a_video() {
        let dir = resume_temp_dir("no-video");
        let mut app = App {
            resume: crate::resume::load_from(Some(&dir.join("resume.toml"))),
            ..App::default()
        };

        remember_playback_position(&mut app);

        assert_eq!(app.resume.lookup(""), None);
        assert!(!dir.join("resume.toml").exists());
    }

    #[tokio::test]
    async fn enter_background_returns_to_results_and_keeps_the_player_alive() {
        let mut app = App {
            screen: Rect::new(0, 0, 80, 24),
            results: vec![result("a")],
            video: Some(sink()),
            playback: Playback {
                title: "song".to_string(),
                ..Playback::default()
            },
            ..playing_app()
        };
        let mut session = Session::default();
        let sent = record(&mut session, Ok(()));

        enter_background(&mut app, &mut session).await;

        assert!(app.background);
        assert_eq!(app.mode, Mode::Results);
        assert!(app.video.is_some(), "映像はそのまま");
        assert_eq!(app.playback.title, "song", "再生中の情報はそのまま");
        assert!(session.player.is_some(), "player はそのまま");
        assert!(session.owe_clear);
        assert!(app.thumbs.take_dirty());

        let geometry = Geometry::new(
            ui::mini_video_area(app.screen, app.display),
            cell_size(),
            MAX_FRAME_PIXELS,
        );
        assert_eq!(
            lines(&sent),
            mpv::resize_video(geometry)
                .iter()
                .map(|c| c.to_line())
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn enter_background_without_results_returns_to_input() {
        let mut app = App {
            screen: Rect::new(0, 0, 80, 24),
            video: Some(sink()),
            ..playing_app()
        };
        let mut session = Session::default();
        enter_background(&mut app, &mut session).await;
        assert_eq!(app.mode, Mode::Input);
        assert!(app.background);
    }

    #[tokio::test]
    async fn enter_background_while_in_a_channel_returns_to_the_channel() {
        let mut app = App {
            screen: Rect::new(0, 0, 80, 24),
            channel: Some(ChannelView::new("UCabc".to_string(), "channel".to_string())),
            video: Some(sink()),
            ..playing_app()
        };
        let mut session = Session::default();
        enter_background(&mut app, &mut session).await;
        assert_eq!(app.mode, Mode::Channel);
        assert!(app.background);
    }

    #[tokio::test]
    async fn enter_background_in_window_mode_sends_no_mpv_commands() {
        let mut app = App {
            screen: Rect::new(0, 0, 80, 24),
            display: DisplayMode::Window,
            video: Some(sink()),
            ..playing_app()
        };
        let mut session = Session::default();
        let sent = record(&mut session, Ok(()));

        enter_background(&mut app, &mut session).await;

        assert!(app.background);
        assert!(lines(&sent).is_empty(), "window 中は追加の映像描画をしない");
        assert!(session.owe_clear);
    }

    #[tokio::test]
    async fn leave_background_returns_to_playing_and_resizes_to_the_full_area() {
        let mut app = App {
            mode: Mode::Results,
            background: true,
            screen: Rect::new(0, 0, 80, 24),
            results: vec![result("a")],
            video: Some(sink()),
            ..App::default()
        };
        let mut session = Session::default();
        let sent = record(&mut session, Ok(()));

        leave_background(&mut app, &mut session).await;

        assert!(!app.background);
        assert_eq!(app.mode, Mode::Playing);
        assert!(session.owe_clear);

        let geometry = Geometry::new(ui::video_area(app.screen), cell_size(), MAX_FRAME_PIXELS);
        assert_eq!(
            lines(&sent),
            mpv::resize_video(geometry)
                .iter()
                .map(|c| c.to_line())
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn leave_background_does_nothing_when_not_in_background() {
        let mut app = App {
            mode: Mode::Results,
            screen: Rect::new(0, 0, 80, 24),
            results: vec![result("a")],
            ..App::default()
        };
        let mut session = Session::default();
        let sent = record(&mut session, Ok(()));

        leave_background(&mut app, &mut session).await;

        assert_eq!(
            app.mode,
            Mode::Results,
            "バックグラウンドでなければ何もしない"
        );
        assert!(lines(&sent).is_empty());
        assert!(!session.owe_clear);
    }

    #[test]
    fn a_blank_query_does_not_start_a_search() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = App {
            query: QueryEditor::from("   "),
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
            query: QueryEditor::from("  ラーメン  "),
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
            query: QueryEditor::from(":ytrec"),
            ..App::default()
        };
        start_search_with(&mut app, &tx, &mut session, StubYtDlp);

        assert!(session.search_task.is_none(), "yt-dlp を起動しない");
        // 断った試行も世代を1つ進める (先行の結果を無効にするため)。
        assert_eq!(session.search_nonce, 1);
        assert!(!app.searching);
        let error = app.error.expect("理由を出す");
        assert!(error.contains("[cookies] browser"), "{error}");
    }

    #[tokio::test]
    async fn a_refused_feed_cancels_the_search_in_flight() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = App {
            query: QueryEditor::from("ラーメン"),
            notice: Some("前の検索の知らせ".to_string()),
            ..App::default()
        };
        start_search_with(&mut app, &tx, &mut session, StubYtDlp);
        tokio::task::yield_now().await;
        assert!(app.searching);

        // 検索中に cookie 無しのフィードを要求する。
        app.query.set(":ytrec");
        start_search_with(&mut app, &tx, &mut session, StubYtDlp);

        // 先行タスクは残さず、その結果が後から採用されないよう nonce も進める。
        assert!(session.search_task.is_none());
        assert_eq!(session.search_nonce, 2);
        assert!(!app.searching);
        assert!(app.notice.is_none());
        let error = app.error.expect("理由を出す");
        assert!(error.contains("[cookies] browser"), "{error}");
    }

    #[tokio::test]
    async fn feed_requiring_login_is_refused_when_suspended() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = App {
            query: QueryEditor::from(":ythis"),
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
            query: QueryEditor::from("ラーメン"),
            notice: Some("前の検索の知らせ".to_string()),
            ..App::default()
        };
        start_search_with(&mut app, &tx, &mut session, StubYtDlp);

        assert!(app.notice.is_none());
        assert!(session.search_task.is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn a_search_waits_as_long_as_the_setting_says() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = App {
            query: QueryEditor::from("ラーメン"),
            ..App::default()
        };
        app.settings.search.timeout = Duration::from_secs(45);
        start_search_with(
            &mut app,
            &tx,
            &mut session,
            FakeYtDlp::new([Step::Hang, Step::Hang]),
        );
        session
            .search_task
            .take()
            .expect("タスク")
            .await
            .expect("完走");

        let Some(AppEvent::SearchDone { report, .. }) = rx.recv().await else {
            panic!("検索結果が届く");
        };
        let error = report.results.expect_err("タイムアウト");
        assert!(error.contains("45 秒"), "{error}");
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
        let path = settings_temp_dir("cycle-no-player").join("config.toml");
        cycle_to(&mut app, &mut session, &path).await;

        assert_eq!(app.display, DisplayMode::Embedded);
        assert_eq!(video.kind(), DecoderKind::Kitty);
        assert!(app.error.is_none());
        assert!(!session.owe_clear);
        assert!(!path.exists(), "切り替えていないので保存もしない");
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
        let path = settings_temp_dir("cycle-embedded-text").join("config.toml");
        cycle_to(&mut app, &mut session, &path).await;

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
        let path = settings_temp_dir("cycle-text-window").join("config.toml");
        cycle_to(&mut app, &mut session, &path).await;

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
        let path = settings_temp_dir("cycle-window-embedded").join("config.toml");
        cycle_to(&mut app, &mut session, &path).await;

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
        let path = settings_temp_dir("cycle-send-failed").join("config.toml");
        cycle_to(&mut app, &mut session, &path).await;

        assert_eq!(app.display, DisplayMode::Embedded);
        assert_eq!(app.error.as_deref(), Some("パイプが閉じました"));
        assert!(!session.owe_clear);
        // 切り替わっていない表示を設定ファイルへ焼き付けない。
        assert_eq!(app.settings.display.mode, DisplayMode::Embedded);
        assert!(!path.exists(), "{path:?}");
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
        let path = settings_temp_dir("cycle-decoder-failed").join("config.toml");
        cycle_to(&mut app, &mut session, &path).await;

        assert_eq!(app.display, DisplayMode::Embedded);
        assert_eq!(video.kind(), DecoderKind::Kitty);
        assert_eq!(app.error.as_deref(), Some("パイプが閉じました"));
    }

    #[tokio::test]
    async fn cycling_saves_the_new_mode_to_the_config_file() {
        let path = settings_temp_dir("cycle-autosave").join("config.toml");
        std::fs::write(&path, "[display]\nmode = \"embedded\"\n").expect("書ける");
        let mut app = playing_app();
        let mut session = Session::default();
        let _sent = record(&mut session, Ok(()));
        cycle_to(&mut app, &mut session, &path).await;

        assert_eq!(app.display, DisplayMode::Text);
        assert_eq!(app.settings.display.mode, DisplayMode::Text);
        let written = std::fs::read_to_string(&path).expect("読める");
        assert_eq!(written, "[display]\nmode = \"text\"\n", "{written}");
        assert!(app.error.is_none());
    }

    #[tokio::test]
    async fn cycling_keeps_the_rest_of_the_config_file() {
        // w は保存を意図した操作ではないので、mode 以外の行を書き換えない。
        let path = settings_temp_dir("cycle-autosave-keep").join("config.toml");
        let before = "# 自分で書いたコメント\n[display]\nmode = \"embedded\"\n\n[search]\nlimit = 5000\nunknown_key = 1\n";
        std::fs::write(&path, before).expect("書ける");
        let mut app = playing_app();
        let mut session = Session::default();
        let _sent = record(&mut session, Ok(()));
        cycle_to(&mut app, &mut session, &path).await;

        let written = std::fs::read_to_string(&path).expect("読める");
        assert_eq!(written, before.replace("embedded", "text"), "{written}");
    }

    #[tokio::test]
    async fn cycling_creates_the_config_file_when_there_is_none() {
        let path = settings_temp_dir("cycle-autosave-new").join("config.toml");
        let mut app = playing_app();
        let mut session = Session::default();
        let _sent = record(&mut session, Ok(()));
        cycle_to(&mut app, &mut session, &path).await;

        let written = std::fs::read_to_string(&path).expect("読める");
        assert_eq!(written, crate::settings::render(&app.settings), "{written}");
    }

    #[tokio::test]
    async fn cycling_says_nothing_when_the_save_goes_through() {
        // 再生行を押し出さないよう、成功したときは黙っている。
        let path = settings_temp_dir("cycle-autosave-quiet").join("config.toml");
        let mut app = playing_app();
        let mut session = Session::default();
        let _sent = record(&mut session, Ok(()));
        cycle_to(&mut app, &mut session, &path).await;

        assert_eq!(app.notice, None);
        assert_eq!(app.error, None);
    }

    #[tokio::test]
    async fn an_autosaved_mode_survives_a_later_escape() {
        // 設定画面を開いて Esc で戻したときに、直前の自動保存まで巻き戻さない。
        let path = settings_temp_dir("cycle-autosave-escape").join("config.toml");
        let mut app = playing_app();
        let mut session = Session::default();
        let _sent = record(&mut session, Ok(()));
        cycle_to(&mut app, &mut session, &path).await;
        assert_eq!(
            app.settings_backup.display.mode,
            DisplayMode::Text,
            "保存した内容が Esc の戻り先になる"
        );

        open_settings(&mut app, &mut session);
        close_settings(&mut app);
        assert_eq!(app.settings.display.mode, DisplayMode::Text);
    }

    #[tokio::test]
    async fn a_failed_autosave_still_switches_the_display() {
        let dir = settings_temp_dir("cycle-autosave-blocked");
        let blocker = dir.join("blocked");
        std::fs::write(&blocker, "ファイルなので中に書けない").expect("書ける");

        let mut app = playing_app();
        let mut session = Session::default();
        let sent = record(&mut session, Ok(()));
        cycle_to(&mut app, &mut session, &blocker.join("config.toml")).await;

        assert_eq!(app.display, DisplayMode::Text, "再生はそのまま続ける");
        assert!(!lines(&sent).is_empty(), "mpv へは送れている");
        let error = app.error.clone().expect("保存できない理由を出す");
        assert!(error.contains("保存できません"), "{error}");
        // 保存できていないので、次の保存で焼き付く値も Esc の戻り先も切り替える前のまま。
        assert_eq!(app.settings.display.mode, DisplayMode::Embedded);
        assert_eq!(app.settings_backup.display.mode, DisplayMode::Embedded);
    }

    #[tokio::test]
    async fn a_failed_autosave_does_not_leave_the_settings_screen_out_of_step() {
        // 保存できなかった値が settings に残ると、後の s がファイルへ書けない値を焼き付ける。
        let dir = settings_temp_dir("cycle-autosave-mismatch");
        let blocker = dir.join("blocked");
        std::fs::write(&blocker, "ファイルなので中に書けない").expect("書ける");

        let mut app = playing_app();
        let mut session = Session::default();
        let _sent = record(&mut session, Ok(()));
        cycle_to(&mut app, &mut session, &blocker.join("config.toml")).await;

        open_settings(&mut app, &mut session);
        close_settings(&mut app);
        let path = dir.join("config.toml");
        save_settings_to(&mut app, Some(&path), std::time::Instant::now());

        let written = std::fs::read_to_string(&path).expect("読める");
        assert!(written.contains("mode = \"embedded\""), "{written}");
    }

    #[tokio::test]
    async fn cycling_without_a_place_to_save_says_so_and_keeps_playing() {
        let mut app = playing_app();
        let mut session = Session::default();
        let _sent = record(&mut session, Ok(()));
        cycle_display_mode(&mut app, &mut session, None, std::time::Instant::now()).await;

        assert_eq!(app.display, DisplayMode::Text);
        assert_eq!(app.error.as_deref(), Some(NO_CONFIG_PATH));
        assert_eq!(app.settings.display.mode, DisplayMode::Embedded);
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
    fn a_cookie_file_reaches_mpv_once_the_search_confirmed_it() {
        let file =
            CookieSource::from_file(Some(std::path::Path::new("/tmp/cookies.txt"))).expect("path");
        let app = App {
            cookies: CookieState::Active(file),
            ..playing_app()
        };
        let (_video, plan) = playback_plan(&app);
        assert!(
            plan.args()
                .contains(&"--ytdl-raw-options-append=cookies=/tmp/cookies.txt".to_string()),
            "{:?}",
            plan.args()
        );

        // 検索で確かめる前 (Armed) は再生に渡さない。既存のブラウザ指定と同じ扱い。
        let app = App {
            cookies: CookieState::Armed(
                CookieSource::from_file(Some(std::path::Path::new("/tmp/cookies.txt")))
                    .expect("path"),
            ),
            ..playing_app()
        };
        let (_video, plan) = playback_plan(&app);
        assert!(
            !plan.args().iter().any(|arg| arg.contains("cookies")),
            "{:?}",
            plan.args()
        );
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

    #[test]
    fn playback_plan_resumes_a_previously_remembered_position() {
        let dir = resume_temp_dir("playback-plan-resume");
        let mut resume = crate::resume::load_from(Some(&dir.join("resume.toml")));
        resume.remember("v1", 120.0, 600.0).expect("書ける");
        let app = App {
            results: vec![result("v1")],
            resume,
            ..playing_app()
        };

        let (_video, plan) = playback_plan(&app);

        assert_eq!(plan.resume_at, Some(120.0));
        assert!(
            plan.args().contains(&"--start=120".to_string()),
            "{:?}",
            plan.args()
        );
    }

    #[test]
    fn playback_plan_starts_from_the_beginning_without_a_remembered_position() {
        let app = App {
            results: vec![result("v1")],
            ..playing_app()
        };

        let (_video, plan) = playback_plan(&app);

        assert_eq!(plan.resume_at, None);
        assert!(!plan.args().iter().any(|a| a.starts_with("--start")));
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

    /// チャンネルへ移れる結果を持つ 80x24 の検索結果画面。
    fn channel_grid_app() -> App {
        let mut app = App {
            mode: Mode::Results,
            screen: Rect::new(0, 0, 80, 24),
            ..App::default()
        };
        let results = vec![
            SearchResult {
                uploader: Some("One Channel".to_string()),
                channel_id: Some("UCone".to_string()),
                ..result("id0")
            },
            SearchResult {
                uploader: Some("Two Channel".to_string()),
                channel_id: Some("UCtwo".to_string()),
                ..result("id1")
            },
        ];
        app.set_results(results, &Target::Search("q".to_string()));
        app.thumbs.take_dirty();
        app
    }

    /// 積まれた検索を走らせて、yt-dlp へ渡った引数を読む。
    async fn finish_search(session: &mut Session) {
        session
            .search_task
            .take()
            .expect("検索タスク")
            .await
            .expect("完走");
    }

    #[tokio::test]
    async fn open_channel_searches_the_video_tab_of_the_selected_result() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = channel_grid_app();
        app.selected = 1;
        app.scroll = 0;
        let runner = FakeYtDlp::new([done(0, "", "")]);

        open_channel_with(&mut app, &tx, &mut session, runner.clone());

        assert_eq!(app.mode, Mode::Channel);
        let channel = app.channel.as_ref().expect("チャンネルへ移る");
        assert_eq!(channel.channel_id, "UCtwo");
        assert_eq!(channel.channel_title, "Two Channel");
        assert_eq!(channel.tab, ChannelTab::Videos);
        assert_eq!(app.result_ids(), ["id0", "id1"], "検索結果は残す");
        assert_eq!(app.tabs.state().selected, 1, "戻る位置を控える");

        finish_search(&mut session).await;
        let args = runner.calls();
        assert_eq!(args[0][0], "https://www.youtube.com/channel/UCtwo/videos");
        assert!(args[0].iter().any(|a| a == "--playlist-end"), "{args:?}");
    }

    // ---- チャンネル登録・いいね ----

    const OAUTH_CLIENT_TOML: &str =
        "client_id = \"dummy-id.apps.googleusercontent.com\"\nclient_secret = \"dummy-secret\"\n";
    const OAUTH_ACCESS_JSON: &str = r#"{"access_token":"at-1","expires_in":3599}"#;

    /// 資格情報と保存済みトークンを置いた一時ディレクトリ。ブラウザは開かない経路になる。
    fn oauth_paths(name: &str) -> crate::oauth::Paths {
        let dir = std::env::temp_dir().join(format!(
            "tuitube-actions-oauth-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let client = dir.join("oauth_client.toml");
        std::fs::write(&client, OAUTH_CLIENT_TOML).expect("書ける");
        let token = dir.join("oauth_token.toml");
        crate::oauth::save_refresh_token(&token, "rt-1").expect("書ける");
        crate::oauth::Paths { client, token }
    }

    fn oauth_deps(
        name: &str,
        responses: Vec<Result<crate::oauth::Response, String>>,
    ) -> Oauth<FakeBackend> {
        Oauth {
            backend: FakeBackend::new().with_responses(responses),
            paths: Some(oauth_paths(name)),
        }
    }

    /// 返らない送信を積む依頼先。打ち切りの検証に使う。
    fn hanging_oauth(name: &str) -> Oauth<FakeBackend> {
        Oauth {
            backend: FakeBackend::new(),
            paths: Some(oauth_paths(name)),
        }
    }

    fn channel_view_app() -> App {
        App {
            mode: Mode::Channel,
            channel: Some(ChannelView::new("UC1".to_string(), "One".to_string())),
            ..App::default()
        }
    }

    /// OauthDone を 1 件受け取る。
    async fn next_oauth(
        rx: &mut mpsc::UnboundedReceiver<AppEvent>,
        session: &mut Session,
    ) -> Option<AppEvent> {
        let task = session.oauth_task.take()?;
        task.await.expect("タスクは panic しない");
        rx.try_recv().ok()
    }

    #[tokio::test]
    async fn subscribing_announces_the_wait_and_reports_the_result() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = channel_view_app();
        let deps = oauth_deps(
            "subscribe",
            vec![
                Ok(crate::oauth::Response {
                    status: 200,
                    body: OAUTH_ACCESS_JSON.to_string(),
                }),
                Ok(crate::oauth::Response {
                    status: 204,
                    body: String::new(),
                }),
            ],
        );
        let backend = deps.backend.clone();

        subscribe_channel(&mut app, &tx, &mut session, deps);
        assert_eq!(app.notice.as_deref(), Some(crate::oauth::SUBSCRIBE_NOTICE));
        let nonce = session.oauth_nonce;

        let event = next_oauth(&mut rx, &mut session).await.expect("結果が届く");
        let AppEvent::OauthDone { nonce: got, result } = event else {
            panic!("OauthDone でない");
        };
        assert_eq!(got, nonce);
        assert_eq!(result.expect("通る"), crate::oauth::SUBSCRIBED_NOTICE);
        assert!(
            backend.opened().is_empty(),
            "保存済みトークンではブラウザを開かない"
        );
        let calls = backend.calls();
        assert_eq!(
            calls[1].url(),
            format!("{}?part=snippet", crate::oauth::SUBSCRIPTIONS_ENDPOINT)
        );
    }

    #[tokio::test]
    async fn subscribing_without_a_channel_does_nothing() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = App::default();

        subscribe_channel(&mut app, &tx, &mut session, hanging_oauth("no-channel"));

        assert!(session.oauth_task.is_none());
        assert!(app.notice.is_none());
    }

    #[tokio::test]
    async fn subscribing_to_an_id_does_not_need_the_channel_view() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        // 再生中は app.channel に無関係な値しか入らないので、ID を直接渡す経路を使う。
        let mut app = playing_app();

        subscribe_to(
            &mut app,
            &tx,
            &mut session,
            "UC9".to_string(),
            hanging_oauth("subscribe-to"),
        );

        assert_eq!(app.notice.as_deref(), Some(crate::oauth::SUBSCRIBE_NOTICE));
        assert_eq!(
            session.oauth_action,
            Some(crate::oauth::Action::Subscribe("UC9".to_string()))
        );
    }

    #[tokio::test]
    async fn subscribing_while_playing_uses_the_channel_of_the_video() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = App {
            playback: Playback {
                channel_id: Some("UC2".to_string()),
                ..Playback::default()
            },
            ..playing_app()
        };

        subscribe_playing_channel(
            &mut app,
            &tx,
            &mut session,
            hanging_oauth("playing-channel"),
        );

        assert_eq!(
            session.oauth_action,
            Some(crate::oauth::Action::Subscribe("UC2".to_string()))
        );
    }

    #[tokio::test]
    async fn subscribing_while_playing_without_a_channel_id_does_nothing() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = playing_app();

        subscribe_playing_channel(&mut app, &tx, &mut session, hanging_oauth("playing-no-id"));

        assert!(session.oauth_task.is_none());
        assert!(app.notice.is_none());
    }

    #[tokio::test]
    async fn liking_sends_the_playing_video_id() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = App {
            mode: Mode::Playing,
            playback: Playback {
                url: "https://www.youtube.com/watch?v=vid1".to_string(),
                ..Playback::default()
            },
            ..App::default()
        };
        let deps = oauth_deps(
            "like",
            vec![
                Ok(crate::oauth::Response {
                    status: 200,
                    body: OAUTH_ACCESS_JSON.to_string(),
                }),
                Ok(crate::oauth::Response {
                    status: 204,
                    body: String::new(),
                }),
            ],
        );
        let backend = deps.backend.clone();

        like_video(&mut app, &tx, &mut session, deps);
        assert_eq!(app.notice.as_deref(), Some(crate::oauth::LIKE_NOTICE));

        let event = next_oauth(&mut rx, &mut session).await.expect("結果が届く");
        let AppEvent::OauthDone { result, .. } = event else {
            panic!("OauthDone でない");
        };
        assert_eq!(result.expect("通る"), crate::oauth::LIKED_NOTICE);
        assert_eq!(
            backend.calls()[1].url(),
            format!("{}?id=vid1&rating=like", crate::oauth::RATE_ENDPOINT)
        );
    }

    #[tokio::test]
    async fn liking_without_a_playing_url_does_nothing() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = playing_app();

        like_video(&mut app, &tx, &mut session, hanging_oauth("no-url"));

        assert!(session.oauth_task.is_none());
        assert!(app.notice.is_none());
    }

    #[tokio::test]
    async fn a_failure_comes_back_as_an_error_result() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = channel_view_app();
        let deps = oauth_deps(
            "failure",
            vec![
                Ok(crate::oauth::Response {
                    status: 200,
                    body: OAUTH_ACCESS_JSON.to_string(),
                }),
                Ok(crate::oauth::Response {
                    status: 403,
                    body: r#"{"error":{"message":"quota","errors":[{"reason":"quotaExceeded"}]}}"#
                        .to_string(),
                }),
            ],
        );

        subscribe_channel(&mut app, &tx, &mut session, deps);
        let event = next_oauth(&mut rx, &mut session).await.expect("結果が届く");
        let AppEvent::OauthDone { result, .. } = event else {
            panic!("OauthDone でない");
        };
        assert!(result.expect_err("失敗").contains("403"));
    }

    #[tokio::test]
    async fn starting_another_one_drops_the_running_send() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = channel_view_app();

        subscribe_channel(&mut app, &tx, &mut session, hanging_oauth("first"));
        let first = session.oauth_nonce;
        assert!(session.oauth_task.is_some());

        subscribe_channel(&mut app, &tx, &mut session, hanging_oauth("second"));

        assert_ne!(session.oauth_nonce, first, "世代が進む");
        assert!(session.oauth_task.is_some());
    }

    #[tokio::test]
    async fn leaving_the_channel_drops_the_waiting_authorization() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = channel_view_app();

        subscribe_channel(&mut app, &tx, &mut session, hanging_oauth("leave"));
        let nonce = session.oauth_nonce;
        assert!(session.oauth_task.is_some());

        leave_channel(&mut app, &mut session);

        // 「…中」だけ消えて待ち続ける状態を残さない。
        assert!(session.oauth_task.is_none(), "認証待ちも畳む");
        assert_ne!(session.oauth_nonce, nonce, "世代が進む");
        assert!(app.notice.is_none());
    }

    #[tokio::test]
    async fn a_missing_config_location_is_reported_without_starting_anything() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = channel_view_app();

        subscribe_channel(
            &mut app,
            &tx,
            &mut session,
            Oauth {
                backend: FakeBackend::new(),
                paths: None,
            },
        );

        assert_eq!(app.error.as_deref(), Some(crate::oauth::NO_CONFIG_PATH));
        assert!(session.oauth_task.is_none());
        assert!(app.notice.is_none());
    }

    // ---- いいね済み/登録済みの状態確認 ----

    const LIKED_PAGE_JSON: &str = r#"{"items":[{"id":"v1"}]}"#;
    const SUBSCRIBED_JSON: &str = r#"{"items":[{"snippet":{"resourceId":{"channelId":"UC1"}}}]}"#;
    const NO_ITEMS_JSON: &str = r#"{"items":[]}"#;

    fn response(status: u16, body: &str) -> Result<crate::oauth::Response, String> {
        Ok(crate::oauth::Response {
            status,
            body: body.to_string(),
        })
    }

    fn ids(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| v.to_string()).collect()
    }

    fn unix(secs: u64) -> std::time::SystemTime {
        std::time::UNIX_EPOCH + Duration::from_secs(secs)
    }

    /// 保存済みトークンを消した置き場。まだ認証していない状態。
    fn paths_without_token(name: &str) -> crate::oauth::Paths {
        let paths = oauth_paths(name);
        std::fs::remove_file(&paths.token).expect("消せる");
        paths
    }

    /// 渡した channel_id を持つ行が並んだ一覧。
    fn engagement_app(channel_ids: &[&str]) -> App {
        let results = channel_ids
            .iter()
            .enumerate()
            .map(|(i, channel_id)| SearchResult {
                id: format!("v{i}"),
                title: format!("title {i}"),
                duration: None,
                uploader: None,
                channel_id: Some(channel_id.to_string()),
            })
            .collect();
        App {
            results,
            ..App::default()
        }
    }

    /// EngagementReady を 1 件受け取る。届かなければ None。
    async fn next_engagement(
        rx: &mut mpsc::UnboundedReceiver<AppEvent>,
        session: &mut Session,
    ) -> Option<AppEvent> {
        let task = session.engagement_task.take()?;
        task.await.expect("タスクは panic しない");
        rx.try_recv().ok()
    }

    #[tokio::test]
    async fn the_state_check_asks_for_the_liked_list_and_the_visible_channels() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = engagement_app(&["UC1", "UC2"]);
        let deps = oauth_deps(
            "engagement",
            vec![
                response(200, OAUTH_ACCESS_JSON),
                response(200, LIKED_PAGE_JSON),
                response(200, SUBSCRIBED_JSON),
            ],
        );
        let backend = deps.backend.clone();

        start_engagement_with(&mut app, &tx, &mut session, deps, unix(1_000));

        let event = next_engagement(&mut rx, &mut session).await.expect("届く");
        let AppEvent::EngagementReady {
            nonce,
            liked_videos,
            asked_channels,
            subscribed_channels,
        } = event
        else {
            panic!("EngagementReady でない");
        };
        assert_eq!(nonce, session.search_nonce);
        assert_eq!(liked_videos, Some(ids(&["v1"])));
        assert_eq!(asked_channels, ids(&["UC1", "UC2"]));
        assert_eq!(subscribed_channels, ids(&["UC1"]));
        assert!(
            backend.opened().is_empty(),
            "背景の確認でブラウザを開かない"
        );
        let calls = backend.calls();
        assert_eq!(calls.len(), 3, "トークン・いいね一覧・登録確認");
        assert!(
            calls[1].url().contains("myRating=like"),
            "{}",
            calls[1].url()
        );
        assert!(
            calls[2].url().ends_with("forChannelId=UC1,UC2"),
            "{}",
            calls[2].url()
        );
    }

    #[tokio::test]
    async fn the_state_check_does_not_run_when_the_marks_are_off() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = engagement_app(&["UC1"]);
        app.settings.engagement.enabled = false;

        start_engagement_with(
            &mut app,
            &tx,
            &mut session,
            hanging_oauth("engagement-off"),
            unix(1_000),
        );

        assert!(session.engagement_task.is_none());
    }

    #[tokio::test]
    async fn nothing_is_asked_while_the_cache_is_still_fresh() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = engagement_app(&["UC1"]);
        app.engagement.replace_liked(ids(&["v1"]), unix(1_000));
        app.engagement
            .remember_channels(&ids(&["UC1"]), &ids(&["UC1"]), unix(1_000));

        start_engagement_with(
            &mut app,
            &tx,
            &mut session,
            hanging_oauth("engagement-fresh"),
            unix(1_000),
        );

        assert!(session.engagement_task.is_none());
    }

    #[tokio::test]
    async fn only_what_needs_confirming_is_asked_again() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = engagement_app(&["UC1", "UC2"]);
        // いいね一覧と UC1 は確認済み。残りは UC2 だけ。
        app.engagement.replace_liked(ids(&["v1"]), unix(1_000));
        app.engagement
            .remember_channels(&ids(&["UC1"]), &ids(&["UC1"]), unix(1_000));
        let deps = oauth_deps(
            "engagement-partial",
            vec![
                response(200, OAUTH_ACCESS_JSON),
                response(200, NO_ITEMS_JSON),
            ],
        );
        let backend = deps.backend.clone();

        start_engagement_with(&mut app, &tx, &mut session, deps, unix(1_000));

        let event = next_engagement(&mut rx, &mut session).await.expect("届く");
        let AppEvent::EngagementReady {
            liked_videos,
            asked_channels,
            subscribed_channels,
            ..
        } = event
        else {
            panic!("EngagementReady でない");
        };
        assert_eq!(liked_videos, None, "いいね一覧は取り直さない");
        assert_eq!(asked_channels, ids(&["UC2"]));
        assert!(subscribed_channels.is_empty());
        let calls = backend.calls();
        assert_eq!(calls.len(), 2, "トークンと登録確認だけ");
        assert!(
            calls[1].url().ends_with("forChannelId=UC2"),
            "{}",
            calls[1].url()
        );
    }

    #[tokio::test]
    async fn while_browsing_a_channel_only_that_channel_is_checked() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = channel_view_app();
        let deps = oauth_deps(
            "engagement-channel",
            vec![
                response(200, OAUTH_ACCESS_JSON),
                response(200, LIKED_PAGE_JSON),
                response(200, SUBSCRIBED_JSON),
            ],
        );

        start_engagement_with(&mut app, &tx, &mut session, deps, unix(1_000));

        let event = next_engagement(&mut rx, &mut session).await.expect("届く");
        let AppEvent::EngagementReady { asked_channels, .. } = event else {
            panic!("EngagementReady でない");
        };
        assert_eq!(asked_channels, ids(&["UC1"]));
    }

    #[tokio::test]
    async fn a_failed_state_check_stays_quiet() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = engagement_app(&[]);
        let deps = oauth_deps(
            "engagement-failure",
            vec![
                response(200, OAUTH_ACCESS_JSON),
                response(
                    403,
                    r#"{"error":{"message":"quota","errors":[{"reason":"quotaExceeded"}]}}"#,
                ),
            ],
        );

        start_engagement_with(&mut app, &tx, &mut session, deps, unix(1_000));

        assert!(
            next_engagement(&mut rx, &mut session).await.is_none(),
            "印は飾りなので、失敗は画面に出さない"
        );
        assert!(app.error.is_none());
        assert!(app.notice.is_none());
    }

    #[tokio::test]
    async fn without_a_saved_token_the_state_check_does_not_open_a_browser() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = engagement_app(&["UC1"]);
        let deps = Oauth {
            backend: FakeBackend::new(),
            paths: Some(paths_without_token("engagement-no-token")),
        };
        let backend = deps.backend.clone();

        start_engagement_with(&mut app, &tx, &mut session, deps, unix(1_000));

        assert!(next_engagement(&mut rx, &mut session).await.is_none());
        assert!(backend.opened().is_empty(), "印のために認可を求めない");
        assert!(backend.calls().is_empty());
    }

    #[tokio::test]
    async fn a_missing_config_location_skips_the_state_check_silently() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = engagement_app(&["UC1"]);

        start_engagement_with(
            &mut app,
            &tx,
            &mut session,
            Oauth {
                backend: FakeBackend::new(),
                paths: None,
            },
            unix(1_000),
        );

        assert!(session.engagement_task.is_none());
        assert!(app.error.is_none(), "操作していないので理由を出さない");
    }

    /// フィード系の 1 本ぶんの非 flat 出力。
    const LOOKUP_JSON: &str =
        r#"{"id":"id1","title":"title id1","channel_id":"UCfeed","uploader":"Feed Channel"}"#;

    /// ChannelLookupDone を 1 件受け取る。届かなければ None。
    async fn next_channel_lookup(
        rx: &mut mpsc::UnboundedReceiver<AppEvent>,
        session: &mut Session,
    ) -> Option<AppEvent> {
        let task = session.channel_lookup_task.take()?;
        task.await.expect("タスクは panic しない");
        rx.try_recv().ok()
    }

    /// 返らないチャンネル引きを積んで、その世代を返す。
    fn pending_lookup(app: &mut App, tx: &UnboundedSender<AppEvent>, session: &mut Session) -> u64 {
        open_channel_with(app, tx, session, FakeYtDlp::new([Step::Hang]));
        assert!(session.channel_lookup_task.is_some(), "引きが積まれている");
        session.channel_lookup_nonce
    }

    #[tokio::test]
    async fn open_channel_looks_the_channel_up_when_the_row_has_no_id() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        // フィード系の行は channel_id を欠くので、その 1 本だけ取りに行く。
        let mut app = grid_app(2);
        app.selected = 1;
        let runner = FakeYtDlp::new([done(0, LOOKUP_JSON, "")]);
        let searching = session.search_nonce;

        open_channel_with(&mut app, &tx, &mut session, runner.clone());

        assert!(app.channel.is_none(), "取れるまでは移らない");
        assert_eq!(app.mode, Mode::Results);
        assert_eq!(app.notice.as_deref(), Some(CHANNEL_LOOKUP_NOTICE));
        assert!(
            app.notice_until.is_none(),
            "引きが返るまで待つので期限では消さない"
        );
        assert_eq!(session.search_nonce, searching, "走っている検索は止めない");

        let event = next_channel_lookup(&mut rx, &mut session)
            .await
            .expect("届く");
        let AppEvent::ChannelLookupDone {
            nonce,
            video_id,
            result,
        } = event
        else {
            panic!("ChannelLookupDone のはず");
        };
        assert_eq!(nonce, session.channel_lookup_nonce);
        assert_eq!(video_id, "id1");
        assert_eq!(
            result,
            Ok(Some(ChannelRef {
                id: "UCfeed".to_string(),
                // 行が名前を持たなくても、引いた行から拾える。
                uploader: Some("Feed Channel".to_string()),
            }))
        );
        assert_eq!(
            runner.calls(),
            [["https://www.youtube.com/watch?v=id1", "--dump-json"]],
            "選んだ 1 本だけを非 flat で取る"
        );
    }

    #[tokio::test]
    async fn open_channel_skips_the_lookup_when_the_row_already_has_the_id() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = channel_grid_app();

        open_channel_with(
            &mut app,
            &tx,
            &mut session,
            FakeYtDlp::new([done(0, "", "")]),
        );

        assert!(session.channel_lookup_task.is_none(), "引き直さない");
        assert_eq!(app.mode, Mode::Channel);
        assert_eq!(app.channel.as_ref().expect("channel").channel_id, "UCone");
    }

    #[tokio::test]
    async fn open_channel_does_nothing_without_a_selected_result() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = App::default();

        open_channel_with(&mut app, &tx, &mut session, StubYtDlp);

        assert!(app.channel.is_none());
        assert!(session.channel_lookup_task.is_none());
        assert!(app.notice.is_none());
    }

    #[tokio::test]
    async fn pressing_the_channel_key_again_abandons_the_first_lookup() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = grid_app(2);
        let first = pending_lookup(&mut app, &tx, &mut session);

        app.selected = 1;
        open_channel_with(&mut app, &tx, &mut session, FakeYtDlp::new([Step::Hang]));

        // 世代が変わるので、先に出した引きが返っても新しい行には効かない。
        assert_ne!(session.channel_lookup_nonce, first);
    }

    #[tokio::test]
    async fn entering_a_channel_opens_its_video_tab() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = grid_app(2);
        app.selected = 1;
        let runner = FakeYtDlp::new([done(0, "", "")]);

        enter_channel_with(
            &mut app,
            &tx,
            &mut session,
            "UCfeed".to_string(),
            "Feed Channel".to_string(),
            runner.clone(),
        );

        assert_eq!(app.mode, Mode::Channel);
        let channel = app.channel.as_ref().expect("チャンネルへ移る");
        assert_eq!(channel.channel_id, "UCfeed");
        assert_eq!(channel.channel_title, "Feed Channel");
        assert_eq!(channel.tab, ChannelTab::Videos);
        assert_eq!(app.tabs.state().selected, 1, "戻る位置を控える");

        finish_search(&mut session).await;
        assert_eq!(
            runner.calls()[0][0],
            "https://www.youtube.com/channel/UCfeed/videos"
        );
    }

    #[tokio::test]
    async fn switching_tabs_drops_a_running_channel_lookup() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = grid_app(2);
        let running = pending_lookup(&mut app, &tx, &mut session);

        switch_tab_with(&mut app, &tx, &mut session, true, StubYtDlp);

        assert!(session.channel_lookup_task.is_none());
        assert_ne!(session.channel_lookup_nonce, running);
    }

    #[tokio::test]
    async fn selecting_a_tab_drops_a_running_channel_lookup() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = grid_app(2);
        let running = pending_lookup(&mut app, &tx, &mut session);

        select_tab_with(&mut app, &tx, &mut session, 1, StubYtDlp);

        assert!(session.channel_lookup_task.is_none());
        assert_ne!(session.channel_lookup_nonce, running);
    }

    #[tokio::test]
    async fn starting_a_search_drops_a_running_channel_lookup() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = grid_app(2);
        app.query = QueryEditor::from("q");
        let running = pending_lookup(&mut app, &tx, &mut session);

        start_search_with(&mut app, &tx, &mut session, StubYtDlp);

        assert!(session.channel_lookup_task.is_none());
        assert_ne!(session.channel_lookup_nonce, running);
    }

    #[tokio::test]
    async fn entering_a_channel_drops_a_running_channel_lookup() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        // 1 行目は channel_id を欠き、2 行目は持っている。
        let mut app = channel_grid_app();
        app.set_results(
            vec![
                result("id0"),
                SearchResult {
                    channel_id: Some("UCtwo".to_string()),
                    ..result("id1")
                },
            ],
            &Target::Search("q".to_string()),
        );
        let running = pending_lookup(&mut app, &tx, &mut session);

        app.selected = 1;
        open_channel_with(&mut app, &tx, &mut session, StubYtDlp);

        assert_eq!(app.mode, Mode::Channel);
        assert!(session.channel_lookup_task.is_none());
        assert_ne!(session.channel_lookup_nonce, running);
    }

    /// チャンネル一覧の中でチャンネル引きが走っている状態。一覧の行は channel_id を欠く。
    fn channel_app_with_lookup(
        tx: &UnboundedSender<AppEvent>,
        session: &mut Session,
    ) -> (App, u64) {
        let mut app = channel_grid_app();
        open_channel_with(&mut app, tx, session, StubYtDlp);
        app.set_results(vec![result("v0")], &channel_target(&app));
        let running = pending_lookup(&mut app, tx, session);
        (app, running)
    }

    #[tokio::test]
    async fn switching_channel_tabs_drops_a_running_channel_lookup() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let (mut app, running) = channel_app_with_lookup(&tx, &mut session);

        switch_channel_tab_with(&mut app, &tx, &mut session, true, StubYtDlp);

        assert!(session.channel_lookup_task.is_none());
        assert_ne!(session.channel_lookup_nonce, running);
    }

    #[tokio::test]
    async fn selecting_a_channel_tab_drops_a_running_channel_lookup() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let (mut app, running) = channel_app_with_lookup(&tx, &mut session);

        select_channel_tab_with(&mut app, &tx, &mut session, 2, StubYtDlp);

        assert!(session.channel_lookup_task.is_none());
        assert_ne!(session.channel_lookup_nonce, running);
    }

    #[tokio::test]
    async fn leaving_a_channel_drops_a_running_channel_lookup() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let (mut app, running) = channel_app_with_lookup(&tx, &mut session);

        leave_channel(&mut app, &mut session);

        assert!(session.channel_lookup_task.is_none());
        assert_ne!(session.channel_lookup_nonce, running);
    }

    #[tokio::test]
    async fn switching_channel_tabs_searches_each_tab_once() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = channel_grid_app();
        let runner = FakeYtDlp::new([done(0, "", ""), done(0, "", "")]);

        open_channel_with(&mut app, &tx, &mut session, runner.clone());
        finish_search(&mut session).await;
        app.set_results(vec![result("v0"), result("v1")], &channel_target(&app));
        app.set_view_selected(1);

        switch_channel_tab_with(&mut app, &tx, &mut session, true, runner.clone());
        assert_eq!(
            app.channel.as_ref().expect("channel").tab,
            ChannelTab::Shorts
        );
        assert!(session.search_task.is_some(), "未読のタブは検索する");
        finish_search(&mut session).await;
        assert_eq!(
            runner.calls()[1][0],
            "https://www.youtube.com/channel/UCone/shorts"
        );
        app.set_results(vec![result("s0")], &channel_target(&app));

        switch_channel_tab_with(&mut app, &tx, &mut session, false, runner.clone());
        assert_eq!(
            app.channel.as_ref().expect("channel").tab,
            ChannelTab::Videos
        );
        assert!(
            session.search_task.is_none(),
            "読み込み済みのタブは投げ直さない"
        );
        assert_eq!(app.view_result_ids(), ["v0", "v1"]);
        assert_eq!(app.view_selected(), 1, "タブごとの選択を持ち越す");
    }

    #[tokio::test]
    async fn reloading_a_channel_tab_searches_it_again() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = channel_grid_app();
        open_channel_with(&mut app, &tx, &mut session, StubYtDlp);
        session.search_task.take().expect("タスク").abort();
        // 取れなかったタブも読み込み済みで残ることがある。そこからでも取り直せる。
        app.set_results(Vec::new(), &channel_target(&app));
        assert!(app.channel.as_ref().expect("channel").state().loaded);

        reload_channel_tab_with(&mut app, &tx, &mut session, StubYtDlp);

        assert!(session.search_task.is_some(), "同じタブを取りに行く");
        assert!(!app.channel.as_ref().expect("channel").state().loaded);
        assert!(app.searching);
    }

    #[tokio::test]
    async fn reloading_does_nothing_outside_a_channel() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = channel_grid_app();

        reload_channel_tab_with(&mut app, &tx, &mut session, StubYtDlp);

        assert!(session.search_task.is_none());
        assert!(!app.searching);
    }

    #[tokio::test]
    async fn switching_channel_tabs_drops_the_running_search() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = channel_grid_app();
        open_channel_with(&mut app, &tx, &mut session, StubYtDlp);
        let running = session.search_nonce;

        switch_channel_tab_with(&mut app, &tx, &mut session, true, StubYtDlp);
        assert_ne!(
            session.search_nonce, running,
            "先に走っていた検索を移った先のタブへ書き込ませない"
        );
        assert!(app.searching, "移った先の検索が走る");
    }

    #[tokio::test]
    async fn selecting_a_channel_tab_by_position_ignores_the_ones_that_do_not_exist() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = channel_grid_app();
        open_channel_with(&mut app, &tx, &mut session, StubYtDlp);
        session.search_task.take().expect("タスク").abort();

        select_channel_tab_with(&mut app, &tx, &mut session, 2, StubYtDlp);
        assert_eq!(
            app.channel.as_ref().expect("channel").tab,
            ChannelTab::Streams
        );
        assert!(session.search_task.is_some());
        let running = session.search_nonce;

        select_channel_tab_with(&mut app, &tx, &mut session, 9, StubYtDlp);
        assert_eq!(
            app.channel.as_ref().expect("channel").tab,
            ChannelTab::Streams,
            "範囲外では動かさない"
        );
        assert_eq!(session.search_nonce, running, "走っている検索も止めない");
    }

    #[tokio::test]
    async fn leaving_the_channel_returns_to_the_search_results() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = channel_grid_app();
        app.selected = 1;
        open_channel_with(&mut app, &tx, &mut session, StubYtDlp);
        let running = session.search_nonce;
        app.set_results(vec![result("v0")], &channel_target(&app));

        leave_channel(&mut app, &mut session);

        assert!(app.channel.is_none());
        assert_eq!(app.mode, Mode::Results);
        assert_eq!(app.view_result_ids(), ["id0", "id1"]);
        assert_eq!(app.view_selected(), 1, "元の選択へ戻る");
        assert_ne!(session.search_nonce, running, "走らせたままにしない");
        assert!(!app.searching);
    }

    #[tokio::test]
    async fn moving_the_selection_stays_inside_the_channel_tab() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = channel_grid_app();
        open_channel_with(&mut app, &tx, &mut session, StubYtDlp);
        let videos: Vec<SearchResult> = (0..6).map(|i| result(&format!("v{i}"))).collect();
        app.set_results(videos, &channel_target(&app));

        move_selection_with(&mut app, CELL, Dir::Right);
        assert_eq!(app.view_selected(), 1);
        move_selection_with(&mut app, CELL, Dir::Down);
        assert_eq!(app.view_selected(), 5, "格子の下の行へ");
        assert_eq!(app.selected, 0, "検索結果側の選択は触らない");
    }

    #[tokio::test]
    async fn the_channel_thumbnails_are_fetched_for_the_channel_videos() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = channel_grid_app();
        open_channel_with(&mut app, &tx, &mut session, StubYtDlp);
        app.set_results(vec![result("v0"), result("v1")], &channel_target(&app));

        assert_eq!(
            app.thumbs.wanted(&app.view_result_ids(), (144, 80)),
            ["v0", "v1"]
        );
    }

    #[tokio::test]
    async fn end_playback_returns_to_the_channel_it_started_from() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = channel_grid_app();
        open_channel_with(&mut app, &tx, &mut session, StubYtDlp);
        app.set_results(vec![result("v0")], &channel_target(&app));
        app.mode = Mode::Playing;
        app.video = Some(sink());

        end_playback(&mut app, &mut session, None).await;
        assert_eq!(app.mode, Mode::Channel, "チャンネル一覧へ戻る");
        assert!(app.channel.is_some());
    }

    /// 今のチャンネルタブの target。set_results へ渡す。
    fn channel_target(app: &App) -> Target {
        app.channel.as_ref().expect("channel").target()
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
            query: QueryEditor::from("ラーメン"),
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
    async fn select_tab_jumps_to_the_given_tab_and_searches_it_once() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = App::default();

        select_tab_with(&mut app, &tx, &mut session, 3, StubYtDlp);
        assert_eq!(app.tabs.selected(), 3, "隣ではなく押されたタブへ移る");
        assert!(session.search_task.is_some(), "未読のタブは検索する");
        assert!(app.searching);

        // 読み込み済みにして別のタブへ行き、戻ってきても検索し直さない。
        app.set_results(vec![result("a")], &Target::Search("ニュース".to_string()));
        session.search_task = None;
        app.searching = false;
        select_tab_with(&mut app, &tx, &mut session, 1, StubYtDlp);
        session.search_task = None;
        select_tab_with(&mut app, &tx, &mut session, 3, StubYtDlp);
        assert!(
            session.search_task.is_none(),
            "保持していた結果をそのまま出す"
        );
        assert_eq!(app.result_ids(), ["a"]);
    }

    #[tokio::test]
    async fn select_tab_drops_the_running_search_of_the_tab_it_leaves() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = grid_app(1);
        select_tab_with(&mut app, &tx, &mut session, 2, StubYtDlp);
        let running = session.search_nonce;
        assert!(session.search_task.is_some());

        select_tab_with(&mut app, &tx, &mut session, 0, StubYtDlp);
        assert!(app.tabs.is_all());
        assert!(session.search_task.is_none(), "走らせたままにしない");
        assert!(!app.searching);
        assert_ne!(
            session.search_nonce, running,
            "先に走っていた検索の結果を移った先のタブへ書き込ませない"
        );
        assert_eq!(app.result_ids(), ["id0"], "保持していた結果のまま");
    }

    #[tokio::test]
    async fn select_tab_leaves_the_current_tab_and_its_search_alone() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = App::default();
        select_tab_with(&mut app, &tx, &mut session, 2, StubYtDlp);
        let running = session.search_nonce;
        assert!(session.search_task.is_some());

        // 押し直しでは走っている検索を止めない。取り直すのは止まっているときだけ。
        select_tab_with(&mut app, &tx, &mut session, 2, StubYtDlp);
        assert_eq!(app.tabs.selected(), 2);
        assert!(session.search_task.is_some());
        assert_eq!(session.search_nonce, running);
        assert!(app.searching);
    }

    #[tokio::test]
    async fn select_tab_retries_a_refused_tab_when_it_is_clicked_again() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = App::default();
        let index = app
            .tabs
            .labels()
            .iter()
            .position(|it| *it == "おすすめ")
            .expect("おすすめ");

        // cookie が無いので断られる。読み込み済みにはならない。
        select_tab_with(&mut app, &tx, &mut session, index, StubYtDlp);
        assert!(session.search_task.is_none());
        assert!(app.error.is_some());
        assert!(!app.tabs.state().loaded);

        // cookie を入れて同じタブを押し直す。Input モードでは r が打てないので、
        // クリックで取り直せないと手が無くなる。
        app.cookies = CookieState::Armed(CookieSource::from_spec(Some("chrome")).expect("spec"));
        select_tab_with(&mut app, &tx, &mut session, index, StubYtDlp);
        assert!(session.search_task.is_some(), "断られたタブを取り直す");
        assert!(app.searching);
        assert!(app.error.is_none());
    }

    #[tokio::test]
    async fn select_tab_does_not_search_a_loaded_tab_that_is_clicked_again() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = grid_app(2);
        assert!(app.tabs.state().loaded);
        let current = app.tabs.selected();

        select_tab_with(&mut app, &tx, &mut session, current, StubYtDlp);
        assert!(
            session.search_task.is_none(),
            "読み込み済みなら投げ直さない"
        );
        assert!(!app.searching);
        assert_eq!(app.result_ids(), ["id0", "id1"]);
    }

    #[tokio::test]
    async fn select_tab_ignores_an_index_that_does_not_exist() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = App::default();
        select_tab_with(&mut app, &tx, &mut session, 1, StubYtDlp);
        let running = session.search_nonce;

        let outside = app.tabs.labels().len();
        select_tab_with(&mut app, &tx, &mut session, outside, StubYtDlp);
        assert_eq!(app.tabs.selected(), 1, "選択は動かさない");
        assert!(session.search_task.is_some(), "走っている検索も止めない");
        assert_eq!(session.search_nonce, running);
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
        app.query.set("ラーメン");
        reload_tab_with(&mut app, &tx, &mut session, StubYtDlp);
        let running = session.search_nonce;
        assert!(session.search_task.is_some());

        // 検索ボックスが空の「すべて」タブは検索を作れない。それでも先行分は止める。
        app.query.set("");
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
    fn entering_playback_keeps_the_url_and_id_of_the_video() {
        let mut app = grid_app(4);
        let mut session = Session::default();
        enter_playback(
            &mut app,
            &mut session,
            "song".to_string(),
            URL.to_string(),
            "id0".to_string(),
            None,
            sink(),
        );
        assert_eq!(app.playback.url, URL);
        assert_eq!(app.playback.id, "id0");
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
            "id0".to_string(),
            None,
            sink(),
        );

        assert_eq!(app.mode, Mode::Playing);
        assert!(app.video.is_some());
        assert_eq!(app.playback.title, "song");
        // 貼ってあるサムネイルは 2J でしか消えない。終了側と同じく持ち越して消す。
        assert!(session.owe_clear, "再生画面の上にサムネイルを残さない");
    }

    const SID_AUTO: &str = "{\"command\":[\"set_property\",\"sid\",\"auto\"]}\n";
    const SID_NO: &str = "{\"command\":[\"set_property\",\"sid\",\"no\"]}\n";

    fn subtitles_off_app() -> App {
        let mut app = playing_app();
        app.subtitles.set_wanted(false, std::time::Instant::now());
        app
    }

    #[tokio::test]
    async fn toggling_subtitles_sends_sid_auto_then_sid_no() {
        let mut app = subtitles_off_app();
        let mut session = Session::default();
        let sent = record(&mut session, Ok(()));
        let now = std::time::Instant::now();

        toggle_subtitles(&mut app, &mut session, now).await;
        assert!(app.subtitles.wanted());
        assert_eq!(app.notice.as_deref(), Some("字幕を出します (ja-orig)"));

        toggle_subtitles(&mut app, &mut session, now).await;
        assert!(!app.subtitles.wanted());
        assert_eq!(app.notice.as_deref(), Some("字幕を消しました"));

        // mpv は再起動しない。sub-visibility は起動引数だけで足りる。
        assert_eq!(lines(&sent), [SID_AUTO, SID_NO]);
    }

    #[tokio::test]
    async fn toggling_subtitles_keeps_the_state_when_sending_fails() {
        let mut app = subtitles_off_app();
        let mut session = Session::default();
        let _sent = record(&mut session, Err("パイプが閉じました".to_string()));

        toggle_subtitles(&mut app, &mut session, std::time::Instant::now()).await;
        assert!(!app.subtitles.wanted());
        assert_eq!(app.error.as_deref(), Some("パイプが閉じました"));
    }

    #[tokio::test]
    async fn toggling_subtitles_without_a_player_changes_nothing() {
        let mut app = subtitles_off_app();
        let mut session = Session::default();

        toggle_subtitles(&mut app, &mut session, std::time::Instant::now()).await;
        assert!(!app.subtitles.wanted());
        assert!(app.error.is_none());
        assert!(app.notice.is_none());
    }

    #[tokio::test]
    async fn a_disabled_setting_only_says_why_nothing_happens() {
        let mut app = playing_app();
        app.settings.subtitles.enabled = false;
        app.subtitles = crate::subtitles::SubtitleState::from_settings(&app.settings.subtitles);
        let mut session = Session::default();
        let sent = record(&mut session, Ok(()));

        toggle_subtitles(&mut app, &mut session, std::time::Instant::now()).await;
        assert!(lines(&sent).is_empty(), "mpv へは何も送らない");
        let notice = app.notice.as_deref().unwrap_or_default();
        assert!(notice.contains("[subtitles] enabled"), "{notice}");
        assert!(!app.subtitles.wanted());
    }

    /// 長さが取れた後、猶予を過ぎても sid が数値にならない = この動画に字幕が無い。
    fn missing_subtitle_app(now: std::time::Instant) -> App {
        let mut app = playing_app();
        app.playback.duration = Some(60.0);
        app.subtitles.begin_playback(now);
        app.subtitles.observe_loaded(true, now);
        app
    }

    #[tokio::test]
    async fn a_video_without_the_language_can_still_be_turned_off() {
        let now = std::time::Instant::now();
        let mut app = missing_subtitle_app(now);
        let mut session = Session::default();
        let sent = record(&mut session, Ok(()));

        toggle_subtitles(&mut app, &mut session, now + crate::subtitles::SELECT_GRACE).await;
        // 止めると要求が次の動画へ持ち越され、設定ファイルを直すまで消せなくなる。
        assert_eq!(lines(&sent), [SID_NO]);
        assert!(!app.subtitles.wanted());
        assert_eq!(
            app.notice.as_deref(),
            Some("この動画に ja-orig,ja の字幕がありません")
        );
    }

    #[tokio::test]
    async fn a_subtitle_dropped_outside_tuitube_comes_back_with_two_presses() {
        // 別ウィンドウで mpv 側の j / v を押されると sid が外れ、字幕なしと同じ見え方になる。
        let now = std::time::Instant::now();
        let mut app = missing_subtitle_app(now);
        let mut session = Session::default();
        let sent = record(&mut session, Ok(()));
        let late = now + crate::subtitles::SELECT_GRACE;

        toggle_subtitles(&mut app, &mut session, late).await;
        toggle_subtitles(&mut app, &mut session, late).await;
        assert_eq!(lines(&sent), [SID_NO, SID_AUTO]);
        assert!(app.subtitles.wanted());
    }

    #[tokio::test]
    async fn the_text_mode_says_that_mpv_does_not_draw_the_subtitle() {
        let mut app = subtitles_off_app();
        app.display = DisplayMode::Text;
        let mut session = Session::default();
        let sent = record(&mut session, Ok(()));

        toggle_subtitles(&mut app, &mut session, std::time::Instant::now()).await;
        // トラックは選ぶ (w で埋め込みへ戻れば出る)。
        assert_eq!(lines(&sent), [SID_AUTO]);
        assert!(app.subtitles.wanted());
        assert_eq!(
            app.notice.as_deref(),
            Some("テキスト表示では映像に字幕が出ません")
        );
    }

    #[test]
    fn the_launch_plan_starts_hidden_when_the_subtitle_is_off() {
        let mut app = grid_app(1);
        let (_video, shown) = playback_plan(&app);
        assert_eq!(
            shown.subtitles,
            crate::subtitles::SubtitleLaunch::new(&app.settings.subtitles, true)
        );
        assert!(
            !shown.args().iter().any(|a| a == "--sid=no"),
            "{:?}",
            shown.args()
        );

        app.subtitles.set_wanted(false, std::time::Instant::now());
        let (_video, hidden) = playback_plan(&app);
        assert!(
            hidden.args().iter().any(|a| a == "--sid=no"),
            "{:?}",
            hidden.args()
        );
        // 非表示でもトラックは用意させる。
        assert!(
            hidden
                .args()
                .iter()
                .any(|a| a == "--ytdl-raw-options-append=write-auto-subs="),
            "{:?}",
            hidden.args()
        );
    }

    #[test]
    fn entering_playback_drops_the_selection_and_keeps_the_wish() {
        let mut app = grid_app(4);
        let mut session = Session::default();
        app.subtitles.observe_sid(Some(&serde_json::json!(1)));
        enter_playback(
            &mut app,
            &mut session,
            "song".to_string(),
            URL.to_string(),
            "id0".to_string(),
            None,
            sink(),
        );

        assert!(app.subtitles.wanted(), "表示の希望は動画をまたいで持ち越す");
        assert_eq!(
            app.subtitles
                .status(&app.settings.subtitles, std::time::Instant::now()),
            crate::subtitles::SubtitleStatus::Loading,
            "前の動画の選択は捨てる"
        );
    }

    #[tokio::test]
    async fn the_subtitle_state_carries_over_to_the_next_video() {
        // 起動 → s で消す → 次の動画も消えたまま始まる。
        let mut app = grid_app(4);
        let mut session = Session::default();
        let sent = record(&mut session, Ok(()));
        let now = std::time::Instant::now();
        enter_playback(
            &mut app,
            &mut session,
            "song".to_string(),
            URL.to_string(),
            "id0".to_string(),
            None,
            sink(),
        );
        assert!(!playback_plan(&app).1.args().iter().any(|a| a == "--sid=no"));

        toggle_subtitles(&mut app, &mut session, now).await;
        let (_video, next) = playback_plan(&app);
        assert!(
            next.args().iter().any(|a| a == "--sid=no"),
            "{:?}",
            next.args()
        );

        toggle_subtitles(&mut app, &mut session, now).await;
        assert_eq!(lines(&sent), [SID_NO, SID_AUTO]);
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

    /// 既定のタブ列で `label` のタブへ送る。
    fn select_tab_by_label(
        app: &mut App,
        tx: &UnboundedSender<AppEvent>,
        session: &mut Session,
        label: &str,
    ) {
        let index = app
            .tabs
            .labels()
            .iter()
            .position(|it| *it == label)
            .unwrap_or_else(|| panic!("{label} のタブがない"));
        while app.tabs.selected() != index {
            switch_tab_with(app, tx, session, true, StubYtDlp);
        }
    }

    #[tokio::test]
    async fn a_cookie_feed_tab_is_refused_without_cookies() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        // 結果を見ている最中に Tab を押した場面。
        let mut app = App {
            mode: Mode::Results,
            ..App::default()
        };

        select_tab_by_label(&mut app, &tx, &mut session, "おすすめ");

        assert!(session.search_task.is_none(), "yt-dlp を起動しない");
        assert!(!app.searching);
        let error = app.error.clone().expect("理由を出す");
        assert!(error.contains("おすすめ"), "{error}");
        assert!(error.contains("[cookies] browser"), "{error}");
        // 結果が空の Mode::Input では Esc が終了になるので、断りでモードは変えない。
        assert_eq!(app.mode, Mode::Results);

        // 断られてもタブは送れる。次のタブへ抜けられずに詰まらない。
        switch_tab_with(&mut app, &tx, &mut session, true, StubYtDlp);
        assert_eq!(app.tabs.labels()[app.tabs.selected()], "履歴");
    }

    #[tokio::test]
    async fn a_cookie_feed_tab_is_refused_while_cookies_are_suspended() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = App {
            cookies: CookieState::Suspended {
                source: CookieSource::from_spec(Some("chrome")).expect("spec"),
                reason: "cookie を読めませんでした".to_string(),
            },
            ..App::default()
        };

        select_tab_by_label(&mut app, &tx, &mut session, "後で見る");

        assert!(session.search_task.is_none(), "yt-dlp を起動しない");
        let error = app.error.expect("理由を出す");
        assert!(error.contains("後で見る"), "{error}");
        assert!(error.contains("停止中"), "{error}");
        assert!(error.contains("cookie を読めませんでした"), "{error}");
    }

    #[tokio::test]
    async fn a_cookie_feed_tab_searches_when_cookies_are_set() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = App {
            cookies: CookieState::Armed(CookieSource::from_spec(Some("chrome")).expect("spec")),
            ..App::default()
        };

        select_tab_by_label(&mut app, &tx, &mut session, "履歴");

        assert!(session.search_task.is_some(), "フィードを取りに行く");
        assert!(app.searching);
        assert!(app.error.is_none());
    }

    #[tokio::test]
    async fn reload_tab_searches_the_current_tab_again() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = grid_app(3);
        assert!(app.tabs.state().loaded);
        // query が空だと「すべて」タブは検索できないので、語を入れておく。
        app.query.set("ラーメン");

        reload_tab_with(&mut app, &tx, &mut session, StubYtDlp);
        assert!(session.search_task.is_some());
        assert!(!app.tabs.state().loaded);
    }

    #[tokio::test]
    async fn reload_keeps_the_count_that_load_more_had_reached() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = grid_app(3);
        app.query.set("ラーメン");
        // 「もっと見る」で 20 件まで伸ばしていた状態を模す。
        app.tabs.state_mut().requested_limit = 20;
        let runner = FakeYtDlp::new([done(0, "", "")]);

        reload_tab_with(&mut app, &tx, &mut session, runner.clone());
        finish_search(&mut session).await;

        let args = runner.calls();
        assert_eq!(
            args[0][0], "ytsearch20:ラーメン",
            "取り直し(r)でも伸ばした件数のまま取りに行く"
        );
        let Some(AppEvent::SearchDone {
            requested_limit, ..
        }) = rx.recv().await
        else {
            panic!("SearchDone が届く");
        };
        assert_eq!(requested_limit, 20);
    }

    #[tokio::test]
    async fn spawn_search_reports_the_limit_it_actually_requested() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = App {
            query: QueryEditor::from("ラーメン"),
            ..App::default()
        };
        app.settings.search.limit = 7;

        start_search_with(
            &mut app,
            &tx,
            &mut session,
            FakeYtDlp::new([done(0, "", "")]),
        );
        finish_search(&mut session).await;

        let Some(AppEvent::SearchDone {
            requested_limit, ..
        }) = rx.recv().await
        else {
            panic!("SearchDone が届く");
        };
        assert_eq!(requested_limit, 7);
    }

    #[tokio::test]
    async fn load_more_does_nothing_before_a_tab_has_requested_a_count() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = grid_app(3);
        app.query.set("ラーメン");
        // まだ一度も取得しておらず requested_limit は 0 のまま。

        load_more_with(&mut app, &tx, &mut session, StubYtDlp);

        assert!(
            session.search_task.is_none(),
            "もっと見られない間は動かない"
        );
    }

    #[tokio::test]
    async fn load_more_does_nothing_while_results_still_fall_short_of_the_request() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = grid_app(3);
        app.query.set("ラーメン");
        // 要求 5 件に対し実際は 3 件しか返らなかった = それ以上は無いと分かっている。
        app.tabs.state_mut().requested_limit = 5;

        load_more_with(&mut app, &tx, &mut session, StubYtDlp);

        assert!(session.search_task.is_none());
    }

    #[tokio::test]
    async fn load_more_requests_the_shown_count_plus_the_setting_limit() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = grid_app(10);
        app.query.set("ラーメン");
        // 前回ちょうど 10 件を要求して 10 件満額で返ってきた状態。
        app.tabs.state_mut().requested_limit = 10;
        let runner = FakeYtDlp::new([done(0, "", "")]);

        load_more_with(&mut app, &tx, &mut session, runner.clone());
        assert!(app.searching);

        finish_search(&mut session).await;
        let args = runner.calls();
        assert_eq!(
            args[0][0], "ytsearch20:ラーメン",
            "表示中 10 件 + limit(10) で 20 件要求"
        );

        let Some(AppEvent::SearchDone {
            requested_limit, ..
        }) = rx.recv().await
        else {
            panic!("SearchDone が届く");
        };
        assert_eq!(requested_limit, 20);
    }

    #[tokio::test]
    async fn load_more_rounds_the_request_down_to_the_max_search_limit() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = grid_app(1);
        app.query.set("ラーメン");
        app.settings.search.limit = MAX_SEARCH_LIMIT;
        app.tabs.state_mut().requested_limit = 1;
        let runner = FakeYtDlp::new([done(0, "", "")]);

        load_more_with(&mut app, &tx, &mut session, runner.clone());

        finish_search(&mut session).await;
        let args = runner.calls();
        assert_eq!(args[0][0], format!("ytsearch{MAX_SEARCH_LIMIT}:ラーメン"));
    }

    #[tokio::test]
    async fn load_more_does_nothing_outside_a_search_tab() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = App::default();
        let feed_index = app
            .tabs
            .labels()
            .iter()
            .position(|label| *label == "履歴")
            .expect("履歴タブがある");
        app.tabs.select(feed_index);
        // 実運用では Target::Feed のタブに requested_limit は立たないが、
        // Target::Search のタブでだけ動く条件を単独で確かめるために強制的に立てる。
        app.tabs.state_mut().requested_limit = 5;
        app.tabs.state_mut().results = (0..5).map(|i| result(&i.to_string())).collect();

        load_more_with(&mut app, &tx, &mut session, StubYtDlp);

        assert!(session.search_task.is_none(), "Feed タブでは動かない");
    }

    /// キャッシュを効かせた検索画面。サムネイルは別の話なので取りに行かせない。
    fn cache_app(ttl_secs: u64) -> App {
        let mut app = App {
            query: QueryEditor::from("ラーメン"),
            ..App::default()
        };
        app.settings.search.cache_enabled = true;
        app.settings.search.cache_ttl = Duration::from_secs(ttl_secs);
        app.settings.thumbnails.enabled = false;
        app
    }

    /// 取りに行った検索が成功して届いた状態を作る。
    fn prime_cache(app: &App, session: &mut Session, target: &Target, results: Vec<SearchResult>) {
        session.pending_cache_key = Some(cache_key(
            target,
            app.settings.search.limit,
            app.cookies.for_search().is_some(),
        ));
        remember_search(session, &results, app.settings.search.cache_ttl);
    }

    fn ramen() -> Target {
        Target::Search("ラーメン".to_string())
    }

    #[test]
    fn the_cache_key_separates_targets_limits_and_cookie_use() {
        let udon = Target::Search("うどん".to_string());
        assert_ne!(cache_key(&ramen(), 20, false), cache_key(&udon, 20, false));
        assert_ne!(
            cache_key(&ramen(), 20, false),
            cache_key(&ramen(), 50, false)
        );
        assert_ne!(
            cache_key(&ramen(), 20, false),
            cache_key(&ramen(), 20, true)
        );
        assert_eq!(cache_key(&ramen(), 20, true), cache_key(&ramen(), 20, true));

        let videos = Target::Channel {
            id: "UCabc".to_string(),
            tab: ChannelTab::Videos,
        };
        let shorts = Target::Channel {
            id: "UCabc".to_string(),
            tab: ChannelTab::Shorts,
        };
        assert_ne!(cache_key(&videos, 20, false), cache_key(&shorts, 20, false));
        assert_ne!(
            cache_key(&videos, 20, false),
            cache_key(&Target::Feed(Feed::History), 20, false)
        );
    }

    #[tokio::test]
    async fn a_cached_search_is_served_without_running_yt_dlp() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = cache_app(300);
        prime_cache(&app, &mut session, &ramen(), vec![result("a"), result("b")]);

        start_search_with(&mut app, &tx, &mut session, StubYtDlp);

        assert!(session.search_task.is_none(), "yt-dlp を起動しない");
        assert!(!app.searching);
        assert_eq!(app.result_ids(), ["a", "b"]);
        assert_eq!(app.mode, Mode::Results);
    }

    #[tokio::test]
    async fn a_cache_hit_still_updates_the_requested_limit() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = cache_app(300);
        // 「もっと見る」で 20 件まで伸ばした後、同じ語をもう一度検索した状況を模す。
        app.tabs.state_mut().requested_limit = 20;
        prime_cache(&app, &mut session, &ramen(), vec![result("a"), result("b")]);

        start_search_with(&mut app, &tx, &mut session, StubYtDlp);

        assert!(session.search_task.is_none(), "yt-dlp を起動しない");
        assert_eq!(
            app.tabs.state().requested_limit,
            app.settings.search.limit,
            "AppEvent::SearchDone を経由しない控え命中でも今回の要求件数に更新される"
        );
    }

    #[tokio::test]
    async fn the_cache_is_ignored_while_the_setting_is_off() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = cache_app(300);
        prime_cache(&app, &mut session, &ramen(), vec![result("a")]);
        app.settings.search.cache_enabled = false;

        start_search_with(&mut app, &tx, &mut session, StubYtDlp);

        assert!(session.search_task.is_some(), "毎回取り直す");
        assert!(app.searching);
        assert!(app.results.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn a_cache_entry_is_served_until_the_ttl_and_refetched_after_it() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = cache_app(60);
        prime_cache(&app, &mut session, &ramen(), vec![result("a")]);

        tokio::time::advance(Duration::from_secs(59)).await;
        start_search_with(&mut app, &tx, &mut session, StubYtDlp);
        assert!(session.search_task.is_none(), "期限内は控えで返す");
        assert_eq!(app.result_ids(), ["a"]);

        tokio::time::advance(Duration::from_secs(2)).await;
        start_search_with(&mut app, &tx, &mut session, StubYtDlp);
        assert!(session.search_task.is_some(), "期限切れは取り直す");
        assert!(app.searching);
    }

    #[tokio::test]
    async fn reload_ignores_the_cache_and_fetches_again() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = cache_app(300);
        prime_cache(&app, &mut session, &ramen(), vec![result("a")]);

        reload_tab_with(&mut app, &tx, &mut session, StubYtDlp);

        assert!(session.search_task.is_some(), "r は常に取り直す");
        assert!(app.searching);
        assert!(app.results.is_empty());
    }

    #[tokio::test]
    async fn reloading_a_channel_tab_ignores_the_cache() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = cache_app(300);
        app.channel = Some(ChannelView::new(
            "UCabc".to_string(),
            "Some Channel".to_string(),
        ));
        let target = app.channel.as_ref().expect("channel").target();
        prime_cache(&app, &mut session, &target, vec![result("a")]);

        reload_channel_tab_with(&mut app, &tx, &mut session, StubYtDlp);

        assert!(session.search_task.is_some(), "r は常に取り直す");
        assert!(app.view_results().is_empty());
    }

    #[tokio::test]
    async fn a_search_with_cookies_is_not_served_to_one_without_them() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = cache_app(300);
        app.cookies = CookieState::Active(CookieSource::from_spec(Some("chrome")).expect("spec"));
        prime_cache(&app, &mut session, &ramen(), vec![result("a")]);

        app.cookies = CookieState::Off;
        start_search_with(&mut app, &tx, &mut session, StubYtDlp);
        assert!(session.search_task.is_some(), "cookie 無しでは取り直す");

        // cookie を戻せば同じ控えが効く。
        app.cookies = CookieState::Active(CookieSource::from_spec(Some("chrome")).expect("spec"));
        start_search_with(&mut app, &tx, &mut session, StubYtDlp);
        assert!(session.search_task.is_none());
        assert_eq!(app.result_ids(), ["a"]);
    }

    #[tokio::test]
    async fn a_cache_hit_leaves_the_cookie_state_alone() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = cache_app(300);
        let armed = CookieState::Armed(CookieSource::from_spec(Some("chrome")).expect("spec"));
        app.cookies = armed.clone();
        prime_cache(&app, &mut session, &ramen(), vec![result("a")]);

        start_search_with(&mut app, &tx, &mut session, StubYtDlp);

        assert!(session.search_task.is_none());
        assert_eq!(app.cookies, armed, "取りに行っていないので確認もしない");
    }

    #[tokio::test]
    async fn a_refused_feed_is_not_served_from_the_cache() {
        // cookie が外れた後は、cookie 付きで取った控えを出さずに理由を出す。
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = cache_app(300);
        app.cookies = CookieState::Active(CookieSource::from_spec(Some("chrome")).expect("spec"));
        let target = Target::Feed(Feed::History);
        prime_cache(&app, &mut session, &target, vec![result("a")]);

        app.cookies = CookieState::Off;
        app.query.set(":ythis");
        start_search_with(&mut app, &tx, &mut session, StubYtDlp);

        assert!(session.search_task.is_none());
        assert!(app.results.is_empty());
        assert!(app.error.is_some(), "理由を出す");
    }

    #[tokio::test]
    async fn a_search_is_not_kept_while_the_setting_is_off() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = cache_app(300);
        app.settings.search.cache_enabled = false;

        start_search_with(&mut app, &tx, &mut session, StubYtDlp);

        assert!(session.search_task.is_some());
        assert!(
            session.pending_cache_key.is_none(),
            "off の間は控え先を作らない"
        );
    }

    #[tokio::test]
    async fn a_search_is_kept_while_the_setting_is_on() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = cache_app(300);

        start_search_with(&mut app, &tx, &mut session, StubYtDlp);

        assert_eq!(
            session.pending_cache_key,
            Some(cache_key(&ramen(), app.settings.search.limit, false))
        );
    }

    #[tokio::test(start_paused = true)]
    async fn expired_entries_are_dropped_when_the_next_search_is_kept() {
        let mut session = Session::default();
        let app = cache_app(60);
        prime_cache(&app, &mut session, &ramen(), vec![result("a")]);

        tokio::time::advance(Duration::from_secs(61)).await;
        let udon = Target::Search("うどん".to_string());
        prime_cache(&app, &mut session, &udon, vec![result("b")]);

        assert_eq!(session.search_cache.len(), 1, "期限切れは残さない");
        assert!(session.search_cache.contains_key(&cache_key(
            &udon,
            app.settings.search.limit,
            false
        )));
    }

    #[tokio::test(start_paused = true)]
    async fn the_cache_stops_at_its_capacity_and_drops_the_oldest() {
        let mut session = Session::default();
        // 期限では減らない幅にして、上限だけが効いていることを見る。
        let app = cache_app(3600);
        let query = |i: usize| Target::Search(format!("q{i}"));
        let key = |target: &Target| cache_key(target, app.settings.search.limit, false);

        for i in 0..CACHE_CAPACITY + 10 {
            prime_cache(&app, &mut session, &query(i), vec![result("a")]);
            // 控えた順を区別できるよう時計を進める。
            tokio::time::advance(Duration::from_secs(1)).await;
        }

        assert_eq!(session.search_cache.len(), CACHE_CAPACITY);
        assert!(
            !session.search_cache.contains_key(&key(&query(0))),
            "古い順に捨てる"
        );
        assert!(
            session
                .search_cache
                .contains_key(&key(&query(CACHE_CAPACITY + 9))),
            "最後に控えたものは残す"
        );
    }

    #[tokio::test]
    async fn a_reload_that_was_cut_short_still_ignores_the_cache() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = cache_app(300);
        prime_cache(&app, &mut session, &ramen(), vec![result("a")]);

        reload_tab_with(&mut app, &tx, &mut session, StubYtDlp);
        // 読み込めないうちに別のタブへ移ると、取り直しは打ち切られる。
        switch_tab_with(&mut app, &tx, &mut session, true, StubYtDlp);
        // 戻って押し直したときも、捨てたはずの控えは出さない。
        select_tab_with(&mut app, &tx, &mut session, 0, StubYtDlp);

        assert!(session.search_task.is_some(), "取り直しを続ける");
        assert!(app.results.is_empty());

        // 読み込めたら頼みは果たされる。次からは控えが効く。
        app.set_results(vec![result("b")], &ramen());
        assert!(!app.tabs.state().reload);
        start_tab_search_with(&mut app, &tx, &mut session, StubYtDlp);
        assert!(session.search_task.is_none());
        assert_eq!(app.result_ids(), ["a"]);
    }

    #[tokio::test]
    async fn a_channel_reload_that_was_cut_short_still_ignores_the_cache() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = cache_app(300);
        app.channel = Some(ChannelView::new(
            "UCabc".to_string(),
            "Some Channel".to_string(),
        ));
        let target = app.channel.as_ref().expect("channel").target();
        prime_cache(&app, &mut session, &target, vec![result("a")]);

        reload_channel_tab_with(&mut app, &tx, &mut session, StubYtDlp);
        // タブを送って打ち切り、押し直しで戻る。
        switch_channel_tab_with(&mut app, &tx, &mut session, true, StubYtDlp);
        select_channel_tab_with(&mut app, &tx, &mut session, 0, StubYtDlp);

        assert!(session.search_task.is_some(), "取り直しを続ける");
        assert!(app.view_results().is_empty());
    }

    #[tokio::test]
    async fn a_search_from_the_input_box_returns_to_the_all_tab() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = App {
            query: QueryEditor::from("ラーメン"),
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
        move_selection_with(&mut app, CELL, Dir::Right);
        assert_eq!(app.selected, 1);
        move_selection_with(&mut app, CELL, Dir::Down);
        assert_eq!(app.selected, 5);
        move_selection_with(&mut app, CELL, Dir::Up);
        assert_eq!(app.selected, 1);
        move_selection_with(&mut app, CELL, Dir::Left);
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

    fn settings_temp_dir(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("tuitube-actions-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn the_settings_screen_returns_to_the_mode_it_was_opened_from() {
        for origin in [Mode::Input, Mode::Results, Mode::Channel] {
            let mut session = Session::default();
            let mut app = App {
                mode: origin,
                ..App::default()
            };
            open_settings(&mut app, &mut session);
            assert_eq!(app.mode, Mode::Settings);

            close_settings(&mut app);
            assert_eq!(app.mode, origin, "{origin:?} から開いた");
        }
    }

    #[test]
    fn opening_the_settings_takes_the_thumbnails_off_the_screen() {
        let mut session = Session::default();
        let mut app = grid_app(4);
        app.thumbs.take_dirty();

        open_settings(&mut app, &mut session);
        assert!(session.owe_clear, "貼ってある画像を剥がす");

        close_settings(&mut app);
        assert!(app.thumbs.take_dirty(), "戻ったら貼り直す");
    }

    #[test]
    fn the_settings_selection_wraps_at_both_ends() {
        let mut app = App::default();
        move_settings_selection(&mut app, -1);
        assert_eq!(app.settings_selected, SETTINGS_ITEMS.len() - 1);
        move_settings_selection(&mut app, 1);
        assert_eq!(app.settings_selected, 0);
        move_settings_selection(&mut app, 1);
        assert_eq!(app.settings_selected, 1);

        // 壊れた値で入ってきても一覧の中に戻す。
        app.settings_selected = 99;
        move_settings_selection(&mut app, 1);
        assert!(app.settings_selected < SETTINGS_ITEMS.len());
    }

    #[test]
    fn adjusting_changes_only_the_selected_row() {
        let mut app = App {
            settings_selected: 2,
            ..App::default()
        };
        adjust_settings_value(&mut app, 1);

        assert_eq!(
            app.settings.fps_cap.map(crate::display::FpsCap::get),
            Some(20)
        );
        let untouched = Settings::default();
        assert_eq!(app.settings.display, untouched.display);
        assert_eq!(app.settings.search, untouched.search);
        assert_eq!(app.settings.thumbnails, untouched.thumbnails);
    }

    #[test]
    fn closing_without_saving_throws_the_edits_away() {
        // search.limit や layout はセッション中に読み直されるので、残すと
        // 「保存しなければ変わらない」という案内と食い違う。
        let mut session = Session::default();
        let mut app = App {
            mode: Mode::Results,
            ..App::default()
        };
        open_settings(&mut app, &mut session);
        app.settings.search.limit = 25;
        app.settings.search.layout = crate::grid::LayoutMode::List;
        app.settings.thumbnails.enabled = false;

        close_settings(&mut app);
        assert_eq!(app.settings, Settings::default(), "開いた時点へ戻す");
        assert_eq!(app.mode, Mode::Results);
    }

    #[test]
    fn what_was_saved_survives_a_later_escape() {
        let dir = settings_temp_dir("save-then-close");
        let path = dir.join("config.toml");
        let mut session = Session::default();
        let mut app = App::default();

        open_settings(&mut app, &mut session);
        app.settings.search.limit = 25;
        save_settings_to(&mut app, Some(&path), std::time::Instant::now());
        // 保存した後の編集だけを捨てる。
        app.settings.search.limit = 40;
        close_settings(&mut app);

        assert_eq!(app.settings.search.limit, 25);
    }

    #[test]
    fn saving_a_changed_display_mode_notes_it_takes_effect_on_next_launch() {
        // 設定画面での編集は app.settings.display.mode だけを進める。
        // app.display (今の実行中の値) は次回起動まで動かないので、その旨を保存の知らせに添える。
        let dir = settings_temp_dir("save-display-mode-deferred");
        let path = dir.join("config.toml");
        let mut app = App::default();
        assert_eq!(app.display, crate::display::DisplayMode::Embedded);
        app.settings.display.mode = crate::display::DisplayMode::Window;

        save_settings_to(&mut app, Some(&path), std::time::Instant::now());

        assert!(
            app.notice
                .as_deref()
                .is_some_and(|n| n.contains("次回起動")),
            "{:?}",
            app.notice
        );
    }

    #[test]
    fn saving_an_unchanged_display_mode_does_not_mention_next_launch() {
        let dir = settings_temp_dir("save-display-mode-unchanged");
        let path = dir.join("config.toml");
        let mut app = App::default();
        app.settings.search.limit = 25;

        save_settings_to(&mut app, Some(&path), std::time::Instant::now());

        assert!(
            app.notice
                .as_deref()
                .is_some_and(|n| !n.contains("次回起動")),
            "{:?}",
            app.notice
        );
    }

    #[test]
    fn saving_leaves_the_values_the_environment_is_holding_alone() {
        let dir = settings_temp_dir("save-env");
        let path = dir.join("config.toml");
        let mut app = App {
            mode: Mode::Settings,
            // 環境変数が 60 を指している。ファイルには 15 が書いてある。
            env_overridden: crate::settings::EnvOverridden {
                fps_cap: Some(crate::display::FpsCap::new(15)),
                ..crate::settings::EnvOverridden::default()
            },
            ..App::default()
        };
        app.settings.fps_cap = crate::display::FpsCap::new(60);
        app.settings.search.limit = 25;
        save_settings_to(&mut app, Some(&path), std::time::Instant::now());

        let written = std::fs::read_to_string(&path).expect("読める");
        assert!(written.contains("fps_cap = 15"), "ファイルの値のまま");
        assert!(!written.contains("fps_cap = 60"), "{written}");
        assert!(written.contains("limit = 25"), "他の行の編集は保存する");

        let notice = app.notice.clone().expect("保存した旨を出す");
        assert!(notice.contains("fps_cap"), "{notice}");
        assert!(app.error.is_none());
    }

    #[test]
    fn a_browser_the_environment_switched_off_is_not_written_back() {
        // TUITUBE_COOKIES=none で開いた状態。s を押しても browser 行を消さない。
        let dir = settings_temp_dir("save-cookies");
        let path = dir.join("config.toml");
        let browser = CookieSource::from_spec(Some("chrome"));
        let mut app = App {
            env_overridden: crate::settings::EnvOverridden {
                cookies: Some(browser.clone()),
                ..crate::settings::EnvOverridden::default()
            },
            ..App::default()
        };
        assert_eq!(app.settings.cookies, None, "実行中は連携なし");
        save_settings_to(&mut app, Some(&path), std::time::Instant::now());

        let written = std::fs::read_to_string(&path).expect("読める");
        // 指定なしのときは `# browser = "chrome"` という例がコメントで出るので、
        // 生きている行かどうかまで見ないと消えたことに気づけない。
        assert!(
            written
                .lines()
                .any(|line| line.trim() == "browser = \"chrome\""),
            "{written}"
        );
        let notice = app.notice.clone().expect("保存した旨を出す");
        assert!(notice.contains("cookies.browser"), "{notice}");
    }

    #[test]
    fn saving_writes_the_edited_settings_to_the_file() {
        let dir = settings_temp_dir("save");
        let path = dir.join("config.toml");
        let mut app = App {
            mode: Mode::Settings,
            ..App::default()
        };
        app.settings.search.limit = 25;
        save_settings_to(&mut app, Some(&path), std::time::Instant::now());

        assert_eq!(
            std::fs::read_to_string(&path).expect("読める"),
            crate::settings::render(&app.settings)
        );
        let notice = app.notice.clone().expect("保存した旨を出す");
        assert!(notice.contains("config.toml"), "{notice}");
        assert!(app.error.is_none());
        assert_eq!(app.mode, Mode::Settings, "保存しても閉じない");
    }

    #[test]
    fn a_save_that_cannot_write_reports_the_reason() {
        let dir = settings_temp_dir("save-blocked");
        let blocker = dir.join("blocked");
        std::fs::write(&blocker, "ファイルなので中に書けない").expect("書ける");

        let mut app = App::default();
        save_settings_to(
            &mut app,
            Some(&blocker.join("config.toml")),
            std::time::Instant::now(),
        );
        let error = app.error.clone().expect("理由を出す");
        assert!(error.contains("保存できません"), "{error}");
        assert!(app.notice.is_none());

        // 置き場が分からないときも黙って失敗しない。
        let mut app = App::default();
        save_settings_to(&mut app, None, std::time::Instant::now());
        assert_eq!(app.error.as_deref(), Some(NO_CONFIG_PATH));
    }

    #[test]
    fn a_save_error_disappears_on_its_own() {
        // ポーリングの無い画面なので、期限つきで出さないと消す機会が無い。
        let t0 = std::time::Instant::now();
        let mut app = App::default();
        save_settings_to(&mut app, None, t0);
        app.expire_error(t0 + crate::app::NOTICE_TTL);
        assert!(app.error.is_none());
    }

    const COMMENTS_JSON: &str = r#"{"id":"abc","comments":[{"author":"alice","text":"first"}]}"#;

    /// CommentsReady を 1 件受け取る。届かなければ None。
    async fn next_comments(
        rx: &mut mpsc::UnboundedReceiver<AppEvent>,
        session: &mut Session,
    ) -> Option<AppEvent> {
        let task = session.comments_task.take()?;
        task.await.expect("タスクは panic しない");
        rx.try_recv().ok()
    }

    #[tokio::test]
    async fn start_comments_fetches_for_the_video_that_just_started() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = App::default();
        let runner = FakeYtDlp::new([done(0, COMMENTS_JSON, "")]);
        start_comments_with(
            &mut app,
            &tx,
            &mut session,
            "abc".to_string(),
            "https://www.youtube.com/watch?v=abc".to_string(),
            runner.clone(),
        );

        assert_eq!(app.comments.state(), Some(&CommentState::Pending));
        let event = next_comments(&mut rx, &mut session).await.expect("届く");
        let AppEvent::CommentsReady {
            nonce,
            video_id,
            comments,
        } = event
        else {
            panic!("CommentsReady のはず");
        };
        assert_eq!(nonce, session.comments_nonce);
        assert_eq!(video_id, "abc");
        assert_eq!(comments.expect("取れる").len(), 1);
        assert_eq!(
            runner.calls()[0][0],
            "https://www.youtube.com/watch?v=abc",
            "再生中の URL を渡す"
        );
    }

    #[tokio::test]
    async fn starting_the_next_video_abandons_the_previous_fetch() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = App::default();
        start_comments_with(
            &mut app,
            &tx,
            &mut session,
            "old".to_string(),
            "u1".to_string(),
            FakeYtDlp::new([Step::Hang]),
        );
        let first = session.comments_nonce;
        start_comments_with(
            &mut app,
            &tx,
            &mut session,
            "new".to_string(),
            "u2".to_string(),
            FakeYtDlp::new([Step::Hang]),
        );

        // 世代が変わるので、先に出した取得が返っても新しい再生には混ざらない。
        assert_ne!(session.comments_nonce, first);
        app.comments.apply("old", Ok(Vec::new()));
        assert_eq!(app.comments.state(), Some(&CommentState::Pending));
    }

    #[tokio::test]
    async fn opening_comments_peels_the_embedded_image_off() {
        let mut app = App::default();
        let mut session = Session::default();
        toggle_comments(&mut app, &mut session);
        assert!(app.comments.visible());
        assert!(session.owe_clear, "貼ってある映像は差分描画では消えない");

        session.owe_clear = false;
        toggle_comments(&mut app, &mut session);
        assert!(!app.comments.visible());
        assert!(!session.owe_clear, "閉じるときは映像が続きを貼る");
    }

    #[tokio::test]
    async fn end_playback_drops_the_comment_fetch() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = App::default();
        start_comments_with(
            &mut app,
            &tx,
            &mut session,
            "abc".to_string(),
            "u".to_string(),
            FakeYtDlp::new([Step::Hang]),
        );
        let nonce = session.comments_nonce;
        end_playback(&mut app, &mut session, None).await;

        assert!(session.comments_task.is_none());
        assert_eq!(app.comments.state(), None);
        assert!(!app.comments.visible());
        assert_ne!(
            session.comments_nonce, nonce,
            "打ち切った世代の結果は nonce でも弾く"
        );
    }

    /// コメント表示中の 80x24。映像領域の枠の内側は 78x19。
    fn commented_app() -> App {
        let mut app = App {
            screen: Rect::new(0, 0, 80, 24),
            ..App::default()
        };
        app.comments.begin("abc".to_string());
        let list = (0..comments::COMMENT_LIMIT)
            .map(|i| comments::Comment {
                author: format!("author{i}"),
                text: format!("本文{i}"),
                like_count: None,
            })
            .collect();
        app.comments.apply("abc", Ok(list));
        app.comments.toggle();
        app
    }

    #[test]
    fn scrolling_moves_by_a_line_and_by_a_screen() {
        let mut app = commented_app();
        let lines = 2 * comments::COMMENT_LIMIT;
        let height = 18;

        scroll_comments(&mut app, CommentScroll::Line(1));
        assert_eq!(app.comments.scroll(lines, height), 1);
        scroll_comments(&mut app, CommentScroll::Page(1));
        assert_eq!(app.comments.scroll(lines, height), 1 + height);
        scroll_comments(&mut app, CommentScroll::Page(-1));
        assert_eq!(app.comments.scroll(lines, height), 1);
        scroll_comments(&mut app, CommentScroll::Line(-1));
        assert_eq!(app.comments.scroll(lines, height), 0);
    }

    #[test]
    fn scrolling_stops_at_the_last_screen_of_the_list() {
        let mut app = commented_app();
        let lines = 2 * comments::COMMENT_LIMIT;
        let height = 18;

        for _ in 0..20 {
            scroll_comments(&mut app, CommentScroll::Page(1));
        }
        assert_eq!(
            app.comments.scroll(lines, height),
            lines - height,
            "最後の画面から先へは送らない"
        );
        scroll_comments(&mut app, CommentScroll::Line(-1));
        assert_eq!(app.comments.scroll(lines, height), lines - height - 1);
    }

    #[test]
    fn scrolling_a_list_that_fits_stays_at_the_top() {
        let mut app = App {
            screen: Rect::new(0, 0, 80, 24),
            ..App::default()
        };
        app.comments.begin("abc".to_string());
        app.comments.apply("abc", Ok(Vec::new()));
        app.comments.toggle();

        scroll_comments(&mut app, CommentScroll::Page(1));
        assert_eq!(app.comments.scroll(1, 19), 0);
    }

    /// 利用者の非表示リストを触らないよう、一時ディレクトリを追記先にする。
    fn hidden_at(name: &str) -> (crate::hidden::Hidden, std::path::PathBuf) {
        let path = settings_temp_dir(name).join("hidden.toml");
        (crate::hidden::load_from(Some(&path)), path)
    }

    fn hidden_ids(path: &Path) -> crate::hidden::Hidden {
        crate::hidden::load_from(Some(path))
    }

    #[test]
    fn hiding_the_selected_video_writes_it_and_takes_it_off_the_list() {
        let (hidden, path) = hidden_at("hide-video");
        let mut app = App {
            hidden,
            ..channel_grid_app()
        };
        app.selected = 1;

        hide_selected(&mut app, std::time::Instant::now());

        assert_eq!(app.result_ids(), ["id0"], "その場で消える");
        assert!(app.hidden.videos.contains("id1"));
        let written = std::fs::read_to_string(&path).expect("読める");
        assert!(written.contains("id1"), "{written}");
        assert!(
            written.contains("title id1"),
            "見返す用の題名も書く: {written}"
        );
        assert_eq!(app.notice.as_deref(), Some(HIDDEN_VIDEO_NOTICE));
        assert!(app.notice_until.is_some(), "期限つきで出す");
        assert!(app.error.is_none());
    }

    #[test]
    fn hiding_clamps_the_selection_to_what_is_left() {
        let (hidden, _path) = hidden_at("hide-last");
        let mut app = App {
            hidden,
            ..grid_app(3)
        };
        app.selected = 2;

        hide_selected(&mut app, std::time::Instant::now());

        assert_eq!(app.result_ids(), ["id0", "id1"]);
        assert_eq!(app.selected, 1, "末尾を消したら手前へ寄せる");
    }

    #[test]
    fn hiding_the_last_row_goes_back_to_the_search_box() {
        let (hidden, _path) = hidden_at("hide-only");
        let mut app = App {
            hidden,
            ..grid_app(1)
        };

        hide_selected(&mut app, std::time::Instant::now());

        assert!(app.results.is_empty());
        assert_eq!(app.mode, Mode::Input, "空の格子に留めない");
        assert_eq!(app.notice.as_deref(), Some(HIDDEN_VIDEO_NOTICE));
    }

    #[test]
    fn hiding_a_row_in_a_channel_stays_in_the_channel() {
        let (hidden, _path) = hidden_at("hide-in-channel");
        let mut app = App {
            hidden,
            ..channel_grid_app()
        };
        app.channel = Some(ChannelView::new(
            "UCone".to_string(),
            "One Channel".to_string(),
        ));
        app.channel.as_mut().expect("channel").state_mut().results = vec![result("only")];
        app.mode = Mode::Channel;
        app.sync_from_view();

        hide_selected(&mut app, std::time::Instant::now());

        assert!(app.view_results().is_empty());
        assert_eq!(app.mode, Mode::Channel, "Esc で戻れる画面のままにする");
    }

    #[test]
    fn hiding_stays_hidden_on_the_next_search() {
        let (hidden, _path) = hidden_at("hide-next-search");
        let mut app = App {
            hidden,
            ..grid_app(2)
        };

        hide_selected(&mut app, std::time::Instant::now());
        // 同じ動画を含む検索が返ってきても、もう並ばない。
        app.set_results(
            vec![result("id0"), result("id9")],
            &Target::Search("q".to_string()),
        );

        assert_eq!(app.result_ids(), ["id9"]);
    }

    #[test]
    fn hiding_without_a_selection_does_nothing() {
        let (hidden, path) = hidden_at("hide-empty");
        let mut app = App {
            hidden,
            ..App::default()
        };

        hide_selected(&mut app, std::time::Instant::now());

        assert!(!path.exists(), "空振りでファイルを作らない");
        assert!(app.notice.is_none());
        assert!(app.error.is_none());
    }

    #[test]
    fn a_hide_that_cannot_be_written_reports_the_reason_and_keeps_the_row() {
        let dir = settings_temp_dir("hide-blocked");
        let blocker = dir.join("blocked");
        std::fs::write(&blocker, "ファイルなので中に書けない").expect("書ける");
        let mut app = App {
            hidden: crate::hidden::load_from(Some(&blocker.join("hidden.toml"))),
            ..grid_app(2)
        };

        hide_selected(&mut app, std::time::Instant::now());

        assert_eq!(app.result_ids(), ["id0", "id1"], "消せていないので残す");
        assert!(app.hidden.videos.is_empty());
        let error = app.error.clone().expect("理由を出す");
        assert!(error.contains("非表示"), "{error}");
        assert!(app.error_until.is_some(), "期限つきで出す");
    }

    #[test]
    fn hiding_the_channel_writes_it_and_returns_to_the_results() {
        let (hidden, path) = hidden_at("hide-channel");
        let mut session = Session::default();
        let mut app = App {
            hidden,
            ..channel_grid_app()
        };
        // id0 の Channel へ移ってから隠す。
        app.store_to_tab();
        app.channel = Some(ChannelView::new(
            "UCone".to_string(),
            "One Channel".to_string(),
        ));
        app.mode = Mode::Channel;
        app.sync_from_view();

        hide_current_channel(&mut app, &mut session, std::time::Instant::now());

        assert!(app.channel.is_none(), "チャンネルから出る");
        assert_eq!(app.mode, Mode::Results);
        assert!(app.hidden.channels.contains("UCone"));
        let written = std::fs::read_to_string(&path).expect("読める");
        assert!(written.contains("UCone"), "{written}");
        assert!(written.contains("One Channel"), "{written}");
        assert_eq!(app.notice.as_deref(), Some(HIDDEN_CHANNEL_NOTICE));
        // 同じチャンネルの他の動画も検索結果から消える。
        assert_eq!(app.result_ids(), ["id1"]);
        assert_eq!(hidden_ids(&path).channels.len(), 1);
    }

    #[test]
    fn hiding_the_channel_of_every_result_leaves_the_search_box() {
        let (hidden, _path) = hidden_at("hide-channel-all");
        let mut session = Session::default();
        let mut app = App {
            hidden,
            ..channel_grid_app()
        };
        app.tabs.state_mut().results = vec![SearchResult {
            channel_id: Some("UCone".to_string()),
            ..result("id0")
        }];
        app.sync_from_tab();
        app.channel = Some(ChannelView::new(
            "UCone".to_string(),
            "One Channel".to_string(),
        ));
        app.mode = Mode::Channel;

        hide_current_channel(&mut app, &mut session, std::time::Instant::now());

        assert!(app.results.is_empty());
        assert_eq!(app.mode, Mode::Input, "見るものが無ければ検索欄へ戻す");
        assert_eq!(app.notice.as_deref(), Some(HIDDEN_CHANNEL_NOTICE));
    }

    #[test]
    fn hiding_a_channel_outside_the_channel_view_does_nothing() {
        let (hidden, path) = hidden_at("hide-channel-none");
        let mut session = Session::default();
        let mut app = App {
            hidden,
            ..grid_app(2)
        };

        hide_current_channel(&mut app, &mut session, std::time::Instant::now());

        assert!(!path.exists());
        assert!(app.notice.is_none());
        assert_eq!(app.result_ids(), ["id0", "id1"]);
    }

    // ---- ダウンロード画面 ----

    fn download_ready_app(target: SearchResult) -> App {
        let mut app = App {
            mode: Mode::Results,
            screen: Rect::new(0, 0, 80, 24),
            ..App::default()
        };
        app.set_results(vec![target], &Target::Search("q".to_string()));
        app
    }

    #[test]
    fn opening_the_download_screen_from_results_uses_the_selected_row() {
        let mut session = Session::default();
        let mut app = download_ready_app(SearchResult {
            title: "面白い/動画".to_string(),
            ..result("id0")
        });

        open_download(&mut app, &mut session);

        assert_eq!(app.mode, Mode::Download);
        assert_eq!(app.download_return, Mode::Results);
        assert_eq!(app.download_url, "https://www.youtube.com/watch?v=id0");
        // `/` はディレクトリを飛び出さないよう `_` へ置き換える。
        assert_eq!(app.download_filename.text(), "面白い_動画");
        assert_eq!(app.download_focus, DownloadField::Dir);
        assert!(!app.download_audio_only, "既定は動画");
        assert!(session.owe_clear, "貼ってあるサムネイルを剥がす");
    }

    #[test]
    fn opening_the_download_screen_from_playing_uses_the_playback() {
        let mut session = Session::default();
        let mut app = App {
            mode: Mode::Playing,
            playback: Playback {
                title: "再生中の動画".to_string(),
                url: "https://www.youtube.com/watch?v=live1".to_string(),
                ..Playback::default()
            },
            ..App::default()
        };

        open_download(&mut app, &mut session);

        assert_eq!(app.download_return, Mode::Playing);
        assert_eq!(app.download_url, "https://www.youtube.com/watch?v=live1");
        assert_eq!(app.download_filename.text(), "再生中の動画");
    }

    #[test]
    fn opening_the_download_screen_without_a_selection_does_nothing() {
        let mut session = Session::default();
        let mut app = App {
            mode: Mode::Results,
            ..App::default()
        };

        open_download(&mut app, &mut session);

        assert_eq!(app.mode, Mode::Results, "一覧が空では開かない");
    }

    #[test]
    fn opening_the_download_screen_defaults_the_dir_from_settings() {
        let mut session = Session::default();
        let mut app = download_ready_app(result("id0"));
        app.settings.download.dir = Some(std::path::PathBuf::from("/configured/dir"));

        open_download(&mut app, &mut session);

        assert_eq!(app.download_dir.text(), "/configured/dir");
    }

    /// チャンネル一覧を開いていて、その一覧に選択中の動画がある画面。
    fn channel_download_app() -> App {
        let mut app = App {
            mode: Mode::Channel,
            screen: Rect::new(0, 0, 80, 24),
            channel: Some(ChannelView::new("UCone".to_string(), "One".to_string())),
            ..App::default()
        };
        app.set_results(
            vec![result("id0")],
            &Target::Channel {
                id: "UCone".to_string(),
                tab: ChannelTab::Videos,
            },
        );
        app
    }

    #[test]
    fn the_download_screen_returns_to_the_mode_it_was_opened_from() {
        for mut app in [download_ready_app(result("id0")), channel_download_app()] {
            let origin = app.mode;
            let mut session = Session::default();

            open_download(&mut app, &mut session);
            assert_eq!(app.mode, Mode::Download, "{origin:?}");

            close_download(&mut app);
            assert_eq!(app.mode, origin, "{origin:?} から開いた");
        }
    }

    #[test]
    fn closing_the_download_screen_marks_the_thumbnails_dirty_again() {
        let mut session = Session::default();
        let mut app = grid_app(4);
        app.thumbs.take_dirty();

        open_download(&mut app, &mut session);
        assert!(session.owe_clear);

        close_download(&mut app);
        assert!(app.thumbs.take_dirty(), "戻ったら貼り直す");
    }

    #[test]
    fn the_download_focus_wraps_at_both_ends() {
        let mut app = App::default();
        assert_eq!(app.download_focus, DownloadField::Dir);

        move_download_focus(&mut app, -1);
        assert_eq!(app.download_focus, DownloadField::Format, "巻き戻る");

        move_download_focus(&mut app, 1);
        assert_eq!(app.download_focus, DownloadField::Dir);
        move_download_focus(&mut app, 1);
        assert_eq!(app.download_focus, DownloadField::Filename);
    }

    #[tokio::test]
    async fn starting_a_download_spawns_the_task_with_the_entered_values() {
        let mut session = Session::default();
        let mut app = download_ready_app(result("id0"));
        open_download(&mut app, &mut session);
        app.download_dir = QueryEditor::from("/tmp/tuitube-dl");
        app.download_filename = QueryEditor::from("my-title");
        app.download_audio_only = true;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let downloader = download::fixtures::FakeDownloader::new([download::fixtures::done(
            0,
            "/tmp/tuitube-dl/my-title.mp3\n",
            "",
        )]);

        start_download(&mut app, &tx, &mut session, downloader.clone());

        // Enter を押した時点で即座に元の画面へ戻る。ダウンロードは背景で進む。
        assert_eq!(app.mode, Mode::Results);
        assert_eq!(app.notice.as_deref(), Some("ダウンロード中: my-title"));
        assert!(session.download_task.is_some());

        // spawn した直後はまだポーリングされていないので、実際に起動したことは
        // タスクの完走を待ってから確かめる。
        session
            .download_task
            .take()
            .expect("タスク")
            .await
            .expect("タスクは panic しない");

        let calls = downloader.calls();
        assert_eq!(calls.len(), 1);
        assert!(
            calls[0].contains(&"--audio-format".to_string()),
            "{calls:?}"
        );
        assert!(
            calls[0].contains(&"/tmp/tuitube-dl/my-title.%(ext)s".to_string()),
            "{calls:?}"
        );
        assert!(
            calls[0].contains(&"https://www.youtube.com/watch?v=id0".to_string()),
            "{calls:?}"
        );

        let event = rx.try_recv().expect("DownloadDone が届く");
        match event {
            AppEvent::DownloadDone { nonce, notice } => {
                assert_eq!(nonce, session.download_nonce);
                assert_eq!(
                    notice.expect("成功"),
                    "保存しました: /tmp/tuitube-dl/my-title.mp3"
                );
            }
            _ => panic!("DownloadDone のはず"),
        }
    }

    #[test]
    fn starting_a_download_with_an_empty_directory_is_refused() {
        let mut session = Session::default();
        let mut app = download_ready_app(result("id0"));
        open_download(&mut app, &mut session);
        app.download_dir = QueryEditor::from("   ");
        app.download_filename = QueryEditor::from("title");
        let (tx, _rx) = mpsc::unbounded_channel();

        start_download(
            &mut app,
            &tx,
            &mut session,
            download::fixtures::FakeDownloader::new([]),
        );

        assert_eq!(app.mode, Mode::Download, "断ったら画面を閉じない");
        assert!(session.download_task.is_none(), "起動しない");
        assert!(
            app.error
                .as_deref()
                .is_some_and(|e| e.contains("入力してください")),
            "{:?}",
            app.error
        );
    }

    #[tokio::test]
    async fn starting_a_download_resanitizes_a_filename_retyped_after_opening() {
        // sanitize_filename は open_download の初期値計算にしか掛からないため、
        // 開いた後に手で `/` を打ち直せてもディレクトリを飛び出さないことを確かめる。
        let mut session = Session::default();
        let mut app = download_ready_app(result("id0"));
        open_download(&mut app, &mut session);
        app.download_dir = QueryEditor::from("/tmp/tuitube-dl");
        app.download_filename = QueryEditor::from("../../etc/evil");
        let (tx, _rx) = mpsc::unbounded_channel();
        let downloader =
            download::fixtures::FakeDownloader::new([download::fixtures::done(0, "", "")]);

        start_download(&mut app, &tx, &mut session, downloader.clone());
        session
            .download_task
            .take()
            .expect("タスク")
            .await
            .expect("タスクは panic しない");

        let calls = downloader.calls();
        assert_eq!(calls.len(), 1);
        assert!(
            calls[0].contains(&"/tmp/tuitube-dl/.._.._etc_evil.%(ext)s".to_string()),
            "パス区切りが残っていない: {calls:?}"
        );
    }

    #[test]
    fn starting_a_download_with_an_empty_filename_is_refused() {
        let mut session = Session::default();
        let mut app = download_ready_app(result("id0"));
        open_download(&mut app, &mut session);
        app.download_filename = QueryEditor::from("");
        let (tx, _rx) = mpsc::unbounded_channel();

        start_download(
            &mut app,
            &tx,
            &mut session,
            download::fixtures::FakeDownloader::new([]),
        );

        assert_eq!(app.mode, Mode::Download);
        assert!(session.download_task.is_none());
    }

    #[tokio::test]
    async fn starting_a_new_download_cancels_the_previous_one() {
        let mut session = Session::default();
        let mut app = download_ready_app(result("id0"));
        open_download(&mut app, &mut session);
        app.download_dir = QueryEditor::from("/tmp/tuitube-dl");
        app.download_filename = QueryEditor::from("first");
        let (tx, _rx) = mpsc::unbounded_channel();
        // 応答しない偽物で、走らせたままの状態を作る。
        struct HangingDownloader;
        impl download::Downloader for HangingDownloader {
            fn run(
                &self,
                _args: Vec<String>,
            ) -> impl Future<Output = std::io::Result<Output>> + Send {
                std::future::pending()
            }
        }
        start_download(&mut app, &tx, &mut session, HangingDownloader);
        let first_nonce = session.download_nonce;
        assert!(session.download_task.is_some());

        open_download(&mut app, &mut session);
        app.download_dir = QueryEditor::from("/tmp/tuitube-dl");
        app.download_filename = QueryEditor::from("second");
        start_download(&mut app, &tx, &mut session, HangingDownloader);

        assert_ne!(session.download_nonce, first_nonce, "世代が進む");
        assert!(session.download_task.is_some(), "新しい方は走らせたまま");
    }

    #[test]
    fn download_debug_log_path_is_none_unless_enabled() {
        // [download] debug が既定 (false) の間は、今までどおりログの置き場を求めない。
        assert_eq!(download_debug_log_path(false), None);
        // enabled のときの実際の置き場計算 (env から求める) は download::debug_log_path 側、
        // それを使ったログの書き込みは download::run 側でそれぞれ確かめている。
        assert!(
            download_debug_log_path(true).is_some(),
            "HOME はテスト環境にもある"
        );
    }
}
