//! プレイリスト一覧 (Mode::Playlists)。一覧の状態、キー操作、描画、一覧の取得と、
//! 1 つのプレイリストを開く・戻る・取り直す操作を持つ。
//! 開いたプレイリストの中身 (Mode::Playlist) の描画とキー処理は結果一覧と同じ側にある。

use crate::actions::{
    Session, cancel_search, leave_background, request_reload, search_return_mode, spawn_search,
};
use crate::app::{App, AppEvent, Mode, PlaylistView};
use crate::screen::browse;
use crate::screen::settings::{is_settings_key, open_settings};
use crate::search::{self, PlaylistEntry, RealYtDlp, YtDlp};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState};
use tokio::sync::mpsc::UnboundedSender;

/// プレイリスト一覧が 0 件だったときの文言。cookie 無しでも失敗せず空で返るので理由を添える。
pub const NO_PLAYLISTS_NOTICE: &str =
    "プレイリストがありません (cookie が YouTube にログイン済みか確認してください)";

/// プレイリストの一覧を開いている間の状態。行はタイトルだけでサムネイルを持たないので、
/// 動画一覧の TabState とは別の入れ物にする。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PlaylistsView {
    pub entries: Vec<PlaylistEntry>,
    pub selected: usize,
    pub loaded: bool,
}

impl PlaylistsView {
    pub fn select_next(&mut self) {
        if self.entries.is_empty() {
            return;
        }
        self.selected = (self.selected + 1) % self.entries.len();
    }

    pub fn select_prev(&mut self) {
        if self.entries.is_empty() {
            return;
        }
        let len = self.entries.len();
        self.selected = (self.selected + len - 1) % len;
    }
}

/// プレイリストの一覧は動画一覧と別の入れ物なので、件数も選択も別に数える。
pub fn playlists_status(app: &App) -> String {
    let Some(playlists) = &app.playlists else {
        return browse::results_status(app);
    };
    let count = format!("{} 件", playlists.entries.len());
    let body = match playlists.entries.get(playlists.selected) {
        Some(entry) => format!("{count}  |  {}", entry.title),
        None => count,
    };
    format!("{}{body}", app.background_marker())
}

// ---- キー ----

/// プレイリスト一覧。サムネイルを持たない 1 列のリストなので、上下と開く・戻るだけ。
pub async fn handle_key_playlists(
    app: &mut App,
    key: KeyEvent,
    tx: &UnboundedSender<AppEvent>,
    session: &mut Session,
) {
    match key.code {
        KeyCode::Down => {
            if let Some(view) = app.playlists.as_mut() {
                view.select_next();
            }
        }
        KeyCode::Up => {
            if let Some(view) = app.playlists.as_mut() {
                view.select_prev();
            }
        }
        KeyCode::Enter => open_playlist(app, tx, session),
        KeyCode::Char('S') => open_settings(app, session),
        KeyCode::Char(c) if is_settings_key(c, key.modifiers) => open_settings(app, session),
        KeyCode::Char(_) if key.modifiers.contains(KeyModifiers::CONTROL) => {}
        // バックグラウンド中でなければ何もしない (leave_background が判定する)。
        KeyCode::Char('b') => leave_background(app, session).await,
        KeyCode::Char('/') | KeyCode::Esc => leave_playlists(app, session),
        KeyCode::Char('q') => app.confirm_quit = true,
        _ => {}
    }
}

/// プレイリスト一覧を開くキー。入力欄では p も検索語なので Ctrl 付きだけを見る。
pub fn is_playlists_key(c: char, modifiers: KeyModifiers) -> bool {
    modifiers.contains(KeyModifiers::CONTROL) && c.eq_ignore_ascii_case(&'p')
}

// ---- 描画 ----

/// プレイリスト一覧。持っているのはタイトルだけなので 1 列のリストで出す。
pub fn draw_playlists(frame: &mut Frame, app: &App, area: Rect) {
    let entries = app.playlists.as_ref().map(|v| v.entries.as_slice());
    let items: Vec<ListItem> = entries
        .unwrap_or(&[])
        .iter()
        .map(|e| ListItem::new(e.title.clone()))
        .collect();
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" プレイリスト "),
        )
        .highlight_symbol("> ")
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    let mut state = ListState::default();
    if let Some(view) = &app.playlists
        && !view.entries.is_empty()
    {
        state.select(Some(view.selected));
    }
    frame.render_stateful_widget(list, area, &mut state);
}

/// プレイリスト一覧の案内。行を選んで開くだけなので短い。
pub fn playlists_hints(background: bool) -> Vec<String> {
    let mut hints = vec![
        "↑↓:選択".to_string(),
        "Enter:開く".to_string(),
        "Esc:戻る".to_string(),
        "q:終了".to_string(),
        "S:設定".to_string(),
    ];
    if background {
        hints.push("b:全画面へ".to_string());
    }
    hints
}

// ---- 開閉と取得 ----

/// プレイリスト一覧を取りに行って一覧画面へ移る。
pub fn open_playlists(app: &mut App, tx: &UnboundedSender<AppEvent>, session: &mut Session) {
    open_playlists_with(app, tx, session, RealYtDlp);
}

pub fn open_playlists_with<R>(
    app: &mut App,
    tx: &UnboundedSender<AppEvent>,
    session: &mut Session,
    runner: R,
) where
    R: YtDlp + Send + Sync + 'static,
{
    // 走っている検索の結果は、戻ってくるまでに行き先が変わっている。
    cancel_search(app, session);
    // 戻ったときに同じ位置から続けられるよう、検索側の選択を控える。
    app.store_to_tab();
    app.playlists = Some(PlaylistsView::default());
    app.mode = Mode::Playlists;
    app.set_error(None);
    // 一覧は結果集合の外にある入れ物なので sync_from_view は通さない。通すと
    // 貼ってある検索結果のサムネイルを捨て、戻ったときに貼り直す合図も出ない。
    // 画像はそのままだと一覧の行に重なるので、剥がすだけ頼む。
    session.owe_clear = true;
    app.searching = true;
    let nonce = session.search_nonce;
    let timeout = app.settings.search.timeout;
    let cookies = app.cookies.for_search().cloned();
    let tx = tx.clone();
    session.search_task = Some(tokio::spawn(async move {
        let entries = search::fetch_playlists(&runner, cookies.as_ref(), timeout).await;
        let _ = tx.send(AppEvent::PlaylistsReady { nonce, entries });
    }));
}

/// 届いた一覧を取り込む。一覧を閉じた後に届いた分は捨てる。
pub fn apply_playlists_ready(
    app: &mut App,
    session: &mut Session,
    nonce: u64,
    entries: Result<Vec<PlaylistEntry>, String>,
) {
    if nonce != session.search_nonce {
        return;
    }
    session.search_task = None;
    app.searching = false;
    let Some(playlists) = app.playlists.as_mut() else {
        return;
    };
    match entries {
        Ok(entries) => {
            playlists.entries = entries;
            playlists.selected = 0;
            playlists.loaded = true;
            // cookie が無くても yt-dlp は失敗せず空で返るので、0 件はここで理由を出す。
            if playlists.entries.is_empty() {
                app.set_notice(Some(NO_PLAYLISTS_NOTICE.to_string()));
            }
        }
        Err(reason) => app.set_error(Some(reason)),
    }
}

/// 選択中のプレイリストの中身を取りに行って動画一覧へ移る。
pub fn open_playlist(app: &mut App, tx: &UnboundedSender<AppEvent>, session: &mut Session) {
    open_playlist_with(app, tx, session, RealYtDlp);
}

pub fn open_playlist_with<R>(
    app: &mut App,
    tx: &UnboundedSender<AppEvent>,
    session: &mut Session,
    runner: R,
) where
    R: YtDlp + Send + Sync + 'static,
{
    let Some(entry) = app
        .playlists
        .as_ref()
        .and_then(|playlists| playlists.entries.get(playlists.selected))
    else {
        return;
    };
    // 取り込み先が要るので、検索を積む前に開いておく。
    app.playlist = Some(PlaylistView::new(entry.id.clone(), entry.title.clone()));
    app.mode = Mode::Playlist;
    app.set_error(None);
    app.sync_from_view();
    start_playlist_search_with(app, tx, session, runner);
}

/// 開いているプレイリストの中身を取りに行く。
fn start_playlist_search_with<R>(
    app: &mut App,
    tx: &UnboundedSender<AppEvent>,
    session: &mut Session,
    runner: R,
) where
    R: YtDlp + Send + Sync + 'static,
{
    let Some(target) = app.playlist.as_ref().map(PlaylistView::target) else {
        return;
    };
    // 件数は Target::Playlist 側が PLAYLIST_LIMIT を渡す。ここの値は控えの鍵にしか効かない。
    spawn_search(app, tx, session, target, app.settings.search.limit, runner);
}

/// プレイリストの中身から一覧へ戻る。一覧は持っているので取り直さない。
pub fn leave_playlist(app: &mut App, session: &mut Session) {
    if app.playlist.is_none() {
        return;
    }
    // 結果が検索側のタブへ流れ込まないよう、先に打ち切る。
    cancel_search(app, session);
    app.playlist = None;
    app.set_error(None);
    app.sync_from_view();
    app.mode = Mode::Playlists;
}

/// プレイリスト一覧を畳んで元の検索画面へ戻る。
pub fn leave_playlists(app: &mut App, session: &mut Session) {
    if app.playlists.is_none() {
        return;
    }
    // 一覧だけ畳むと、戻る先を失ったプレイリストが開いたまま残る。
    leave_playlist(app, session);
    cancel_search(app, session);
    app.playlists = None;
    app.set_error(None);
    // 一覧を出すときに剥がしただけで、控えは残っている。貼り直しを頼む。
    app.thumbs.mark_dirty();
    app.mode = search_return_mode(app);
}

pub fn reload_playlist(app: &mut App, tx: &UnboundedSender<AppEvent>, session: &mut Session) {
    reload_playlist_with(app, tx, session, RealYtDlp);
}

/// 開いているプレイリストを取り直す。骨格は reload_channel_tab_with と同じ。
pub fn reload_playlist_with<R>(
    app: &mut App,
    tx: &UnboundedSender<AppEvent>,
    session: &mut Session,
    runner: R,
) where
    R: YtDlp + Send + Sync + 'static,
{
    if app.playlist.is_none() {
        return;
    }
    request_reload(app);
    start_playlist_search_with(app, tx, session, runner);
}

#[cfg(test)]
mod tests {
    // 観点ごとに画面の組み立て方が違うので、小分けにしてそれぞれにヘルパーを持たせる。
    /// 一覧の選択と状態行。
    mod state {
        use super::super::*;
        use crate::app::{App, Mode, PlaylistView};
        use crate::cookies::Target;
        use crate::search::{PlaylistEntry, SearchResult};

        fn search_target() -> Target {
            Target::Search("q".to_string())
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

        fn entry(id: &str, title: &str) -> PlaylistEntry {
            PlaylistEntry {
                id: id.to_string(),
                title: title.to_string(),
            }
        }

        /// 検索結果を 2 件持ち、そこからプレイリストの動画一覧まで開いた App。
        /// 選択位置はまだタブへ書き戻していない。取り込みで巻き戻さないことも見たいため。
        fn playlist_app() -> App {
            let mut app = App::default();
            app.set_results(vec![result("a"), result("b")], &search_target());
            app.selected = 1;
            app.scroll = 4;
            app.playlists = Some(PlaylistsView {
                entries: vec![entry("PL1", "作業用BGM"), entry("PL2", "あとで見る")],
                selected: 0,
                loaded: true,
            });
            app.playlist = Some(PlaylistView::new(
                "PL1".to_string(),
                "作業用BGM".to_string(),
            ));
            app.mode = Mode::Playlist;
            app.sync_from_view();
            app
        }

        #[test]
        fn the_playlists_selection_wraps_around_like_the_results() {
            let mut list = PlaylistsView {
                entries: vec![entry("PL1", "作業用BGM"), entry("PL2", "あとで見る")],
                selected: 0,
                loaded: true,
            };

            list.select_next();
            assert_eq!(list.selected, 1);
            list.select_next();
            assert_eq!(list.selected, 0, "末尾から下は先頭へ");
            list.select_prev();
            assert_eq!(list.selected, 1, "先頭から上は末尾へ");
        }

        #[test]
        fn an_empty_playlists_list_has_nothing_to_select() {
            let mut list = PlaylistsView::default();
            list.select_next();
            list.select_prev();
            assert_eq!(list.selected, 0);
        }

        #[test]
        fn the_status_line_counts_the_playlists_on_the_list_screen() {
            // 一覧画面が数えるのは動画ではなくプレイリストの件数。
            let mut app = playlist_app();
            app.playlist = None;
            app.mode = Mode::Playlists;
            app.playlists.as_mut().expect("playlists").selected = 1;

            let line = app.status_line();
            assert!(line.contains("2 件"), "{line}");
            assert!(line.contains("あとで見る"), "{line}");
        }
    }

    /// 一覧画面でのキー。
    mod keys {
        use super::super::*;
        use crate::app::{App, AppEvent, Mode};
        use crate::grid::LayoutMode;
        use crate::input::handle_key;
        use crate::search::{PlaylistEntry, SearchResult};
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use ratatui::layout::Rect;
        use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

        fn key(code: KeyCode) -> KeyEvent {
            KeyEvent::new(code, KeyModifiers::NONE)
        }

        fn channel() -> (UnboundedSender<AppEvent>, UnboundedReceiver<AppEvent>) {
            unbounded_channel()
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

        /// 積まれた検索タスクを、外部プロセスへ届く前に捨てる。
        /// キーの振り分けだけを見るので、実行者の差し替えはアクション側のテストで確かめる。
        fn take_search(session: &mut Session) -> bool {
            match session.search_task.take() {
                Some(task) => {
                    task.abort();
                    true
                }
                None => false,
            }
        }

        /// 格子の `index` 番目のセル (画像の左上) の押し込み。
        /// チャンネルへ移れる結果を 4 件持つ 80x24 の検索結果画面。
        fn channel_source_app() -> App {
            let mut app = App {
                mode: Mode::Results,
                screen: Rect::new(0, 0, 80, 24),
                ..App::default()
            };
            let results: Vec<SearchResult> = (0..4)
                .map(|i| SearchResult {
                    uploader: Some(format!("Channel {i}")),
                    channel_id: Some(format!("UC{i}")),
                    ..result(&format!("id{i}"))
                })
                .collect();
            app.set_results(results, &crate::cookies::Target::Search("q".to_string()));
            app.mark_drawn();
            app
        }

        /// プレイリスト一覧を開いた画面。検索結果は控えたまま `count` 件の一覧を出す。
        fn playlists_app(count: usize) -> App {
            let mut app = channel_source_app();
            open_playlists_on(&mut app, count);
            app
        }

        fn open_playlists_on(app: &mut App, count: usize) {
            app.store_to_tab();
            app.playlists = Some(PlaylistsView {
                entries: (0..count)
                    .map(|i| PlaylistEntry {
                        id: format!("PL{i}"),
                        title: format!("list {i}"),
                    })
                    .collect(),
                selected: 0,
                loaded: true,
            });
            app.mode = Mode::Playlists;
        }

        #[tokio::test]
        async fn v_does_nothing_in_the_playlists_list() {
            // 対象外 (v1): サムネイルを持たないタイトルのみの一覧なので、grid/list 切替の対象にしない。
            let (tx, _rx) = channel();
            let mut session = Session::default();
            let mut app = playlists_app(2);

            handle_key(&mut app, key(KeyCode::Char('v')), &tx, &mut session).await;

            assert_eq!(app.settings.search.layout, LayoutMode::Grid);
        }

        #[tokio::test]
        async fn the_arrow_keys_move_the_selection_in_the_playlists_list() {
            let (tx, _rx) = channel();
            let mut session = Session::default();
            let mut app = playlists_app(3);

            handle_key(&mut app, key(KeyCode::Down), &tx, &mut session).await;
            assert_eq!(app.playlists.as_ref().expect("playlists").selected, 1);
            handle_key(&mut app, key(KeyCode::Up), &tx, &mut session).await;
            assert_eq!(app.playlists.as_ref().expect("playlists").selected, 0);
            handle_key(&mut app, key(KeyCode::Up), &tx, &mut session).await;
            assert_eq!(
                app.playlists.as_ref().expect("playlists").selected,
                2,
                "先頭から上は末尾へ"
            );
            assert_eq!(app.selected, 0, "検索結果側の選択は動かさない");
            assert!(
                !take_search(&mut session),
                "行を動かすだけでは取りに行かない"
            );
        }

        #[tokio::test]
        async fn enter_opens_the_selected_playlist() {
            let (tx, _rx) = channel();
            let mut session = Session::default();
            let mut app = playlists_app(3);
            app.playlists.as_mut().expect("playlists").selected = 1;

            handle_key(&mut app, key(KeyCode::Enter), &tx, &mut session).await;

            assert_eq!(app.mode, Mode::Playlist);
            let view = app.playlist.as_ref().expect("中身へ移る");
            assert_eq!(view.playlist_id, "PL1");
            assert_eq!(view.playlist_title, "list 1");
            assert!(take_search(&mut session), "中身を取りに行く");
        }

        #[tokio::test]
        async fn esc_and_slash_fold_the_playlists_list() {
            for code in [KeyCode::Esc, KeyCode::Char('/')] {
                let (tx, _rx) = channel();
                let mut session = Session::default();
                let mut app = playlists_app(3);

                handle_key(&mut app, key(code), &tx, &mut session).await;

                assert!(app.playlists.is_none(), "{code:?}");
                assert_eq!(app.mode, Mode::Results, "{code:?}");
                assert_eq!(app.view_result_ids(), ["id0", "id1", "id2", "id3"]);
                assert!(!take_search(&mut session), "戻るだけでは取り直さない");
            }
        }
    }

    /// 一覧の描画と案内。
    mod draw {
        use super::super::*;
        use crate::app::{App, Mode};
        use crate::grid;
        use crate::search::{PlaylistEntry, SearchResult};
        use crate::ui::{self, grid_layout};
        use crate::video::CellSize;
        use ratatui::layout::Rect;

        fn result(index: usize) -> SearchResult {
            SearchResult {
                id: format!("id{index}"),
                title: format!("title {index}"),
                duration: None,
                uploader: None,
                channel_id: None,
                is_live: false,
            }
        }

        /// 割り付けを端末の申告で揺らさないための寸法。
        const CELL: CellSize = CellSize {
            width_px: 8,
            height_px: 16,
        };

        /// 80x24 の検索画面。CELL なら格子は 4 列 2 行になる。
        fn grid_app(count: usize) -> App {
            App {
                mode: Mode::Results,
                screen: Rect::new(0, 0, 80, 24),
                results: (0..count).map(result).collect(),
                ..App::default()
            }
        }

        /// TestBackend に 1 フレーム描いて、画面の文字だけを行ごとに取り出す。
        fn rendered(app: &App, width: u16, height: u16) -> String {
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height))
                    .expect("端末");
            terminal.draw(|frame| ui::draw(frame, app)).expect("描ける");
            let buffer = terminal.backend().buffer().clone();
            (0..buffer.area.height)
                .map(|y| {
                    let mut line = String::new();
                    // 全角文字は 2 セルを占め、後ろのセルは埋め草なので読み飛ばす。
                    let mut skip = 0;
                    for x in 0..buffer.area.width {
                        if skip > 0 {
                            skip -= 1;
                            continue;
                        }
                        let symbol = buffer[(x, y)].symbol();
                        skip = grid::display_width(symbol).saturating_sub(1);
                        line.push_str(symbol);
                    }
                    line
                })
                .collect::<Vec<_>>()
                .join("\n")
        }

        /// プレイリスト一覧を開いた 80x24 の画面。検索結果 4 件は控えたまま。
        fn playlists_app(count: usize) -> App {
            let mut app = grid_app(4);
            app.playlists = Some(PlaylistsView {
                entries: (0..count)
                    .map(|i| PlaylistEntry {
                        id: format!("PL{i}"),
                        title: format!("list {i}"),
                    })
                    .collect(),
                selected: 0,
                loaded: true,
            });
            app.mode = Mode::Playlists;
            app
        }

        #[test]
        fn the_playlists_help_names_the_open_and_back_keys() {
            // タイトルだけの一覧なので、案内に出ないと開き方も戻り方も分からない。
            let help = ui::fit_hints(&playlists_hints(false), 80);
            for key in ["↑↓:選択", "Enter:開く", "Esc:戻る", "q:終了", "S:設定"] {
                assert!(help.contains(key), "{key} が無い: {help}");
            }
            assert!(grid::display_width(&help) <= 80, "{help}");

            // 画面の最下行 (ui のフッタ) にも同じ文言が出る。
            let screen = rendered(&playlists_app(3), 80, 24);
            assert_eq!(screen.lines().last().unwrap_or_default().trim_end(), help);
        }

        #[test]
        fn the_playlists_list_shows_the_titles_and_marks_the_selection() {
            let mut app = playlists_app(3);
            app.playlists.as_mut().expect("playlists").selected = 1;

            let screen = rendered(&app, 80, 24);

            assert!(screen.contains("プレイリスト"), "枠の見出し: {screen}");
            for title in ["list 0", "list 1", "list 2"] {
                assert!(screen.contains(title), "{title} が無い: {screen}");
            }
            assert!(
                screen.lines().any(|line| line.contains("> list 1")),
                "選択行に印を付ける: {screen}"
            );
            assert!(!screen.contains("title 0"), "検索結果は出さない: {screen}");
        }

        #[test]
        fn the_playlists_list_never_lets_the_thumbnails_be_pasted_over_it() {
            // 割り付けを返すと、検索結果に貼ってあった画像が一覧の行の上に乗る。
            let app = playlists_app(3);
            assert!(grid_layout(&app, CELL).is_none());
            assert!(
                grid_layout(&grid_app(4), CELL).is_some(),
                "結果一覧では返す"
            );
        }
    }

    /// 一覧の取得、中身を開く・戻る・取り直す。
    mod ops {
        use super::super::*;
        use crate::app::{App, AppEvent, Mode};
        use crate::cookies::Target;
        use crate::rgb::RgbImage;
        use crate::search::fixtures::{FakeYtDlp, done};
        use crate::search::{self, PlaylistEntry, SearchResult, YtDlp};
        use ratatui::layout::Rect;
        use std::future::Future;
        use std::process::Output;
        use tokio::sync::mpsc::{self, UnboundedSender};

        /// 応答を返さない偽の yt-dlp。タスクは積まれるが外部プロセスは起動しない。
        struct StubYtDlp;

        impl YtDlp for StubYtDlp {
            fn run(
                &self,
                _args: Vec<String>,
            ) -> impl Future<Output = std::io::Result<Output>> + Send {
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
                is_live: false,
            }
        }

        /// 80x24 の検索画面。格子は 4 列 2 行になる。
        fn grid_app(count: usize) -> App {
            let mut app = App {
                mode: Mode::Results,
                screen: Rect::new(0, 0, 80, 24),
                ..App::default()
            };
            let results: Vec<SearchResult> =
                (0..count).map(|i| result(&format!("id{i}"))).collect();
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

        const PLAYLIST_LINES: &str = concat!(
            r#"{"id":"PL1","title":"作業用BGM","uploader":"View full playlist"}"#,
            "\n",
            r#"{"id":"PL2","title":"あとで見る","uploader":"View full playlist"}"#,
            "\n"
        );

        fn entry(id: &str, title: &str) -> PlaylistEntry {
            PlaylistEntry {
                id: id.to_string(),
                title: title.to_string(),
            }
        }

        /// 積まれた一覧取得を走らせて、届いた分を取り込む。
        async fn finish_playlists(
            app: &mut App,
            rx: &mut mpsc::UnboundedReceiver<AppEvent>,
            session: &mut Session,
        ) {
            finish_search(session).await;
            let Ok(AppEvent::PlaylistsReady { nonce, entries }) = rx.try_recv() else {
                panic!("PlaylistsReady のはず");
            };
            apply_playlists_ready(app, session, nonce, entries);
        }

        /// 検索結果 2 件の画面から、プレイリスト一覧まで開いた状態。
        async fn playlists_app(
            tx: &UnboundedSender<AppEvent>,
            rx: &mut mpsc::UnboundedReceiver<AppEvent>,
            session: &mut Session,
        ) -> App {
            let mut app = grid_app(2);
            app.selected = 1;
            let runner = FakeYtDlp::new([done(0, PLAYLIST_LINES, "")]);
            open_playlists_with(&mut app, tx, session, runner);
            finish_playlists(&mut app, rx, session).await;
            app
        }

        /// 開いているプレイリストの target。set_results へ渡す。
        fn playlist_target(app: &App) -> Target {
            app.playlist.as_ref().expect("playlist").target()
        }

        #[tokio::test]
        async fn open_playlists_reads_the_playlists_feed_and_shows_the_list() {
            let (tx, mut rx) = mpsc::unbounded_channel();
            let mut session = Session::default();
            let mut app = grid_app(2);
            app.selected = 1;
            let runner = FakeYtDlp::new([done(0, PLAYLIST_LINES, "")]);

            open_playlists_with(&mut app, &tx, &mut session, runner.clone());

            assert_eq!(app.mode, Mode::Playlists);
            assert!(app.searching, "取りに行っている間はそう見せる");
            assert!(!app.playlists.as_ref().expect("一覧へ移る").loaded);
            assert_eq!(app.result_ids(), ["id0", "id1"], "検索結果は残す");
            assert_eq!(app.tabs.state().selected, 1, "戻る位置を控える");

            finish_playlists(&mut app, &mut rx, &mut session).await;

            assert_eq!(runner.calls(), [search::playlists_args(None)]);
            let playlists = app.playlists.as_ref().expect("一覧が入る");
            assert_eq!(
                playlists.entries,
                [entry("PL1", "作業用BGM"), entry("PL2", "あとで見る")]
            );
            assert_eq!(playlists.selected, 0);
            assert!(playlists.loaded);
            assert!(!app.searching);
        }

        #[tokio::test]
        async fn an_empty_playlists_list_is_told_as_a_notice_not_an_error() {
            // cookie が無くても yt-dlp は失敗せず空で返るので、件数でなく理由を出す。
            let (tx, mut rx) = mpsc::unbounded_channel();
            let mut session = Session::default();
            let mut app = grid_app(2);

            open_playlists_with(
                &mut app,
                &tx,
                &mut session,
                FakeYtDlp::new([done(0, "", "")]),
            );
            finish_playlists(&mut app, &mut rx, &mut session).await;

            assert_eq!(app.notice.as_deref(), Some(NO_PLAYLISTS_NOTICE));
            assert_eq!(app.error, None);
            assert_eq!(app.mode, Mode::Playlists);
            assert!(app.playlists.as_ref().expect("一覧は開いたまま").loaded);
        }

        #[tokio::test]
        async fn a_failed_playlists_list_shows_the_reason() {
            let (tx, mut rx) = mpsc::unbounded_channel();
            let mut session = Session::default();
            let mut app = grid_app(2);

            open_playlists_with(
                &mut app,
                &tx,
                &mut session,
                FakeYtDlp::new([done(1, "", "ERROR: Sign in to confirm\n")]),
            );
            finish_playlists(&mut app, &mut rx, &mut session).await;

            let error = app.error.as_deref().expect("理由を出す");
            assert!(error.contains("Sign in to confirm"), "{error}");
            assert!(!app.playlists.as_ref().expect("一覧は開いたまま").loaded);
            assert!(!app.searching);
        }

        #[tokio::test]
        async fn a_playlists_list_that_arrives_after_leaving_is_dropped() {
            let (tx, mut rx) = mpsc::unbounded_channel();
            let mut session = Session::default();
            let mut app = grid_app(2);
            open_playlists_with(
                &mut app,
                &tx,
                &mut session,
                FakeYtDlp::new([done(0, PLAYLIST_LINES, "")]),
            );
            finish_search(&mut session).await;
            let Ok(AppEvent::PlaylistsReady { nonce, entries }) = rx.try_recv() else {
                panic!("PlaylistsReady のはず");
            };
            leave_playlists(&mut app, &mut session);

            apply_playlists_ready(&mut app, &mut session, nonce, entries);

            assert!(app.playlists.is_none(), "閉じた一覧へは書き戻さない");
            assert_eq!(app.mode, Mode::Results);
        }

        #[tokio::test]
        async fn open_playlist_searches_the_videos_of_the_selected_playlist() {
            let (tx, mut rx) = mpsc::unbounded_channel();
            let mut session = Session::default();
            let mut app = playlists_app(&tx, &mut rx, &mut session).await;
            app.playlists.as_mut().expect("一覧").selected = 1;
            let runner = FakeYtDlp::new([done(0, "", "")]);

            open_playlist_with(&mut app, &tx, &mut session, runner.clone());

            assert_eq!(app.mode, Mode::Playlist);
            let playlist = app.playlist.as_ref().expect("中身へ移る");
            assert_eq!(playlist.playlist_id, "PL2");
            assert_eq!(playlist.playlist_title, "あとで見る");
            assert!(app.playlists.is_some(), "一覧は残す");
            assert!(app.searching);

            finish_search(&mut session).await;
            let args = runner.calls();
            assert_eq!(args[0][0], "https://www.youtube.com/playlist?list=PL2");
            assert!(args[0].iter().any(|a| a == "--playlist-end"), "{args:?}");
        }

        #[tokio::test]
        async fn open_playlist_does_nothing_without_a_row_to_open() {
            let (tx, _rx) = mpsc::unbounded_channel();
            let mut session = Session::default();
            let mut app = grid_app(2);
            open_playlists_with(&mut app, &tx, &mut session, StubYtDlp);

            // まだ 1 件も届いていない一覧で Enter を押しても、中身は開かない。
            open_playlist_with(&mut app, &tx, &mut session, StubYtDlp);

            assert!(app.playlist.is_none());
            assert_eq!(app.mode, Mode::Playlists);
        }

        #[tokio::test]
        async fn leaving_a_playlist_returns_to_the_list_without_reading_it_again() {
            let (tx, mut rx) = mpsc::unbounded_channel();
            let mut session = Session::default();
            let mut app = playlists_app(&tx, &mut rx, &mut session).await;
            open_playlist_with(&mut app, &tx, &mut session, StubYtDlp);
            let running = session.search_nonce;
            app.set_results(vec![result("v0")], &playlist_target(&app));

            leave_playlist(&mut app, &mut session);

            assert!(app.playlist.is_none());
            assert_eq!(app.mode, Mode::Playlists);
            assert_eq!(
                app.playlists.as_ref().expect("一覧は残す").entries,
                [entry("PL1", "作業用BGM"), entry("PL2", "あとで見る")]
            );
            assert!(session.search_task.is_none(), "一覧を取り直さない");
            assert_ne!(session.search_nonce, running, "走らせたままにしない");
            assert!(!app.searching);
        }

        #[tokio::test]
        async fn leaving_the_playlists_list_returns_to_the_search_results() {
            let (tx, mut rx) = mpsc::unbounded_channel();
            let mut session = Session::default();
            let mut app = playlists_app(&tx, &mut rx, &mut session).await;

            leave_playlists(&mut app, &mut session);

            assert!(app.playlists.is_none());
            assert_eq!(app.mode, Mode::Results);
            assert_eq!(app.view_result_ids(), ["id0", "id1"]);
            assert_eq!(app.view_selected(), 1, "元の選択へ戻る");
            assert!(!app.searching);
        }

        #[tokio::test]
        async fn leaving_the_playlists_list_folds_an_open_playlist_too() {
            let (tx, mut rx) = mpsc::unbounded_channel();
            let mut session = Session::default();
            let mut app = playlists_app(&tx, &mut rx, &mut session).await;
            open_playlist_with(&mut app, &tx, &mut session, StubYtDlp);

            leave_playlists(&mut app, &mut session);

            assert!(app.playlist.is_none(), "中身も閉じる");
            assert!(app.playlists.is_none());
            assert_eq!(app.mode, Mode::Results);
        }

        #[tokio::test]
        async fn leaving_the_playlists_list_without_results_returns_to_input() {
            let (tx, _rx) = mpsc::unbounded_channel();
            let mut session = Session::default();
            let mut app = App::default();
            open_playlists_with(&mut app, &tx, &mut session, StubYtDlp);

            leave_playlists(&mut app, &mut session);
            assert_eq!(app.mode, Mode::Input, "検索結果が無ければ入力へ戻る");
        }

        #[tokio::test]
        async fn the_search_thumbnails_survive_a_trip_into_the_playlists_list() {
            // 一覧は検索結果とは別の入れ物なので、行き帰りで貼ってある画像を捨てない。
            let (tx, mut rx) = mpsc::unbounded_channel();
            let mut session = Session::default();
            let mut app = grid_app(2);
            let image = RgbImage::new(2, 2, vec![0; 12]).expect("長さは合っている");
            app.thumbs
                .apply(vec![("id0".to_string(), Ok(image))], (144, 80));

            open_playlists_with(
                &mut app,
                &tx,
                &mut session,
                FakeYtDlp::new([done(0, PLAYLIST_LINES, "")]),
            );
            finish_playlists(&mut app, &mut rx, &mut session).await;
            assert!(app.thumbs.get("id0").is_some(), "一覧へ移っても捨てない");
            assert!(session.owe_clear, "一覧の行に画像が重なったままにしない");

            leave_playlists(&mut app, &mut session);
            assert!(app.thumbs.get("id0").is_some(), "取り直しは要らない");
            assert!(app.thumbs.take_dirty(), "剥がした画像は戻ったら貼り直す");
        }

        #[tokio::test]
        async fn reloading_a_playlist_searches_it_again() {
            let (tx, mut rx) = mpsc::unbounded_channel();
            let mut session = Session::default();
            let mut app = playlists_app(&tx, &mut rx, &mut session).await;
            open_playlist_with(&mut app, &tx, &mut session, StubYtDlp);
            session.search_task.take().expect("タスク").abort();
            // 取れなかったプレイリストも読み込み済みで残ることがある。そこからでも取り直せる。
            app.set_results(Vec::new(), &playlist_target(&app));
            assert!(app.playlist.as_ref().expect("playlist").state.loaded);

            reload_playlist_with(&mut app, &tx, &mut session, StubYtDlp);

            assert!(
                session.search_task.is_some(),
                "同じプレイリストを取りに行く"
            );
            assert!(!app.playlist.as_ref().expect("playlist").state.loaded);
            assert!(app.searching);
            assert_eq!(app.mode, Mode::Playlist);
        }

        #[tokio::test]
        async fn reloading_does_nothing_outside_a_playlist() {
            let (tx, mut rx) = mpsc::unbounded_channel();
            let mut session = Session::default();
            let mut app = playlists_app(&tx, &mut rx, &mut session).await;

            reload_playlist_with(&mut app, &tx, &mut session, StubYtDlp);

            assert!(session.search_task.is_none());
            assert!(!app.searching);
        }
    }
}
