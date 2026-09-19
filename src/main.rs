mod actions;
mod app;
mod category;
mod clipboard;
mod comments;
mod cookies;
mod display;
mod fetch;
mod geometry;
mod grid;
mod input;
mod jpeg;
mod kitty;
mod mpv;
mod query;
mod rgb;
mod search;
mod seekbar;
mod settings;
mod speed;
mod subtitles;
mod tct;
mod thumbs;
mod ui;
mod video;

use actions::{Session, apply_resize, end_playback, on_tick, schedule_resize, stop_playback};
use anyhow::Result;
use app::{App, AppEvent, Mode};
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
fn app_from(loaded: settings::Loaded) -> App {
    App {
        display: loaded.settings.display.mode,
        // 実際に効くかは最初の検索で分かる。ここでは指定の有無だけを持つ。
        cookies: CookieState::from_source(loaded.settings.cookies.clone()),
        tabs: Tabs::with_categories(loaded.settings.categories.clone()),
        subtitles: SubtitleState::from_settings(&loaded.settings.subtitles),
        settings_backup: loaded.settings.clone(),
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
    let mut app = app_from(settings::load());
    if let Some(dir) = app.settings.thumbnails.dir() {
        thumbs::prune_cache(&dir, app.settings.thumbnails.max_cached);
    }
    let mut session = Session::default();
    let mut ticker = tokio::time::interval(Duration::from_secs(1));

    loop {
        // draw は必ず MoveTo で始まり SGR を閉じて flush するので、その直後なら割り込まずに書ける。
        // マウスの当たり判定は「ユーザーが今見ている画面」で行うので、描いた寸法を控える。
        app.screen = terminal.draw(|frame| ui::draw(frame, &app))?.area;
        app.mark_drawn();
        let area = ui::video_area(app.screen);
        present_video(&mut session, &app, area, terminal.backend_mut())?;
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
    ]
    .into_iter()
    .flatten()
    {
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
    // 再生中と設定画面では他の描画と重なるので 1 枚も書かない。
    // 戻ったときに貼り直せるよう dirty は残す。
    if matches!(app.mode, Mode::Playing | Mode::Settings) {
        return Ok(());
    }
    if !app.thumbs.take_dirty() {
        return Ok(());
    }
    let mut bytes = Vec::new();
    video::encode_clear(&mut bytes);
    let mut incomplete = false;
    if let Some(layout) = ui::grid_layout(app, cell) {
        for (i, rect) in layout.cells.iter().enumerate() {
            let Some(result) = app.results.get(layout.offset + i) else {
                break;
            };
            let Some(image) = app.thumbs.get(&result.id) else {
                continue;
            };
            match video::placement(rect.image, cell, (image.width, image.height)) {
                Some(at) => rgb::encode_image(image, at, &mut bytes),
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
        let (x, y) = ui::input_cursor(app.screen, &app.query);
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
    let tab = app.tabs.selected();
    handle_key(app, key, tx, session).await;
    refetch_thumbnails_if_tab_moved(app, tx, session, fetcher, tab);
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
    let tab = app.tabs.selected();
    handle_mouse(app, mouse, tx, session).await;
    refetch_thumbnails_if_tab_moved(app, tx, session, fetcher, tab);
}

/// タブを移ると結果集合ごと入れ替わる。読み込み済みのタブでも
/// メモリ上の画像は捨ててあるので、キャッシュから読み直す。
fn refetch_thumbnails_if_tab_moved<F>(
    app: &mut App,
    tx: &UnboundedSender<AppEvent>,
    session: &mut Session,
    fetcher: F,
    before: usize,
) where
    F: Fetcher + Send + Sync + 'static,
{
    if app.tabs.selected() != before {
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
        } => {
            if nonce != session.search_nonce {
                return;
            }
            session.search_task = None;
            app.searching = false;
            apply_search_done(app, &target, report);
            actions::start_thumbnails(app, tx, session);
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
    }
}

/// cookie の状態を進めてから、結果かエラーを画面へ渡す。
fn apply_search_done(app: &mut App, target: &Target, report: search::SearchReport) {
    let armed = matches!(app.cookies, CookieState::Armed(_));
    let source = app.cookies.for_search().cloned();
    app.cookies.observe(&report.outcome);

    if let Some(source) = &source {
        app.set_notice(match &report.outcome {
            CookieOutcome::Degraded(_) => Some(cookies::describe(&report.outcome, source)),
            // 「cookie 無しで検索しました」は、実際に出し直せたときだけ言う。
            CookieOutcome::Unreadable(_) if report.fell_back => {
                Some(cookies::describe(&report.outcome, source))
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
        Ok(results) => app.set_results(results, target),
        Err(e) => {
            // 初回のタイムアウトはキーチェーンのダイアログ待ちの可能性があるので、そちらを案内する。
            app.set_error(match (&report.outcome, &source) {
                (CookieOutcome::TimedOut, Some(source)) if armed => {
                    Some(cookies::describe(&report.outcome, source))
                }
                _ => Some(e),
            });
            app.enter_search_mode(Mode::Input);
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
    use crate::app::Playback;
    use crate::cookies::CookieSource;
    use crate::fetch::fixtures::{CurlResult, FakeCurl};
    use crate::kitty::fixtures::{KITTY_RECONFIG, frame};
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

    fn search_done(
        nonce: u64,
        outcome: CookieOutcome,
        results: Result<Vec<SearchResult>, String>,
    ) -> AppEvent {
        AppEvent::SearchDone {
            nonce,
            target: Target::Search("q".to_string()),
            report: SearchReport {
                results,
                outcome,
                fell_back: false,
            },
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
            search_done(1, CookieOutcome::Ok, Ok(vec![result("a")])),
            &tx,
            &mut session,
        )
        .await;

        assert_eq!(app.cookies, CookieState::Active(source()));
        assert_eq!(app.mode, Mode::Results);
        assert!(!app.searching);
        assert!(app.notice.is_none());
    }

    #[test]
    fn saving_from_the_settings_screen_does_not_bake_in_the_environment() {
        // 起動 (設定の読み込み) から保存までの通し。利用者が書いた行を s で消さない。
        let dir = std::env::temp_dir().join(format!("tuitube-main-{}", std::process::id()));
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
        let mut app = app_from(loaded);
        assert_eq!(app.settings.cookies, None, "実行中は連携を切る");

        actions::save_settings_to(&mut app, Some(&path), std::time::Instant::now());
        // 書いた中身を読み直して見る。指定なしだと同じ行がコメントで出るため。
        let written = std::fs::read_to_string(&path).expect("読める");
        let reread = settings::load_from(Some(&path), settings::EnvOverrides::default());
        assert_eq!(
            reread.settings.cookies,
            CookieSource::from_spec(Some("chrome")),
            "{written}"
        );
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
            settings_return: Mode::Input,
            searching: true,
            ..App::default()
        };
        handle_event(
            &mut app,
            search_done(1, CookieOutcome::Ok, results),
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
        assert_eq!(app.settings_return, Mode::Results, "閉じたら結果へ戻す");
        assert_eq!(app.results.len(), 1, "結果は受け取っておく");
        assert!(!app.searching);
    }

    #[tokio::test]
    async fn a_failed_search_behind_the_settings_screen_does_not_close_it_either() {
        let app = settings_open_when_the_search_lands(Err("yt-dlp が落ちた".to_string())).await;
        assert_eq!(app.mode, Mode::Settings);
        assert_eq!(app.settings_return, Mode::Input);
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
            },
        };
        handle_event(&mut app, event, &tx, &mut session).await;

        // cookie 無しの結果は出しつつ、効いていない旨を伝える。
        assert_eq!(app.mode, Mode::Results);
        assert!(app.error.is_none());
        let notice = app.notice.expect("説明を出す");
        assert!(notice.contains("cookie 無しで検索しました"), "{notice}");
        assert!(matches!(app.cookies, CookieState::Suspended { .. }));
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

        let (x, y) = ui::input_cursor(app.screen, &app.query);
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
        let dir = std::env::temp_dir().join(format!("tuitube-main-{}", std::process::id()));
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
}
