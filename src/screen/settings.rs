//! 設定画面 (Mode::Settings)。編集できる項目、キー操作、描画、開閉と保存を持つ。

use crate::actions::{NO_CONFIG_PATH, Session, config_path_from_env};
use crate::app::{App, Mode};
use crate::display::FpsCap;
use crate::settings::{
    self, EnvOverridden, FPS_LIMIT_VAR, MAX_FPS_CAP, MAX_SEARCH_CACHE_TTL_SECS, MAX_SEARCH_LIMIT,
    MAX_SEARCH_TIMEOUT_SECS, MAX_THUMB_TIMEOUT_SECS, MIN_SEARCH_CACHE_TTL_SECS, MIN_SEARCH_LIMIT,
    MIN_SEARCH_TIMEOUT_SECS, Settings,
};
use crate::ui;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::Span;
use ratatui::widgets::{List, ListItem, ListState, Paragraph};
use std::path::Path;
use std::time::Duration;

/// 設定画面の状態。
#[derive(Debug, Clone)]
pub struct SettingsScreen {
    /// 選んでいる行。SETTINGS_ITEMS の範囲へ丸めて使う。
    pub selected: usize,
    /// 開いた元のモード。閉じたらここへ戻る。
    pub return_mode: Mode,
    /// 開いた時点 (または最後に保存した時点) の設定。Esc はここへ戻す。
    pub backup: Settings,
    /// 数値項目へ打ち込んでいる途中の文字列。None なら通常表示。
    /// settings へ書くのは確定したときだけなので、backup とは別に持つ。
    pub edit: Option<String>,
}

impl Default for SettingsScreen {
    fn default() -> Self {
        Self {
            selected: 0,
            return_mode: Mode::Input,
            backup: Settings::default(),
            edit: None,
        }
    }
}

impl SettingsScreen {
    /// 選択中の設定項目。壊れた添字でも必ず 1 つ返す。
    pub fn item(&self) -> SettingsItem {
        SETTINGS_ITEMS[self.selected.min(SETTINGS_ITEMS.len() - 1)]
    }
}

/// 設定画面で編集できる項目。画面の並びはこの順。
/// ここに無い値 (window.ontop 以外の window.* / cookies.* / mpv.extra_args /
/// subtitles.lang / categories) は config.toml を直接編集する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsItem {
    DisplayMode,
    DisplayQuality,
    FpsCap,
    SubtitlesEnabled,
    WindowOntop,
    SearchLayout,
    SearchLimit,
    SearchTimeoutSecs,
    SearchCacheEnabled,
    SearchCacheTtlSecs,
    ThumbnailsEnabled,
    ThumbnailsMaxCached,
    ThumbnailsTimeoutSecs,
    DownloadDebug,
}

pub const SETTINGS_ITEMS: [SettingsItem; 14] = [
    SettingsItem::DisplayMode,
    SettingsItem::DisplayQuality,
    SettingsItem::FpsCap,
    SettingsItem::SubtitlesEnabled,
    SettingsItem::WindowOntop,
    SettingsItem::SearchLayout,
    SettingsItem::SearchLimit,
    SettingsItem::SearchTimeoutSecs,
    SettingsItem::SearchCacheEnabled,
    SettingsItem::SearchCacheTtlSecs,
    SettingsItem::ThumbnailsEnabled,
    SettingsItem::ThumbnailsMaxCached,
    SettingsItem::ThumbnailsTimeoutSecs,
    SettingsItem::DownloadDebug,
];

/// 数値項目の 1 回ぶんの刻み。
const FPS_CAP_STEP: u64 = 5;
const SEARCH_LIMIT_STEP: u64 = 1;
const SEARCH_TIMEOUT_STEP: u64 = 5;
const SEARCH_CACHE_TTL_STEP: u64 = 5;
const MAX_CACHED_STEP: u64 = 50;
const THUMB_TIMEOUT_STEP: u64 = 5;
/// 0 秒では 1 枚も取れないので、秒数はここまでしか下げない。
const MIN_THUMB_TIMEOUT_SECS: u64 = 1;
/// max_cached に上限は無い。刻みの計算にだけ使う。
const NO_MAX: u64 = u64::MAX;

/// 刻みの次の目盛り。上限では止まる。
fn step_up(current: u64, step: u64, max: u64) -> u64 {
    (current / step * step).saturating_add(step).min(max)
}

/// 刻みの 1 つ前の目盛り。下限が刻みに乗っていなくても、目盛りの基準はずらさない。
/// (例: 刻み 5 / 下限 1 で 5 → 1 → 5 と戻る)
fn step_down(current: u64, step: u64, min: u64) -> u64 {
    (current.saturating_sub(1) / step * step).max(min)
}

impl SettingsItem {
    /// 設定ファイルのキーをそのまま出す。画面で見た項目を config.toml で探せるため。
    pub fn label(self) -> &'static str {
        match self {
            Self::DisplayMode => "display.mode",
            Self::DisplayQuality => "display.quality",
            Self::FpsCap => "fps_cap",
            Self::SubtitlesEnabled => "subtitles.enabled",
            Self::WindowOntop => "window.ontop",
            Self::SearchLayout => "search.layout",
            Self::SearchLimit => "search.limit",
            Self::SearchTimeoutSecs => "search.timeout_secs",
            Self::SearchCacheEnabled => "search.cache_enabled",
            Self::SearchCacheTtlSecs => "search.cache_ttl_secs",
            Self::ThumbnailsEnabled => "thumbnails.enabled",
            Self::ThumbnailsMaxCached => "thumbnails.max_cached",
            Self::ThumbnailsTimeoutSecs => "thumbnails.timeout_secs",
            Self::DownloadDebug => "download.debug",
        }
    }

    /// 値も設定ファイルの表記に合わせる。
    pub fn value(self, settings: &Settings) -> String {
        match self {
            Self::DisplayMode => settings.display.mode.key().to_string(),
            Self::DisplayQuality => settings.display.quality.label().to_string(),
            Self::FpsCap => match settings.fps_cap {
                Some(cap) => cap.get().to_string(),
                None => "0 (無制限)".to_string(),
            },
            Self::SubtitlesEnabled => settings.subtitles.enabled.to_string(),
            Self::WindowOntop => settings.window.ontop.to_string(),
            Self::SearchLayout => settings.search.layout.key().to_string(),
            Self::SearchLimit => settings.search.limit.to_string(),
            Self::SearchTimeoutSecs => settings.search.timeout.as_secs().to_string(),
            Self::SearchCacheEnabled => settings.search.cache_enabled.to_string(),
            Self::SearchCacheTtlSecs => settings.search.cache_ttl.as_secs().to_string(),
            Self::ThumbnailsEnabled => settings.thumbnails.enabled.to_string(),
            Self::ThumbnailsMaxCached => settings.thumbnails.max_cached.to_string(),
            Self::ThumbnailsTimeoutSecs => settings.thumbnails.timeout.as_secs().to_string(),
            Self::DownloadDebug => settings.download.debug.to_string(),
        }
    }

    pub fn row(self, settings: &Settings) -> String {
        format!("{}: {}", self.label(), self.value(settings))
    }

    /// 環境変数が上書きしている項目なら、その変数名。保存では書き換えないので画面にも出す。
    pub fn env_var(self, overridden: &EnvOverridden) -> Option<&'static str> {
        match self {
            Self::FpsCap => overridden.fps_cap.is_some().then_some(FPS_LIMIT_VAR),
            _ => None,
        }
    }

    /// 数字キーで値を直接打ち込める項目。選択肢と bool は ←→ だけで動かす。
    pub fn is_numeric(self) -> bool {
        matches!(
            self,
            Self::FpsCap
                | Self::SearchLimit
                | Self::SearchTimeoutSecs
                | Self::SearchCacheTtlSecs
                | Self::ThumbnailsMaxCached
                | Self::ThumbnailsTimeoutSecs
        )
    }

    /// 上限の桁数。これを超えた入力はパースできず捨てられるので、打ち込みはここで止める。
    /// 数値でない項目は 0 桁 = 打ち込みを受けない。
    pub fn max_digits(self) -> usize {
        let max: u64 = match self {
            Self::FpsCap => u64::from(MAX_FPS_CAP),
            Self::SearchLimit => MAX_SEARCH_LIMIT as u64,
            Self::SearchTimeoutSecs => MAX_SEARCH_TIMEOUT_SECS,
            Self::SearchCacheTtlSecs => MAX_SEARCH_CACHE_TTL_SECS,
            // max_cached に上限は無いので、apply_numeric が受け取れる最大値で数える。
            Self::ThumbnailsMaxCached => u64::try_from(usize::MAX).unwrap_or(u64::MAX),
            Self::ThumbnailsTimeoutSecs => MAX_THUMB_TIMEOUT_SECS,
            Self::DisplayMode
            | Self::DisplayQuality
            | Self::SubtitlesEnabled
            | Self::WindowOntop
            | Self::SearchLayout
            | Self::SearchCacheEnabled
            | Self::ThumbnailsEnabled
            | Self::DownloadDebug => return 0,
        };
        max.to_string().len()
    }

    /// 打ち込んだ値の確定。範囲は ←→ と同じで、外れていれば端へ寄せる。
    /// 数として読めない入力と数値でない項目は何もせず false を返す。
    pub fn apply_numeric(self, settings: &mut Settings, raw: &str) -> bool {
        let Ok(value) = raw.parse::<u64>() else {
            return false;
        };
        match self {
            Self::FpsCap => {
                // 0 は「制限なし」。FpsCap は 1 以上しか作れない。
                settings.fps_cap = FpsCap::new(value.min(u64::from(MAX_FPS_CAP)) as u32);
            }
            Self::SearchLimit => {
                let min = MIN_SEARCH_LIMIT as u64;
                let max = MAX_SEARCH_LIMIT as u64;
                settings.search.limit = value.clamp(min, max) as usize;
            }
            Self::SearchTimeoutSecs => {
                let secs = value.clamp(MIN_SEARCH_TIMEOUT_SECS, MAX_SEARCH_TIMEOUT_SECS);
                settings.search.timeout = Duration::from_secs(secs);
            }
            Self::SearchCacheTtlSecs => {
                let secs = value.clamp(MIN_SEARCH_CACHE_TTL_SECS, MAX_SEARCH_CACHE_TTL_SECS);
                settings.search.cache_ttl = Duration::from_secs(secs);
            }
            Self::ThumbnailsMaxCached => {
                settings.thumbnails.max_cached = usize::try_from(value).unwrap_or(usize::MAX);
            }
            Self::ThumbnailsTimeoutSecs => {
                let secs = value.clamp(MIN_THUMB_TIMEOUT_SECS, MAX_THUMB_TIMEOUT_SECS);
                settings.thumbnails.timeout = Duration::from_secs(secs);
            }
            // 選択肢と bool は打ち込みを受けない。is_numeric() と対で書き分ける。
            Self::DisplayMode
            | Self::DisplayQuality
            | Self::SubtitlesEnabled
            | Self::WindowOntop
            | Self::SearchLayout
            | Self::SearchCacheEnabled
            | Self::ThumbnailsEnabled
            | Self::DownloadDebug => return false,
        }
        true
    }

    /// ←→ 1 回ぶんの変化。→ は次の値、← は前の値。bool はどちらでも反転する。
    /// 数値は刻みの目盛りを動き、範囲の端で止まる (ラップしない)。
    pub fn adjust(self, settings: &mut Settings, delta: i32) {
        let up = delta >= 0;
        match self {
            Self::DisplayMode => {
                let mode = settings.display.mode;
                settings.display.mode = if up { mode.next() } else { mode.prev() };
            }
            Self::DisplayQuality => {
                let quality = settings.display.quality;
                settings.display.quality = if up { quality.next() } else { quality.prev() };
            }
            Self::FpsCap => {
                let current = u64::from(settings.fps_cap.map(FpsCap::get).unwrap_or(0));
                // 0 は「制限なし」。FpsCap は 1 以上しか作れない。
                let next = step(current, FPS_CAP_STEP, 0, u64::from(MAX_FPS_CAP), up);
                settings.fps_cap = FpsCap::new(next as u32);
            }
            Self::SubtitlesEnabled => settings.subtitles.enabled = !settings.subtitles.enabled,
            Self::WindowOntop => settings.window.ontop = !settings.window.ontop,
            Self::SearchLayout => {
                let layout = settings.search.layout;
                settings.search.layout = if up { layout.next() } else { layout.prev() };
            }
            Self::SearchLimit => {
                let current = settings.search.limit as u64;
                let min = MIN_SEARCH_LIMIT as u64;
                let max = MAX_SEARCH_LIMIT as u64;
                settings.search.limit = step(current, SEARCH_LIMIT_STEP, min, max, up) as usize;
            }
            Self::SearchTimeoutSecs => {
                let current = settings.search.timeout.as_secs();
                let min = MIN_SEARCH_TIMEOUT_SECS;
                let secs = step(
                    current,
                    SEARCH_TIMEOUT_STEP,
                    min,
                    MAX_SEARCH_TIMEOUT_SECS,
                    up,
                );
                settings.search.timeout = Duration::from_secs(secs);
            }
            Self::SearchCacheEnabled => {
                settings.search.cache_enabled = !settings.search.cache_enabled;
            }
            Self::SearchCacheTtlSecs => {
                let current = settings.search.cache_ttl.as_secs();
                let secs = step(
                    current,
                    SEARCH_CACHE_TTL_STEP,
                    MIN_SEARCH_CACHE_TTL_SECS,
                    MAX_SEARCH_CACHE_TTL_SECS,
                    up,
                );
                settings.search.cache_ttl = Duration::from_secs(secs);
            }
            Self::ThumbnailsEnabled => settings.thumbnails.enabled = !settings.thumbnails.enabled,
            Self::ThumbnailsMaxCached => {
                let current = settings.thumbnails.max_cached as u64;
                settings.thumbnails.max_cached =
                    step(current, MAX_CACHED_STEP, 0, NO_MAX, up) as usize;
            }
            Self::ThumbnailsTimeoutSecs => {
                let current = settings.thumbnails.timeout.as_secs();
                let min = MIN_THUMB_TIMEOUT_SECS;
                let secs = step(current, THUMB_TIMEOUT_STEP, min, MAX_THUMB_TIMEOUT_SECS, up);
                settings.thumbnails.timeout = Duration::from_secs(secs);
            }
            Self::DownloadDebug => settings.download.debug = !settings.download.debug,
        }
    }
}

/// 数値項目を 1 目盛りぶん動かす。
fn step(current: u64, width: u64, min: u64, max: u64, up: bool) -> u64 {
    if up {
        step_up(current, width, max).max(min)
    } else {
        step_down(current, width, min).min(max)
    }
}

/// 設定画面に並べる `ラベル: 現在値` の行。
/// 環境変数が効いている行には、保存しても書き換わらない旨を添える。
pub fn settings_rows(app: &App) -> Vec<String> {
    SETTINGS_ITEMS
        .iter()
        .map(|item| {
            let row = item.row(&app.settings);
            match item.env_var(&app.env_overridden) {
                Some(var) => format!("{row}  ({var} が指定中。保存しません)"),
                None => row,
            }
        })
        .collect()
}

/// 設定画面は保存の成否をここで返す。ポーリングが上書きしない画面なので、
/// エラーが出ていればそれだけを出す。
pub fn settings_status(app: &App) -> String {
    if let Some(error) = &app.error {
        return format!("エラー: {error}");
    }
    format!(
        "設定 {}/{}",
        app.settings_screen.selected.min(SETTINGS_ITEMS.len() - 1) + 1,
        SETTINGS_ITEMS.len()
    )
}

// ---- キー ----

/// 設定画面を開くキー。Ctrl+S はどちらの検索画面でも使える。
pub fn is_settings_key(c: char, modifiers: KeyModifiers) -> bool {
    modifiers.contains(KeyModifiers::CONTROL) && c.eq_ignore_ascii_case(&'s')
}

/// 設定画面の操作。保存以外は app.settings をその場で書き換えるだけ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettingsAction {
    Move(i32),
    Adjust(i32),
    Save,
    Close,
}

/// 設定画面のキーと操作の対応表。
fn settings_action(code: KeyCode) -> Option<SettingsAction> {
    match code {
        KeyCode::Up => Some(SettingsAction::Move(-1)),
        KeyCode::Down => Some(SettingsAction::Move(1)),
        KeyCode::Left => Some(SettingsAction::Adjust(-1)),
        // Enter / Space は bool の切替に要る。選択肢や数値では → と同じ扱い。
        KeyCode::Right | KeyCode::Enter | KeyCode::Char(' ') => Some(SettingsAction::Adjust(1)),
        KeyCode::Char('s') => Some(SettingsAction::Save),
        KeyCode::Esc | KeyCode::Char('q') => Some(SettingsAction::Close),
        _ => None,
    }
}

/// 数値項目へ数字を打ち込んでいる間の操作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditAction {
    Push(char),
    Backspace,
    Commit,
    Cancel,
}

/// 修飾キーの付かない数字キー。Ctrl+3 や Alt+3 は設定画面では何もしないままにする。
fn plain_digit(key: KeyEvent) -> Option<char> {
    let modified = key
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER);
    match key.code {
        KeyCode::Char(c) if c.is_ascii_digit() && !modified => Some(c),
        _ => None,
    }
}

/// 打ち込み中のキーと操作の対応表。ここに無いキーは打ち込みを邪魔しないよう捨てる。
fn settings_edit_action(key: KeyEvent) -> Option<EditAction> {
    if let Some(c) = plain_digit(key) {
        return Some(EditAction::Push(c));
    }
    match key.code {
        KeyCode::Backspace => Some(EditAction::Backspace),
        KeyCode::Enter => Some(EditAction::Commit),
        KeyCode::Esc => Some(EditAction::Cancel),
        _ => None,
    }
}

pub fn handle_key_settings(app: &mut App, key: KeyEvent) {
    // 打ち込んでいる間は他のキーを通さない。抜けるのは Enter か Esc だけ。
    if app.settings_screen.edit.is_some() {
        if let Some(action) = settings_edit_action(key) {
            apply_settings_edit(app, action);
        }
        return;
    }
    // 数値項目での数字キーは打ち込みの始まり。選択肢の行では今までどおり効かない。
    if let Some(c) = plain_digit(key)
        && app.settings_screen.item().is_numeric()
    {
        app.settings_screen.edit = Some(c.to_string());
        return;
    }
    match settings_action(key.code) {
        Some(SettingsAction::Move(delta)) => move_settings_selection(app, delta),
        Some(SettingsAction::Adjust(delta)) => adjust_settings_value(app, delta),
        Some(SettingsAction::Save) => save_settings(app, std::time::Instant::now()),
        Some(SettingsAction::Close) => close_settings(app),
        None => {}
    }
}

fn apply_settings_edit(app: &mut App, action: EditAction) {
    match action {
        // 上限の桁数で止める。それ以上はパースできず、Enter が効かないように見える。
        EditAction::Push(c) => {
            let digits = app.settings_screen.item().max_digits();
            if let Some(buffer) = app.settings_screen.edit.as_mut()
                && buffer.len() < digits
            {
                buffer.push(c);
            }
        }
        EditAction::Backspace => {
            if let Some(buffer) = app.settings_screen.edit.as_mut() {
                buffer.pop();
            }
        }
        // 読めない入力は黙って捨てる。設定画面には知らせを消す機会が少ない。
        EditAction::Commit => {
            if let Some(raw) = app.settings_screen.edit.take() {
                app.settings_screen
                    .item()
                    .apply_numeric(&mut app.settings, &raw);
            }
        }
        EditAction::Cancel => app.settings_screen.edit = None,
    }
}

// ---- 描画 ----

/// 設定画面は [タイトル, 項目, ステータス, ヘルプ] の4段。項目に残り全体を渡す。
pub fn settings_areas(area: Rect) -> [Rect; 4] {
    Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area)
}

const SETTINGS_TITLE: &str = "設定 (s で保存。Esc は編集を捨てて戻る)";

/// 打ち込み中は s が効かず、Esc も打ち込みを捨てるだけで画面は閉じない。
const SETTINGS_TYPING_TITLE: &str = "設定 (数値を打ち込み中)";

/// 選択中の行の目印。カーソルの桁を数えるときも同じ幅を足す。
const SETTINGS_MARKER: &str = "> ";

/// タイトルもヘルプ行と同じ条件で切り替える。片方だけ残すと案内が食い違う。
fn settings_title(app: &App) -> &'static str {
    if app.settings_screen.edit.is_some() {
        SETTINGS_TYPING_TITLE
    } else {
        SETTINGS_TITLE
    }
}

pub fn draw_settings(frame: &mut Frame, app: &App) {
    let areas = settings_areas(frame.area());
    frame.render_widget(
        Paragraph::new(settings_title(app)).style(Style::default().add_modifier(Modifier::BOLD)),
        areas[0],
    );

    let mut rows = settings_rows(app);
    let selected = app
        .settings_screen
        .selected
        .min(rows.len().saturating_sub(1));
    // 打ち込み中の行は値だけを差し替える。行末に足してある断りはそのまま残す。
    if let Some(raw) = &app.settings_screen.edit
        && let Some(row) = rows.get_mut(selected)
    {
        let item = app.settings_screen.item();
        let note = row
            .strip_prefix(&item.row(&app.settings))
            .unwrap_or_default()
            .to_string();
        *row = format!("{}: {}{note}", item.label(), raw);
    }
    let items: Vec<ListItem> = rows.into_iter().map(ListItem::new).collect();
    let list = List::new(items)
        .highlight_symbol(SETTINGS_MARKER)
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    let mut state = ListState::default();
    state.select(Some(selected));
    frame.render_stateful_widget(list, areas[1], &mut state);

    ui::draw_footer(frame, app, areas[2], areas[3]);

    if let Some(raw) = &app.settings_screen.edit {
        let at = settings_cursor(
            frame.area(),
            selected,
            app.settings_screen.item().label(),
            raw,
        );
        frame.set_cursor_position(at);
    }
}

/// 打ち込み中の行のカーソル位置 (0 始まり)。
/// 行数が入り切らない端末では一覧が送られて選択行が末尾に来るので、そこへ置く。
pub fn settings_cursor(screen: Rect, index: usize, label: &str, raw: &str) -> (u16, u16) {
    let area = settings_areas(screen)[1];
    let text = format!("{SETTINGS_MARKER}{label}: {raw}");
    let width = Span::raw(text.as_str()).width().min(u16::MAX as usize) as u16;
    let x = area
        .x
        .saturating_add(width)
        .min(area.right().saturating_sub(1));
    let last = area.height.saturating_sub(1) as usize;
    (x, area.y.saturating_add(index.min(last) as u16))
}

/// 今の画面の案内。数値を打ち込んでいる間だけ、その操作へ差し替える。
pub fn settings_help(app: &App, width: usize) -> String {
    let hints = if app.settings_screen.edit.is_some() {
        settings_typing_hints()
    } else {
        settings_hints()
    };
    ui::fit_hints(&hints, width)
}

/// 設定画面の案内。全部で 71 桁ほどで 80 桁端末に収まる。
/// 幅が足りないと後ろから落ちるので、←→ でも代用できる直接入力は保存と終了の後ろに置く。
pub fn settings_hints() -> Vec<String> {
    vec![
        "↑↓:選択".to_string(),
        "←→:値変更".to_string(),
        "Enter/Space:切替".to_string(),
        "s:保存".to_string(),
        "Esc:破棄して戻る".to_string(),
        "0-9:直接入力".to_string(),
    ]
}

/// 数値を打ち込んでいる間の案内。この間は他のキーが効かないので、抜け方を先に出す。
fn settings_typing_hints() -> Vec<String> {
    vec![
        "Enter:確定".to_string(),
        "Esc:取消".to_string(),
        "0-9:入力".to_string(),
        "BS:1字削除".to_string(),
    ]
}

// ---- 開閉と保存 ----

/// 設定画面へ入る。開いた元のモードは close_settings が戻り先に使う。
pub fn open_settings(app: &mut App, session: &mut Session) {
    app.settings_screen.return_mode = app.mode;
    app.mode = Mode::Settings;
    app.settings_screen.selected = app.settings_screen.selected.min(SETTINGS_ITEMS.len() - 1);
    // 打ち込みかけの数字を持ち越さない。残ると開いた直後から打ち込み中になる。
    app.settings_screen.edit = None;
    // Esc の戻り先。保存していない編集は、この値で捨てる。
    app.settings_screen.backup = app.settings.clone();
    // 貼ってあるサムネイルは ratatui の差分描画では消えないので、設定の行に重ならないよう剥がす。
    session.owe_clear = true;
}

/// 保存せず閉じる。編集した値は開いた時点 (保存していればその時点) の値へ戻す。
/// search.limit や search.layout はセッション中に読み直されるので、残すと
/// 「保存しなければ何も変わらない」と食い違う。
pub fn close_settings(app: &mut App) {
    app.settings = app.settings_screen.backup.clone();
    app.settings_screen.edit = None;
    app.mode = app.settings_screen.return_mode;
    // 開くときに剥がしたサムネイルを貼り直す。
    app.thumbs.mark_dirty();
}

/// ↑↓ の選択移動。行数が少ないので端では巻き戻す。
pub fn move_settings_selection(app: &mut App, delta: i32) {
    let count = SETTINGS_ITEMS.len();
    let current = app.settings_screen.selected.min(count - 1);
    app.settings_screen.selected = if delta < 0 {
        (current + count - 1) % count
    } else {
        (current + 1) % count
    };
}

/// ←→ (と Enter / Space) の値変更。変えるのは選択中の行だけ。
pub fn adjust_settings_value(app: &mut App, delta: i32) {
    app.settings_screen.item().adjust(&mut app.settings, delta);
}

/// s での保存。設定ファイルの場所は起動時と同じ規則で決める。
pub fn save_settings(app: &mut App, now: std::time::Instant) {
    save_settings_to(app, config_path_from_env().as_deref(), now);
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
            app.settings_screen.backup = app.settings.clone();
            app.set_temporary_notice(
                saved_notice(path, &app.env_overridden, display_mode_deferred),
                now,
            );
        }
        Err(e) => app.set_temporary_error(format!("設定を保存できません: {e}"), now),
    }
}

/// 保存できた旨。書き換えなかった項目があれば、その名前も出す。
fn saved_notice(path: &Path, overridden: &EnvOverridden, display_mode_deferred: bool) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::{NO_CONFIG_PATH, Session};
    use crate::app::{App, AppEvent, Mode};
    use crate::cookies::{CookieSource, Target};
    use crate::display::{DisplayMode, FpsCap, Quality, WindowOptions};
    use crate::grid::{self, LayoutMode};
    use crate::input;
    use crate::query::QueryEditor;
    use crate::search::SearchResult;
    use crate::settings::{EnvOverridden, Settings};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::layout::Rect;
    use std::time::{Duration, Instant};
    use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// Ctrl を押しながらのキー。
    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
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

    /// 80x24 の検索結果画面。
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

    // ---- 項目と値 ----

    /// 1 項目だけ動かした結果。他の行に触れていないことも見られるよう丸ごと返す。
    fn adjusted(item: SettingsItem, delta: i32, settings: &Settings) -> Settings {
        let mut next = settings.clone();
        item.adjust(&mut next, delta);
        next
    }

    #[test]
    fn the_settings_screen_lists_every_editable_item_in_order() {
        let labels: Vec<&str> = SETTINGS_ITEMS.iter().map(|item| item.label()).collect();
        assert_eq!(
            labels,
            [
                "display.mode",
                "display.quality",
                "fps_cap",
                "subtitles.enabled",
                "window.ontop",
                "search.layout",
                "search.limit",
                "search.timeout_secs",
                "search.cache_enabled",
                "search.cache_ttl_secs",
                "thumbnails.enabled",
                "thumbnails.max_cached",
                "thumbnails.timeout_secs",
                "download.debug",
            ]
        );
    }

    #[test]
    fn each_settings_row_shows_the_value_the_config_file_holds() {
        // 行の値は設定ファイルの表記と揃える。画面で見た値をそのまま探せるため。
        assert_eq!(
            settings_rows(&App::default()),
            [
                "display.mode: embedded",
                "display.quality: medium",
                "fps_cap: 15",
                "subtitles.enabled: true",
                "window.ontop: false",
                "search.layout: grid",
                "search.limit: 10",
                "search.timeout_secs: 30",
                "search.cache_enabled: false",
                "search.cache_ttl_secs: 300",
                "thumbnails.enabled: true",
                "thumbnails.max_cached: 500",
                "thumbnails.timeout_secs: 10",
                "download.debug: false",
            ]
        );
    }

    #[test]
    fn the_download_debug_toggle_flips_either_way() {
        let mut settings = Settings::default();
        for delta in [-1, 1] {
            SettingsItem::DownloadDebug.adjust(&mut settings, delta);
            assert!(settings.download.debug, "{delta}");
            SettingsItem::DownloadDebug.adjust(&mut settings, delta);
            assert!(!settings.download.debug, "{delta}");
        }
        assert!(!SettingsItem::DownloadDebug.is_numeric());
        assert!(!SettingsItem::DownloadDebug.apply_numeric(&mut settings, "1"));
    }

    #[test]
    fn an_unlimited_fps_cap_is_shown_as_zero() {
        let mut app = App::default();
        app.settings.fps_cap = None;
        assert_eq!(settings_rows(&app)[2], "fps_cap: 0 (無制限)");
    }

    #[test]
    fn a_row_the_environment_holds_says_that_it_is_not_saved() {
        // 環境変数が効いている間は保存でファイルへ書かないので、画面で断っておく。
        let app = App {
            env_overridden: EnvOverridden {
                fps_cap: Some(FpsCap::new(30)),
                ..EnvOverridden::default()
            },
            ..App::default()
        };
        let row = &settings_rows(&app)[2];
        assert!(row.starts_with("fps_cap: "), "{row}");
        assert!(row.contains(FPS_LIMIT_VAR), "{row}");
        assert!(row.contains("保存しません"), "{row}");

        // 上書きされていない行には何も足さない。
        assert_eq!(settings_rows(&app)[6], "search.limit: 10");
        assert_eq!(settings_rows(&App::default())[2], "fps_cap: 15");
    }

    #[test]
    fn the_choice_rows_send_forward_on_the_right_and_back_on_the_left() {
        let settings = Settings::default();
        assert_eq!(
            adjusted(SettingsItem::DisplayMode, 1, &settings)
                .display
                .mode,
            DisplayMode::default().next()
        );
        assert_eq!(
            adjusted(SettingsItem::DisplayMode, -1, &settings)
                .display
                .mode,
            DisplayMode::default().prev()
        );
        assert_eq!(
            adjusted(SettingsItem::DisplayQuality, 1, &settings)
                .display
                .quality,
            Quality::default().next()
        );
        assert_eq!(
            adjusted(SettingsItem::DisplayQuality, -1, &settings)
                .display
                .quality,
            Quality::default().prev()
        );
        assert_eq!(
            adjusted(SettingsItem::SearchLayout, 1, &settings)
                .search
                .layout,
            LayoutMode::default().next()
        );
        assert_eq!(
            adjusted(SettingsItem::SearchLayout, -1, &settings)
                .search
                .layout,
            LayoutMode::default().prev()
        );
    }

    #[test]
    fn a_choice_row_comes_back_to_where_it_started() {
        // ← が「戻る」でないと、4 値ある quality は選び直しに 3 回かかる。
        let settings = Settings::default();
        let mut next = settings.clone();
        SettingsItem::DisplayQuality.adjust(&mut next, 1);
        SettingsItem::DisplayQuality.adjust(&mut next, -1);
        assert_eq!(next.display.quality, settings.display.quality);

        let mut back = settings.clone();
        SettingsItem::DisplayQuality.adjust(&mut back, -1);
        assert_eq!(back.display.quality, Quality::Low, "medium の 1 つ前");
    }

    #[test]
    fn the_flag_rows_toggle_whichever_way_they_are_pushed() {
        let mut settings = Settings::default();
        for delta in [-1, 1] {
            SettingsItem::SubtitlesEnabled.adjust(&mut settings, delta);
            assert!(!settings.subtitles.enabled, "{delta}");
            SettingsItem::SubtitlesEnabled.adjust(&mut settings, delta);
            assert!(settings.subtitles.enabled, "{delta}");

            SettingsItem::ThumbnailsEnabled.adjust(&mut settings, delta);
            assert!(!settings.thumbnails.enabled, "{delta}");
            SettingsItem::ThumbnailsEnabled.adjust(&mut settings, delta);
            assert!(settings.thumbnails.enabled, "{delta}");

            SettingsItem::WindowOntop.adjust(&mut settings, delta);
            assert!(settings.window.ontop, "{delta}");
            SettingsItem::WindowOntop.adjust(&mut settings, delta);
            assert!(!settings.window.ontop, "{delta}");
        }
    }

    #[test]
    fn the_ontop_row_changes_only_the_window_section() {
        let settings = Settings::default();
        let next = adjusted(SettingsItem::WindowOntop, 1, &settings);
        assert!(next.window.ontop);
        assert_eq!(
            next,
            Settings {
                window: WindowOptions {
                    ontop: true,
                    ..settings.window.clone()
                },
                ..settings
            }
        );
    }

    #[test]
    fn the_fps_cap_steps_by_five_and_stops_at_both_ends() {
        let mut settings = Settings::default();
        SettingsItem::FpsCap.adjust(&mut settings, 1);
        assert_eq!(settings.fps_cap.map(FpsCap::get), Some(20));
        SettingsItem::FpsCap.adjust(&mut settings, -1);
        assert_eq!(settings.fps_cap.map(FpsCap::get), Some(15));

        // 0 まで下げると制限なし。そこから下は動かない (ラップしない)。
        for _ in 0..10 {
            SettingsItem::FpsCap.adjust(&mut settings, -1);
        }
        assert_eq!(settings.fps_cap, None);

        for _ in 0..40 {
            SettingsItem::FpsCap.adjust(&mut settings, 1);
        }
        assert_eq!(settings.fps_cap.map(FpsCap::get), Some(MAX_FPS_CAP));
    }

    #[test]
    fn the_search_limit_steps_by_one_and_stays_inside_its_range() {
        let mut settings = Settings::default();
        SettingsItem::SearchLimit.adjust(&mut settings, 1);
        assert_eq!(settings.search.limit, 11);

        for _ in 0..20 {
            SettingsItem::SearchLimit.adjust(&mut settings, -1);
        }
        assert_eq!(settings.search.limit, MIN_SEARCH_LIMIT);

        for _ in 0..(MAX_SEARCH_LIMIT + 10) {
            SettingsItem::SearchLimit.adjust(&mut settings, 1);
        }
        assert_eq!(settings.search.limit, MAX_SEARCH_LIMIT);
    }

    #[test]
    fn the_search_timeout_steps_by_five_and_stays_inside_its_range() {
        let mut settings = Settings::default();
        let secs = |settings: &Settings| settings.search.timeout.as_secs();
        assert_eq!(secs(&settings), 30, "既定は 30 秒のまま");

        SettingsItem::SearchTimeoutSecs.adjust(&mut settings, 1);
        assert_eq!(secs(&settings), 35);
        SettingsItem::SearchTimeoutSecs.adjust(&mut settings, -1);
        assert_eq!(secs(&settings), 30);

        for _ in 0..20 {
            SettingsItem::SearchTimeoutSecs.adjust(&mut settings, -1);
        }
        assert_eq!(secs(&settings), MIN_SEARCH_TIMEOUT_SECS, "下限で止まる");

        for _ in 0..100 {
            SettingsItem::SearchTimeoutSecs.adjust(&mut settings, 1);
        }
        assert_eq!(secs(&settings), MAX_SEARCH_TIMEOUT_SECS, "上限で止まる");
    }

    #[test]
    fn the_search_timeout_row_shows_the_seconds() {
        let settings = Settings::default();
        assert_eq!(
            SettingsItem::SearchTimeoutSecs.row(&settings),
            "search.timeout_secs: 30"
        );
    }

    #[test]
    fn the_search_cache_toggle_flips_either_way() {
        let mut settings = Settings::default();
        assert!(!settings.search.cache_enabled, "既定は off のまま");
        for delta in [1, -1] {
            SettingsItem::SearchCacheEnabled.adjust(&mut settings, delta);
            assert!(settings.search.cache_enabled);
            SettingsItem::SearchCacheEnabled.adjust(&mut settings, delta);
            assert!(!settings.search.cache_enabled);
        }
    }

    #[test]
    fn the_search_cache_ttl_steps_by_five_and_stays_inside_its_range() {
        let mut settings = Settings::default();
        let secs = |settings: &Settings| settings.search.cache_ttl.as_secs();
        assert_eq!(secs(&settings), 300, "既定は 300 秒のまま");

        SettingsItem::SearchCacheTtlSecs.adjust(&mut settings, 1);
        assert_eq!(secs(&settings), 305);
        SettingsItem::SearchCacheTtlSecs.adjust(&mut settings, -1);
        assert_eq!(secs(&settings), 300);

        for _ in 0..200 {
            SettingsItem::SearchCacheTtlSecs.adjust(&mut settings, -1);
        }
        assert_eq!(secs(&settings), MIN_SEARCH_CACHE_TTL_SECS, "下限で止まる");

        for _ in 0..1000 {
            SettingsItem::SearchCacheTtlSecs.adjust(&mut settings, 1);
        }
        assert_eq!(secs(&settings), MAX_SEARCH_CACHE_TTL_SECS, "上限で止まる");
    }

    #[test]
    fn the_search_cache_ttl_takes_typed_numbers_and_the_toggle_does_not() {
        let mut settings = Settings::default();
        assert!(SettingsItem::SearchCacheTtlSecs.is_numeric());
        assert!(!SettingsItem::SearchCacheEnabled.is_numeric());
        assert_eq!(
            SettingsItem::SearchCacheTtlSecs.max_digits(),
            MAX_SEARCH_CACHE_TTL_SECS.to_string().len()
        );

        assert!(SettingsItem::SearchCacheTtlSecs.apply_numeric(&mut settings, "45"));
        assert_eq!(settings.search.cache_ttl, Duration::from_secs(45));
        assert!(SettingsItem::SearchCacheTtlSecs.apply_numeric(&mut settings, "1"));
        assert_eq!(
            settings.search.cache_ttl,
            Duration::from_secs(MIN_SEARCH_CACHE_TTL_SECS)
        );
        assert!(SettingsItem::SearchCacheTtlSecs.apply_numeric(&mut settings, "99999"));
        assert_eq!(
            settings.search.cache_ttl,
            Duration::from_secs(MAX_SEARCH_CACHE_TTL_SECS)
        );
        assert!(!SettingsItem::SearchCacheEnabled.apply_numeric(&mut settings, "1"));
    }

    #[test]
    fn the_search_cache_rows_show_the_config_keys() {
        let settings = Settings::default();
        assert_eq!(
            SettingsItem::SearchCacheEnabled.row(&settings),
            "search.cache_enabled: false"
        );
        assert_eq!(
            SettingsItem::SearchCacheTtlSecs.row(&settings),
            "search.cache_ttl_secs: 300"
        );
        assert!(SETTINGS_ITEMS.contains(&SettingsItem::SearchCacheEnabled));
        assert!(SETTINGS_ITEMS.contains(&SettingsItem::SearchCacheTtlSecs));
    }

    #[test]
    fn the_thumbnail_numbers_step_by_their_own_width() {
        let mut settings = Settings::default();
        SettingsItem::ThumbnailsMaxCached.adjust(&mut settings, 1);
        assert_eq!(settings.thumbnails.max_cached, 550);
        for _ in 0..20 {
            SettingsItem::ThumbnailsMaxCached.adjust(&mut settings, -1);
        }
        assert_eq!(settings.thumbnails.max_cached, 0, "0 枚で止まる");

        SettingsItem::ThumbnailsTimeoutSecs.adjust(&mut settings, 1);
        assert_eq!(settings.thumbnails.timeout, Duration::from_secs(15));
        for _ in 0..10 {
            SettingsItem::ThumbnailsTimeoutSecs.adjust(&mut settings, -1);
        }
        assert_eq!(
            settings.thumbnails.timeout,
            Duration::from_secs(1),
            "0 秒では 1 枚も取れない"
        );
        for _ in 0..40 {
            SettingsItem::ThumbnailsTimeoutSecs.adjust(&mut settings, 1);
        }
        assert_eq!(
            settings.thumbnails.timeout,
            Duration::from_secs(MAX_THUMB_TIMEOUT_SECS)
        );
    }

    #[test]
    fn the_timeout_keeps_its_step_after_hitting_the_lower_bound() {
        // 下限 1 は刻みに乗っていない。ここで基準がずれると既定の 10 へ戻せなくなる。
        let mut settings = Settings::default();
        let secs = |settings: &Settings| settings.thumbnails.timeout.as_secs();

        SettingsItem::ThumbnailsTimeoutSecs.adjust(&mut settings, -1);
        assert_eq!(secs(&settings), 5);
        SettingsItem::ThumbnailsTimeoutSecs.adjust(&mut settings, -1);
        assert_eq!(secs(&settings), 1, "0 秒では取れないので 1 で止まる");

        SettingsItem::ThumbnailsTimeoutSecs.adjust(&mut settings, 1);
        assert_eq!(secs(&settings), 5, "刻みの目盛りへ戻る");
        SettingsItem::ThumbnailsTimeoutSecs.adjust(&mut settings, 1);
        assert_eq!(secs(&settings), 10, "既定値に戻せる");
    }

    #[test]
    fn a_value_off_the_step_is_pulled_back_onto_it() {
        // 設定ファイルに手で書いた端数から始めても、目盛りの上を動く。
        let mut settings = Settings::default();
        settings.thumbnails.timeout = Duration::from_secs(12);
        SettingsItem::ThumbnailsTimeoutSecs.adjust(&mut settings, -1);
        assert_eq!(settings.thumbnails.timeout, Duration::from_secs(10));

        settings.thumbnails.timeout = Duration::from_secs(12);
        SettingsItem::ThumbnailsTimeoutSecs.adjust(&mut settings, 1);
        assert_eq!(settings.thumbnails.timeout, Duration::from_secs(15));
    }

    #[test]
    fn an_out_of_range_selection_still_points_at_a_row() {
        let mut app = App::default();
        assert_eq!(app.settings_screen.selected, 0);
        assert_eq!(app.settings_screen.item(), SETTINGS_ITEMS[0]);

        app.settings_screen.selected = 99;
        assert_eq!(
            app.settings_screen.item(),
            SETTINGS_ITEMS[SETTINGS_ITEMS.len() - 1]
        );
    }

    #[test]
    fn only_the_number_rows_take_a_typed_value() {
        let numeric: Vec<&str> = SETTINGS_ITEMS
            .iter()
            .filter(|item| item.is_numeric())
            .map(|item| item.label())
            .collect();
        assert_eq!(
            numeric,
            [
                "fps_cap",
                "search.limit",
                "search.timeout_secs",
                "search.cache_ttl_secs",
                "thumbnails.max_cached",
                "thumbnails.timeout_secs",
            ]
        );
    }

    #[test]
    fn every_number_row_actually_takes_the_value_it_advertises() {
        // is_numeric() に項目を足して apply_numeric() の腕を足し忘れると、
        // 数字は打てるのに Enter が効かない行ができる。
        let settings = Settings::default();
        for item in SETTINGS_ITEMS {
            let mut next = settings.clone();
            assert_eq!(
                item.apply_numeric(&mut next, "1"),
                item.is_numeric(),
                "{}",
                item.label()
            );
            assert_eq!(item.max_digits() > 0, item.is_numeric(), "{}", item.label());
        }
    }

    #[test]
    fn the_digit_limit_still_lets_an_out_of_range_number_be_typed() {
        // 端へ寄せる入力 (search.limit の 9999 等) は打てる長さに収める。
        assert_eq!(SettingsItem::FpsCap.max_digits(), 3);
        assert_eq!(SettingsItem::SearchLimit.max_digits(), 4);
        assert_eq!(SettingsItem::SearchTimeoutSecs.max_digits(), 3);
        assert_eq!(SettingsItem::ThumbnailsTimeoutSecs.max_digits(), 3);
        assert_eq!(SettingsItem::ThumbnailsMaxCached.max_digits(), 20);
    }

    /// 打ち込んだ値を 1 項目だけ確定した結果。
    fn typed(item: SettingsItem, raw: &str, settings: &Settings) -> Settings {
        let mut next = settings.clone();
        item.apply_numeric(&mut next, raw);
        next
    }

    #[test]
    fn a_typed_number_lands_inside_the_range_the_arrows_use() {
        let settings = Settings::default();
        let limit = |raw| {
            typed(SettingsItem::SearchLimit, raw, &settings)
                .search
                .limit
        };
        assert_eq!(limit("250"), 250);
        // 範囲の外は ←→ と同じ端で止める。
        assert_eq!(limit("0"), MIN_SEARCH_LIMIT);
        assert_eq!(limit("5000"), MAX_SEARCH_LIMIT);

        let fps = |raw| typed(SettingsItem::FpsCap, raw, &settings).fps_cap;
        assert_eq!(fps("24"), FpsCap::new(24));
        assert_eq!(fps("999"), FpsCap::new(MAX_FPS_CAP));
        // 0 は「制限なし」。
        assert_eq!(fps("0"), None);

        let search_timeout = |raw| {
            typed(SettingsItem::SearchTimeoutSecs, raw, &settings)
                .search
                .timeout
        };
        assert_eq!(search_timeout("120"), Duration::from_secs(120));
        assert_eq!(
            search_timeout("0"),
            Duration::from_secs(MIN_SEARCH_TIMEOUT_SECS)
        );
        assert_eq!(
            search_timeout("9999"),
            Duration::from_secs(MAX_SEARCH_TIMEOUT_SECS)
        );

        let cached = |raw| {
            typed(SettingsItem::ThumbnailsMaxCached, raw, &settings)
                .thumbnails
                .max_cached
        };
        assert_eq!(cached("1200"), 1200);
        assert_eq!(cached("0"), 0);

        let timeout = |raw| {
            typed(SettingsItem::ThumbnailsTimeoutSecs, raw, &settings)
                .thumbnails
                .timeout
        };
        assert_eq!(timeout("30"), Duration::from_secs(30));
        assert_eq!(timeout("0"), Duration::from_secs(MIN_THUMB_TIMEOUT_SECS));
        assert_eq!(timeout("999"), Duration::from_secs(MAX_THUMB_TIMEOUT_SECS));
    }

    #[test]
    fn a_typed_value_that_is_not_a_number_leaves_the_setting_alone() {
        let settings = Settings::default();
        // 空入力・符号・小数・桁あふれは読めないので捨てる。
        for raw in ["", " ", "abc", "-5", "1.5", "12 ", "99999999999999999999"] {
            let mut next = settings.clone();
            assert!(
                !SettingsItem::SearchLimit.apply_numeric(&mut next, raw),
                "{raw:?} は受け付けない"
            );
            assert_eq!(next, settings, "{raw:?}");
        }
    }

    #[test]
    fn typing_a_number_into_a_choice_row_changes_nothing() {
        let settings = Settings::default();
        for item in SETTINGS_ITEMS.iter().filter(|item| !item.is_numeric()) {
            let mut next = settings.clone();
            assert!(!item.apply_numeric(&mut next, "1"), "{}", item.label());
            assert_eq!(next, settings, "{}", item.label());
        }
    }

    #[test]
    fn the_settings_screen_starts_without_a_typed_value() {
        assert_eq!(App::default().settings_screen.edit, None);
    }

    #[test]
    fn the_settings_status_line_shows_the_position_and_keeps_the_notice() {
        let t0 = Instant::now();
        let mut app = App {
            mode: Mode::Settings,
            ..App::default()
        };
        assert_eq!(
            app.status_line(),
            format!("設定 1/{}", SETTINGS_ITEMS.len())
        );

        app.settings_screen.selected = 2;
        assert!(
            app.status_line().starts_with("設定 3/"),
            "{}",
            app.status_line()
        );

        app.set_temporary_notice("保存しました".to_string(), t0);
        assert!(app.status_line().ends_with("保存しました"));

        // 保存に失敗した理由はエラーとして出す。
        app.set_temporary_error("保存できません".to_string(), t0);
        assert_eq!(app.status_line(), "エラー: 保存できません");
    }

    // ---- キー ----

    #[test]
    fn the_settings_keys_map_to_their_actions() {
        assert_eq!(settings_action(KeyCode::Up), Some(SettingsAction::Move(-1)));
        assert_eq!(
            settings_action(KeyCode::Down),
            Some(SettingsAction::Move(1))
        );
        assert_eq!(
            settings_action(KeyCode::Left),
            Some(SettingsAction::Adjust(-1))
        );
        assert_eq!(
            settings_action(KeyCode::Right),
            Some(SettingsAction::Adjust(1))
        );
        assert_eq!(
            settings_action(KeyCode::Enter),
            Some(SettingsAction::Adjust(1))
        );
        assert_eq!(
            settings_action(KeyCode::Char(' ')),
            Some(SettingsAction::Adjust(1))
        );
        assert_eq!(
            settings_action(KeyCode::Char('s')),
            Some(SettingsAction::Save)
        );
        assert_eq!(settings_action(KeyCode::Esc), Some(SettingsAction::Close));
        assert_eq!(
            settings_action(KeyCode::Char('q')),
            Some(SettingsAction::Close)
        );
        // 開くのに使う S では保存しない。
        assert_eq!(settings_action(KeyCode::Char('S')), None);
        assert_eq!(settings_action(KeyCode::Tab), None);
        assert_eq!(settings_action(KeyCode::Char('x')), None);
    }

    #[tokio::test]
    async fn the_settings_keys_move_the_selection_and_change_the_value() {
        let (tx, _rx) = channel();
        let mut session = Session::default();
        let mut app = App::default();
        input::handle_key(&mut app, ctrl(KeyCode::Char('s')), &tx, &mut session).await;

        input::handle_key(&mut app, key(KeyCode::Down), &tx, &mut session).await;
        assert_eq!(app.settings_screen.selected, 1);
        input::handle_key(&mut app, key(KeyCode::Right), &tx, &mut session).await;
        assert_eq!(
            app.settings.display.quality,
            crate::display::Quality::default().next()
        );
        // ← は前の値へ戻す。
        input::handle_key(&mut app, key(KeyCode::Left), &tx, &mut session).await;
        assert_eq!(
            app.settings.display.quality,
            crate::display::Quality::default()
        );

        input::handle_key(&mut app, key(KeyCode::Up), &tx, &mut session).await;
        input::handle_key(&mut app, key(KeyCode::Enter), &tx, &mut session).await;
        assert_eq!(app.settings.display.mode, DisplayMode::default().next());

        // 設定画面では文字は検索語にならない。
        input::handle_key(&mut app, key(KeyCode::Char('x')), &tx, &mut session).await;
        assert!(app.query.text().is_empty());

        input::handle_key(&mut app, key(KeyCode::Esc), &tx, &mut session).await;
        assert_eq!(app.mode, Mode::Input, "保存せず閉じる");
        assert_eq!(
            app.settings,
            crate::settings::Settings::default(),
            "閉じたら編集前へ戻す"
        );
    }

    #[test]
    fn the_typing_keys_map_to_their_actions() {
        for c in ['0', '5', '9'] {
            assert_eq!(
                settings_edit_action(key(KeyCode::Char(c))),
                Some(EditAction::Push(c))
            );
        }
        assert_eq!(
            settings_edit_action(key(KeyCode::Backspace)),
            Some(EditAction::Backspace)
        );
        assert_eq!(
            settings_edit_action(key(KeyCode::Enter)),
            Some(EditAction::Commit)
        );
        assert_eq!(
            settings_edit_action(key(KeyCode::Esc)),
            Some(EditAction::Cancel)
        );
        // 打ち込んでいる間は移動も保存も終了も効かない。抜けるのは Enter か Esc。
        for code in [
            KeyCode::Up,
            KeyCode::Down,
            KeyCode::Left,
            KeyCode::Right,
            KeyCode::Char(' '),
            KeyCode::Char('s'),
            KeyCode::Char('q'),
            KeyCode::Tab,
        ] {
            assert_eq!(settings_edit_action(key(code)), None, "{code:?}");
        }
        // 修飾キー付きの数字は数字として扱わない。
        for modifiers in [
            KeyModifiers::CONTROL,
            KeyModifiers::ALT,
            KeyModifiers::SUPER,
        ] {
            let event = KeyEvent::new(KeyCode::Char('3'), modifiers);
            assert_eq!(settings_edit_action(event), None, "{modifiers:?}");
        }
    }

    /// 設定画面を開き、上から index 番目の行を選んだところまで進める。
    async fn settings_at(
        index: usize,
        tx: &UnboundedSender<AppEvent>,
        session: &mut Session,
    ) -> App {
        let mut app = App::default();
        input::handle_key(&mut app, ctrl(KeyCode::Char('s')), tx, session).await;
        for _ in 0..index {
            input::handle_key(&mut app, key(KeyCode::Down), tx, session).await;
        }
        assert_eq!(app.settings_screen.selected, index);
        app
    }

    /// 数字を順に打ち込む。
    async fn type_digits(
        app: &mut App,
        digits: &str,
        tx: &UnboundedSender<AppEvent>,
        session: &mut Session,
    ) {
        for c in digits.chars() {
            input::handle_key(app, key(KeyCode::Char(c)), tx, session).await;
        }
    }

    #[tokio::test]
    async fn a_digit_on_a_number_row_types_the_value_and_enter_writes_it() {
        let (tx, _rx) = channel();
        let mut session = Session::default();
        let mut app = settings_at(2, &tx, &mut session).await;
        assert_eq!(app.settings_screen.item().label(), "fps_cap");

        type_digits(&mut app, "30", &tx, &mut session).await;
        assert_eq!(app.settings_screen.edit.as_deref(), Some("30"));
        assert_eq!(
            app.settings.fps_cap,
            crate::settings::Settings::default().fps_cap,
            "確定するまで設定は変えない"
        );

        input::handle_key(&mut app, key(KeyCode::Enter), &tx, &mut session).await;
        assert_eq!(app.settings.fps_cap, crate::display::FpsCap::new(30));
        assert_eq!(app.settings_screen.edit, None);
        assert_eq!(app.mode, Mode::Settings, "確定しても画面は閉じない");
    }

    #[tokio::test]
    async fn a_digit_on_a_choice_row_is_ignored() {
        let (tx, _rx) = channel();
        let mut session = Session::default();
        let mut app = settings_at(0, &tx, &mut session).await;
        assert_eq!(app.settings_screen.item().label(), "display.mode");

        type_digits(&mut app, "3", &tx, &mut session).await;

        assert_eq!(app.settings_screen.edit, None);
        assert_eq!(app.settings, crate::settings::Settings::default());
    }

    #[tokio::test]
    async fn backspace_takes_back_a_digit_and_an_empty_enter_changes_nothing() {
        let (tx, _rx) = channel();
        let mut session = Session::default();
        let mut app = settings_at(6, &tx, &mut session).await;
        assert_eq!(app.settings_screen.item().label(), "search.limit");

        type_digits(&mut app, "12", &tx, &mut session).await;
        input::handle_key(&mut app, key(KeyCode::Backspace), &tx, &mut session).await;
        assert_eq!(app.settings_screen.edit.as_deref(), Some("1"));

        // 空になっても打ち込み中のまま。消しすぎても画面は変わらない。
        for _ in 0..2 {
            input::handle_key(&mut app, key(KeyCode::Backspace), &tx, &mut session).await;
            assert_eq!(app.settings_screen.edit.as_deref(), Some(""));
        }

        input::handle_key(&mut app, key(KeyCode::Enter), &tx, &mut session).await;
        assert_eq!(app.settings_screen.edit, None);
        assert_eq!(
            app.settings.search.limit,
            crate::settings::Settings::default().search.limit
        );
    }

    #[tokio::test]
    async fn a_number_beyond_the_range_is_pulled_back_to_the_edge() {
        let (tx, _rx) = channel();
        let mut session = Session::default();
        let mut app = settings_at(6, &tx, &mut session).await;

        type_digits(&mut app, "9999", &tx, &mut session).await;
        input::handle_key(&mut app, key(KeyCode::Enter), &tx, &mut session).await;

        assert_eq!(app.settings.search.limit, crate::settings::MAX_SEARCH_LIMIT);
    }

    #[tokio::test]
    async fn escape_while_typing_only_drops_what_was_typed() {
        let (tx, _rx) = channel();
        let mut session = Session::default();
        let mut app = settings_at(2, &tx, &mut session).await;
        type_digits(&mut app, "90", &tx, &mut session).await;

        input::handle_key(&mut app, key(KeyCode::Esc), &tx, &mut session).await;
        assert_eq!(app.settings_screen.edit, None);
        assert_eq!(app.mode, Mode::Settings, "1 回目は画面を閉じない");
        assert_eq!(app.settings, crate::settings::Settings::default());

        // 打ち込みを抜けた後の Esc は今までどおり設定画面を閉じる。
        input::handle_key(&mut app, key(KeyCode::Esc), &tx, &mut session).await;
        assert_eq!(app.mode, Mode::Input);
    }

    #[tokio::test]
    async fn the_other_keys_do_nothing_while_a_number_is_being_typed() {
        let (tx, _rx) = channel();
        let mut session = Session::default();
        let mut app = settings_at(6, &tx, &mut session).await;
        type_digits(&mut app, "1", &tx, &mut session).await;

        for code in [
            KeyCode::Up,
            KeyCode::Down,
            KeyCode::Left,
            KeyCode::Right,
            KeyCode::Char(' '),
            KeyCode::Char('q'),
            KeyCode::Tab,
        ] {
            input::handle_key(&mut app, key(code), &tx, &mut session).await;
        }

        assert_eq!(app.settings_screen.selected, 6, "行は動かさない");
        assert_eq!(app.settings_screen.edit.as_deref(), Some("1"));
        assert_eq!(app.settings, crate::settings::Settings::default());
        assert_eq!(app.mode, Mode::Settings);
        assert!(!app.should_quit);
    }

    #[tokio::test]
    async fn a_digit_with_a_modifier_does_not_start_or_feed_the_typing() {
        let (tx, _rx) = channel();
        let mut session = Session::default();
        let mut app = settings_at(6, &tx, &mut session).await;

        for modifiers in [KeyModifiers::ALT, KeyModifiers::SUPER] {
            let event = KeyEvent::new(KeyCode::Char('3'), modifiers);
            input::handle_key(&mut app, event, &tx, &mut session).await;
            assert_eq!(
                app.settings_screen.edit, None,
                "{modifiers:?} で打ち込みが始まった"
            );
        }
        // Ctrl+3 も同じ。Ctrl+C だけが最上位で終了に割り当ててある。
        input::handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('3'), KeyModifiers::CONTROL),
            &tx,
            &mut session,
        )
        .await;
        assert_eq!(app.settings_screen.edit, None);

        // 打ち込み中に来ても桁は増えない。
        type_digits(&mut app, "1", &tx, &mut session).await;
        input::handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('3'), KeyModifiers::CONTROL),
            &tx,
            &mut session,
        )
        .await;
        assert_eq!(app.settings_screen.edit.as_deref(), Some("1"));
    }

    #[tokio::test]
    async fn the_typing_stops_at_the_digits_the_value_can_take() {
        let (tx, _rx) = channel();
        let mut session = Session::default();
        let mut app = settings_at(6, &tx, &mut session).await;
        assert_eq!(app.settings_screen.item().label(), "search.limit");

        // 上限 1000 の 4 桁まで。キーリピートで伸び続けると Enter が効かなくなる。
        type_digits(&mut app, "999999", &tx, &mut session).await;
        assert_eq!(app.settings_screen.edit.as_deref(), Some("9999"));

        input::handle_key(&mut app, key(KeyCode::Enter), &tx, &mut session).await;
        assert_eq!(app.settings.search.limit, crate::settings::MAX_SEARCH_LIMIT);
    }

    #[tokio::test]
    async fn opening_and_closing_the_settings_drops_a_half_typed_number() {
        let (tx, _rx) = channel();
        let mut session = Session::default();
        let mut app = settings_at(6, &tx, &mut session).await;
        type_digits(&mut app, "12", &tx, &mut session).await;

        // 打ち込み中のまま画面を離れても、次に開いたときは通常の操作から始める。
        close_settings(&mut app);
        assert_eq!(app.settings_screen.edit, None);

        app.settings_screen.edit = Some("34".to_string());
        open_settings(&mut app, &mut session);
        assert_eq!(app.settings_screen.edit, None);
        assert_eq!(app.settings, crate::settings::Settings::default());
    }

    #[tokio::test]
    async fn q_closes_the_settings_instead_of_quitting_the_app() {
        let (tx, _rx) = channel();
        let mut session = Session::default();
        let mut app = App {
            mode: Mode::Results,
            results: vec![result("a")],
            ..App::default()
        };
        input::handle_key(&mut app, key(KeyCode::Char('S')), &tx, &mut session).await;
        input::handle_key(&mut app, key(KeyCode::Char('q')), &tx, &mut session).await;

        assert!(!app.should_quit);
        assert_eq!(app.mode, Mode::Results);
    }

    // ---- 描画 ----

    #[test]
    fn settings_rows_do_not_overlap_and_cover_the_screen() {
        let area = Rect::new(0, 0, 80, 24);
        let areas = settings_areas(area);
        assert_eq!(areas[0], Rect::new(0, 0, 80, 1), "タイトルは 1 行");
        assert_eq!(areas[1], Rect::new(0, 1, 80, 21), "項目に残り全部");
        assert_eq!(areas[2], Rect::new(0, 22, 80, 1));
        assert_eq!(areas[3], Rect::new(0, 23, 80, 1));
        for pair in areas.windows(2) {
            assert_eq!(pair[0].bottom(), pair[1].y, "{pair:?}");
            assert_eq!(pair[0].width, area.width);
        }
    }

    #[test]
    fn settings_areas_survive_a_terminal_too_short_for_every_row() {
        for height in 0..6 {
            let area = Rect::new(0, 0, 40, height);
            for rect in settings_areas(area) {
                assert!(rect.bottom() <= area.bottom(), "{rect:?} / {area:?}");
            }
        }
    }

    #[test]
    fn settings_help_lists_the_keys_inside_80_columns() {
        let help = ui::fit_hints(&settings_hints(), 80);
        for key in [
            "↑↓:選択",
            "←→:値変更",
            "Enter/Space:切替",
            "s:保存",
            // 閉じるだけでなく編集を捨てることが分かる文言にする。
            "Esc:破棄して戻る",
        ] {
            assert!(help.contains(key), "{key} が落ちた: {help}");
        }
        assert!(grid::display_width(&help) <= 80, "{help}");
        // 狭い端末では途中で切らず丸ごと落とす。
        assert_eq!(ui::fit_hints(&settings_hints(), 0), "");
    }

    #[test]
    fn a_narrow_settings_help_drops_the_direct_input_before_the_way_out() {
        // 直接入力を足す前の 5 つは 58 桁に収まる。足りないぶんは末尾から落とす。
        let narrow = ui::fit_hints(&settings_hints(), 58);
        for key in ["↑↓:選択", "←→:値変更", "Enter/Space:切替", "s:保存"] {
            assert!(narrow.contains(key), "{key} が落ちた: {narrow}");
        }
        assert!(narrow.contains("Esc:破棄して戻る"), "{narrow}");
        assert!(!narrow.contains("0-9"), "{narrow}");
        assert!(grid::display_width(&narrow) <= 58, "{narrow}");
    }

    #[test]
    fn the_settings_screen_draws_every_row_and_marks_the_selection() {
        let mut app = App {
            mode: Mode::Settings,
            ..App::default()
        };
        app.settings_screen.selected = 2;
        let screen = rendered(&app, 80, 24);

        for row in settings_rows(&app) {
            assert!(screen.contains(&row), "{row} がない:\n{screen}");
        }
        assert!(
            screen.contains("> fps_cap"),
            "選択中の行に印が出る:\n{screen}"
        );
        assert!(screen.contains("設定"), "タイトルが出る:\n{screen}");
        assert!(screen.contains("s:保存"), "ヘルプが出る:\n{screen}");
    }

    /// search.limit の行を打ち込み中にした設定画面。
    fn typing_app(raw: &str) -> App {
        let mut app = App {
            mode: Mode::Settings,
            ..App::default()
        };
        app.settings_screen.selected = 6;
        app.settings_screen.edit = Some(raw.to_string());
        app
    }

    #[test]
    fn the_row_being_typed_shows_the_digits_instead_of_the_stored_value() {
        let app = typing_app("12");
        let screen = rendered(&app, 80, 24);

        assert!(screen.contains("search.limit: 12"), "{screen}");
        assert!(!screen.contains("search.limit: 10"), "{screen}");
        // 打ち込んでいない行はそのまま。
        assert!(screen.contains("fps_cap: 15"), "{screen}");
        assert!(screen.contains("Enter:確定"), "案内も切り替わる:\n{screen}");
    }

    #[test]
    fn the_row_being_typed_keeps_the_note_about_the_environment() {
        // 環境変数が効いている断りは、値を打ち込んでいる間も消さない。
        let mut app = App {
            mode: Mode::Settings,
            env_overridden: crate::settings::EnvOverridden {
                fps_cap: Some(crate::display::FpsCap::new(30)),
                ..crate::settings::EnvOverridden::default()
            },
            ..App::default()
        };
        app.settings_screen.selected = 2;
        app.settings_screen.edit = Some("24".to_string());
        let screen = rendered(&app, 120, 24);

        assert!(screen.contains("fps_cap: 24"), "{screen}");
        assert!(screen.contains(crate::settings::FPS_LIMIT_VAR), "{screen}");
        assert!(screen.contains("保存しません"), "{screen}");
    }

    #[test]
    fn the_title_stops_promising_the_save_and_close_keys_while_typing() {
        // 打ち込み中は s も Esc も画面を閉じないので、タイトルにも出さない。
        let app = App {
            mode: Mode::Settings,
            ..App::default()
        };
        assert!(rendered(&app, 80, 24).contains("Esc は編集を捨てて戻る"));

        let typing = rendered(&typing_app("12"), 80, 24);
        assert!(!typing.contains("Esc は編集を捨てて戻る"), "{typing}");
        assert!(typing.contains("打ち込み中"), "{typing}");
    }

    #[test]
    fn an_emptied_row_shows_no_value_while_it_is_being_typed() {
        let app = typing_app("");
        let screen = rendered(&app, 80, 24);

        assert!(screen.contains("search.limit:"), "{screen}");
        assert!(!screen.contains("search.limit: 10"), "{screen}");
    }

    #[test]
    fn the_cursor_follows_the_digits_on_the_row_being_typed() {
        // "> search.limit: 12" の後ろ。行は項目欄の 6 行目。
        assert_eq!(
            settings_cursor(Rect::new(0, 0, 80, 24), 5, "search.limit", "12"),
            (18, 6)
        );
        // 幅も高さも足りない端末でも画面の中に収める。
        let screen = Rect::new(0, 0, 20, 5);
        let (x, y) = settings_cursor(screen, 8, "thumbnails.max_cached", "123");
        assert!(x < screen.width, "{x}");
        assert!(y < screen.height, "{y}");
    }

    #[test]
    fn the_settings_help_changes_while_a_number_is_typed() {
        let app = App {
            mode: Mode::Settings,
            ..App::default()
        };
        let normal = settings_help(&app, 80);
        assert!(normal.contains("0-9:直接入力"), "{normal}");
        assert!(normal.contains("s:保存"), "{normal}");
        assert!(grid::display_width(&normal) <= 80, "{normal}");

        let typing = settings_help(&typing_app("12"), 80);
        for hint in ["0-9:入力", "BS:1字削除", "Enter:確定", "Esc:取消"] {
            assert!(typing.contains(hint), "{hint} がない: {typing}");
        }
        assert!(!typing.contains("s:保存"), "保存は効かない: {typing}");
        assert!(grid::display_width(&typing) <= 80, "{typing}");

        // 画面の最下行 (ui のフッタ) にも同じ文言が出る。
        for app in [app, typing_app("12")] {
            let screen = rendered(&app, 80, 24);
            let shown = screen.lines().last().unwrap_or_default().trim_end();
            assert_eq!(shown, settings_help(&app, 80));
        }
    }

    #[test]
    fn the_settings_screen_does_not_draw_the_search_boxes() {
        // 検索画面の上に重ねず、設定だけの画面にする。
        let app = App {
            mode: Mode::Settings,
            query: QueryEditor::from("ラーメン"),
            ..App::default()
        };
        let screen = rendered(&app, 80, 24);
        assert!(!screen.contains("ラーメン"), "{screen}");
        assert!(!screen.contains("結果"), "{screen}");
    }

    // ---- 開閉と保存 ----

    fn settings_temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tuitube-settings-screen-{}-{name}",
            std::process::id()
        ));
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
        assert_eq!(app.settings_screen.selected, SETTINGS_ITEMS.len() - 1);
        move_settings_selection(&mut app, 1);
        assert_eq!(app.settings_screen.selected, 0);
        move_settings_selection(&mut app, 1);
        assert_eq!(app.settings_screen.selected, 1);

        // 壊れた値で入ってきても一覧の中に戻す。
        app.settings_screen.selected = 99;
        move_settings_selection(&mut app, 1);
        assert!(app.settings_screen.selected < SETTINGS_ITEMS.len());
    }

    #[test]
    fn adjusting_changes_only_the_selected_row() {
        let mut app = App::default();
        app.settings_screen.selected = 2;
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
}
