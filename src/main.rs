mod actions;
mod app;
mod badge;
mod category;
mod clipboard;
mod comments;
mod cookies;
mod display;
mod download;
mod engagement;
mod fetch;
mod geometry;
mod grid;
mod hidden;
mod input;
mod jpeg;
mod kitty;
mod mpv;
mod oauth;
mod query;
mod resume;
mod rgb;
mod screen;
mod search;
mod seekbar;
mod settings;
mod speed;
mod subtitles;
mod tct;
mod thumbs;
mod ui;
mod video;
mod youtube_url;

use actions::{Session, apply_resize, end_playback, on_tick, schedule_resize, stop_playback};
use anyhow::Result;
use app::{App, AppEvent, Mode, ViewKey};
use category::Tabs;
use cookies::{CookieOutcome, CookieState, Target};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event as CrosstermEvent, KeyEventKind,
};
use crossterm::execute;
use fetch::{Fetcher, RealCurl};
use geometry::cell_size;
use input::{handle_key, handle_mouse};
use ratatui::DefaultTerminal;
use ratatui::layout::Rect;
use screen::settings::SettingsScreen;
use search::{ChannelRef, RealYtDlp, YtDlp};
use std::io::Write;
use std::time::Duration;
use subtitles::SubtitleState;
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

/// 読んだ設定から画面側の初期状態を組む。環境変数の上書きもここで持ち回す。
fn app_from(
    loaded: settings::Loaded,
    hidden: hidden::Hidden,
    resume: resume::Resume,
    engagement: engagement::EngagementCache,
) -> App {
    App {
        hidden,
        resume,
        engagement,
        display: loaded.settings.display.mode,
        // 実際に効くかは最初の検索で分かる。ここでは指定の有無だけを持つ。
        cookies: CookieState::from_source(loaded.settings.cookies.clone()),
        tabs: Tabs::with_categories(loaded.settings.categories.clone()),
        subtitles: SubtitleState::from_settings(&loaded.settings.subtitles),
        settings_screen: SettingsScreen {
            backup: loaded.settings.clone(),
            ..SettingsScreen::default()
        },
        settings: loaded.settings,
        // 環境変数の一時的な上書きは、設定画面から保存してもファイルへ書かない。
        env_overridden: loaded.overridden,
        notice: loaded.notice,
        ..App::default()
    }
}

async fn run(terminal: &mut DefaultTerminal) -> Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    spawn_input_reader(tx.clone());

    // 設定は起動時に一度だけ読む。読み替えたときは notice がステータス行に出る。
    let mut app = app_from(
        settings::load(),
        hidden::load(),
        resume::load(),
        engagement::load(),
    );
    if let Some(dir) = app.settings.thumbnails.dir() {
        thumbs::prune_cache(&dir, app.settings.thumbnails.max_cached);
    }
    let mut session = Session::default();
    // cookie 連携があればおすすめ、無ければ先頭のカテゴリを起動直後に取りに行く。
    // 検索できないタブなら startup_index が None を返すので、その場合は検索欄のまま待つ。
    if let Some(index) = app.tabs.startup_index(&app.cookies) {
        actions::select_tab(&mut app, &tx, &mut session, index);
    }
    let mut ticker = tokio::time::interval(Duration::from_secs(1));

    loop {
        // draw は必ず MoveTo で始まり SGR を閉じて flush するので、その直後なら割り込まずに書ける。
        // マウスの当たり判定は「ユーザーが今見ている画面」で行うので、描いた寸法を控える。
        app.screen = terminal.draw(|frame| ui::draw(frame, &app))?.area;
        app.mark_drawn();
        // 映像が無い間 (設定画面など) の owe_clear は present_video でしか処理できない
        // ので、映像の置き場所が無くても毎フレーム呼ぶ。
        present_video(
            &mut session,
            &app,
            video_present_area(&app),
            terminal.backend_mut(),
        )?;
        // present_video が先。再生終了で持ち越した a=d が、貼ったばかりの画像を消さない順序。
        present_thumbs(&mut app, cell_size(), terminal.backend_mut())?;
        tokio::select! {
            event = rx.recv() => {
                let Some(event) = event else { break };
                handle_batch(&mut app, event, &mut rx, &tx, &mut session).await;
            }
            _ = ticker.tick() => on_tick(&mut app, &mut session, std::time::Instant::now()).await,
            _ = wait_until(session.resize_at) => {
                session.resize_at = None;
                apply_resize(&mut app, &mut session, &tx).await;
            }
        }
        if app.should_quit {
            break;
        }
    }
    for task in [
        session.search_task.take(),
        session.thumbs_task.take(),
        session.comments_task.take(),
        session.channel_lookup_task.take(),
        session.oauth_task.take(),
        session.engagement_task.take(),
    ]
    .into_iter()
    .flatten()
    {
        task.abort();
    }
    // 書けなくても終了は止めない。次の起動で取り直すだけ。
    let _ = engagement::save(&app.engagement);
    stop_playback(&mut session).await;
    // alt screen を抜ければ仕様上は消えるが、端末差を当てにしない。
    let mut out = Vec::new();
    video::encode_clear(&mut out);
    let backend = terminal.backend_mut();
    let _ = backend.write_all(&out);
    let _ = backend.flush();
    Ok(())
}

/// present_video に渡す area。映像の置き場所が無くても、貼ってあるサムネイルを
/// 剥がす owe_clear は present_video でしか処理できないので、対象が無い間も
/// 意味のある矩形を返し、present_video を毎フレーム呼び続けられるようにする。
fn video_present_area(app: &App) -> Rect {
    screen::playing::video_target_area(app, app.screen)
        .unwrap_or_else(|| screen::playing::video_area(app.screen))
}

/// draw の直後に、保留中の画像削除と最新フレームを実端末へ書く。書き手はここだけ。
fn present_video(
    session: &mut Session,
    app: &App,
    area: Rect,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    // コメントを出している間は映像領域を一覧が占めるので、フレームを取り出さない。
    // 取り出して捨てると sink からも消えて、閉じたときに貼り直すものが無くなる。
    let mut pending = if app.comments.visible() {
        video::Pending::default()
    } else {
        app.video
            .as_ref()
            .and_then(VideoSink::take)
            .unwrap_or_default()
    };
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

/// 変化があったときだけ、画面上の画像を消してから可視セルぶんを貼り直す。
/// 毎周書くと 1 画面で 370KB 前後になり端末が詰まる。
fn present_thumbs(
    app: &mut App,
    cell: video::CellSize,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    // 再生中・設定画面・ダウンロード画面では他の描画と重なるので 1 枚も書かない。
    // 戻ったときに貼り直せるよう dirty は残す。
    if matches!(app.mode, Mode::Playing | Mode::Settings | Mode::Download) {
        return Ok(());
    }
    if !app.thumbs.take_dirty() {
        return Ok(());
    }
    let mut bytes = Vec::new();
    video::encode_clear(&mut bytes);
    let mut incomplete = false;
    let shorts = screen::browse::viewing_shorts(app);
    if let Some(layout) = ui::grid_layout(app, cell) {
        for (i, rect) in layout.cells.iter().enumerate() {
            let Some(result) = app.view_results().get(layout.offset + i) else {
                break;
            };
            let Some(image) = app.thumbs.get(&result.id) else {
                continue;
            };
            match video::placement(rect.image, cell, (image.width, image.height)) {
                Some(at) => {
                    rgb::encode_image(image, at, &mut bytes);
                    // 印はサムネイルより後に送る (画像は後から貼った方が上に出る)。
                    let badges: Vec<badge::Badge> = badge::badges_for(&app.engagement, result)
                        .into_iter()
                        .filter(|b| app.settings.engagement.enabled || !b.engagement())
                        .collect();
                    badge::encode(&badges, at, cell, &mut bytes);
                    if shorts {
                        badge::encode_tab(&[badge::Badge::Shorts], at, cell, &mut bytes);
                    }
                }
                // セル寸法と噛み合わず描けなかった。次のフレームで取り直す
                // (take_dirty は成否を見ずに消費済みなので、ここで戻さないと直らない)。
                None => incomplete = true,
            }
        }
    }
    if incomplete {
        app.thumbs.mark_dirty();
    }
    // 画像は CUP で絶対位置へ寄せる。入力中は検索欄へ戻さないと、
    // 次の draw までカーソルが格子の中で点滅する。
    if app.mode == Mode::Input {
        let (x, y) = screen::browse::input_cursor(app.screen, &app.query);
        bytes.extend_from_slice(format!("\x1b[{};{}H", y + 1, x + 1).as_bytes());
    }
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

/// キーを捌き、タブが移っていたらサムネイルを取り直す。
/// 取得者を引数に取るのは、テストから curl を起動させないため。
async fn handle_key_event<F>(
    app: &mut App,
    key: crossterm::event::KeyEvent,
    tx: &UnboundedSender<AppEvent>,
    session: &mut Session,
    fetcher: F,
) where
    F: Fetcher + Send + Sync + 'static,
{
    let before = app.view_key();
    handle_key(app, key, tx, session).await;
    refetch_thumbnails_if_view_moved(app, tx, session, fetcher, before);
}

/// マウスを捌き、タブが移っていたらサムネイルを取り直す。
async fn handle_mouse_event<F>(
    app: &mut App,
    mouse: crossterm::event::MouseEvent,
    tx: &UnboundedSender<AppEvent>,
    session: &mut Session,
    fetcher: F,
) where
    F: Fetcher + Send + Sync + 'static,
{
    let before = app.view_key();
    handle_mouse(app, mouse, tx, session).await;
    refetch_thumbnails_if_view_moved(app, tx, session, fetcher, before);
}

/// タブやチャンネルを移ると結果集合ごと入れ替わる。読み込み済みでも
/// メモリ上の画像は捨ててあるので、キャッシュから読み直す。
fn refetch_thumbnails_if_view_moved<F>(
    app: &mut App,
    tx: &UnboundedSender<AppEvent>,
    session: &mut Session,
    fetcher: F,
    before: ViewKey,
) where
    F: Fetcher + Send + Sync + 'static,
{
    if app.view_key() != before {
        actions::start_thumbnails_with(app, tx, session, fetcher);
    }
}

/// 溜まった分をまとめて捌く。Resize はそこで区切る。
/// マウスの当たり判定は直前に描いた寸法 (app.screen) で行うので、
/// 同じ束で続けて捌くと、リサイズ前の寸法でクリックを判定してしまう。
async fn handle_batch(
    app: &mut App,
    first: AppEvent,
    rx: &mut mpsc::UnboundedReceiver<AppEvent>,
    tx: &UnboundedSender<AppEvent>,
    session: &mut Session,
) {
    let mut resized = is_resize(&first);
    handle_event(app, first, tx, session).await;
    for _ in 0..EVENT_DRAIN_LIMIT {
        if app.should_quit || resized {
            return;
        }
        let Ok(event) = rx.try_recv() else { return };
        resized = is_resize(&event);
        handle_event(app, event, tx, session).await;
    }
}

fn is_resize(event: &AppEvent) -> bool {
    matches!(event, AppEvent::Resize { .. })
}

async fn handle_event(
    app: &mut App,
    event: AppEvent,
    tx: &UnboundedSender<AppEvent>,
    session: &mut Session,
) {
    match event {
        AppEvent::Key(key) => handle_key_event(app, key, tx, session, RealCurl).await,
        AppEvent::Mouse(mouse) => handle_mouse_event(app, mouse, tx, session, RealCurl).await,
        AppEvent::Resize { width, height } => schedule_resize(session, width, height),
        AppEvent::SearchDone {
            nonce,
            target,
            report,
            requested_limit,
        } => {
            if nonce != session.search_nonce {
                return;
            }
            session.search_task = None;
            app.searching = false;
            apply_search_done(app, session, &target, report, requested_limit);
            actions::start_thumbnails(app, tx, session);
            actions::start_engagement(app, tx, session);
            if plays_on_arrival(&target, app) {
                actions::start_playback(app, tx, session).await;
            }
        }
        AppEvent::PlaylistsReady { nonce, entries } => {
            screen::playlists::apply_playlists_ready(app, session, nonce, entries);
        }
        AppEvent::MpvProperty { nonce, id, data } => {
            if session.player.as_ref().is_some_and(|p| p.nonce == nonce) {
                actions::apply_property(app, session, id, data).await;
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
                let error = note_cookie_failure(app, error);
                end_playback(app, session, error).await;
            }
        }
        AppEvent::ThumbsReady {
            nonce,
            target_px,
            images,
            notice,
            disable,
        } => {
            if nonce != session.search_nonce {
                return;
            }
            session.thumbs_task = None;
            app.thumbs.set_fetching(false);
            if let Some(notice) = notice {
                app.set_notice(Some(notice));
            }
            if let Some(reason) = disable {
                app.thumbs.disable(reason.clone());
                app.set_notice(Some(reason));
            }
            app.thumbs.apply(images, target_px);
        }
        AppEvent::CommentsReady {
            nonce,
            video_id,
            comments,
        } => {
            if nonce != session.comments_nonce {
                return;
            }
            session.comments_task = None;
            app.comments.apply(&video_id, comments);
        }
        AppEvent::ChannelLookupDone {
            nonce,
            video_id,
            result,
        } => apply_channel_lookup_with(app, tx, session, nonce, video_id, result, RealYtDlp),
        AppEvent::OauthDone { nonce, result } => apply_oauth_done(app, session, nonce, result),
        AppEvent::EngagementReady {
            nonce,
            liked_videos,
            asked_channels,
            subscribed_channels,
        } => apply_engagement_ready(
            app,
            session,
            nonce,
            liked_videos,
            asked_channels,
            subscribed_channels,
        ),
        AppEvent::DownloadDone { nonce, notice } => {
            screen::download::apply_download_done(app, session, nonce, notice)
        }
    }
}

/// 問い合わせで分かったいいね済み/登録済みを控えへ写す。取れなかった側は触らない。
fn apply_engagement_ready(
    app: &mut App,
    session: &mut Session,
    nonce: u64,
    liked_videos: Option<Vec<String>>,
    asked_channels: Vec<String>,
    subscribed_channels: Vec<String>,
) {
    if nonce != session.search_nonce {
        return;
    }
    session.engagement_task = None;
    let now = std::time::SystemTime::now();
    let mut changed = false;
    if let Some(liked) = liked_videos {
        app.engagement.replace_liked(liked, now);
        changed = true;
    }
    if !asked_channels.is_empty() {
        app.engagement
            .remember_channels(&asked_channels, &subscribed_channels, now);
        changed = true;
    }
    if changed {
        app.thumbs.mark_dirty();
    }
}

/// チャンネル登録・いいねの結果を画面へ渡す。
fn apply_oauth_done(
    app: &mut App,
    session: &mut Session,
    nonce: u64,
    result: Result<String, String>,
) {
    if nonce != session.oauth_nonce {
        return;
    }
    session.oauth_task = None;
    let action = session.oauth_action.take();
    // 失敗のときはエラーを出すだけでは「…中」が残るので、ここで畳む。
    if app.notice.as_deref().is_some_and(|n| {
        n == oauth::SUBSCRIBE_NOTICE || n == oauth::LIKE_NOTICE || n == oauth::SAVE_NOTICE
    }) {
        app.set_notice(None);
    }
    match result {
        Ok(notice) => {
            // 本家で確定したので、次の問い合わせを待たずに印を出す。
            if let Some(action) = action {
                remember_engagement(app, &action, std::time::SystemTime::now());
                app.thumbs.mark_dirty();
            }
            app.set_temporary_notice(notice, std::time::Instant::now());
        }
        Err(e) => app.set_error(Some(e)),
    }
}

/// 送った操作の内容を控えへ写す。
fn remember_engagement(app: &mut App, action: &oauth::Action, now: std::time::SystemTime) {
    match action {
        oauth::Action::Like(video_id) => app.engagement.remember_like(video_id, true, now),
        oauth::Action::Subscribe(channel_id) => {
            app.engagement.remember_subscription(channel_id, true, now)
        }
        oauth::Action::Save(video_id) => {
            app.saved_videos.insert(video_id.clone());
        }
    }
}

/// 引き終えたチャンネルへ移る。runner はチャンネルのタブを取りに行く側へ渡す。
fn apply_channel_lookup_with<R>(
    app: &mut App,
    tx: &UnboundedSender<AppEvent>,
    session: &mut Session,
    nonce: u64,
    video_id: String,
    result: Result<Option<ChannelRef>, String>,
    runner: R,
) where
    R: YtDlp + Send + Sync + 'static,
{
    if nonce != session.channel_lookup_nonce {
        return;
    }
    session.channel_lookup_task = None;
    // 待っている間の知らせは、移るときも捨てるときもここで畳む。
    if app.notice.as_deref() == Some(actions::CHANNEL_LOOKUP_NOTICE) {
        app.set_notice(None);
    }
    // 押した後に再生や設定へ移っていれば、そちらから引きずり出さない。
    if !matches!(app.mode, Mode::Results | Mode::Channel | Mode::Playlist) {
        return;
    }
    // 押した後に選択が動いていれば、届いたのは今の行のものではない。
    let Some((selected, uploader)) = app
        .view_selected_result()
        .map(|r| (r.id.clone(), r.uploader.clone()))
    else {
        return;
    };
    if selected != video_id {
        return;
    }
    match result {
        Ok(Some(channel)) => {
            // 履歴タブの行は名前を持たないので、引いた行の名前を先に使う。
            let title = channel
                .uploader
                .or(uploader)
                .unwrap_or_else(|| channel.id.clone());
            actions::enter_channel_with(app, tx, session, channel.id, title, runner);
        }
        Ok(None) => app.set_temporary_notice(
            actions::NO_CHANNEL_NOTICE.to_string(),
            std::time::Instant::now(),
        ),
        Err(e) => app.set_error(Some(e)),
    }
}

/// cookie の状態を進めてから、結果かエラーを画面へ渡す。
/// URL で指した動画が届いたらそのまま再生する。届くまでに別の画面へ移っていたら始めない。
fn plays_on_arrival(target: &Target, app: &App) -> bool {
    matches!(target, Target::Video(_))
        && app.mode == Mode::Results
        && app.error.is_none()
        && app.view_results().len() == 1
}

fn apply_search_done(
    app: &mut App,
    session: &mut Session,
    target: &Target,
    report: search::SearchReport,
    requested_limit: usize,
) {
    // URL で開いたプレイリストは名前を持たずに開くので、届いた中身から入れる。
    if let (Target::Playlist(id), Some(title)) = (target, &report.playlist_title)
        && let Some(view) = app.playlist.as_mut()
        && view.playlist_id == *id
        && view.playlist_title.is_empty()
    {
        view.playlist_title = title.clone();
    }
    let armed = matches!(app.cookies, CookieState::Armed(_));
    let source = app.cookies.for_search().cloned();
    // 待った上限は報告から取る。検索中に設定画面で変えられても文言がずれない。
    let timeout = report.timeout;
    app.cookies.observe(&report.outcome, timeout);

    if let Some(source) = &source {
        app.set_notice(match &report.outcome {
            CookieOutcome::Degraded(_) => Some(cookies::describe(&report.outcome, source, timeout)),
            // 「cookie 無しで検索しました」は、実際に出し直せたときだけ言う。
            CookieOutcome::Unreadable(_) if report.fell_back => {
                Some(cookies::describe(&report.outcome, source, timeout))
            }
            _ => None,
        });
        // ログインが要るだけなら cookie 連携自体は生きている。結果は出さず理由だけ出す。
        // モードは動かさない (actions::spawn_search の断りと同じ理由)。
        if let (CookieOutcome::LoginRequired, Target::Feed(feed)) = (&report.outcome, target) {
            app.set_error(Some(cookies::login_required_message(*feed, source)));
            return;
        }
    }

    match report.results {
        Ok(results) => {
            actions::remember_search(session, &results, app.settings.search.cache_ttl);
            app.set_results(results, target);
            // Target::Feed / Target::Channel は requested_limit を立てないままにする。
            // (App::can_load_more の判定式にそのまま乗るので、個別の場合分けが要らない)
            if matches!(target, Target::Search(_)) {
                app.view_state_mut().requested_limit = requested_limit;
            }
        }
        // 配信タブを持たないチャンネルでは yt-dlp が「そのタブは無い」と言って失敗する。
        // 一覧が無いだけなので、タブごとの文言に寄せて 0 件として扱う。
        // 起動失敗・通信失敗・タイムアウトはここへ入れない。原因が画面から消える。
        Err(e) if matches!(target, Target::Channel { .. }) && search::is_missing_tab(&e) => {
            app.set_results(Vec::new(), target)
        }
        Err(e) => {
            let in_channel = matches!(target, Target::Channel { .. }) && app.channel.is_some();
            // 初回のタイムアウトはキーチェーンのダイアログ待ちの可能性があるので、そちらを案内する。
            app.set_error(match (&report.outcome, &source) {
                (CookieOutcome::TimedOut, Some(source)) if armed => {
                    Some(cookies::describe(&report.outcome, source, timeout))
                }
                _ => Some(e),
            });
            // チャンネルの失敗で検索欄へ落とすと一覧ごと見失う。r で取り直せる場所に留める。
            app.enter_search_mode(if in_channel {
                Mode::Channel
            } else {
                Mode::Input
            });
        }
    }
}

/// mpv の失敗が cookie 由来なら連携を止める。mpv の作り直しは利用者の Enter に任せる。
fn note_cookie_failure(app: &mut App, error: Option<String>) -> Option<String> {
    let text = error?;
    let Some(source) = app.cookies.for_playback().cloned() else {
        return Some(text);
    };
    let Some(reason) = cookies::cookie_store_failure(&text, &source) else {
        return Some(text);
    };
    app.cookies.suspend(reason);
    Some(format!(
        "{text}。cookie 連携を停止しました。Enter でもう一度再生すると cookie 無しで再生します"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{ChannelView, Playback, PlaylistView};
    use crate::cookies::{ChannelTab, CookieSource};
    use crate::fetch::fixtures::{CurlResult, FakeCurl};
    use crate::kitty::fixtures::{KITTY_RECONFIG, frame};
    use crate::search::fixtures::{FakeYtDlp, done};
    use crate::search::{SearchReport, SearchResult};
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
            channel_id: None,
            is_live: false,
        }
    }

    fn key(c: char) -> AppEvent {
        AppEvent::Key(event::KeyEvent::new(
            event::KeyCode::Char(c),
            event::KeyModifiers::NONE,
        ))
    }

    fn click(column: u16, row: u16) -> AppEvent {
        AppEvent::Mouse(event::MouseEvent {
            kind: event::MouseEventKind::Down(event::MouseButton::Left),
            column,
            row,
            modifiers: event::KeyModifiers::NONE,
        })
    }

    #[tokio::test]
    async fn a_batch_takes_the_events_that_are_already_waiting() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut app = App::default();
        let mut session = Session::default();
        for c in ['b', 'c'] {
            tx.send(key(c)).expect("送れる");
        }

        handle_batch(&mut app, key('a'), &mut rx, &tx, &mut session).await;

        assert_eq!(app.query.text(), "abc");
        assert!(rx.try_recv().is_err(), "溜まっていた分は残さない");
    }

    #[tokio::test]
    async fn a_resize_ends_the_batch_before_the_next_click() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut app = App::default();
        let mut session = Session::default();
        tx.send(click(3, 1)).expect("送れる");

        handle_batch(
            &mut app,
            AppEvent::Resize {
                width: 100,
                height: 40,
            },
            &mut rx,
            &tx,
            &mut session,
        )
        .await;

        assert_eq!(session.pending_resize, Some((100, 40)));
        // 描き直して app.screen を入れ替えてから当たり判定に掛けるので、ここでは捌かない。
        assert!(matches!(rx.try_recv(), Ok(AppEvent::Mouse(_))));
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
    fn video_present_area_falls_back_to_the_full_video_area_without_a_target() {
        // 設定画面(映像が無い)でも present_video を毎フレーム呼び続けられるよう、
        // 対象が無いときも意味のある矩形を返す (呼ぶかどうかは run 側で判断しない)。
        let mut app = App {
            mode: Mode::Settings,
            ..App::default()
        };
        app.screen = Rect::new(0, 0, 80, 24);
        assert!(app.video.is_none());
        assert_eq!(
            video_present_area(&app),
            screen::playing::video_area(app.screen)
        );
    }

    #[test]
    fn video_present_area_matches_the_target_area_while_playing() {
        let video = sink();
        let mut app = App {
            mode: Mode::Playing,
            video: Some(video),
            ..App::default()
        };
        app.screen = Rect::new(0, 0, 80, 24);
        assert_eq!(
            video_present_area(&app),
            screen::playing::video_area(app.screen)
        );
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

    fn source() -> CookieSource {
        CookieSource::from_spec(Some("chrome")).expect("spec")
    }

    /// チャンネルのタブを取りに行った結果が届いた形。
    fn channel_done(
        nonce: u64,
        tab: cookies::ChannelTab,
        results: Result<Vec<SearchResult>, String>,
    ) -> AppEvent {
        AppEvent::SearchDone {
            nonce,
            target: Target::Channel {
                id: "UCabc".to_string(),
                tab,
            },
            report: SearchReport {
                results,
                outcome: CookieOutcome::NotUsed,
                fell_back: false,
                timeout: search::YT_DLP_TIMEOUT,
                playlist_title: None,
            },
            // Target::Channel には requested_limit を立てないので値は無関係。
            requested_limit: 0,
        }
    }

    /// チャンネル一覧を開いた直後の App。
    fn channel_app() -> App {
        let mut app = App {
            mode: Mode::Channel,
            searching: true,
            ..App::default()
        };
        app.results = vec![result("a")];
        app.channel = Some(app::ChannelView::new(
            "UCabc".to_string(),
            "Some Channel".to_string(),
        ));
        app
    }

    #[tokio::test]
    async fn a_channel_tab_that_does_not_exist_is_shown_as_empty_not_as_an_error() {
        // 配信タブを持たないチャンネルでは yt-dlp が失敗で返る。
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session {
            search_nonce: 1,
            ..Session::default()
        };
        let mut app = channel_app();
        app.channel.as_mut().expect("channel").tab = cookies::ChannelTab::Streams;

        handle_event(
            &mut app,
            channel_done(
                1,
                cookies::ChannelTab::Streams,
                Err("yt-dlp が失敗しました: This channel does not have a streams tab".to_string()),
            ),
            &tx,
            &mut session,
        )
        .await;

        assert_eq!(app.mode, Mode::Channel, "検索欄へ落とさない");
        assert!(app.error.is_none(), "{:?}", app.error);
        let notice = app.notice.as_deref().expect("文言を出す");
        assert!(notice.contains("ライブ配信"), "{notice}");
        assert!(app.view_results().is_empty());
        assert!(!app.searching);
    }

    #[tokio::test]
    async fn a_channel_tab_that_fails_for_another_reason_keeps_the_reason_on_screen() {
        // yt-dlp が無い・通信が切れた等を 0 件に潰すと、原因が画面から消えてしまう。
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session {
            search_nonce: 1,
            ..Session::default()
        };
        let mut app = channel_app();

        handle_event(
            &mut app,
            channel_done(
                1,
                cookies::ChannelTab::Videos,
                Err("yt-dlp が見つかりません (PATH を確認してください)".to_string()),
            ),
            &tx,
            &mut session,
        )
        .await;

        let error = app.error.as_deref().expect("原因を出す");
        assert!(error.contains("yt-dlp が見つかりません"), "{error}");
        assert_eq!(app.mode, Mode::Channel, "検索欄へ落とさない");
        let state = app.channel.as_ref().expect("channel").state();
        assert!(!state.loaded, "r で取り直せるよう読み込み済みにしない");
        assert!(app.notice.is_none(), "{:?}", app.notice);
    }

    #[tokio::test]
    async fn a_channel_search_that_times_out_explains_the_stopped_cookies() {
        // cookie 連携を止める以上、その理由も出す。0 件に潰すと黙って止まる。
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session {
            search_nonce: 1,
            ..Session::default()
        };
        let mut app = App {
            cookies: CookieState::Armed(source()),
            ..channel_app()
        };

        handle_event(
            &mut app,
            AppEvent::SearchDone {
                nonce: 1,
                target: Target::Channel {
                    id: "UCabc".to_string(),
                    tab: cookies::ChannelTab::Videos,
                },
                report: SearchReport {
                    results: Err("検索がタイムアウトしました (30 秒)".to_string()),
                    outcome: CookieOutcome::TimedOut,
                    fell_back: false,
                    timeout: Duration::from_secs(30),
                    playlist_title: None,
                },
                requested_limit: 0,
            },
            &tx,
            &mut session,
        )
        .await;

        assert!(matches!(app.cookies, CookieState::Suspended { .. }));
        let error = app.error.as_deref().expect("説明を出す");
        assert!(error.contains("30 秒"), "{error}");
        assert_eq!(app.mode, Mode::Channel);
    }

    #[tokio::test]
    async fn a_channel_tab_that_returns_videos_fills_the_channel_list() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session {
            search_nonce: 1,
            ..Session::default()
        };
        let mut app = channel_app();

        handle_event(
            &mut app,
            channel_done(
                1,
                cookies::ChannelTab::Videos,
                Ok(vec![result("v0"), result("v1")]),
            ),
            &tx,
            &mut session,
        )
        .await;

        assert_eq!(app.mode, Mode::Channel);
        assert_eq!(app.view_result_ids(), ["v0", "v1"]);
        assert_eq!(app.result_ids(), ["a"], "検索結果は残す");
        assert!(app.error.is_none());
    }

    fn search_done(
        nonce: u64,
        outcome: CookieOutcome,
        results: Result<Vec<SearchResult>, String>,
        requested_limit: usize,
    ) -> AppEvent {
        AppEvent::SearchDone {
            nonce,
            target: Target::Search("q".to_string()),
            report: SearchReport {
                results,
                outcome,
                fell_back: false,
                timeout: search::YT_DLP_TIMEOUT,
                playlist_title: None,
            },
            requested_limit,
        }
    }

    #[tokio::test]
    async fn search_done_advances_the_cookie_state() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session {
            search_nonce: 1,
            ..Session::default()
        };
        let mut app = App {
            searching: true,
            cookies: CookieState::Armed(source()),
            ..App::default()
        };
        handle_event(
            &mut app,
            search_done(1, CookieOutcome::Ok, Ok(vec![result("a")]), 1),
            &tx,
            &mut session,
        )
        .await;

        assert_eq!(app.cookies, CookieState::Active(source()));
        assert_eq!(app.mode, Mode::Results);
        assert!(!app.searching);
        assert!(app.notice.is_none());
    }

    #[tokio::test]
    async fn a_successful_search_records_the_limit_it_actually_requested() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session {
            search_nonce: 1,
            ..Session::default()
        };
        let mut app = App {
            searching: true,
            ..App::default()
        };

        handle_event(
            &mut app,
            search_done(1, CookieOutcome::Ok, Ok(vec![result("a"), result("b")]), 20),
            &tx,
            &mut session,
        )
        .await;

        assert_eq!(app.tabs.state().requested_limit, 20);
    }

    #[tokio::test]
    async fn a_feed_tab_never_records_a_requested_limit() {
        // Target::Feed は search.limit と無関係な固定件数を使うので、
        // App::can_load_more の式に個別の場合分けを足さずに済むよう 0 のままにする。
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session {
            search_nonce: 1,
            ..Session::default()
        };
        let mut app = App {
            searching: true,
            cookies: CookieState::Active(source()),
            ..App::default()
        };

        handle_event(
            &mut app,
            AppEvent::SearchDone {
                nonce: 1,
                target: Target::Feed(cookies::Feed::History),
                report: SearchReport {
                    results: Ok(vec![result("a")]),
                    outcome: CookieOutcome::Ok,
                    fell_back: false,
                    timeout: search::YT_DLP_TIMEOUT,
                    playlist_title: None,
                },
                requested_limit: 30,
            },
            &tx,
            &mut session,
        )
        .await;

        assert_eq!(app.tabs.state().requested_limit, 0, "Feed タブには立てない");
    }

    /// 取りに行った検索の鍵を控えた状態。設定が on のとき spawn_search が置いていくもの。
    fn pending_key(app: &App, session: &mut Session, target: &Target) {
        session.pending_cache_key = Some(actions::cache_key(
            target,
            app.settings.search.limit,
            app.cookies.for_search().is_some(),
        ));
    }

    #[test]
    fn only_a_video_asked_by_url_starts_playing_when_it_lands() {
        let video = Target::Video("jNQXAC9IVRw".to_string());
        let landed = || App {
            mode: Mode::Results,
            results: vec![result("jNQXAC9IVRw")],
            ..App::default()
        };
        assert!(plays_on_arrival(&video, &landed()));

        assert!(
            !plays_on_arrival(&Target::Search("q".to_string()), &landed()),
            "普通の検索では始めない"
        );
        let moved_away = App {
            mode: Mode::Settings,
            ..landed()
        };
        assert!(
            !plays_on_arrival(&video, &moved_away),
            "設定画面へ移っていたら始めない"
        );
        let failed = App {
            error: Some("失敗".to_string()),
            ..landed()
        };
        assert!(!plays_on_arrival(&video, &failed));
        let empty = App {
            mode: Mode::Input,
            ..App::default()
        };
        assert!(!plays_on_arrival(&video, &empty));
    }

    #[tokio::test]
    async fn a_playlist_opened_by_url_takes_its_name_from_the_results() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session {
            search_nonce: 1,
            ..Session::default()
        };
        let mut app = App {
            mode: Mode::Playlist,
            playlist: Some(PlaylistView::new("PLabc".to_string(), String::new())),
            ..App::default()
        };
        // 後に続くサムネイル取得・状態確認は外へ出るので止めておく。
        app.settings.thumbnails.enabled = false;
        app.settings.engagement.enabled = false;
        let event = AppEvent::SearchDone {
            nonce: 1,
            target: Target::Playlist("PLabc".to_string()),
            report: SearchReport {
                results: Ok(vec![result("v1")]),
                outcome: CookieOutcome::NotUsed,
                fell_back: false,
                timeout: search::YT_DLP_TIMEOUT,
                playlist_title: Some("作業用BGM".to_string()),
            },
            requested_limit: 1,
        };
        handle_event(&mut app, event, &tx, &mut session).await;

        let playlist = app.playlist.as_ref().expect("開いたまま");
        assert_eq!(playlist.playlist_title, "作業用BGM");
    }

    #[tokio::test]
    async fn a_playlist_opened_from_the_list_keeps_its_name() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session {
            search_nonce: 1,
            ..Session::default()
        };
        let mut app = App {
            mode: Mode::Playlist,
            playlist: Some(PlaylistView::new(
                "PLabc".to_string(),
                "一覧の名前".to_string(),
            )),
            ..App::default()
        };
        app.settings.thumbnails.enabled = false;
        app.settings.engagement.enabled = false;
        let event = AppEvent::SearchDone {
            nonce: 1,
            target: Target::Playlist("PLabc".to_string()),
            report: SearchReport {
                results: Ok(vec![result("v1")]),
                outcome: CookieOutcome::NotUsed,
                fell_back: false,
                timeout: search::YT_DLP_TIMEOUT,
                playlist_title: Some("別の名前".to_string()),
            },
            requested_limit: 1,
        };
        handle_event(&mut app, event, &tx, &mut session).await;

        assert_eq!(
            app.playlist.as_ref().expect("開いたまま").playlist_title,
            "一覧の名前"
        );
    }

    #[tokio::test]
    async fn a_successful_search_is_remembered() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session {
            search_nonce: 1,
            ..Session::default()
        };
        let mut app = App {
            searching: true,
            ..App::default()
        };
        app.settings.search.cache_enabled = true;
        let target = Target::Search("q".to_string());
        pending_key(&app, &mut session, &target);

        handle_event(
            &mut app,
            search_done(1, CookieOutcome::NotUsed, Ok(vec![result("a")]), 1),
            &tx,
            &mut session,
        )
        .await;

        let key = actions::cache_key(&target, app.settings.search.limit, false);
        let entry = session.search_cache.get(&key).expect("控える");
        assert_eq!(entry.results, vec![result("a")]);
        assert!(session.pending_cache_key.is_none());
    }

    #[tokio::test]
    async fn a_failed_search_is_not_remembered() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session {
            search_nonce: 1,
            ..Session::default()
        };
        let mut app = App {
            searching: true,
            ..App::default()
        };
        pending_key(&app, &mut session, &Target::Search("q".to_string()));

        handle_event(
            &mut app,
            search_done(
                1,
                CookieOutcome::TimedOut,
                Err("検索がタイムアウトしました (30 秒)".to_string()),
                10,
            ),
            &tx,
            &mut session,
        )
        .await;

        assert!(session.search_cache.is_empty(), "失敗は控えない");
    }

    #[tokio::test]
    async fn a_feed_that_needs_a_login_is_not_remembered() {
        // 結果は 0 件で返るが、出すのは理由。控えると次回その理由が出なくなる。
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session {
            search_nonce: 1,
            ..Session::default()
        };
        let mut app = App {
            searching: true,
            cookies: CookieState::Active(source()),
            ..App::default()
        };
        let target = Target::Feed(cookies::Feed::History);
        pending_key(&app, &mut session, &target);

        handle_event(
            &mut app,
            AppEvent::SearchDone {
                nonce: 1,
                target,
                report: SearchReport {
                    results: Ok(Vec::new()),
                    outcome: CookieOutcome::LoginRequired,
                    fell_back: false,
                    timeout: search::YT_DLP_TIMEOUT,
                    playlist_title: None,
                },
                requested_limit: 0,
            },
            &tx,
            &mut session,
        )
        .await;

        assert!(session.search_cache.is_empty());
        assert!(app.error.is_some(), "理由を出す");
    }

    #[test]
    fn saving_from_the_settings_screen_does_not_bake_in_the_environment() {
        // 起動 (設定の読み込み) から保存までの通し。利用者が書いた行を s で消さない。
        // 置き場はテストごとに分ける。同じ名前だと並列実行で互いのファイルを消し合う。
        let dir =
            std::env::temp_dir().join(format!("tuitube-main-settings-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("config.toml");
        std::fs::write(&path, "[cookies]\nbrowser = \"chrome\"\n").expect("書ける");

        let loaded = settings::load_from(
            Some(&path),
            settings::EnvOverrides {
                cookies: Some("none"),
                ..settings::EnvOverrides::default()
            },
        );
        let mut app = app_from(
            loaded,
            hidden::Hidden::default(),
            resume::Resume::default(),
            engagement::EngagementCache::default(),
        );
        assert_eq!(app.settings.cookies, None, "実行中は連携を切る");

        screen::settings::save_settings_to(&mut app, Some(&path), std::time::Instant::now());
        // 書いた中身を読み直して見る。指定なしだと同じ行がコメントで出るため。
        let written = std::fs::read_to_string(&path).expect("読める");
        let reread = settings::load_from(Some(&path), settings::EnvOverrides::default());
        assert_eq!(
            reread.settings.cookies,
            CookieSource::from_spec(Some("chrome")),
            "{written}"
        );
    }

    #[test]
    fn the_hidden_list_is_read_at_startup() {
        let dir = std::env::temp_dir().join(format!("tuitube-main-hidden-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("hidden.toml");
        std::fs::write(&path, "[[videos]]\nid = \"v1\"\ntitle = \"動画\"\n").expect("書ける");

        let loaded = settings::load_from(None, settings::EnvOverrides::default());
        let app = app_from(
            loaded,
            hidden::load_from(Some(&path)),
            resume::Resume::default(),
            engagement::EngagementCache::default(),
        );

        assert!(app.hidden.videos.contains("v1"), "起動時に読み込む");
        assert_eq!(app.hidden.path.as_deref(), Some(path.as_path()));
    }

    #[test]
    fn the_resume_list_is_read_at_startup() {
        let dir = std::env::temp_dir().join(format!("tuitube-main-resume-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("resume.toml");
        std::fs::write(
            &path,
            "[[videos]]\nid = \"v1\"\nposition_secs = 42.0\nduration_secs = 600.0\n",
        )
        .expect("書ける");

        let loaded = settings::load_from(None, settings::EnvOverrides::default());
        let app = app_from(
            loaded,
            hidden::Hidden::default(),
            resume::load_from(Some(&path)),
            engagement::EngagementCache::default(),
        );

        assert_eq!(app.resume.lookup("v1"), Some(42.0), "起動時に読み込む");
    }

    /// 検索中に S を押した形。裏で検索が終わっても設定画面は開いたままにする。
    async fn settings_open_when_the_search_lands(
        results: Result<Vec<SearchResult>, String>,
    ) -> App {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session {
            search_nonce: 1,
            ..Session::default()
        };
        let mut app = App {
            mode: Mode::Settings,
            settings_screen: SettingsScreen {
                return_mode: Mode::Input,
                ..SettingsScreen::default()
            },
            searching: true,
            ..App::default()
        };
        handle_event(
            &mut app,
            search_done(1, CookieOutcome::Ok, results, 10),
            &tx,
            &mut session,
        )
        .await;
        app
    }

    #[tokio::test]
    async fn a_search_landing_behind_the_settings_screen_does_not_close_it() {
        let app = settings_open_when_the_search_lands(Ok(vec![result("a")])).await;
        assert_eq!(app.mode, Mode::Settings, "編集中の画面を閉じない");
        assert_eq!(
            app.settings_screen.return_mode,
            Mode::Results,
            "閉じたら結果へ戻す"
        );
        assert_eq!(app.results.len(), 1, "結果は受け取っておく");
        assert!(!app.searching);
    }

    #[tokio::test]
    async fn a_failed_search_behind_the_settings_screen_does_not_close_it_either() {
        let app = settings_open_when_the_search_lands(Err("yt-dlp が落ちた".to_string())).await;
        assert_eq!(app.mode, Mode::Settings);
        assert_eq!(app.settings_screen.return_mode, Mode::Input);
        assert_eq!(app.error.as_deref(), Some("yt-dlp が落ちた"));
    }

    #[tokio::test]
    async fn search_done_from_a_superseded_nonce_does_not_touch_the_state() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session {
            search_nonce: 2,
            ..Session::default()
        };
        let mut app = App {
            searching: true,
            cookies: CookieState::Armed(source()),
            ..App::default()
        };
        handle_event(
            &mut app,
            search_done(
                1,
                CookieOutcome::Unreadable("could not find chrome cookies database".to_string()),
                Ok(Vec::new()),
                10,
            ),
            &tx,
            &mut session,
        )
        .await;

        assert_eq!(app.cookies, CookieState::Armed(source()));
        assert!(app.searching, "古い試行では検索中のままにする");
        assert!(app.error.is_none());
    }

    #[tokio::test]
    async fn search_done_with_fallback_sets_the_notice() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session {
            search_nonce: 1,
            ..Session::default()
        };
        let mut app = App {
            cookies: CookieState::Armed(source()),
            ..App::default()
        };
        let event = AppEvent::SearchDone {
            nonce: 1,
            target: Target::Search("q".to_string()),
            report: SearchReport {
                results: Ok(vec![result("a")]),
                outcome: CookieOutcome::Unreadable(
                    "could not find chrome cookies database in '/x'".to_string(),
                ),
                fell_back: true,
                timeout: search::YT_DLP_TIMEOUT,
                playlist_title: None,
            },
            requested_limit: 1,
        };
        handle_event(&mut app, event, &tx, &mut session).await;

        // cookie 無しの結果は出しつつ、効いていない旨を伝える。
        assert_eq!(app.mode, Mode::Results);
        assert!(app.error.is_none());
        let notice = app.notice.expect("説明を出す");
        assert!(notice.contains("cookie 無しで検索しました"), "{notice}");
        assert!(matches!(app.cookies, CookieState::Suspended { .. }));
    }

    #[tokio::test]
    async fn a_search_that_could_not_decrypt_the_cookies_says_so() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session {
            search_nonce: 1,
            ..Session::default()
        };
        let mut app = App {
            cookies: CookieState::Armed(source()),
            ..App::default()
        };
        // 後に続くサムネイル取得・状態確認は外へ出るので止めておく。
        app.settings.thumbnails.enabled = false;
        app.settings.engagement.enabled = false;
        let event = AppEvent::SearchDone {
            nonce: 1,
            target: Target::Search("q".to_string()),
            report: SearchReport {
                results: Ok(vec![result("a")]),
                outcome: CookieOutcome::Degraded("failed to decrypt".to_string()),
                fell_back: false,
                timeout: search::YT_DLP_TIMEOUT,
                playlist_title: None,
            },
            requested_limit: 1,
        };
        handle_event(&mut app, event, &tx, &mut session).await;

        assert_eq!(app.mode, Mode::Results, "結果は出す");
        assert!(app.error.is_none());
        let notice = app.notice.expect("説明を出す");
        assert!(notice.contains("cookie を復号できませんでした"), "{notice}");
    }

    #[tokio::test]
    async fn a_timeout_names_the_seconds_the_search_actually_waited() {
        // 検索中に設定画面で秒数を変えても、文言は打ち切った側の秒数で出す。
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session {
            search_nonce: 1,
            ..Session::default()
        };
        let mut app = App {
            searching: true,
            cookies: CookieState::Armed(source()),
            ..App::default()
        };
        app.settings.search.timeout = Duration::from_secs(120);
        let event = AppEvent::SearchDone {
            nonce: 1,
            target: Target::Search("q".to_string()),
            report: SearchReport {
                results: Err("検索がタイムアウトしました (30 秒)".to_string()),
                outcome: CookieOutcome::TimedOut,
                fell_back: false,
                timeout: Duration::from_secs(30),
                playlist_title: None,
            },
            requested_limit: 10,
        };
        handle_event(&mut app, event, &tx, &mut session).await;

        let error = app.error.clone().expect("説明を出す");
        assert!(error.contains("30 秒"), "{error}");
        assert!(!error.contains("120 秒"), "{error}");
        let CookieState::Suspended { reason, .. } = &app.cookies else {
            panic!("停止する");
        };
        assert!(reason.contains("30 秒"), "{reason}");
        assert!(!reason.contains("120 秒"), "{reason}");
    }

    #[test]
    fn mpv_exit_with_an_unreadable_cookie_store_suspends() {
        let mut app = App {
            cookies: CookieState::Active(source()),
            ..App::default()
        };
        let error = note_cookie_failure(
            &mut app,
            Some(
                "mpv が異常終了しました (終了コード 2): ERROR: could not find chrome cookies database in '/x'"
                    .to_string(),
            ),
        )
        .expect("エラーは残す");
        assert!(error.contains("cookie 連携を停止しました"), "{error}");
        assert!(matches!(app.cookies, CookieState::Suspended { .. }));

        // cookie と関係のない失敗では触らない。
        let mut app = App {
            cookies: CookieState::Active(source()),
            ..App::default()
        };
        let error = note_cookie_failure(&mut app, Some("mpv が異常終了しました".to_string()))
            .expect("エラーは残す");
        assert_eq!(error, "mpv が異常終了しました");
        assert_eq!(app.cookies, CookieState::Active(source()));
        assert_eq!(note_cookie_failure(&mut app, None), None);
    }

    #[test]
    fn an_unrelated_permission_error_from_mpv_keeps_the_cookie_state() {
        // mpv はストリームやソケットの失敗でも同じ文言を出すので、cookie 連携は止めない。
        let mut app = App {
            cookies: CookieState::Active(source()),
            ..App::default()
        };
        let text = "mpv が異常終了しました (終了コード 2): Operation not permitted: '/dev/dsp'";
        let error = note_cookie_failure(&mut app, Some(text.to_string())).expect("エラーは残す");
        assert_eq!(error, text);
        assert_eq!(app.cookies, CookieState::Active(source()));

        // cookie ファイルを指す権限エラーなら止める。
        let mut app = App {
            cookies: CookieState::Active(source()),
            ..App::default()
        };
        let error = note_cookie_failure(
            &mut app,
            Some(
                "mpv が異常終了しました (終了コード 2): ERROR: [Errno 1] Operation not permitted: '/Users/x/Library/Containers/com.apple.Safari/Data/Library/Cookies/Cookies.binarycookies'"
                    .to_string(),
            ),
        )
        .expect("エラーは残す");
        assert!(error.contains("cookie 連携を停止しました"), "{error}");
        assert!(matches!(app.cookies, CookieState::Suspended { .. }));
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

    fn thumb_app(count: usize) -> App {
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

    /// 1 セルに収まる小さな画像。placement が拒まない寸法にしておく。
    fn thumb() -> rgb::RgbImage {
        rgb::RgbImage::new(16, 16, vec![9; 16 * 16 * 3]).expect("長さは合っている")
    }

    fn make_ready(app: &mut App, ids: &[&str]) {
        let images = ids
            .iter()
            .map(|id| ((*id).to_string(), Ok(thumb())))
            .collect();
        app.thumbs.apply(images, (144, 80));
    }

    fn thumbs_bytes(app: &mut App) -> Vec<u8> {
        let mut out = Vec::new();
        present_thumbs(app, CELL, &mut out).expect("Vec への書き込みは失敗しない");
        out
    }

    fn count_images(out: &[u8]) -> usize {
        out.windows(6).filter(|w| *w == b"\x1b_Ga=T").count()
    }

    #[test]
    fn present_thumbs_writes_nothing_when_not_dirty() {
        let mut app = thumb_app(4);
        make_ready(&mut app, &["id0"]);
        assert!(!thumbs_bytes(&mut app).is_empty(), "変化があれば書く");
        assert!(thumbs_bytes(&mut app).is_empty(), "変化が無ければ書かない");
    }

    #[test]
    fn present_thumbs_writes_clear_then_one_apc_per_ready_image() {
        let mut app = thumb_app(4);
        make_ready(&mut app, &["id0", "id1", "id2", "id3"]);
        let out = thumbs_bytes(&mut app);

        assert!(
            out.starts_with(&clear_bytes()),
            "貼り直す前に画面の画像を消す"
        );
        assert_eq!(count_images(&out), 4);
        // 1 枚ごとに CUP で絶対位置へ寄せる。
        assert_eq!(
            out.windows(2).filter(|w| *w == b"\x1b[").count(),
            4,
            "CUP の数が画像の数と合わない"
        );
    }

    #[test]
    fn present_thumbs_skips_cells_without_an_image() {
        let mut app = thumb_app(4);
        make_ready(&mut app, &["id1"]);
        let out = thumbs_bytes(&mut app);

        assert!(out.starts_with(&clear_bytes()));
        assert_eq!(count_images(&out), 1, "届いた 1 枚だけ貼る");
    }

    #[test]
    fn present_thumbs_writes_nothing_while_playing() {
        let mut app = thumb_app(4);
        make_ready(&mut app, &["id0"]);
        app.mode = Mode::Playing;
        assert!(thumbs_bytes(&mut app).is_empty(), "映像と重なる");

        // 戻れば貼り直す。再生中に dirty を食い潰さない。
        app.mode = Mode::Results;
        assert_eq!(count_images(&thumbs_bytes(&mut app)), 1);
    }

    #[test]
    fn present_thumbs_marks_dirty_again_after_a_placement_failure_so_it_retries() {
        // 画面に絶対収まらない寸法。cell_size が取得時とズレたケースの代わり。
        let huge =
            rgb::RgbImage::new(2000, 2000, vec![9; 2000 * 2000 * 3]).expect("長さは合っている");
        let mut app = thumb_app(4);
        app.thumbs
            .apply(vec![("id0".to_string(), Ok(huge))], (2000, 2000));

        let out = thumbs_bytes(&mut app);
        assert_eq!(count_images(&out), 0, "サイズが収まらず描けない");
        assert_eq!(out, clear_bytes(), "描けなかった分は消すだけ書く");

        // 一度失敗しても dirty を戻すので、次のフレームでまた描こうとする
        // (直せば描けるようになるが、直らない間も食い潰されず毎回試す)。
        let out2 = thumbs_bytes(&mut app);
        assert_eq!(out2, clear_bytes(), "再試行が起きるので2回目も空にならない");
    }

    #[test]
    fn present_thumbs_clears_the_dirty_flag_after_writing() {
        let mut app = thumb_app(4);
        app.thumbs.mark_dirty();
        assert!(!thumbs_bytes(&mut app).is_empty());
        assert!(thumbs_bytes(&mut app).is_empty());

        // スクロールや結果の入れ替えでまた立つ。
        app.thumbs.mark_dirty();
        assert!(!thumbs_bytes(&mut app).is_empty());
    }

    #[test]
    fn present_thumbs_returns_the_cursor_to_the_search_box_in_input_mode() {
        let mut app = thumb_app(4);
        make_ready(&mut app, &["id0"]);
        app.mode = Mode::Input;
        app.query.set("らー");
        let out = thumbs_bytes(&mut app);

        let (x, y) = screen::browse::input_cursor(app.screen, &app.query);
        let cup = format!("\x1b[{};{}H", y + 1, x + 1).into_bytes();
        assert!(count_images(&out) > 0, "画像を貼ってからの話");
        assert!(out.ends_with(&cup), "入力欄へ戻さないと格子の中で点滅する");

        // 結果一覧ではカーソルを出していないので戻す必要がない。
        let mut app = thumb_app(4);
        make_ready(&mut app, &["id0"]);
        assert!(!thumbs_bytes(&mut app).ends_with(&cup));
    }

    #[test]
    fn present_thumbs_writes_only_the_clear_in_list_mode() {
        let mut app = thumb_app(4);
        make_ready(&mut app, &["id0"]);
        app.settings.search.layout = crate::grid::LayoutMode::List;
        let out = thumbs_bytes(&mut app);

        assert_eq!(out, clear_bytes(), "リスト表示では貼らず、残骸だけ消す");
    }

    /// `index` 番目のセルに貼られるサムネイルの配置。
    fn thumb_placement(app: &App, index: usize) -> video::Placement {
        let layout = ui::grid_layout(app, CELL).expect("格子を組める");
        video::placement(layout.cells[index].image, CELL, (16, 16)).expect("置ける")
    }

    fn find(out: &[u8], needle: &[u8]) -> Option<usize> {
        out.windows(needle.len()).position(|w| w == needle)
    }

    /// 印の送出列。判定は badge 側の実装を通す。
    fn badge_bytes(badges: &[badge::Badge], at: video::Placement) -> Vec<u8> {
        let mut out = Vec::new();
        badge::encode(badges, at, CELL, &mut out);
        out
    }

    #[test]
    fn present_thumbs_overlays_a_badge_on_a_liked_thumbnail() {
        let mut app = thumb_app(4);
        make_ready(&mut app, &["id0", "id1", "id2", "id3"]);
        app.engagement
            .remember_like("id1", true, std::time::UNIX_EPOCH);
        app.thumbs.mark_dirty();
        let at = thumb_placement(&app, 1);

        let out = thumbs_bytes(&mut app);

        assert_eq!(count_images(&out), 5, "サムネイル 4 枚 + 印 1 個");
        let badge = badge_bytes(&[badge::Badge::Liked], at);
        let badge_at = find(&out, &badge).expect("いいね済みの印が無い");
        let mut thumbnail = Vec::new();
        rgb::encode_image(&thumb(), at, &mut thumbnail);
        let thumbnail_at = find(&out, &thumbnail).expect("サムネイルが無い");
        assert!(thumbnail_at < badge_at, "印が先だとサムネイルの下に隠れる");
    }

    #[test]
    fn present_thumbs_overlays_both_badges_on_a_liked_video_of_a_subscribed_channel() {
        let mut app = thumb_app(4);
        make_ready(&mut app, &["id0", "id1", "id2", "id3"]);
        app.results[1].channel_id = Some("UC1".to_string());
        app.engagement
            .remember_like("id1", true, std::time::UNIX_EPOCH);
        app.engagement
            .remember_subscription("UC1", true, std::time::UNIX_EPOCH);
        app.thumbs.mark_dirty();
        let at = thumb_placement(&app, 1);

        let out = thumbs_bytes(&mut app);

        assert_eq!(count_images(&out), 6, "サムネイル 4 枚 + 印 2 個");
        let badges = badge_bytes(&[badge::Badge::Liked, badge::Badge::Subscribed], at);
        assert!(find(&out, &badges).is_some(), "印が 2 個並んでいない");
    }

    #[test]
    fn present_thumbs_writes_no_badge_when_the_engagement_setting_is_off() {
        let mut app = thumb_app(4);
        make_ready(&mut app, &["id0", "id1", "id2", "id3"]);
        app.engagement
            .remember_like("id1", true, std::time::UNIX_EPOCH);
        app.settings.engagement.enabled = false;
        app.thumbs.mark_dirty();

        assert_eq!(count_images(&thumbs_bytes(&mut app)), 4, "印を出さない");
    }

    /// タブ単位の印の送出列。位置は badge 側の実装を通す。
    fn tab_badge_bytes(badges: &[badge::Badge], at: video::Placement) -> Vec<u8> {
        let mut out = Vec::new();
        badge::encode_tab(badges, at, CELL, &mut out);
        out
    }

    /// ショートのタブを見ている 80x24 の画面。サムネイルは検索結果と同じ id で持つ。
    fn shorts_thumb_app(count: usize) -> App {
        let mut app = thumb_app(count);
        let mut view = ChannelView::new("UCabc".to_string(), "Some Channel".to_string());
        // タブを切り替えるテストがあるので、どちらのタブでも同じ行が見えるようにする。
        for tab in [ChannelTab::Videos, ChannelTab::Shorts] {
            view.tab = tab;
            view.state_mut().results = app.results.clone();
            view.state_mut().loaded = true;
        }
        app.channel = Some(view);
        app.mode = Mode::Channel;
        app
    }

    #[test]
    fn present_thumbs_marks_every_thumbnail_on_the_shorts_tab() {
        let mut app = shorts_thumb_app(2);
        make_ready(&mut app, &["id0", "id1"]);
        app.thumbs.mark_dirty();
        let at = thumb_placement(&app, 1);

        let out = thumbs_bytes(&mut app);

        assert_eq!(count_images(&out), 4, "サムネイル 2 枚 + 印 2 個");
        let mark = tab_badge_bytes(&[badge::Badge::Shorts], at);
        assert!(find(&out, &mark).is_some(), "ショートの印が無い");
    }

    #[test]
    fn present_thumbs_leaves_the_thumbnails_bare_on_the_other_channel_tabs() {
        let mut app = shorts_thumb_app(2);
        app.channel.as_mut().expect("チャンネル").tab = ChannelTab::Videos;
        make_ready(&mut app, &["id0", "id1"]);
        app.thumbs.mark_dirty();

        assert_eq!(count_images(&thumbs_bytes(&mut app)), 2, "印を出さない");
    }

    #[test]
    fn present_thumbs_keeps_the_kind_badges_when_the_engagement_setting_is_off() {
        // ライブ・ショートは検索結果だけで分かるので、OAuth の設定で消さない。
        let mut app = shorts_thumb_app(2);
        app.channel
            .as_mut()
            .expect("チャンネル")
            .state_mut()
            .results[1]
            .is_live = true;
        make_ready(&mut app, &["id0", "id1"]);
        app.engagement
            .remember_like("id1", true, std::time::UNIX_EPOCH);
        app.settings.engagement.enabled = false;
        app.thumbs.mark_dirty();
        let at = thumb_placement(&app, 1);

        let out = thumbs_bytes(&mut app);

        assert_eq!(
            count_images(&out),
            5,
            "サムネイル 2 枚 + ショート 2 個 + ライブ 1 個"
        );
        let live = badge_bytes(&[badge::Badge::Live], at);
        assert!(find(&out, &live).is_some(), "ライブの印が無い");
        let liked = badge_bytes(&[badge::Badge::Liked], at);
        assert!(find(&out, &liked).is_none(), "いいねの印は出さない");
    }

    #[test]
    fn present_thumbs_adds_the_badge_after_the_thumbnail_without_changing_it() {
        let mut app = thumb_app(1);
        make_ready(&mut app, &["id0"]);
        let plain = thumbs_bytes(&mut app);

        app.engagement
            .remember_like("id0", true, std::time::UNIX_EPOCH);
        app.thumbs.mark_dirty();
        let out = thumbs_bytes(&mut app);

        assert!(
            out.starts_with(&plain),
            "サムネイル側の送出列は変えず、後ろへ足すだけ"
        );
        assert_eq!(count_images(&out), 2);
    }

    #[test]
    fn starting_playback_erases_the_thumbnails_from_the_screen() {
        let mut app = thumb_app(4);
        make_ready(&mut app, &["id0"]);
        // 画像を貼った状態から再生へ移る。
        assert_eq!(count_images(&thumbs_bytes(&mut app)), 1);

        let mut session = Session::default();
        actions::enter_playback(
            &mut app,
            &mut session,
            "song".to_string(),
            "https://www.youtube.com/watch?v=id0".to_string(),
            "id0".to_string(),
            None,
            sink(),
        );
        let out = present(&mut session, &app);

        assert!(out.starts_with(&clear_bytes()), "a=d が出ない");
        // 再生中は present_thumbs が 1 バイトも書かないので、消せるのはここだけ。
        assert!(thumbs_bytes(&mut app).is_empty());
    }

    #[test]
    fn present_video_runs_before_present_thumbs() {
        // 再生終了で持ち越した a=d が、貼ったばかりのサムネイルを消さない順序であること。
        let mut app = thumb_app(4);
        make_ready(&mut app, &["id0"]);
        let mut session = Session {
            owe_clear: true,
            ..Session::default()
        };

        let mut out = Vec::new();
        present_video(&mut session, &app, area(), &mut out).expect("書ける");
        present_thumbs(&mut app, CELL, &mut out).expect("書ける");

        let clear = clear_bytes();
        let last_clear = out
            .windows(clear.len())
            .rposition(|w| w == clear.as_slice())
            .expect("画像の削除が入っている");
        let first_image = out
            .windows(6)
            .position(|w| w == b"\x1b_Ga=T")
            .expect("画像が入っている");
        assert!(
            last_clear < first_image,
            "最後の a=d より後に画像が来ていない"
        );
    }

    #[tokio::test]
    async fn thumbs_ready_from_a_superseded_nonce_is_discarded() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session {
            search_nonce: 2,
            ..Session::default()
        };
        let mut app = thumb_app(2);
        handle_event(
            &mut app,
            AppEvent::ThumbsReady {
                nonce: 1,
                target_px: (144, 80),
                images: vec![("id0".to_string(), Ok(thumb()))],
                notice: None,
                disable: None,
            },
            &tx,
            &mut session,
        )
        .await;

        assert!(app.thumbs.get("id0").is_none(), "古い検索の画像は捨てる");
        assert!(!app.thumbs.take_dirty());
    }

    #[tokio::test]
    async fn thumbs_ready_applies_the_images_and_the_notice() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = thumb_app(2);
        app.thumbs.set_fetching(true);
        handle_event(
            &mut app,
            AppEvent::ThumbsReady {
                nonce: 0,
                target_px: (144, 80),
                images: vec![
                    ("id0".to_string(), Ok(thumb())),
                    ("id1".to_string(), Err(())),
                ],
                notice: Some("保存先を作れませんでした".to_string()),
                disable: None,
            },
            &tx,
            &mut session,
        )
        .await;

        assert!(app.thumbs.get("id0").is_some());
        assert!(app.thumbs.get("id1").is_none());
        assert!(app.thumbs.take_dirty());
        assert!(!app.thumbs.is_fetching());
        assert_eq!(app.notice.as_deref(), Some("保存先を作れませんでした"));
    }

    #[tokio::test]
    async fn a_missing_curl_disables_thumbnails_with_a_notice() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = thumb_app(2);
        handle_event(
            &mut app,
            AppEvent::ThumbsReady {
                nonce: 0,
                target_px: (144, 80),
                images: vec![("id0".to_string(), Err(()))],
                notice: None,
                disable: Some(thumbs::MISSING_CURL.to_string()),
            },
            &tx,
            &mut session,
        )
        .await;

        assert_eq!(app.notice.as_deref(), Some(thumbs::MISSING_CURL));
        // 以後は取りに行かない。
        assert!(app.thumbs.wanted(&app.result_ids(), (144, 80)).is_empty());
    }

    #[tokio::test]
    async fn switching_to_a_loaded_tab_refetches_its_thumbnails_from_the_cache() {
        // タブを戻したときメモリ上の画像は捨ててあるので、キャッシュから読み直す。
        let dir = std::env::temp_dir().join(format!("tuitube-main-switch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(
            dir.join("id0.jpg"),
            include_bytes!("testdata/tiny8x4.jpg").as_slice(),
        )
        .expect("書ける");

        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = thumb_app(0);
        app.settings.thumbnails.cache_dir = Some(dir.clone());

        // 2 つ目のタブに結果を持たせてから「すべて」へ戻る。
        app.tabs.next();
        app.set_results(vec![result("id0")], &Target::Search("音楽".to_string()));
        app.store_to_tab();
        app.tabs.prev();
        app.sync_from_tab();
        app.thumbs.take_dirty();

        // Tab で読み込み済みのタブへ移ると、yt-dlp は動かずサムネイルだけ取り直す。
        // 取得は渡した偽物を通るので、キャッシュが外れても curl は起動しない。
        handle_key_event(
            &mut app,
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Tab,
                crossterm::event::KeyModifiers::NONE,
            ),
            &tx,
            &mut session,
            FakeCurl::new(CurlResult::Failed, b""),
        )
        .await;

        assert!(session.search_task.is_none(), "保持していた結果を出すだけ");
        let task = session.thumbs_task.take().expect("取り直しが積まれる");
        task.await.expect("タスクは panic しない");
        let event = rx.try_recv().expect("ThumbsReady が届く");
        let AppEvent::ThumbsReady { images, .. } = &event else {
            panic!("ThumbsReady のはず");
        };
        assert_eq!(images.len(), 1);
        assert!(images[0].1.is_ok(), "キャッシュから読めている");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn clicking_a_loaded_tab_refetches_its_thumbnails_from_the_cache() {
        // マウスで移ったときもキー起因と同じく画像を読み直す。
        let dir = std::env::temp_dir().join(format!("tuitube-main-click-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(
            dir.join("id0.jpg"),
            include_bytes!("testdata/tiny8x4.jpg").as_slice(),
        )
        .expect("書ける");

        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = thumb_app(0);
        app.settings.thumbnails.cache_dir = Some(dir.clone());

        app.tabs.next();
        app.set_results(vec![result("id0")], &Target::Search("音楽".to_string()));
        app.store_to_tab();
        app.tabs.prev();
        app.sync_from_tab();
        app.thumbs.take_dirty();

        // タブ行 (y=3) の「音楽」の桁を押す。
        handle_mouse_event(
            &mut app,
            crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
                column: 9,
                row: 3,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
            &tx,
            &mut session,
            FakeCurl::new(CurlResult::Failed, b""),
        )
        .await;

        assert_eq!(app.tabs.selected(), 1);
        assert!(session.search_task.is_none(), "保持していた結果を出すだけ");
        let task = session.thumbs_task.take().expect("取り直しが積まれる");
        task.await.expect("タスクは panic しない");
        let event = rx.try_recv().expect("ThumbsReady が届く");
        let AppEvent::ThumbsReady { images, .. } = &event else {
            panic!("ThumbsReady のはず");
        };
        assert_eq!(images.len(), 1);
        assert!(images[0].1.is_ok(), "キャッシュから読めている");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn comment(text: &str) -> crate::comments::Comment {
        crate::comments::Comment {
            author: "alice".to_string(),
            text: text.to_string(),
            like_count: None,
        }
    }

    /// コメント取得中の再生画面。
    fn commenting_app() -> App {
        let mut app = App {
            mode: Mode::Playing,
            ..App::default()
        };
        app.comments.begin("abc".to_string());
        app
    }

    #[tokio::test]
    async fn comments_ready_is_taken_in_for_the_video_that_is_playing() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session {
            comments_nonce: 1,
            ..Session::default()
        };
        let mut app = commenting_app();
        handle_event(
            &mut app,
            AppEvent::CommentsReady {
                nonce: 1,
                video_id: "abc".to_string(),
                comments: Ok(vec![comment("first")]),
            },
            &tx,
            &mut session,
        )
        .await;

        assert_eq!(
            app.comments.state(),
            Some(&comments::CommentState::Ready(vec![comment("first")]))
        );
        assert!(session.comments_task.is_none());
    }

    #[tokio::test]
    async fn comments_from_a_superseded_playback_are_discarded() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session {
            comments_nonce: 2,
            ..Session::default()
        };
        let mut app = commenting_app();
        handle_event(
            &mut app,
            AppEvent::CommentsReady {
                nonce: 1,
                video_id: "abc".to_string(),
                comments: Ok(vec![comment("古い")]),
            },
            &tx,
            &mut session,
        )
        .await;

        assert_eq!(
            app.comments.state(),
            Some(&comments::CommentState::Pending),
            "前の再生ぶんは捨てる"
        );
    }

    /// チャンネル引きの結果を待っている一覧。1 行目は履歴タブと同じく名前も持たない。
    fn lookup_app() -> App {
        let mut app = App {
            mode: Mode::Results,
            screen: Rect::new(0, 0, 80, 24),
            ..App::default()
        };
        app.set_results(
            vec![
                result("id0"),
                SearchResult {
                    uploader: Some("Row Channel".to_string()),
                    ..result("id1")
                },
            ],
            &Target::Search("q".to_string()),
        );
        app.selected = 1;
        app.set_notice(Some(actions::CHANNEL_LOOKUP_NOTICE.to_string()));
        app
    }

    fn found(id: &str, uploader: Option<&str>) -> Result<Option<ChannelRef>, String> {
        Ok(Some(ChannelRef {
            id: id.to_string(),
            uploader: uploader.map(str::to_string),
        }))
    }

    /// 引き終えた結果を、実 yt-dlp を起こさない runner で捌く。
    fn lookup_done(
        app: &mut App,
        session: &mut Session,
        tx: &UnboundedSender<AppEvent>,
        nonce: u64,
        video_id: &str,
        result: Result<Option<ChannelRef>, String>,
    ) {
        apply_channel_lookup_with(
            app,
            tx,
            session,
            nonce,
            video_id.to_string(),
            result,
            FakeYtDlp::new([done(0, "", "")]),
        );
    }

    /// チャンネルのタブを取りに行くタスクを終わらせる。
    async fn finish_search(session: &mut Session) {
        session
            .search_task
            .take()
            .expect("タブの検索")
            .await
            .expect("タスクは panic しない");
    }

    fn looking_up() -> Session {
        Session {
            channel_lookup_nonce: 1,
            ..Session::default()
        }
    }

    #[test]
    fn an_oauth_result_replaces_the_waiting_notice() {
        let mut app = App {
            notice: Some(oauth::SUBSCRIBE_NOTICE.to_string()),
            ..App::default()
        };
        let mut session = Session {
            oauth_nonce: 3,
            ..Session::default()
        };

        apply_oauth_done(
            &mut app,
            &mut session,
            3,
            Ok(oauth::SUBSCRIBED_NOTICE.to_string()),
        );

        assert_eq!(app.notice.as_deref(), Some(oauth::SUBSCRIBED_NOTICE));
        assert!(app.notice_until.is_some(), "返事なので期限で消える");
        assert!(app.error.is_none());
    }

    #[test]
    fn a_failed_oauth_clears_the_waiting_notice_and_shows_the_reason() {
        let mut app = App {
            notice: Some(oauth::LIKE_NOTICE.to_string()),
            ..App::default()
        };
        let mut session = Session {
            oauth_nonce: 1,
            ..Session::default()
        };

        apply_oauth_done(&mut app, &mut session, 1, Err("拒否されました".to_string()));

        assert!(app.notice.is_none(), "「…中」を残さない");
        assert_eq!(app.error.as_deref(), Some("拒否されました"));
    }

    #[test]
    fn an_oauth_result_keeps_an_unrelated_notice() {
        let mut app = App {
            notice: Some("別の知らせ".to_string()),
            ..App::default()
        };
        let mut session = Session::default();

        apply_oauth_done(&mut app, &mut session, 0, Err("拒否されました".to_string()));

        assert_eq!(app.notice.as_deref(), Some("別の知らせ"));
        assert_eq!(app.error.as_deref(), Some("拒否されました"));
    }

    #[test]
    fn a_superseded_oauth_result_is_discarded() {
        let mut app = App {
            notice: Some(oauth::SUBSCRIBE_NOTICE.to_string()),
            ..App::default()
        };
        let mut session = Session {
            oauth_nonce: 5,
            ..Session::default()
        };

        apply_oauth_done(&mut app, &mut session, 4, Ok("古い返事".to_string()));

        assert_eq!(
            app.notice.as_deref(),
            Some(oauth::SUBSCRIBE_NOTICE),
            "走っている方の知らせを畳まない"
        );
        assert!(app.error.is_none());
    }

    #[test]
    fn a_like_shows_up_on_the_thumbnails_right_away() {
        let mut app = App::default();
        app.thumbs.take_dirty();
        let mut session = Session {
            oauth_action: Some(oauth::Action::Like("v1".to_string())),
            ..Session::default()
        };

        apply_oauth_done(
            &mut app,
            &mut session,
            0,
            Ok(oauth::LIKED_NOTICE.to_string()),
        );

        assert!(app.engagement.is_liked("v1"));
        assert!(app.thumbs.take_dirty(), "印を出すために貼り直す");
        assert!(session.oauth_action.is_none(), "反映した操作は持ち越さない");
    }

    #[test]
    fn a_subscription_shows_up_right_away() {
        let mut app = App::default();
        let mut session = Session {
            oauth_action: Some(oauth::Action::Subscribe("UC1".to_string())),
            ..Session::default()
        };

        apply_oauth_done(
            &mut app,
            &mut session,
            0,
            Ok(oauth::SUBSCRIBED_NOTICE.to_string()),
        );

        assert_eq!(app.engagement.is_subscribed("UC1"), Some(true));
    }

    #[test]
    fn a_failed_operation_does_not_touch_the_engagement_cache() {
        let mut app = App::default();
        let mut session = Session {
            oauth_action: Some(oauth::Action::Like("v1".to_string())),
            ..Session::default()
        };

        apply_oauth_done(&mut app, &mut session, 0, Err("拒否されました".to_string()));

        assert!(!app.engagement.is_liked("v1"));
    }

    #[test]
    fn the_state_check_result_lands_in_the_cache() {
        let mut app = App::default();
        app.thumbs.take_dirty();
        let mut session = Session::default();

        apply_engagement_ready(
            &mut app,
            &mut session,
            0,
            Some(vec!["v1".to_string()]),
            vec!["UC1".to_string(), "UC2".to_string()],
            vec!["UC2".to_string()],
        );

        assert!(app.engagement.is_liked("v1"));
        assert_eq!(app.engagement.is_subscribed("UC1"), Some(false));
        assert_eq!(app.engagement.is_subscribed("UC2"), Some(true));
        assert!(app.thumbs.take_dirty());
    }

    #[test]
    fn a_state_check_result_from_the_previous_search_is_dropped() {
        let mut app = App::default();
        let mut session = Session {
            search_nonce: 2,
            ..Session::default()
        };

        apply_engagement_ready(
            &mut app,
            &mut session,
            1,
            Some(vec!["v1".to_string()]),
            Vec::new(),
            Vec::new(),
        );

        assert!(!app.engagement.is_liked("v1"));
    }

    #[test]
    fn the_engagement_cache_is_read_at_startup() {
        let dir =
            std::env::temp_dir().join(format!("tuitube-main-engagement-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("engagement.json");
        std::fs::write(&path, r#"{"liked_videos":["v1"]}"#).expect("書ける");

        let loaded = settings::load_from(None, settings::EnvOverrides::default());
        let app = app_from(
            loaded,
            hidden::Hidden::default(),
            resume::Resume::default(),
            engagement::load_from(Some(&path)),
        );

        assert!(app.engagement.is_liked("v1"), "起動時に読み込む");
    }

    #[tokio::test]
    async fn a_channel_lookup_moves_to_the_channel_it_found() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = looking_up();
        let mut app = lookup_app();
        // 履歴タブの行は名前を持たないので、引いた行の名前を使う。
        app.selected = 0;

        lookup_done(
            &mut app,
            &mut session,
            &tx,
            1,
            "id0",
            found("UCfeed", Some("History Channel")),
        );

        assert_eq!(app.mode, Mode::Channel);
        let channel = app.channel.as_ref().expect("チャンネルへ移る");
        assert_eq!(channel.channel_id, "UCfeed");
        assert_eq!(channel.channel_title, "History Channel");
        assert!(session.channel_lookup_task.is_none());
        assert!(app.notice.is_none(), "取得中の知らせは残さない");
        finish_search(&mut session).await;
    }

    #[tokio::test]
    async fn a_channel_lookup_takes_the_title_from_the_row_when_the_line_has_no_name() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = looking_up();
        let mut app = lookup_app();

        lookup_done(&mut app, &mut session, &tx, 1, "id1", found("UCfeed", None));

        let channel = app.channel.as_ref().expect("チャンネルへ移る");
        assert_eq!(channel.channel_title, "Row Channel");
        finish_search(&mut session).await;
    }

    #[tokio::test]
    async fn a_channel_lookup_falls_back_to_the_channel_id_for_the_title() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = looking_up();
        let mut app = lookup_app();
        app.selected = 0;

        lookup_done(&mut app, &mut session, &tx, 1, "id0", found("UCfeed", None));

        let channel = app.channel.as_ref().expect("チャンネルへ移る");
        assert_eq!(channel.channel_title, "UCfeed");
        finish_search(&mut session).await;
    }

    #[tokio::test]
    async fn a_channel_lookup_started_in_a_playlist_still_moves_to_the_channel() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = looking_up();
        let mut app = lookup_app();
        app.playlist = Some(PlaylistView::new("PL0".to_string(), "list 0".to_string()));
        app.mode = Mode::Playlist;
        app.set_results(
            vec![SearchResult {
                uploader: Some("Row Channel".to_string()),
                ..result("id1")
            }],
            &Target::Playlist("PL0".to_string()),
        );
        app.set_notice(Some(actions::CHANNEL_LOOKUP_NOTICE.to_string()));

        lookup_done(&mut app, &mut session, &tx, 1, "id1", found("UCfeed", None));

        assert_eq!(app.mode, Mode::Channel);
        assert_eq!(
            app.channel.as_ref().expect("チャンネルへ移る").channel_id,
            "UCfeed"
        );
        finish_search(&mut session).await;
    }

    #[tokio::test]
    async fn a_channel_lookup_for_another_row_is_discarded() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = looking_up();
        let mut app = lookup_app();

        // 押した後に選択が動いていれば、その結果は今の行のものではない。
        lookup_done(
            &mut app,
            &mut session,
            &tx,
            1,
            "id0",
            found("UCold", Some("Old Channel")),
        );

        assert!(app.channel.is_none());
        assert_eq!(app.mode, Mode::Results);
        assert!(session.search_task.is_none());
        assert!(app.notice.is_none(), "取得中の知らせは残さない");
    }

    #[tokio::test]
    async fn a_channel_lookup_that_lands_after_moving_on_is_discarded() {
        // 引いている 3〜4 秒の間に再生や設定へ移ることがある。そこから引きずり出さない。
        for mode in [Mode::Playing, Mode::Settings, Mode::Input] {
            let (tx, _rx) = mpsc::unbounded_channel();
            let mut session = looking_up();
            let mut app = lookup_app();
            app.mode = mode;

            lookup_done(
                &mut app,
                &mut session,
                &tx,
                1,
                "id1",
                found("UCfeed", Some("Feed Channel")),
            );

            assert!(app.channel.is_none(), "{mode:?}");
            assert_eq!(app.mode, mode, "{mode:?}");
            assert!(session.search_task.is_none(), "{mode:?}");
            assert!(app.notice.is_none(), "{mode:?}");
        }
    }

    #[tokio::test]
    async fn a_superseded_channel_lookup_is_discarded() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = looking_up();
        let mut app = lookup_app();

        lookup_done(
            &mut app,
            &mut session,
            &tx,
            0,
            "id1",
            found("UCold", Some("Old Channel")),
        );

        assert!(app.channel.is_none());
        // 打ち切った側が知らせを入れ替えるので、ここでは触らない。
        assert_eq!(app.notice.as_deref(), Some(actions::CHANNEL_LOOKUP_NOTICE));
    }

    #[tokio::test]
    async fn a_video_without_a_channel_only_says_so() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = looking_up();
        let mut app = lookup_app();

        lookup_done(&mut app, &mut session, &tx, 1, "id1", Ok(None));

        assert!(app.channel.is_none());
        assert_eq!(app.notice.as_deref(), Some(actions::NO_CHANNEL_NOTICE));
        assert!(app.error.is_none());
    }

    #[tokio::test]
    async fn a_failed_channel_lookup_is_shown_as_an_error() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = looking_up();
        let mut app = lookup_app();

        lookup_done(
            &mut app,
            &mut session,
            &tx,
            1,
            "id1",
            Err("boom".to_string()),
        );

        assert!(app.channel.is_none());
        assert_eq!(app.error.as_deref(), Some("boom"));
        assert!(app.notice.is_none(), "取得中の知らせは残さない");
    }

    #[test]
    fn present_stops_the_video_while_the_comments_are_open() {
        let video = sink();
        assert!(video.feed(KITTY_RECONFIG));
        assert!(video.feed(&frame(320, 176, b"DATA")));
        let mut app = App {
            video: Some(video),
            ..App::default()
        };
        app.comments.toggle();
        // コメントを出した側が予約した削除は書く。貼ってある映像を残さない。
        let mut session = Session {
            owe_clear: true,
            ..Session::default()
        };

        let out = present(&mut session, &app);
        assert_eq!(out, clear_bytes(), "削除だけで、フレームは書かない");
    }

    #[test]
    fn present_resumes_the_video_once_the_comments_are_closed() {
        let video = sink();
        assert!(video.feed(KITTY_RECONFIG));
        assert!(video.feed(&frame(320, 176, b"DATA")));
        let mut app = App {
            video: Some(video),
            ..App::default()
        };
        app.comments.toggle();
        app.comments.toggle();
        let mut session = Session::default();

        assert!(!present(&mut session, &app).is_empty());
    }

    #[test]
    fn a_frame_that_arrives_while_the_comments_are_open_is_kept_for_when_they_close() {
        let video = sink();
        let mut app = App {
            video: Some(video.clone()),
            ..App::default()
        };
        app.comments.toggle();
        let mut session = Session {
            owe_clear: true,
            ..Session::default()
        };
        assert!(video.feed(KITTY_RECONFIG));
        assert!(video.feed(&frame(320, 176, b"DATA")));

        assert_eq!(
            present(&mut session, &app),
            clear_bytes(),
            "表示中は削除だけ書く"
        );

        app.comments.toggle();
        assert!(
            !present(&mut session, &app).is_empty(),
            "閉じたら保留していたフレームを貼り直す"
        );
    }

    #[test]
    fn the_configured_categories_reach_the_tab_row() {
        // App::default() の既定ではなく、設定ファイルの [[categories]] を出す。
        let categories = vec![crate::category::Category::new("将棋", "将棋 対局")];
        let app = App {
            tabs: Tabs::with_categories(categories),
            ..App::default()
        };
        assert_eq!(app.tabs.labels(), ["すべて", "将棋"]);
    }

    // ---- 再生中のイベント ----

    /// 送ったものを捨てる player。nonce の照合だけを見たいテストで使う。
    struct NullSink;

    impl actions::PlayerSink for NullSink {
        fn send<'a>(&'a mut self, _command: &'a mpv::MpvCommand) -> actions::Sending<'a> {
            Box::pin(async { Ok(()) })
        }
    }

    /// nonce 2 の player で再生中。一覧に 1 件あるので、終われば結果へ戻る。
    fn playing(session: &mut Session) -> App {
        session.player = Some(actions::Player {
            sink: Box::new(NullSink),
            nonce: 2,
        });
        App {
            mode: Mode::Playing,
            results: vec![result("a")],
            video: Some(sink()),
            playback: Playback {
                title: "song".to_string(),
                time_pos: Some(5.0),
                ..Playback::default()
            },
            ..App::default()
        }
    }

    #[tokio::test]
    async fn events_from_a_replaced_player_leave_the_playback_alone() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = playing(&mut session);
        app.video.as_ref().expect("映像").request_redraw();

        for event in [
            AppEvent::MpvProperty {
                nonce: 1,
                id: mpv::REQ_TIME_POS,
                data: Some(serde_json::json!(40.0)),
            },
            AppEvent::VideoFrame { nonce: 1 },
            AppEvent::VideoError {
                nonce: 1,
                error: "前の映像".to_string(),
            },
            AppEvent::MpvExited {
                nonce: 1,
                error: Some("前の mpv".to_string()),
            },
        ] {
            handle_event(&mut app, event, &tx, &mut session).await;
        }

        assert_eq!(app.mode, Mode::Playing);
        assert_eq!(
            app.playback.time_pos,
            Some(5.0),
            "前の再生位置で上書きしない"
        );
        assert!(
            !app.video.as_ref().expect("映像").request_redraw(),
            "前の再生の描き直し要求は受け取らない"
        );
        assert_eq!(app.error, None);
        assert!(session.player.is_some(), "今の再生は続ける");
    }

    #[tokio::test]
    async fn a_property_from_the_current_player_updates_the_playback() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = playing(&mut session);

        let event = AppEvent::MpvProperty {
            nonce: 2,
            id: mpv::REQ_TIME_POS,
            data: Some(serde_json::json!(40.0)),
        };
        handle_event(&mut app, event, &tx, &mut session).await;

        assert_eq!(app.playback.time_pos, Some(40.0));
    }

    #[tokio::test]
    async fn a_frame_from_the_current_player_takes_the_redraw_request() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = playing(&mut session);
        app.video.as_ref().expect("映像").request_redraw();

        handle_event(
            &mut app,
            AppEvent::VideoFrame { nonce: 2 },
            &tx,
            &mut session,
        )
        .await;

        assert!(
            app.video.as_ref().expect("映像").request_redraw(),
            "受け取った後は次の要求を通す"
        );
    }

    #[tokio::test]
    async fn a_video_error_from_the_current_player_ends_the_playback() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = playing(&mut session);

        let event = AppEvent::VideoError {
            nonce: 2,
            error: "映像が読めません".to_string(),
        };
        handle_event(&mut app, event, &tx, &mut session).await;

        assert_eq!(app.mode, Mode::Results);
        assert_eq!(app.error.as_deref(), Some("映像が読めません"));
        assert!(session.player.is_none());
        assert!(app.video.is_none());
    }

    #[tokio::test]
    async fn mpv_exiting_ends_the_current_playback() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = playing(&mut session);

        let event = AppEvent::MpvExited {
            nonce: 2,
            error: None,
        };
        handle_event(&mut app, event, &tx, &mut session).await;

        assert_eq!(app.mode, Mode::Results);
        assert_eq!(app.error, None, "普通に終わったらエラーは出さない");
        assert!(session.player.is_none());
    }

    // ---- 振り分け ----

    #[tokio::test]
    async fn a_playlists_list_reaches_the_open_list() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = App {
            mode: Mode::Playlists,
            playlists: Some(screen::playlists::PlaylistsView::default()),
            ..App::default()
        };

        let entries = vec![search::PlaylistEntry {
            id: "PL1".to_string(),
            title: "作業用BGM".to_string(),
        }];
        let event = AppEvent::PlaylistsReady {
            nonce: session.search_nonce,
            entries: Ok(entries.clone()),
        };
        handle_event(&mut app, event, &tx, &mut session).await;

        let playlists = app.playlists.as_ref().expect("一覧");
        assert_eq!(playlists.entries, entries);
        assert!(playlists.loaded);
    }

    #[tokio::test]
    async fn a_finished_download_reaches_the_status_line() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = App::default();

        let event = AppEvent::DownloadDone {
            nonce: session.download_nonce,
            notice: Ok("保存しました: /tmp/a.mp4".to_string()),
        };
        handle_event(&mut app, event, &tx, &mut session).await;

        assert_eq!(app.notice.as_deref(), Some("保存しました: /tmp/a.mp4"));
    }

    #[tokio::test]
    async fn a_failed_like_reaches_the_status_line() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = App::default();

        let event = AppEvent::OauthDone {
            nonce: session.oauth_nonce,
            result: Err("いいねできませんでした".to_string()),
        };
        handle_event(&mut app, event, &tx, &mut session).await;

        assert_eq!(app.error.as_deref(), Some("いいねできませんでした"));
    }

    #[tokio::test]
    async fn a_finished_save_marks_the_video_and_replaces_the_progress_notice() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session {
            oauth_action: Some(oauth::Action::Save("v1".to_string())),
            ..Session::default()
        };
        let mut app = App::default();
        app.set_notice(Some(oauth::SAVE_NOTICE.to_string()));

        let event = AppEvent::OauthDone {
            nonce: session.oauth_nonce,
            result: Ok(oauth::SAVED_NOTICE.to_string()),
        };
        handle_event(&mut app, event, &tx, &mut session).await;

        assert!(app.saved_videos.contains("v1"));
        assert_eq!(app.notice.as_deref(), Some(oauth::SAVED_NOTICE));
    }

    #[tokio::test]
    async fn a_failed_save_folds_the_progress_notice() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session {
            oauth_action: Some(oauth::Action::Save("v1".to_string())),
            ..Session::default()
        };
        let mut app = App::default();
        app.set_notice(Some(oauth::SAVE_NOTICE.to_string()));

        let event = AppEvent::OauthDone {
            nonce: session.oauth_nonce,
            result: Err("quota".to_string()),
        };
        handle_event(&mut app, event, &tx, &mut session).await;

        assert_eq!(app.notice, None, "「保存中」を残さない");
        assert_eq!(app.error.as_deref(), Some("quota"));
        assert!(app.saved_videos.is_empty());
    }

    #[tokio::test]
    async fn the_engagement_state_reaches_the_cache() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = Session::default();
        let mut app = App::default();

        let event = AppEvent::EngagementReady {
            nonce: session.search_nonce,
            liked_videos: Some(vec!["v1".to_string()]),
            asked_channels: vec!["UC1".to_string()],
            subscribed_channels: vec!["UC1".to_string()],
        };
        handle_event(&mut app, event, &tx, &mut session).await;

        assert!(app.engagement.is_liked("v1"));
        assert_eq!(app.engagement.is_subscribed("UC1"), Some(true));
    }
}
