//! ダウンロード画面 (Mode::Download)。入力欄、キー操作、描画、開閉と開始、完了の反映を持つ。
//! yt-dlp の呼び出しそのものは crate::download にある。

use crate::actions::Session;
use crate::app::{App, AppEvent, Mode};
use crate::download;
use crate::query::QueryEditor;
use crate::ui;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::Span;
use ratatui::widgets::{List, ListItem, ListState, Paragraph};
use tokio::sync::mpsc::UnboundedSender;

/// ダウンロード画面の状態。
#[derive(Debug, Clone)]
pub struct DownloadForm {
    /// 開いた元のモード。閉じたらここへ戻る。
    pub return_mode: Mode,
    /// 保存先の入力欄。開いた時点で `[download] dir` かその既定値を入れておく。
    pub dir: QueryEditor,
    /// ファイル名の入力欄。拡張子 (`.%(ext)s`) は含まない。
    pub filename: QueryEditor,
    /// true なら音声のみ (`-x --audio-format mp3`)。
    pub audio_only: bool,
    pub focus: DownloadField,
    /// 開いた時点の対象 URL。タイトルは初期ファイル名の計算にだけ使うので保持しない。
    pub url: String,
}

impl Default for DownloadForm {
    fn default() -> Self {
        Self {
            return_mode: Mode::Input,
            dir: QueryEditor::default(),
            filename: QueryEditor::default(),
            audio_only: false,
            focus: DownloadField::default(),
            url: String::new(),
        }
    }
}

/// ダウンロード画面でフォーカス中の行。↑↓ で巡回する。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DownloadField {
    #[default]
    Dir,
    Filename,
    Format,
}

impl DownloadField {
    pub fn next(self) -> Self {
        match self {
            Self::Dir => Self::Filename,
            Self::Filename => Self::Format,
            Self::Format => Self::Dir,
        }
    }

    pub fn prev(self) -> Self {
        match self {
            Self::Dir => Self::Format,
            Self::Filename => Self::Dir,
            Self::Format => Self::Filename,
        }
    }

    /// リストの行番号 (0 始まり)。draw_download のカーソル計算にも使う。
    pub fn index(self) -> usize {
        match self {
            Self::Dir => 0,
            Self::Filename => 1,
            Self::Format => 2,
        }
    }
}

/// ダウンロード画面は保存の成否をここで返す。設定画面と同じくポーリングが無い。
pub fn download_status(app: &App) -> String {
    if let Some(error) = &app.error {
        return format!("エラー: {error}");
    }
    "ダウンロード保存先の入力".to_string()
}

// ---- キー ----

/// ダウンロード画面の操作。yt-dlp を実際に起動させないよう Downloader を差し替えられる形。
pub async fn handle_key_download(
    app: &mut App,
    key: KeyEvent,
    tx: &UnboundedSender<AppEvent>,
    session: &mut Session,
) {
    handle_key_download_with(app, key, tx, session, download::RealYtDlp).await;
}

/// フォーカス中の行が Dir/Filename なら、その QueryEditor。Format には無い。
fn download_editor_mut(app: &mut App) -> Option<&mut QueryEditor> {
    match app.download.focus {
        DownloadField::Dir => Some(&mut app.download.dir),
        DownloadField::Filename => Some(&mut app.download.filename),
        DownloadField::Format => None,
    }
}

async fn handle_key_download_with<D: download::Downloader + Send + Sync + 'static>(
    app: &mut App,
    key: KeyEvent,
    tx: &UnboundedSender<AppEvent>,
    session: &mut Session,
    downloader: D,
) {
    match key.code {
        KeyCode::Up => move_download_focus(app, -1),
        KeyCode::Down => move_download_focus(app, 1),
        // 形式の行では ← → Enter Space が切替、それ以外はカーソル移動/決定。
        KeyCode::Left | KeyCode::Right | KeyCode::Enter | KeyCode::Char(' ')
            if app.download.focus == DownloadField::Format =>
        {
            app.download.audio_only = !app.download.audio_only;
        }
        KeyCode::Left => {
            if let Some(editor) = download_editor_mut(app) {
                editor.move_left(false);
            }
        }
        KeyCode::Right => {
            if let Some(editor) = download_editor_mut(app) {
                editor.move_right(false);
            }
        }
        KeyCode::Backspace => {
            if let Some(editor) = download_editor_mut(app) {
                editor.backspace();
            }
        }
        // 制御文字混じりの Ctrl+ 何かは入力に混ぜない。
        KeyCode::Char(_) if key.modifiers.contains(KeyModifiers::CONTROL) => {}
        KeyCode::Char(c) => {
            if let Some(editor) = download_editor_mut(app) {
                editor.insert(c);
            }
        }
        KeyCode::Enter => start_download(app, tx, session, downloader),
        KeyCode::Esc => close_download(app),
        _ => {}
    }
}

// ---- 描画 ----

/// ダウンロード画面は設定画面と同じ [タイトル, 項目, ステータス, ヘルプ] の4段。
pub fn download_areas(area: Rect) -> [Rect; 4] {
    Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area)
}

const DOWNLOAD_TITLE: &str = "ダウンロード (Enter で開始。Esc は戻る)";

const DOWNLOAD_MARKER: &str = "> ";

const DOWNLOAD_DIR_LABEL: &str = "保存先: ";

const DOWNLOAD_FILENAME_LABEL: &str = "ファイル名: ";

/// 項目の 3 行。フォーマット行は編集不可の値をそのまま出す。
fn download_rows(app: &App) -> Vec<String> {
    vec![
        format!("{DOWNLOAD_DIR_LABEL}{}", app.download.dir.text()),
        // 拡張子は yt-dlp が決めるので、末尾に固定で見せるだけで編集はさせない。
        format!(
            "{DOWNLOAD_FILENAME_LABEL}{}.%(ext)s",
            app.download.filename.text()
        ),
        format!(
            "形式: {}",
            if app.download.audio_only {
                "音声のみ"
            } else {
                "動画"
            }
        ),
    ]
}

pub fn draw_download(frame: &mut Frame, app: &App) {
    let areas = download_areas(frame.area());
    frame.render_widget(
        Paragraph::new(DOWNLOAD_TITLE).style(Style::default().add_modifier(Modifier::BOLD)),
        areas[0],
    );

    let rows = download_rows(app);
    let items: Vec<ListItem> = rows.into_iter().map(ListItem::new).collect();
    let list = List::new(items)
        .highlight_symbol(DOWNLOAD_MARKER)
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    let mut state = ListState::default();
    state.select(Some(app.download.focus.index()));
    frame.render_stateful_widget(list, areas[1], &mut state);

    ui::draw_footer(frame, app, areas[2], areas[3]);

    if let Some(at) = download_cursor(frame.area(), app) {
        frame.set_cursor_position(at);
    }
}

/// フォーカス中が Dir/Filename のときだけカーソルを出す。Format には無い。
fn download_cursor(screen: Rect, app: &App) -> Option<(u16, u16)> {
    let area = download_areas(screen)[1];
    let (label, editor) = match app.download.focus {
        DownloadField::Dir => (DOWNLOAD_DIR_LABEL, &app.download.dir),
        DownloadField::Filename => (DOWNLOAD_FILENAME_LABEL, &app.download.filename),
        DownloadField::Format => return None,
    };
    let prefix = format!("{DOWNLOAD_MARKER}{label}{}", editor.before_cursor());
    let width = Span::raw(prefix.as_str()).width().min(u16::MAX as usize) as u16;
    let x = area
        .x
        .saturating_add(width)
        .min(area.right().saturating_sub(1));
    let last = area.height.saturating_sub(1) as usize;
    let y = area
        .y
        .saturating_add(app.download.focus.index().min(last) as u16);
    Some((x, y))
}

/// ダウンロード画面の案内。全部で 40 桁ほど。
pub fn download_hints() -> Vec<String> {
    vec![
        "↑↓:選択".to_string(),
        "←→:カーソル/形式切替".to_string(),
        "Enter:開始".to_string(),
        "Esc:戻る".to_string(),
    ]
}

// ---- 開閉と開始 ----

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
    app.download.return_mode = app.mode;
    app.mode = Mode::Download;
    app.download.url = url;
    let home = std::env::var_os("HOME");
    let dir = download::default_dir(app.settings.download.dir.as_deref(), home.as_deref());
    let dir_text = dir.map(|d| d.display().to_string()).unwrap_or_default();
    app.download.dir = QueryEditor::from(dir_text.as_str());
    app.download.filename = QueryEditor::from(download::sanitize_filename(&title).as_str());
    app.download.audio_only = false;
    app.download.focus = DownloadField::Dir;
    // 貼ってあるサムネイル/映像は差分描画では消えないので、剥がしてから画面を出す。
    session.owe_clear = true;
}

/// 何もせず閉じる。ダウンロードを始めていれば背景で進んだまま。
pub fn close_download(app: &mut App) {
    app.mode = app.download.return_mode;
    // 開くときに剥がしたサムネイル/映像を貼り直す。
    app.thumbs.mark_dirty();
}

/// ↑↓ のフォーカス移動。3 行しか無いので端では巻き戻す。
pub fn move_download_focus(app: &mut App, delta: i32) {
    app.download.focus = if delta < 0 {
        app.download.focus.prev()
    } else {
        app.download.focus.next()
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
    let dir_text = app.download.dir.text().trim().to_string();
    // 初期値は open_download でサニタイズ済みだが、その後の自由入力で `/` `\` を
    // 打ち直されるとディレクトリを飛び出せてしまうため、開始直前にも掛け直す。
    let filename_text = download::sanitize_filename(app.download.filename.text().trim());
    if dir_text.is_empty() || filename_text.is_empty() {
        app.set_temporary_error(
            "保存先とファイル名を入力してください".to_string(),
            std::time::Instant::now(),
        );
        return;
    }
    cancel_download(session);
    let nonce = session.download_nonce;
    let audio_only = app.download.audio_only;
    let url = app.download.url.clone();
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

/// ダウンロードの結果を画面へ渡す。
pub fn apply_download_done(
    app: &mut App,
    session: &mut Session,
    nonce: u64,
    notice: Result<String, String>,
) {
    if nonce != session.download_nonce {
        return;
    }
    session.download_task = None;
    match notice {
        Ok(notice) => app.set_temporary_notice(notice, std::time::Instant::now()),
        Err(e) => {
            // 失敗のときは set_error だけでは「ダウンロード中…」が残るので、ここで畳む。
            if app
                .notice
                .as_deref()
                .is_some_and(|n| n.starts_with(download::DOWNLOADING_PREFIX))
            {
                app.set_notice(None);
            }
            app.set_error(Some(e));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{AppEvent, ChannelView, Playback};
    use crate::cookies::{ChannelTab, Target};
    use crate::grid;
    use crate::search::SearchResult;
    use crossterm::event::{KeyCode, KeyModifiers};
    use std::future::Future;
    use std::process::Output;
    use tokio::sync::mpsc::{self, UnboundedReceiver, unbounded_channel};

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

    fn grid_app(count: usize) -> App {
        let mut app = App {
            mode: Mode::Results,
            screen: Rect::new(0, 0, 80, 24),
            ..App::default()
        };
        let results: Vec<SearchResult> = (0..count).map(|i| result(&format!("id{i}"))).collect();
        app.set_results(results, &Target::Search("q".to_string()));
        // 本番では set_results の後に必ず draw が挟まる。
        app.mark_drawn();
        app
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

    // ---- 行と状態行 ----

    #[test]
    fn download_field_cycles_forward_and_wraps() {
        assert_eq!(DownloadField::Dir.next(), DownloadField::Filename);
        assert_eq!(DownloadField::Filename.next(), DownloadField::Format);
        assert_eq!(DownloadField::Format.next(), DownloadField::Dir, "巻き戻る");
    }

    #[test]
    fn download_field_cycles_backward_and_wraps() {
        assert_eq!(DownloadField::Dir.prev(), DownloadField::Format, "巻き戻る");
        assert_eq!(DownloadField::Format.prev(), DownloadField::Filename);
        assert_eq!(DownloadField::Filename.prev(), DownloadField::Dir);
    }

    #[test]
    fn download_field_index_matches_the_row_order() {
        assert_eq!(DownloadField::Dir.index(), 0);
        assert_eq!(DownloadField::Filename.index(), 1);
        assert_eq!(DownloadField::Format.index(), 2);
    }

    #[test]
    fn the_download_status_line_names_the_screen() {
        let app = App {
            mode: Mode::Download,
            ..App::default()
        };
        assert_eq!(app.status_line(), "ダウンロード保存先の入力");
    }

    #[test]
    fn the_download_status_line_shows_an_error_over_the_screen_name() {
        let mut app = App {
            mode: Mode::Download,
            ..App::default()
        };
        app.set_error(Some("保存先とファイル名を入力してください".to_string()));
        assert_eq!(
            app.status_line(),
            "エラー: 保存先とファイル名を入力してください"
        );
    }

    // ---- キー ----

    /// ダウンロード画面を開いた状態 (Results から)。
    fn download_app() -> App {
        let mut app = grid_app(1);
        let mut session = Session::default();
        open_download(&mut app, &mut session);
        app
    }

    #[tokio::test]
    async fn up_and_down_move_the_focus_between_the_three_rows() {
        let mut app = download_app();
        let (tx, _rx) = channel();
        let mut session = Session::default();
        assert_eq!(app.download.focus, DownloadField::Dir);

        handle_key_download_with(
            &mut app,
            key(KeyCode::Down),
            &tx,
            &mut session,
            crate::download::fixtures::FakeDownloader::new([]),
        )
        .await;
        assert_eq!(app.download.focus, DownloadField::Filename);

        handle_key_download_with(
            &mut app,
            key(KeyCode::Up),
            &tx,
            &mut session,
            crate::download::fixtures::FakeDownloader::new([]),
        )
        .await;
        assert_eq!(app.download.focus, DownloadField::Dir);
    }

    #[tokio::test]
    async fn arrow_keys_move_the_cursor_in_the_focused_editor() {
        let mut app = download_app();
        let (tx, _rx) = channel();
        let mut session = Session::default();
        app.download.dir = QueryEditor::from("/tmp/out");

        handle_key_download_with(
            &mut app,
            key(KeyCode::Left),
            &tx,
            &mut session,
            crate::download::fixtures::FakeDownloader::new([]),
        )
        .await;

        assert_eq!(app.download.dir.before_cursor(), "/tmp/ou");
    }

    #[tokio::test]
    async fn typing_inserts_into_the_focused_editor() {
        let mut app = download_app();
        let (tx, _rx) = channel();
        let mut session = Session::default();
        app.download.dir = QueryEditor::default();

        handle_key_download_with(
            &mut app,
            key(KeyCode::Char('/')),
            &tx,
            &mut session,
            crate::download::fixtures::FakeDownloader::new([]),
        )
        .await;
        handle_key_download_with(
            &mut app,
            key(KeyCode::Char('x')),
            &tx,
            &mut session,
            crate::download::fixtures::FakeDownloader::new([]),
        )
        .await;

        assert_eq!(app.download.dir.text(), "/x");
    }

    #[tokio::test]
    async fn backspace_edits_the_focused_editor() {
        let mut app = download_app();
        let (tx, _rx) = channel();
        let mut session = Session::default();
        app.download.dir = QueryEditor::from("/tmp");

        handle_key_download_with(
            &mut app,
            key(KeyCode::Backspace),
            &tx,
            &mut session,
            crate::download::fixtures::FakeDownloader::new([]),
        )
        .await;

        assert_eq!(app.download.dir.text(), "/tm");
    }

    #[tokio::test]
    async fn left_and_right_toggle_the_format_row_instead_of_moving_a_cursor() {
        let mut app = download_app();
        let (tx, _rx) = channel();
        let mut session = Session::default();
        app.download.focus = DownloadField::Format;
        assert!(!app.download.audio_only);

        handle_key_download_with(
            &mut app,
            key(KeyCode::Right),
            &tx,
            &mut session,
            crate::download::fixtures::FakeDownloader::new([]),
        )
        .await;
        assert!(app.download.audio_only, "音声のみへ切替");

        handle_key_download_with(
            &mut app,
            key(KeyCode::Left),
            &tx,
            &mut session,
            crate::download::fixtures::FakeDownloader::new([]),
        )
        .await;
        assert!(!app.download.audio_only, "動画へ戻る");
    }

    #[tokio::test]
    async fn enter_and_space_also_toggle_the_format_row() {
        let mut app = download_app();
        let (tx, _rx) = channel();
        let mut session = Session::default();
        app.download.focus = DownloadField::Format;
        assert!(!app.download.audio_only);

        handle_key_download_with(
            &mut app,
            key(KeyCode::Enter),
            &tx,
            &mut session,
            crate::download::fixtures::FakeDownloader::new([]),
        )
        .await;
        assert!(app.download.audio_only, "Enter で音声のみへ切替");
        assert_eq!(app.mode, Mode::Download, "ダウンロードを開始しない");
        assert!(session.download_task.is_none());

        handle_key_download_with(
            &mut app,
            key(KeyCode::Char(' ')),
            &tx,
            &mut session,
            crate::download::fixtures::FakeDownloader::new([]),
        )
        .await;
        assert!(!app.download.audio_only, "Space で動画へ戻る");
    }

    #[tokio::test]
    async fn typing_on_the_format_row_does_nothing() {
        let mut app = download_app();
        let (tx, _rx) = channel();
        let mut session = Session::default();
        app.download.focus = DownloadField::Format;

        handle_key_download_with(
            &mut app,
            key(KeyCode::Char('x')),
            &tx,
            &mut session,
            crate::download::fixtures::FakeDownloader::new([]),
        )
        .await;

        assert!(!app.download.audio_only, "文字入力では切り替わらない");
    }

    #[tokio::test]
    async fn esc_closes_the_download_screen_without_starting_anything() {
        let mut app = download_app();
        let (tx, _rx) = channel();
        let mut session = Session::default();

        handle_key_download_with(
            &mut app,
            key(KeyCode::Esc),
            &tx,
            &mut session,
            crate::download::fixtures::FakeDownloader::new([]),
        )
        .await;

        assert_eq!(app.mode, Mode::Results);
        assert!(session.download_task.is_none());
    }

    #[tokio::test]
    async fn enter_starts_the_download_and_returns_to_the_previous_screen() {
        let mut app = download_app();
        app.download.dir = QueryEditor::from("/tmp/out");
        app.download.filename = QueryEditor::from("title");
        let (tx, _rx) = channel();
        let mut session = Session::default();
        let downloader =
            crate::download::fixtures::FakeDownloader::new([crate::download::fixtures::done(
                0,
                "/tmp/out/title.mp4\n",
                "",
            )]);

        handle_key_download_with(&mut app, key(KeyCode::Enter), &tx, &mut session, downloader)
            .await;

        assert_eq!(app.mode, Mode::Results, "即座に元の画面へ戻る");
        assert!(session.download_task.is_some(), "背景で進む");
        session.download_task.take().unwrap().abort();
    }

    // ---- 描画 ----

    #[test]
    fn download_areas_match_the_settings_screen_layout() {
        let area = Rect::new(0, 0, 80, 24);
        assert_eq!(
            download_areas(area),
            crate::screen::settings::settings_areas(area)
        );
    }

    #[test]
    fn download_help_lists_the_keys() {
        let help = ui::fit_hints(&download_hints(), 80);
        for key in ["↑↓:選択", "Enter:開始", "Esc:戻る"] {
            assert!(help.contains(key), "{key} が落ちた: {help}");
        }
        assert!(grid::display_width(&help) <= 80, "{help}");

        // 画面の最下行 (ui のフッタ) にも同じ文言が出る。
        let screen = rendered(&filled_app(), 80, 24);
        assert_eq!(screen.lines().last().unwrap_or_default().trim_end(), help);
    }

    /// 保存先とファイル名を入れたダウンロード画面。
    fn filled_app() -> App {
        let mut app = App {
            mode: Mode::Download,
            ..App::default()
        };
        app.download.dir = QueryEditor::from("/home/x/Downloads");
        app.download.filename = QueryEditor::from("面白い動画");
        app
    }

    #[test]
    fn the_download_screen_draws_all_three_rows() {
        let app = filled_app();
        let screen = rendered(&app, 80, 24);

        assert!(screen.contains("保存先: /home/x/Downloads"), "{screen}");
        assert!(
            screen.contains("ファイル名: 面白い動画.%(ext)s"),
            "{screen}"
        );
        assert!(screen.contains("形式: 動画"), "{screen}");
        assert!(screen.contains("ダウンロード"), "タイトルが出る:\n{screen}");
    }

    #[test]
    fn the_download_screen_marks_the_focused_row() {
        let mut app = filled_app();
        app.download.focus = DownloadField::Filename;
        let screen = rendered(&app, 80, 24);

        assert!(screen.contains("> ファイル名"), "{screen}");
        assert!(!screen.contains("> 保存先"), "{screen}");
    }

    #[test]
    fn the_download_screen_shows_audio_only_when_set() {
        let mut app = filled_app();
        app.download.audio_only = true;
        let screen = rendered(&app, 80, 24);

        assert!(screen.contains("形式: 音声のみ"), "{screen}");
        assert!(!screen.contains("形式: 動画"), "{screen}");
    }

    #[test]
    fn the_download_cursor_follows_the_focused_editor() {
        let screen = Rect::new(0, 0, 80, 24);
        let mut app = filled_app();
        app.download.dir = QueryEditor::from("abc");
        app.download.focus = DownloadField::Dir;
        let with_full_text = download_cursor(screen, &app).expect("Dir にはカーソルがある");

        app.download.dir.move_left(false);
        let after_move_left = download_cursor(screen, &app).expect("Dir にはカーソルがある");
        assert_eq!(after_move_left.0 + 1, with_full_text.0, "1 文字ぶん手前へ");
    }

    #[test]
    fn the_download_cursor_is_absent_on_the_format_row() {
        let mut app = filled_app();
        app.download.focus = DownloadField::Format;
        assert_eq!(download_cursor(Rect::new(0, 0, 80, 24), &app), None);
    }

    // ---- 開閉と開始 ----

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
        assert_eq!(app.download.return_mode, Mode::Results);
        assert_eq!(app.download.url, "https://www.youtube.com/watch?v=id0");
        // `/` はディレクトリを飛び出さないよう `_` へ置き換える。
        assert_eq!(app.download.filename.text(), "面白い_動画");
        assert_eq!(app.download.focus, DownloadField::Dir);
        assert!(!app.download.audio_only, "既定は動画");
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

        assert_eq!(app.download.return_mode, Mode::Playing);
        assert_eq!(app.download.url, "https://www.youtube.com/watch?v=live1");
        assert_eq!(app.download.filename.text(), "再生中の動画");
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

        assert_eq!(app.download.dir.text(), "/configured/dir");
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
        assert_eq!(app.download.focus, DownloadField::Dir);

        move_download_focus(&mut app, -1);
        assert_eq!(app.download.focus, DownloadField::Format, "巻き戻る");

        move_download_focus(&mut app, 1);
        assert_eq!(app.download.focus, DownloadField::Dir);
        move_download_focus(&mut app, 1);
        assert_eq!(app.download.focus, DownloadField::Filename);
    }

    #[tokio::test]
    async fn starting_a_download_spawns_the_task_with_the_entered_values() {
        let mut session = Session::default();
        let mut app = download_ready_app(result("id0"));
        open_download(&mut app, &mut session);
        app.download.dir = QueryEditor::from("/tmp/tuitube-dl");
        app.download.filename = QueryEditor::from("my-title");
        app.download.audio_only = true;
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
        app.download.dir = QueryEditor::from("   ");
        app.download.filename = QueryEditor::from("title");
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
        app.download.dir = QueryEditor::from("/tmp/tuitube-dl");
        app.download.filename = QueryEditor::from("../../etc/evil");
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
        app.download.filename = QueryEditor::from("");
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
        app.download.dir = QueryEditor::from("/tmp/tuitube-dl");
        app.download.filename = QueryEditor::from("first");
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
        app.download.dir = QueryEditor::from("/tmp/tuitube-dl");
        app.download.filename = QueryEditor::from("second");
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
