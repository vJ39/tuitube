//! 設定ファイル (TOML) のパス決定・読み込み・検証・テンプレート生成・保存。

use crate::category::{Category, default_categories};
use crate::cookies::{self, CookieSource, Feed};
use crate::display::{DisplayMode, FocusOn, FpsCap, Quality, WindowOptions};
use crate::grid::LayoutMode;
use crate::subtitles::{SubLang, SubtitleSettings};
use crate::thumbs;
use serde::{Deserialize, Serialize};
use std::ffi::OsStr;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// fps 上限を一時的に上書きする環境変数。0 か unlimited で制限を外す。
pub const FPS_LIMIT_VAR: &str = "TUITUBE_FPS_LIMIT";
/// fps 上限の既定値。kitty 出力は 1 フレームごとに画素を CPU で作るため、
/// 制限しないと再生が重くなる (実測 CPU 122% → 40%)。
pub const DEFAULT_FPS_CAP: u32 = 15;
/// 受け付ける上限値。桁を打ち間違えた値をそのまま渡すと端末とパイプが詰まる。
pub const MAX_FPS_CAP: u32 = 120;
pub const MIN_FRAME_PIXELS: u32 = 64 * 36;
pub const MAX_FRAME_PIXELS_LIMIT: u32 = 3840 * 2160;
/// 1 回の検索で取る件数 (ytsearchN の N)。
pub const DEFAULT_SEARCH_LIMIT: usize = 10;
pub const MIN_SEARCH_LIMIT: usize = 1;
pub const MAX_SEARCH_LIMIT: usize = 1000;
/// yt-dlp 1 回ぶんを待つ上限秒数。既定は search::YT_DLP_TIMEOUT。
/// limit を大きくすると yt-dlp の所要時間が伸びるので、ここで延ばせるようにする。
pub const MIN_SEARCH_TIMEOUT_SECS: u64 = 5;
pub const MAX_SEARCH_TIMEOUT_SECS: u64 = 300;
/// 同じ検索の結果を使い回す時間。
pub const DEFAULT_SEARCH_CACHE_TTL_SECS: u64 = 300;
pub const MIN_SEARCH_CACHE_TTL_SECS: u64 = 10;
pub const MAX_SEARCH_CACHE_TTL_SECS: u64 = 3600;
/// 起動時にディスクキャッシュへ残す枚数。
pub const DEFAULT_MAX_CACHED: usize = 500;
/// サムネイル 1 枚あたりのダウンロード上限秒数。
pub const DEFAULT_THUMB_TIMEOUT_SECS: u64 = 10;
pub const MAX_THUMB_TIMEOUT_SECS: u64 = 120;
/// いいね済み/登録済みの控えを取り直すまでの間隔。既定は 1 週間。
pub const DEFAULT_ENGAGEMENT_TTL_SECS: u64 = 604_800;
pub const MIN_ENGAGEMENT_TTL_SECS: u64 = 60;
pub const MAX_ENGAGEMENT_TTL_SECS: u64 = 30 * 24 * 60 * 60;
/// 状態確認をいくつまで同時に投げるか。YouTube 側の割り当てを使い切らないよう控えめに。
pub const DEFAULT_ENGAGEMENT_CONCURRENCY: usize = 3;
pub const MIN_ENGAGEMENT_CONCURRENCY: usize = 1;
pub const MAX_ENGAGEMENT_CONCURRENCY: usize = 10;

const CONFIG_FILE: &str = "config.toml";
const APP_DIR: &str = "tuitube";

/// ファイルの生の形。全キー任意。未知のキーは無視する。
#[derive(Debug, Default, Clone, PartialEq, Deserialize, Serialize)]
pub struct RawConfig {
    pub display: Option<RawDisplay>,
    pub playback: Option<RawPlayback>,
    pub subtitles: Option<RawSubtitles>,
    pub window: Option<RawWindow>,
    pub mpv: Option<RawMpv>,
    pub cookies: Option<RawCookies>,
    pub search: Option<RawSearch>,
    pub thumbnails: Option<RawThumbnails>,
    pub engagement: Option<RawEngagement>,
    pub download: Option<RawDownload>,
    pub categories: Option<Vec<RawCategory>>,
}

#[derive(Debug, Default, Clone, PartialEq, Deserialize, Serialize)]
pub struct RawSearch {
    pub layout: Option<String>,
    pub limit: Option<i64>,
    pub timeout_secs: Option<i64>,
    pub cache_enabled: Option<bool>,
    pub cache_ttl_secs: Option<i64>,
}

#[derive(Debug, Default, Clone, PartialEq, Deserialize, Serialize)]
pub struct RawThumbnails {
    pub enabled: Option<bool>,
    pub cache_dir: Option<String>,
    pub max_cached: Option<i64>,
    pub timeout_secs: Option<i64>,
}

#[derive(Debug, Default, Clone, PartialEq, Deserialize, Serialize)]
pub struct RawEngagement {
    pub enabled: Option<bool>,
    pub ttl_secs: Option<i64>,
    pub max_concurrent_requests: Option<i64>,
}

/// ダウンロード画面 (`Mode::Download`) の保存先。dir は自由文字列のため設定画面 (v1) には出さない。
#[derive(Debug, Default, Clone, PartialEq, Deserialize, Serialize)]
pub struct RawDownload {
    pub dir: Option<String>,
    pub debug: Option<bool>,
}

#[derive(Debug, Default, Clone, PartialEq, Deserialize, Serialize)]
pub struct RawCategory {
    pub label: Option<String>,
    pub query: Option<String>,
}

/// 選択肢のキーは文字列で受ける。serde の enum で受けるとファイル全体が
/// デシリアライズエラーになり、無関係なキーまで既定値に倒れてしまう。
#[derive(Debug, Default, Clone, PartialEq, Deserialize, Serialize)]
pub struct RawDisplay {
    pub mode: Option<String>,
    pub quality: Option<String>,
    pub max_frame_pixels: Option<i64>,
}

#[derive(Debug, Default, Clone, PartialEq, Deserialize, Serialize)]
pub struct RawWindow {
    pub vo: Option<String>,
    pub autofit: Option<String>,
    pub geometry: Option<String>,
    #[serde(default)]
    pub ontop: bool,
    #[serde(default)]
    pub fullscreen: bool,
    pub focus_on: Option<String>,
    pub title: Option<String>,
}

#[derive(Debug, Default, Clone, PartialEq, Deserialize, Serialize)]
pub struct RawPlayback {
    /// 0 は制限なし。負値・120 超は丸めて notice を出す。
    pub fps_cap: Option<i64>,
}

/// 自動生成字幕の要求。lang は yt-dlp の sub-langs と mpv の --slang の両方へ渡る。
#[derive(Debug, Default, Clone, PartialEq, Deserialize, Serialize)]
pub struct RawSubtitles {
    pub enabled: Option<bool>,
    pub lang: Option<String>,
}

#[derive(Debug, Default, Clone, PartialEq, Deserialize, Serialize)]
pub struct RawMpv {
    pub extra_args: Option<Vec<String>>,
}

/// yt-dlp へ渡すブラウザ指定と cookies.txt のパス。cookie の値そのものは保存しない。
/// フィールドを足すと validate の網羅分解が止まるので、そこで是非を判断する。
#[derive(Debug, Default, Clone, PartialEq, Deserialize, Serialize)]
pub struct RawCookies {
    pub browser: Option<String>,
    pub file: Option<String>,
}

/// 環境変数による上書き。未設定は None。
#[derive(Debug, Default, Clone, Copy)]
pub struct EnvOverrides<'a> {
    pub fps_limit: Option<&'a str>,
    pub cookies: Option<&'a str>,
}

/// 環境変数が上書きした項目と、上書きされる前にファイルが持っていた値。
/// 保存時はこの値を書き戻して、一時的な上書きを設定ファイルへ残さない。
#[derive(Debug, Default, Clone, PartialEq)]
pub struct EnvOverridden {
    pub fps_cap: Option<Option<FpsCap>>,
    pub cookies: Option<Option<CookieSource>>,
}

impl EnvOverridden {
    pub fn is_empty(&self) -> bool {
        self.fps_cap.is_none() && self.cookies.is_none()
    }

    /// 上書き中の項目を設定ファイルのキー名で並べる。保存したときの知らせに使う。
    pub fn keys(&self) -> Vec<&'static str> {
        let mut keys = Vec::new();
        if self.fps_cap.is_some() {
            keys.push("fps_cap");
        }
        // 書き戻すのはファイルが持っていた値なので、キー名もその値に合わせる。
        if let Some(cookies) = &self.cookies {
            keys.push(match cookies {
                Some(CookieSource::File(_)) => "cookies.file",
                _ => "cookies.browser",
            });
        }
        keys
    }

    /// 保存用の設定。上書きされた項目だけファイル側の値へ戻す。
    pub fn restore(&self, settings: &Settings) -> Settings {
        let mut out = settings.clone();
        if let Some(fps_cap) = self.fps_cap {
            out.fps_cap = fps_cap;
        }
        if let Some(cookies) = self.cookies.clone() {
            out.cookies = cookies;
        }
        out
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DisplaySettings {
    pub mode: DisplayMode,
    pub quality: Quality,
    /// quality を無視してピクセル予算を直接指定する値。
    pub max_frame_pixels: Option<u32>,
}

impl DisplaySettings {
    pub fn max_pixels(&self) -> u32 {
        self.max_frame_pixels
            .unwrap_or_else(|| self.quality.max_pixels())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchSettings {
    pub layout: LayoutMode,
    pub limit: usize,
    pub timeout: Duration,
    /// 同じ検索を cache_ttl の間は取り直さない。
    pub cache_enabled: bool,
    pub cache_ttl: Duration,
}

impl Default for SearchSettings {
    fn default() -> Self {
        Self {
            layout: LayoutMode::default(),
            limit: DEFAULT_SEARCH_LIMIT,
            timeout: crate::search::YT_DLP_TIMEOUT,
            cache_enabled: false,
            cache_ttl: Duration::from_secs(DEFAULT_SEARCH_CACHE_TTL_SECS),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThumbnailSettings {
    pub enabled: bool,
    /// 明示指定だけを持つ。未指定のときは `dir()` が環境変数から決める。
    pub cache_dir: Option<PathBuf>,
    pub max_cached: usize,
    pub timeout: Duration,
}

impl Default for ThumbnailSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            cache_dir: None,
            max_cached: DEFAULT_MAX_CACHED,
            timeout: Duration::from_secs(DEFAULT_THUMB_TIMEOUT_SECS),
        }
    }
}

impl ThumbnailSettings {
    /// 実際に使う置き場。設定が無ければ $XDG_CACHE_HOME / $HOME から決める。
    pub fn dir(&self) -> Option<PathBuf> {
        if self.cache_dir.is_some() {
            return self.cache_dir.clone();
        }
        let xdg = std::env::var_os("XDG_CACHE_HOME");
        let home = std::env::var_os("HOME");
        thumbs::cache_dir(xdg.as_deref(), home.as_deref())
    }
}

/// いいね済み/登録済みの印の出し方。控えの中身は engagement.rs が持つ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngagementSettings {
    pub enabled: bool,
    /// 控えを取り直すまでの間隔。
    pub ttl: Duration,
    pub max_concurrent_requests: usize,
}

impl Default for EngagementSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            ttl: Duration::from_secs(DEFAULT_ENGAGEMENT_TTL_SECS),
            max_concurrent_requests: DEFAULT_ENGAGEMENT_CONCURRENCY,
        }
    }
}

/// ダウンロード画面 (`Mode::Download`) の保存先。未指定なら download::default_dir が決める。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DownloadSettings {
    pub dir: Option<PathBuf>,
    /// yt-dlp への実引数・終了コード・標準出力/エラーを download-debug.log へ記録するか。
    pub debug: bool,
}

/// 検証済みの値。App が持つのはこれ。
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub display: DisplaySettings,
    /// None は制限なし。
    pub fps_cap: Option<FpsCap>,
    pub subtitles: SubtitleSettings,
    pub window: WindowOptions,
    pub extra_args: Vec<String>,
    /// None は cookie 連携 Off。
    pub cookies: Option<CookieSource>,
    pub search: SearchSettings,
    pub thumbnails: ThumbnailSettings,
    pub engagement: EngagementSettings,
    pub download: DownloadSettings,
    /// 「すべて」を除いたカテゴリタブ。
    pub categories: Vec<Category>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            display: DisplaySettings::default(),
            fps_cap: FpsCap::new(DEFAULT_FPS_CAP),
            subtitles: SubtitleSettings::default(),
            window: WindowOptions::default(),
            extra_args: Vec::new(),
            cookies: None,
            search: SearchSettings::default(),
            thumbnails: ThumbnailSettings::default(),
            engagement: EngagementSettings::default(),
            download: DownloadSettings::default(),
            categories: default_categories(),
        }
    }
}

/// 読み込み結果。notice は利用者に見せる 1 行。
#[derive(Debug, Clone, PartialEq)]
pub struct Loaded {
    pub settings: Settings,
    /// 環境変数が上書きした項目。保存で焼き付けないために持ち回る。
    pub overridden: EnvOverridden,
    pub notice: Option<String>,
}

/// 検証結果。settings は丸めた後の値、overridden は環境変数が触った項目。
#[derive(Debug, Clone, PartialEq)]
pub struct Validated {
    pub settings: Settings,
    pub overridden: EnvOverridden,
    pub notices: Vec<String>,
}

/// `$XDG_CONFIG_HOME/tuitube` か `$HOME/.config/tuitube`。非表示リストもここへ並べる。
pub fn app_config_dir(xdg_config_home: Option<&OsStr>, home: Option<&OsStr>) -> Option<PathBuf> {
    if let Some(xdg) = xdg_config_home.filter(|v| !v.is_empty()) {
        return Some(Path::new(xdg).join(APP_DIR));
    }
    let home = home.filter(|v| !v.is_empty())?;
    Some(Path::new(home).join(".config").join(APP_DIR))
}

/// `$XDG_CONFIG_HOME/tuitube/config.toml` か `$HOME/.config/tuitube/config.toml`。
pub fn config_path(xdg_config_home: Option<&OsStr>, home: Option<&OsStr>) -> Option<PathBuf> {
    Some(app_config_dir(xdg_config_home, home)?.join(CONFIG_FILE))
}

pub fn parse(text: &str) -> Result<RawConfig, String> {
    toml::from_str(text).map_err(|e| e.to_string())
}

/// 検証と丸め。壊れた値で再生できなくなる方が困るので、起動は止めず notice を積む。
pub fn validate(raw: RawConfig, env: EnvOverrides) -> Validated {
    let mut notices = Vec::new();
    let mut overridden = EnvOverridden::default();

    let display = raw.display.unwrap_or_default();
    let display = DisplaySettings {
        mode: parse_choice(
            "[display] mode",
            display.mode.as_deref(),
            DisplayMode::from_key,
            &format!("{} で再生します", DisplayMode::default().key()),
            &mut notices,
        )
        .unwrap_or_default(),
        quality: parse_choice(
            "[display] quality",
            display.quality.as_deref(),
            Quality::from_label,
            &format!("{} で表示します", Quality::default().label()),
            &mut notices,
        )
        .unwrap_or_default(),
        max_frame_pixels: display
            .max_frame_pixels
            .map(|px| clamp_pixels(px, &mut notices)),
    };

    let mut fps_cap = validate_fps_cap(raw.playback.unwrap_or_default().fps_cap, &mut notices);
    let (env_limit, env_notice) = parse_fps_limit_env(env.fps_limit);
    if let Some(limit) = env_limit {
        overridden.fps_cap = Some(fps_cap);
        fps_cap = limit.and_then(FpsCap::new);
    }
    notices.extend(env_notice);

    let subtitles = validate_subtitles(raw.subtitles.unwrap_or_default(), &mut notices);

    let mut cookies = validate_cookies(raw.cookies.unwrap_or_default(), &mut notices);
    if let Some(from_env) = parse_cookies_env(env.cookies) {
        overridden.cookies = Some(cookies.clone());
        cookies = from_env;
    }

    let window = validate_window(raw.window.unwrap_or_default(), &mut notices);
    let extra_args = raw.mpv.unwrap_or_default().extra_args.unwrap_or_default();
    let odd: Vec<&str> = extra_args
        .iter()
        .filter(|arg| !arg.starts_with("--"))
        .map(String::as_str)
        .collect();
    if !odd.is_empty() {
        notices.push(format!(
            "[mpv] extra_args に -- で始まらない要素があります ({})。そのまま mpv に渡します",
            odd.join(", ")
        ));
    }

    let search = validate_search(raw.search.unwrap_or_default(), &mut notices);
    let thumbnails = validate_thumbnails(raw.thumbnails.unwrap_or_default(), &mut notices);
    let engagement = validate_engagement(raw.engagement.unwrap_or_default(), &mut notices);
    let download = validate_download(raw.download.unwrap_or_default(), &mut notices);
    let categories = validate_categories(raw.categories, &mut notices);

    Validated {
        settings: Settings {
            display,
            fps_cap,
            subtitles,
            window,
            extra_args,
            cookies,
            search,
            thumbnails,
            engagement,
            download,
            categories,
        },
        overridden,
        notices,
    }
}

/// lang は言語コードのカンマ区切りだけ。--slang は yt-dlp の記法 (all・正規表現) を読めない。
fn validate_subtitles(raw: RawSubtitles, notices: &mut Vec<String>) -> SubtitleSettings {
    // 網羅分解。キーを足すとここで止まる。
    let RawSubtitles { enabled, lang } = raw;
    let defaults = SubtitleSettings::default();
    SubtitleSettings {
        enabled: enabled.unwrap_or(defaults.enabled),
        lang: parse_choice(
            "[subtitles] lang",
            lang.as_deref(),
            SubLang::parse,
            &format!("{} で取得します", SubLang::DEFAULT),
            notices,
        )
        .unwrap_or(defaults.lang),
    }
}

fn validate_search(raw: RawSearch, notices: &mut Vec<String>) -> SearchSettings {
    let layout = parse_choice(
        "[search] layout",
        raw.layout.as_deref(),
        LayoutMode::from_key,
        &format!("{} で表示します", LayoutMode::default().key()),
        notices,
    )
    .unwrap_or_default();
    let limit = match raw.limit {
        None => DEFAULT_SEARCH_LIMIT,
        Some(limit) => {
            let clamped = limit.clamp(MIN_SEARCH_LIMIT as i64, MAX_SEARCH_LIMIT as i64);
            if clamped != limit {
                notices.push(format!("[search] limit={limit} は {clamped} に丸めました"));
            }
            clamped as usize
        }
    };
    let defaults = SearchSettings::default();
    let timeout = match raw.timeout_secs {
        None => defaults.timeout,
        Some(secs) => {
            let clamped = secs.clamp(
                MIN_SEARCH_TIMEOUT_SECS as i64,
                MAX_SEARCH_TIMEOUT_SECS as i64,
            );
            if clamped != secs {
                notices.push(format!(
                    "[search] timeout_secs={secs} は {clamped} に丸めました"
                ));
            }
            Duration::from_secs(clamped as u64)
        }
    };
    let cache_ttl = match raw.cache_ttl_secs {
        None => defaults.cache_ttl,
        Some(secs) => {
            let clamped = secs.clamp(
                MIN_SEARCH_CACHE_TTL_SECS as i64,
                MAX_SEARCH_CACHE_TTL_SECS as i64,
            );
            if clamped != secs {
                notices.push(format!(
                    "[search] cache_ttl_secs={secs} は {clamped} に丸めました"
                ));
            }
            Duration::from_secs(clamped as u64)
        }
    };
    SearchSettings {
        layout,
        limit,
        timeout,
        cache_enabled: raw.cache_enabled.unwrap_or(defaults.cache_enabled),
        cache_ttl,
    }
}

fn validate_thumbnails(raw: RawThumbnails, notices: &mut Vec<String>) -> ThumbnailSettings {
    let defaults = ThumbnailSettings::default();
    let cache_dir = raw
        .cache_dir
        .as_deref()
        .map(str::trim)
        .filter(|dir| !dir.is_empty())
        .map(expand_home);
    if raw.cache_dir.is_some() && cache_dir.is_none() {
        notices.push("[thumbnails] cache_dir が空です。既定の場所を使います".to_string());
    }
    let max_cached = match raw.max_cached {
        None => defaults.max_cached,
        Some(count) if count >= 0 => count as usize,
        Some(count) => {
            notices.push(format!(
                "[thumbnails] max_cached={count} は読めません。{DEFAULT_MAX_CACHED} 枚まで残します"
            ));
            defaults.max_cached
        }
    };
    let timeout = match raw.timeout_secs {
        None => defaults.timeout,
        Some(secs) if (1..=MAX_THUMB_TIMEOUT_SECS as i64).contains(&secs) => {
            Duration::from_secs(secs as u64)
        }
        Some(secs) => {
            let clamped = secs.clamp(1, MAX_THUMB_TIMEOUT_SECS as i64);
            notices.push(format!(
                "[thumbnails] timeout_secs={secs} は {clamped} に丸めました"
            ));
            Duration::from_secs(clamped as u64)
        }
    };
    ThumbnailSettings {
        enabled: raw.enabled.unwrap_or(defaults.enabled),
        cache_dir,
        max_cached,
        timeout,
    }
}

/// 印の出し方だけ。控えの読み書きは engagement.rs 側。
fn validate_engagement(raw: RawEngagement, notices: &mut Vec<String>) -> EngagementSettings {
    // 網羅分解。キーを足すとここで止まる。
    let RawEngagement {
        enabled,
        ttl_secs,
        max_concurrent_requests,
    } = raw;
    let defaults = EngagementSettings::default();
    let min_ttl = MIN_ENGAGEMENT_TTL_SECS as i64;
    let max_ttl = MAX_ENGAGEMENT_TTL_SECS as i64;
    let ttl = match ttl_secs {
        None => defaults.ttl,
        Some(secs) if (min_ttl..=max_ttl).contains(&secs) => Duration::from_secs(secs as u64),
        Some(secs) => {
            let clamped = secs.clamp(min_ttl, max_ttl);
            notices.push(format!(
                "[engagement] ttl_secs={secs} は {clamped} に丸めました"
            ));
            Duration::from_secs(clamped as u64)
        }
    };
    let min_concurrency = MIN_ENGAGEMENT_CONCURRENCY as i64;
    let max_concurrency = MAX_ENGAGEMENT_CONCURRENCY as i64;
    let max_concurrent_requests = match max_concurrent_requests {
        None => defaults.max_concurrent_requests,
        Some(count) if (min_concurrency..=max_concurrency).contains(&count) => count as usize,
        Some(count) => {
            let clamped = count.clamp(min_concurrency, max_concurrency);
            notices.push(format!(
                "[engagement] max_concurrent_requests={count} は {clamped} に丸めました"
            ));
            clamped as usize
        }
    };
    EngagementSettings {
        enabled: enabled.unwrap_or(defaults.enabled),
        ttl,
        max_concurrent_requests,
    }
}

/// ダウンロード画面の保存先。存在確認はしない (yt-dlp が -o の既定動作で作る)。
fn validate_download(raw: RawDownload, notices: &mut Vec<String>) -> DownloadSettings {
    let dir = raw
        .dir
        .as_deref()
        .map(str::trim)
        .filter(|dir| !dir.is_empty())
        .map(expand_home);
    if raw.dir.is_some() && dir.is_none() {
        notices.push("[download] dir が空です。既定の場所を使います".to_string());
    }
    let defaults = DownloadSettings::default();
    DownloadSettings {
        dir,
        debug: raw.debug.unwrap_or(defaults.debug),
    }
}

/// 先頭の `~/` だけ $HOME へ置き換える。そのままだと "~" という名前の
/// ディレクトリが作られてしまう。download.rs からも再利用する。
pub(crate) fn expand_home(dir: &str) -> PathBuf {
    let Some(rest) = dir.strip_prefix("~/") else {
        return PathBuf::from(dir);
    };
    match std::env::var_os("HOME").filter(|home| !home.is_empty()) {
        Some(home) => Path::new(&home).join(rest),
        None => PathBuf::from(dir),
    }
}

fn validate_categories(raw: Option<Vec<RawCategory>>, notices: &mut Vec<String>) -> Vec<Category> {
    let Some(raw) = raw else {
        return default_categories();
    };
    let listed = raw.len();
    let categories: Vec<Category> = raw
        .into_iter()
        .filter_map(|entry| {
            let label = entry.label?;
            let query = entry.query?;
            (!label.trim().is_empty() && !query.trim().is_empty())
                .then(|| Category::new(label.trim(), query.trim()))
        })
        .collect();
    if categories.len() != listed {
        notices.push(format!(
            "[[categories]] の {} 件は label か query が空でした。その項目は出しません",
            listed - categories.len()
        ));
    }
    if categories.is_empty() {
        notices.push("[[categories]] が空です。既定のカテゴリを出します".to_string());
        return default_categories();
    }
    for category in &categories {
        // ":yt" 始まりはフィードのつもりの綴りとみなす。そのまま検索語として
        // 渡すと 0 件で返るだけで、間違いに気づけない。
        if category.query.starts_with(":yt") && Feed::parse(&category.query).is_none() {
            notices.push(format!(
                "[[categories]] query=\"{}\" は一覧のキーワードではありません ({})。検索語として扱います",
                category.query,
                feed_keywords()
            ));
        }
    }
    categories
}

/// 設定ファイルの案内に出すフィードのキーワード一覧。
fn feed_keywords() -> String {
    Feed::ALL
        .iter()
        .map(|feed| format!("{} = {}", feed.keyword(), feed.label()))
        .collect::<Vec<_>>()
        .join(" / ")
}

/// 選択肢のキーを読む。綴りが違うときはそのキーだけ落として notice を出し、
/// 他のキーを巻き添えにしない。
fn parse_choice<T>(
    label: &str,
    raw: Option<&str>,
    parse: fn(&str) -> Option<T>,
    fallback: &str,
    notices: &mut Vec<String>,
) -> Option<T> {
    let raw = raw?;
    let value = parse(raw);
    if value.is_none() {
        notices.push(format!("{label}=\"{raw}\" は読めません。{fallback}"));
    }
    value
}

fn clamp_pixels(px: i64, notices: &mut Vec<String>) -> u32 {
    let min = i64::from(MIN_FRAME_PIXELS);
    let max = i64::from(MAX_FRAME_PIXELS_LIMIT);
    if px < min {
        notices.push(format!(
            "[display] max_frame_pixels={px} は下限の {MIN_FRAME_PIXELS} に丸めました"
        ));
        return MIN_FRAME_PIXELS;
    }
    if px > max {
        notices.push(format!(
            "[display] max_frame_pixels={px} は上限の {MAX_FRAME_PIXELS_LIMIT} に丸めました"
        ));
        return MAX_FRAME_PIXELS_LIMIT;
    }
    px as u32
}

fn validate_fps_cap(raw: Option<i64>, notices: &mut Vec<String>) -> Option<FpsCap> {
    let Some(fps) = raw else {
        return FpsCap::new(DEFAULT_FPS_CAP);
    };
    if fps == 0 {
        return None;
    }
    if fps < 0 {
        notices.push(format!(
            "[playback] fps_cap={fps} は読めません。{DEFAULT_FPS_CAP} fps で再生します"
        ));
        return FpsCap::new(DEFAULT_FPS_CAP);
    }
    if fps > i64::from(MAX_FPS_CAP) {
        notices.push(format!(
            "[playback] fps_cap={fps} は上限の {MAX_FPS_CAP} に丸めました"
        ));
        return FpsCap::new(MAX_FPS_CAP);
    }
    FpsCap::new(fps as u32)
}

fn validate_window(window: RawWindow, notices: &mut Vec<String>) -> WindowOptions {
    let mut checked = WindowOptions {
        vo: window.vo,
        autofit: window.autofit,
        geometry: window.geometry,
        ontop: window.ontop,
        fullscreen: window.fullscreen,
        focus_on: parse_choice(
            "[window] focus_on",
            window.focus_on.as_deref(),
            FocusOn::from_value,
            "指定なしとして扱います",
            notices,
        ),
        title: window.title,
    };
    for (key, value) in [
        ("vo", &mut checked.vo),
        ("autofit", &mut checked.autofit),
        ("geometry", &mut checked.geometry),
        ("title", &mut checked.title),
    ] {
        if value.as_deref().is_some_and(|v| v.trim().is_empty()) {
            notices.push(format!("[window] {key} が空です。指定なしとして扱います"));
            *value = None;
        }
    }
    checked
}

/// cookie の渡し方の検証。ブラウザ名の綴りは yt-dlp に、cookies.txt の中身の
/// 形式も yt-dlp に任せ、ここで見るのはファイルを読み書きできるかどうかだけ。
fn validate_cookies(raw: RawCookies, notices: &mut Vec<String>) -> Option<CookieSource> {
    // 網羅分解。cookie の値に当たるフィールドを足すとここで止まる。
    let RawCookies { browser, file } = raw;
    let path = file
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(expand_home);
    if file.is_some() && path.is_none() {
        notices.push("[cookies] file が空です。指定なしとして扱います".to_string());
    }
    if let Some(path) = path {
        // 排他の知らせは file を使えると分かってから。使えないときは browser も使わないので、
        // 「file を使います」と「cookie 連携なしで動きます」が並ぶと矛盾して見える。
        if let Err(e) = usable_cookie_file(&path) {
            notices.push(format!(
                "[cookies] file {} を使えません: {e}。cookie 連携なしで動きます",
                path.display()
            ));
            return None;
        }
        if browser.as_deref().is_some_and(|b| !b.trim().is_empty()) {
            notices.push(
                "[cookies] browser と file の両方が指定されています。file を使います".to_string(),
            );
        }
        return CookieSource::from_file(Some(&path));
    }
    let browser = browser?;
    let source = CookieSource::from_spec(Some(&browser));
    if source.is_none() {
        notices.push("[cookies] browser が空です。指定なしとして扱います".to_string());
    }
    source
}

/// yt-dlp は終了時に --cookies のファイルへ cookie を書き戻すので、読めるだけでは足りない。
/// 書き込めないと検索のたびに失敗し、しかもエラーは結果を出した後に出る。
/// append で開けば中身を変えずに書き込み可否だけ確かめられる。
fn usable_cookie_file(path: &Path) -> Result<(), String> {
    let meta = fs::metadata(path).map_err(|e| e.to_string())?;
    if !meta.is_file() {
        return Err("ファイルではありません".to_string());
    }
    fs::File::open(path).map_err(|e| format!("読めません ({e})"))?;
    fs::OpenOptions::new()
        .append(true)
        .open(path)
        .map(|_| ())
        .map_err(|e| format!("書き込めません ({e})。yt-dlp が cookie を書き戻します"))
}

/// 環境変数 TUITUBE_COOKIES_FROM_BROWSER の解釈。
/// None = 上書きしない、Some(None) = 連携 Off、Some(Some(_)) = その指定。
pub fn parse_cookies_env(raw: Option<&str>) -> Option<Option<CookieSource>> {
    let trimmed = raw.unwrap_or_default().trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.eq_ignore_ascii_case("none") {
        return Some(None);
    }
    Some(CookieSource::from_spec(Some(trimmed)))
}

/// 環境変数 TUITUBE_FPS_LIMIT の解釈。
/// Some(Some(n)) = その値、Some(None) = 制限なし、None = 上書きしない。
pub fn parse_fps_limit_env(raw: Option<&str>) -> (Option<Option<u32>>, Option<String>) {
    let trimmed = raw.unwrap_or_default().trim();
    if trimmed.is_empty() {
        return (None, None);
    }
    if trimmed.eq_ignore_ascii_case("unlimited") {
        return (Some(None), None);
    }
    match trimmed.parse::<u32>() {
        Ok(0) => (Some(None), None),
        Ok(fps) if fps <= MAX_FPS_CAP => (Some(Some(fps)), None),
        Ok(fps) => (
            Some(Some(MAX_FPS_CAP)),
            Some(format!(
                "{FPS_LIMIT_VAR}={fps} は上限の {MAX_FPS_CAP} に丸めました"
            )),
        ),
        // 読めない指定でファイルの値まで巻き添えにしない。
        Err(_) => (
            None,
            Some(format!(
                "{FPS_LIMIT_VAR}={trimmed} を数値として読めません。設定ファイルの値で再生します"
            )),
        ),
    }
}

/// コメント付きのテンプレート。値は引数の Settings。
pub fn render(settings: &Settings) -> String {
    let display = &settings.display;
    let window = &settings.window;
    let mut out = String::new();
    out.push_str("# tuitube の設定。編集後は tuitube を再起動する。\n");
    out.push_str(
        "# このファイルは tuitube が書き直すことがあり、自分で書いたコメントは残らない。\n\n",
    );

    out.push_str("[display]\n");
    out.push_str("# 再生開始時の表示。\"embedded\" = TUI 内に埋め込み (Kitty graphics protocol)、\"text\" = 文字ブロック(Kitty 非対応端末向け)、\n");
    out.push_str("# \"window\" = mpv の別ウィンドウ。再生中は w で順に切り替え。\n");
    out.push_str(&format!("mode = \"{}\"\n", display.mode.key()));
    out.push_str("# 埋め込み表示の細かさ。\"low\" / \"medium\" / \"high\" / \"native\"。\n");
    out.push_str(
        "# 変わるのは映像の細かさと端末へ送るデータ量。mpv のデコード負荷は変わらない。\n",
    );
    out.push_str(&format!("quality = \"{}\"\n", display.quality.label()));
    out.push_str(
        "# 細かさをピクセル数で直接指定したいとき (quality より優先)。例: 640*360 = 230400\n",
    );
    match display.max_frame_pixels {
        Some(px) => out.push_str(&format!("max_frame_pixels = {px}\n")),
        None => out.push_str("# max_frame_pixels = 230400\n"),
    }

    out.push_str("\n[playback]\n");
    out.push_str("# 埋め込み・テキスト表示の fps 上限。端末へ送るフレーム数を抑える。0 で制限なし。別ウィンドウには適用しない。\n");
    out.push_str(&format!(
        "fps_cap = {}\n",
        settings.fps_cap.map(FpsCap::get).unwrap_or(0)
    ));

    out.push_str("\n[subtitles]\n");
    out.push_str(
        "# YouTube の自動生成字幕を要求するか。false のときは再生中に s を押しても出せない。\n",
    );
    out.push_str(&format!("enabled = {}\n", settings.subtitles.enabled));
    out.push_str("# 取得する字幕の言語。カンマ区切りで複数書ける。\n");
    out.push_str("# 例: \"ja-orig\" (原語の文字起こし) / \"ja\" (自動翻訳) / \"ja-orig,ja\"\n");
    out.push_str(
        "# 複数書いたときにどれを出すかは mpv が決める。先頭が選ばれるとは限らない (実測)。\n",
    );
    out.push_str("# yt-dlp の sub-langs と mpv の --slang に同じ値を渡すので、言語コード以外 (\"all\" や正規表現) は書けない。\n");
    out.push_str(&format!(
        "lang = \"{}\"\n",
        escape(settings.subtitles.lang.as_str())
    ));

    out.push_str("\n[window]\n");
    out.push_str("# 別ウィンドウ時の mpv オプション。値はそのまま mpv に渡る。\n");
    out.push_str(&string_line("vo", window.vo.as_deref(), "gpu-next"));
    out.push_str(&string_line(
        "autofit",
        window.autofit.as_deref(),
        "640x360",
    ));
    out.push_str(&string_line(
        "geometry",
        window.geometry.as_deref(),
        "50%+0+0",
    ));
    if window.ontop {
        out.push_str("ontop = true\n");
    } else {
        out.push_str("# ontop = false\n");
    }
    if window.fullscreen {
        out.push_str("fullscreen = true\n");
    } else {
        out.push_str("# fullscreen = false\n");
    }
    match window.focus_on {
        Some(focus_on) => out.push_str(&format!("focus_on = \"{}\"\n", focus_on.value())),
        None => out.push_str("# focus_on = \"never\"\n"),
    }
    out.push_str(&string_line("title", window.title.as_deref(), "tuitube"));

    out.push_str("\n[cookies]\n");
    out.push_str("# YouTube のログイン連携。yt-dlp の --cookies-from-browser に渡すブラウザ指定 (BROWSER[+KEYRING][:PROFILE][::CONTAINER])。\n");
    out.push_str("# 例: \"chrome\" / \"safari\" / \"firefox\" / \"chrome:Profile 1\"。ここに入るのはブラウザ名だけで、cookie の値は保存しない。\n");
    out.push_str(&format!(
        "# 環境変数 {} があればそちらが優先 (\"none\" で一時的に連携を切る)。\n",
        cookies::ENV_VAR
    ));
    out.push_str(&string_line(
        "browser",
        settings.cookies.as_ref().and_then(CookieSource::spec),
        "chrome",
    ));
    out.push_str("# ブラウザもキーチェーンも無い環境向けに、エクスポートした cookies.txt (Netscape 形式) を渡す指定。yt-dlp の --cookies に渡す。\n");
    out.push_str("# browser と両方書いたときは file を使う。使えないファイルを指した場合は cookie 連携なしで起動する。\n");
    out.push_str("# yt-dlp が終了時にこのファイルへ cookie を書き戻すので、読み取り専用にせず書き込みも許可しておく。\n");
    out.push_str(&string_line(
        "file",
        settings
            .cookies
            .as_ref()
            .and_then(CookieSource::file)
            .and_then(Path::to_str),
        "~/.config/tuitube/cookies.txt",
    ));

    out.push_str("\n[mpv]\n");
    out.push_str("# mpv にそのまま渡す追加引数。\n");
    if settings.extra_args.is_empty() {
        out.push_str("# extra_args = [\"--hwdec=videotoolbox-copy\"]\n");
    } else {
        let values: Vec<String> = settings
            .extra_args
            .iter()
            .map(|arg| format!("\"{}\"", escape(arg)))
            .collect();
        out.push_str(&format!("extra_args = [{}]\n", values.join(", ")));
    }

    out.push_str("\n[search]\n");
    out.push_str(
        "# 検索結果の見せ方。\"grid\" = サムネイル付きの格子、\"list\" = 1 行ずつのリスト。\n",
    );
    out.push_str("# Kitty graphics protocol 非対応の端末では \"list\" にする。\n");
    out.push_str(&format!("layout = \"{}\"\n", settings.search.layout.key()));
    out.push_str(&format!(
        "# 1 回の検索で取る件数。{MIN_SEARCH_LIMIT}..={MAX_SEARCH_LIMIT}。\n"
    ));
    out.push_str(&format!("limit = {}\n", settings.search.limit));
    out.push_str(&format!(
        "# yt-dlp 1 回ぶんを待つ上限秒数。{MIN_SEARCH_TIMEOUT_SECS}..={MAX_SEARCH_TIMEOUT_SECS}。\n"
    ));
    out.push_str("# cookie を読めずに出し直すときは、最大 2 回ぶん待つ。\n");
    out.push_str(
        "# limit を大きくすると検索に時間がかかる。「検索がタイムアウトしました」が出るなら延ばす。\n",
    );
    out.push_str(&format!(
        "timeout_secs = {}\n",
        settings.search.timeout.as_secs()
    ));
    out.push_str("# true にすると、同じ検索を cache_ttl_secs の間は取り直さない。\n");
    out.push_str(
        "# 控えはメモリ上だけなので、アプリを終了すると消える。r を押せば必ず取り直す。\n",
    );
    out.push_str(&format!(
        "cache_enabled = {}\n",
        settings.search.cache_enabled
    ));
    out.push_str(&format!(
        "# 控えを使い回す秒数。{MIN_SEARCH_CACHE_TTL_SECS}..={MAX_SEARCH_CACHE_TTL_SECS}。\n"
    ));
    out.push_str(&format!(
        "cache_ttl_secs = {}\n",
        settings.search.cache_ttl.as_secs()
    ));

    let thumbnails = &settings.thumbnails;
    out.push_str("\n[thumbnails]\n");
    out.push_str("# false にすると取得しない。格子のまま枠だけが出る。\n");
    out.push_str(&format!("enabled = {}\n", thumbnails.enabled));
    out.push_str("# 取得したサムネイルの置き場。既定は $XDG_CACHE_HOME/tuitube/thumbs。\n");
    out.push_str(&string_line(
        "cache_dir",
        thumbnails.cache_dir.as_deref().and_then(Path::to_str),
        "~/.cache/tuitube/thumbs",
    ));
    out.push_str("# 起動時にここまで間引く枚数。\n");
    out.push_str(&format!("max_cached = {}\n", thumbnails.max_cached));
    out.push_str(&format!(
        "# 1 枚あたりのダウンロード上限秒数。1..={MAX_THUMB_TIMEOUT_SECS}。\n"
    ));
    out.push_str(&format!(
        "timeout_secs = {}\n",
        thumbnails.timeout.as_secs()
    ));

    let engagement = &settings.engagement;
    out.push_str("\n[engagement]\n");
    out.push_str(
        "# いいね済み/登録済みの印。false にすると印を出さず、状態の問い合わせもしない。\n",
    );
    out.push_str(&format!("enabled = {}\n", engagement.enabled));
    out.push_str(&format!(
        "# 印を取り直すまでの秒数。{MIN_ENGAGEMENT_TTL_SECS}..={MAX_ENGAGEMENT_TTL_SECS} (既定は 1 週間)。\n"
    ));
    out.push_str(&format!("ttl_secs = {}\n", engagement.ttl.as_secs()));
    out.push_str(&format!(
        "# 状態確認を同時に投げる本数。{MIN_ENGAGEMENT_CONCURRENCY}..={MAX_ENGAGEMENT_CONCURRENCY}。\n"
    ));
    out.push_str(&format!(
        "max_concurrent_requests = {}\n",
        engagement.max_concurrent_requests
    ));

    let download = &settings.download;
    out.push_str("\n[download]\n");
    out.push_str("# ダウンロード画面 (D) の保存先。空欄のまま使うと $HOME/Downloads (無ければ空欄) から始まる。\n");
    out.push_str(&string_line(
        "dir",
        download.dir.as_deref().and_then(Path::to_str),
        "~/Movies",
    ));
    out.push_str(
        "# true にすると、yt-dlp への実引数・終了コード・標準出力/エラーを\n# $XDG_CONFIG_HOME/tuitube/download-debug.log (無ければ $HOME/.config/tuitube/...) へ追記する。\n",
    );
    out.push_str("# ダウンロードがうまく動かないときの調査用。使い終わったら false に戻す (ログは増え続ける)。\n");
    out.push_str(&format!("debug = {}\n", download.debug));

    out.push_str("\n# カテゴリタブ。書いた場合は既定の一覧を丸ごと置き換える。\n");
    out.push_str("# 先頭の「すべて」タブは常に自動で付くので書かない。\n");
    out.push_str(&format!(
        "# query が次のキーワードならログイン連動の一覧のタブになる: {}\n",
        feed_keywords()
    ));
    out.push_str("# 既定の一覧にはこの 4 つも入っているので、書き換えるときは残す分も並べる。\n");
    if settings.categories == default_categories() {
        out.push_str("# [[categories]]\n# label = \"音楽\"\n# query = \"音楽\"\n");
        out.push_str("# [[categories]]\n# label = \"おすすめ\"\n# query = \":ytrec\"\n");
    } else {
        for category in &settings.categories {
            out.push_str("\n[[categories]]\n");
            out.push_str(&format!("label = \"{}\"\n", escape(&category.label)));
            out.push_str(&format!("query = \"{}\"\n", escape(&category.query)));
        }
    }
    out
}

/// 値があれば有効な行、無ければ書き方の例をコメントで出す。
fn string_line(key: &str, value: Option<&str>, example: &str) -> String {
    match value {
        Some(value) => format!("{key} = \"{}\"\n", escape(value)),
        None => format!("# {key} = \"{example}\"\n"),
    }
}

fn escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

/// ステータス行は 1 行なので、TOML の複数行エラーを畳んで載せる。
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn join(notices: Vec<String>) -> Option<String> {
    (!notices.is_empty()).then(|| notices.join(" / "))
}

pub fn load_from(path: Option<&Path>, env: EnvOverrides) -> Loaded {
    let Some(path) = path else {
        return loaded(validate(RawConfig::default(), env));
    };
    match fs::read_to_string(path) {
        Ok(text) => match parse(&text) {
            Ok(raw) => loaded(validate(raw, env)),
            // 半端に効いた状態は原因を追いにくいので、全体を既定値に倒す。
            Err(e) => fallback(
                env,
                format!(
                    "{} を読めません: {}。既定値で動きます",
                    path.display(),
                    one_line(&e)
                ),
            ),
        },
        Err(e) if e.kind() == ErrorKind::NotFound => create_template(path, env),
        Err(e) => fallback(
            env,
            format!("{} を開けません: {e}。既定値で動きます", path.display()),
        ),
    }
}

fn loaded(validated: Validated) -> Loaded {
    Loaded {
        settings: validated.settings,
        overridden: validated.overridden,
        notice: join(validated.notices),
    }
}

fn fallback(env: EnvOverrides, reason: String) -> Loaded {
    let mut validated = validate(RawConfig::default(), env);
    validated.notices.insert(0, reason);
    loaded(validated)
}

/// 生成に失敗しても起動は止めず、理由だけ伝える。
fn create_template(path: &Path, env: EnvOverrides) -> Loaded {
    let mut validated = validate(RawConfig::default(), env);
    validated.notices.insert(
        0,
        match save_to(path, &Settings::default()) {
            Ok(()) => format!("{} を作成しました", path.display()),
            Err(e) => format!("{} を作成できません: {e}", path.display()),
        },
    );
    loaded(validated)
}

fn write_new(path: &Path, text: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    fs::write(path, text).map_err(|e| e.to_string())
}

/// 途中で落ちても壊れたファイルを残さないよう、一時ファイルへ書いてから置き換える。
fn write_atomic(path: &Path, text: &str) -> Result<(), String> {
    let tmp = path.with_extension("toml.tmp");
    write_new(&tmp, text)?;
    fs::rename(&tmp, path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        e.to_string()
    })
}

/// 設定画面の s での保存。render で全文を作り直すので、ファイルに書いてあった
/// コメントと tuitube が持たないキーは残らない。
pub fn save_to(path: &Path, settings: &Settings) -> Result<(), String> {
    write_atomic(path, &render(settings))
}

/// display.mode の行だけを差し替える。再生中の w は保存を意図した操作ではないので、
/// 全文の書き直し (= 利用者のコメントと未知のキーが消える) を起こさない。
pub fn save_display_mode_to(path: &Path, mode: DisplayMode) -> Result<(), String> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        // まだファイルが無ければ残すものも無いので、起動時と同じテンプレートを作る。
        Err(e) if e.kind() == ErrorKind::NotFound => {
            let settings = Settings {
                display: DisplaySettings {
                    mode,
                    ..DisplaySettings::default()
                },
                ..Settings::default()
            };
            return save_to(path, &settings);
        }
        Err(e) => return Err(e.to_string()),
    };
    let updated = with_display_mode(&text, mode);
    // 読み直して値が入っているか確かめてから置き換える。手書きの書き方 (dotted key など)
    // では行を見つけられず、足した行が重複キーになることがある。
    if parse(&updated)
        .ok()
        .and_then(|raw| raw.display?.mode)
        .as_deref()
        != Some(mode.key())
    {
        return Err("display.mode の行を書き換えられません".to_string());
    }
    write_atomic(path, &updated)
}

/// `[display]` の mode 行だけを差し替えた全文。行が無ければ節の頭へ、
/// 節ごと無ければ末尾へ足す。他の行には触らない。
fn with_display_mode(text: &str, mode: DisplayMode) -> String {
    let new_line = format!("mode = \"{}\"", mode.key());
    let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
    let mut in_display = false;
    let (mut header, mut target) = (None, None);
    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_display = trimmed == "[display]";
            if in_display {
                header = Some(i);
            }
        } else if in_display && target.is_none() && is_key_line(trimmed, "mode") {
            target = Some(i);
        }
    }
    match (target, header) {
        (Some(i), _) => lines[i] = new_line,
        (None, Some(i)) => lines.insert(i + 1, new_line),
        (None, None) => lines.extend([String::new(), "[display]".to_string(), new_line]),
    }
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

/// `key = ...` の行か。コメント行や前方一致する別のキーには当てない。
fn is_key_line(trimmed: &str, key: &str) -> bool {
    trimmed
        .strip_prefix(key)
        .is_some_and(|rest| rest.trim_start().starts_with('='))
}

pub fn load() -> Loaded {
    let xdg = std::env::var_os("XDG_CONFIG_HOME");
    let home = std::env::var_os("HOME");
    let path = config_path(xdg.as_deref(), home.as_deref());
    let env_fps_limit = std::env::var(FPS_LIMIT_VAR).ok();
    let env_cookies = std::env::var(cookies::ENV_VAR).ok();
    load_from(
        path.as_deref(),
        EnvOverrides {
            fps_limit: env_fps_limit.as_deref(),
            cookies: env_cookies.as_deref(),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cookies::CookieSource;
    use crate::display::{DisplayMode, FocusOn, FpsCap, Quality, WindowOptions};
    use crate::grid::LayoutMode;
    use std::ffi::OsStr;
    use std::fs;

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("tuitube-settings-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn settings_of(text: &str) -> Settings {
        validate(parse(text).expect("読めるはず"), EnvOverrides::default()).settings
    }

    fn notices_of(text: &str) -> Vec<String> {
        validate(parse(text).expect("読めるはず"), EnvOverrides::default()).notices
    }

    fn spec(value: &str) -> Option<CookieSource> {
        CookieSource::from_spec(Some(value))
    }

    fn file_source(path: &Path) -> Option<CookieSource> {
        CookieSource::from_file(Some(path))
    }

    /// 実在する cookies.txt を 1 つ置く。後始末は呼び出し側。
    fn cookie_file(name: &str) -> (PathBuf, PathBuf) {
        let dir = temp_dir(name);
        let path = dir.join("cookies.txt");
        fs::write(&path, "# Netscape HTTP Cookie File\n").expect("書ける");
        (dir, path)
    }

    #[test]
    fn download_dir_is_read_from_the_download_section() {
        let text = "[download]\ndir = \"/tmp/out\"\n";
        assert_eq!(
            settings_of(text).download.dir,
            Some(PathBuf::from("/tmp/out"))
        );
        assert!(notices_of(text).is_empty(), "{:?}", notices_of(text));
    }

    #[test]
    fn download_dir_expands_a_leading_tilde() {
        let home = std::env::var("HOME").expect("HOME");
        let text = "[download]\ndir = \"~/Movies\"\n";
        assert_eq!(
            settings_of(text).download.dir,
            Some(PathBuf::from(format!("{home}/Movies")))
        );
    }

    #[test]
    fn rendering_keeps_the_configured_download_dir_when_only_debug_changes() {
        // debug の toggle だけを保存しても、[download] dir が消えないこと
        // (render() が [download] セクション全体を書く前提が壊れていないか)。
        let mut settings = settings_of("[download]\ndir = \"/tmp/out\"\n");
        assert_eq!(settings.download.dir, Some(PathBuf::from("/tmp/out")));
        settings.download.debug = true;

        let rendered = render(&settings);

        let reloaded = settings_of(&rendered);
        assert_eq!(reloaded.download.dir, Some(PathBuf::from("/tmp/out")));
        assert!(reloaded.download.debug);
    }

    #[test]
    fn a_blank_download_dir_falls_back_with_a_notice() {
        let text = "[download]\ndir = \"  \"\n";
        assert_eq!(settings_of(text).download.dir, None);
        let notices = notices_of(text);
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert!(notices[0].contains("[download] dir"), "{notices:?}");
    }

    #[test]
    fn an_unset_download_dir_is_none_without_a_notice() {
        assert_eq!(settings_of("").download.dir, None);
        assert!(notices_of("").is_empty());
    }

    #[test]
    fn download_debug_defaults_to_false_without_a_notice() {
        assert!(!settings_of("").download.debug);
        assert!(notices_of("").is_empty());
        assert!(!DownloadSettings::default().debug);
    }

    #[test]
    fn download_debug_is_read_from_the_download_section() {
        let text = "[download]\ndebug = true\n";
        assert!(settings_of(text).download.debug);
        assert!(notices_of(text).is_empty());
    }

    #[test]
    fn render_round_trips_the_download_section() {
        let custom = Settings {
            download: DownloadSettings {
                dir: Some(PathBuf::from("/tmp/out")),
                debug: true,
            },
            ..Settings::default()
        };
        let text = render(&custom);
        assert!(text.contains("[download]"), "{text}");
        assert!(text.contains("dir = \"/tmp/out\""), "{text}");
        assert!(text.contains("debug = true"), "{text}");
        assert_eq!(settings_of(&text), custom);

        // 既定 (dir 未設定) は書き方の例をコメントで出し、debug は false のまま。
        let text = render(&Settings::default());
        assert!(text.contains("# dir = "), "{text}");
        assert!(text.contains("debug = false"), "{text}");
        assert_eq!(settings_of(&text), Settings::default());
    }

    #[test]
    fn cookies_file_is_read_into_a_file_source() {
        let (dir, path) = cookie_file("cookies-file");
        let text = format!("[cookies]\nfile = \"{}\"\n", path.display());
        assert_eq!(settings_of(&text).cookies, file_source(&path));
        assert!(notices_of(&text).is_empty(), "{:?}", notices_of(&text));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_cookie_file_wins_over_a_browser_with_a_notice() {
        let (dir, path) = cookie_file("cookies-both");
        let text = format!(
            "[cookies]\nbrowser = \"chrome\"\nfile = \"{}\"\n",
            path.display()
        );
        assert_eq!(settings_of(&text).cookies, file_source(&path));
        let notices = notices_of(&text);
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert!(
            notices[0].contains("browser") && notices[0].contains("file"),
            "{notices:?}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_cookie_file_starts_without_cookies_and_says_so() {
        let dir = temp_dir("cookies-missing");
        let path = dir.join("nope.txt");
        // browser も書いてあるが、file を優先した結果として cookie 連携なしで起動する。
        let text = format!(
            "[cookies]\nbrowser = \"chrome\"\nfile = \"{}\"\n",
            path.display()
        );
        assert_eq!(settings_of(&text).cookies, None);
        let notice = notices_of(&text).join(" / ");
        assert!(notice.contains(&path.display().to_string()), "{notice}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_blank_cookie_file_leaves_the_browser_in_place_with_a_notice() {
        for value in ["", "  "] {
            let text = format!("[cookies]\nbrowser = \"chrome\"\nfile = \"{value}\"\n");
            assert_eq!(settings_of(&text).cookies, spec("chrome"), "{text}");
            let notices = notices_of(&text);
            assert_eq!(notices.len(), 1, "{notices:?}");
            assert!(notices[0].contains("[cookies] file"), "{notices:?}");
        }
    }

    #[test]
    fn a_cookie_file_that_is_a_directory_is_ignored_with_a_notice() {
        let dir = temp_dir("cookies-dir");
        let text = format!("[cookies]\nfile = \"{}\"\n", dir.display());
        assert_eq!(settings_of(&text).cookies, None);
        assert_eq!(notices_of(&text).len(), 1, "{:?}", notices_of(&text));
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_cookie_file_is_ignored_with_a_notice() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, path) = cookie_file("cookies-unreadable");
        let mut perms = fs::metadata(&path).expect("メタデータ").permissions();
        perms.set_mode(0o000);
        fs::set_permissions(&path, perms).expect("権限を落とせる");
        // root は 0o000 でも読めるので、その環境では確かめない。
        if fs::File::open(&path).is_ok() {
            let _ = fs::remove_dir_all(&dir);
            return;
        }
        let text = format!("[cookies]\nfile = \"{}\"\n", path.display());
        assert_eq!(settings_of(&text).cookies, None);
        let notice = notices_of(&text).join(" / ");
        assert!(notice.contains(&path.display().to_string()), "{notice}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_read_only_cookie_file_is_refused_because_yt_dlp_writes_it_back() {
        use std::os::unix::fs::PermissionsExt;
        // yt-dlp は終了時に --cookies のファイルを開き直して書き戻す。読めるだけでは
        // 検索のたびに PermissionError で落ちるので、起動時に弾く。
        let (dir, path) = cookie_file("cookies-read-only");
        let mut perms = fs::metadata(&path).expect("メタデータ").permissions();
        perms.set_mode(0o444);
        fs::set_permissions(&path, perms).expect("権限を落とせる");
        // root は 0o444 でも書けるので、その環境では確かめない。
        if fs::OpenOptions::new().append(true).open(&path).is_ok() {
            let _ = fs::remove_dir_all(&dir);
            return;
        }
        let text = format!("[cookies]\nfile = \"{}\"\n", path.display());
        assert_eq!(settings_of(&text).cookies, None);
        let notice = notices_of(&text).join(" / ");
        assert!(notice.contains(&path.display().to_string()), "{notice}");
        assert!(notice.contains("書き込めません"), "{notice}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unusable_cookie_file_does_not_also_claim_the_file_is_in_use() {
        // 「file を使います」と「cookie 連携なしで動きます」が並ぶと矛盾して読める。
        let dir = temp_dir("cookies-both-missing");
        let path = dir.join("nope.txt");
        let text = format!(
            "[cookies]\nbrowser = \"chrome\"\nfile = \"{}\"\n",
            path.display()
        );
        let notices = notices_of(&text);
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert!(
            notices[0].contains("cookie 連携なしで動きます"),
            "{notices:?}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_cookie_file_starting_with_a_tilde_is_expanded_to_the_home_directory() {
        let home = std::env::var("HOME").expect("HOME");
        let text = "[cookies]\nfile = \"~/no-such-tuitube-cookies.txt\"\n";
        let notice = notices_of(text).join(" / ");
        assert!(
            notice.contains(&format!("{home}/no-such-tuitube-cookies.txt")),
            "{notice}"
        );
    }

    #[test]
    fn the_cookies_environment_variable_also_overrides_a_file() {
        let (dir, path) = cookie_file("cookies-env");
        let file =
            parse(&format!("[cookies]\nfile = \"{}\"\n", path.display())).expect("読めるはず");
        let with = |cookies| {
            validate(
                file.clone(),
                EnvOverrides {
                    cookies,
                    ..EnvOverrides::default()
                },
            )
            .settings
            .cookies
        };
        assert_eq!(with(Some("safari")), spec("safari"));
        assert_eq!(with(Some("none")), None);
        assert_eq!(with(None), file_source(&path));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn render_round_trips_a_cookie_file() {
        let (dir, path) = cookie_file("cookies-render");
        let settings = Settings {
            cookies: file_source(&path),
            ..Settings::default()
        };
        let text = render(&settings);
        assert!(
            text.contains(&format!("file = \"{}\"", path.display())),
            "{text}"
        );
        // browser 行は書き方の例のコメントに戻る。
        assert!(text.contains("# browser = \"chrome\""), "{text}");
        assert_eq!(settings_of(&text), settings);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn render_shows_how_to_write_the_cookie_file_key() {
        let text = render(&Settings::default());
        assert!(text.contains("# file = "), "{text}");
    }

    #[test]
    fn cookies_browser_is_read_into_a_cookie_source() {
        let text = "[cookies]\nbrowser = \"chrome:Profile 1\"\n";
        assert_eq!(settings_of(text).cookies, spec("chrome:Profile 1"));
        assert!(notices_of(text).is_empty());
    }

    #[test]
    fn a_blank_cookies_browser_is_ignored_with_a_notice() {
        for value in ["", "  "] {
            let text = format!("[cookies]\nbrowser = \"{value}\"\n");
            assert_eq!(settings_of(&text).cookies, None, "{text}");
            let notices = notices_of(&text);
            assert_eq!(notices.len(), 1, "{notices:?}");
            assert!(notices[0].contains("[cookies] browser"), "{notices:?}");
        }

        // 綴りの合っている他のキーを巻き添えにしない。
        let text = "[display]\nmode = \"window\"\n\n[cookies]\nbrowser = \"\"\n";
        assert_eq!(settings_of(text).display.mode, DisplayMode::Window);
        assert_eq!(settings_of(text).cookies, None);
    }

    #[test]
    fn an_absent_cookies_section_means_no_cookies() {
        assert_eq!(settings_of("").cookies, None);
        assert!(notices_of("").is_empty());
        assert_eq!(Settings::default().cookies, None);
    }

    #[test]
    fn the_cookies_environment_variable_overrides_the_file() {
        let file = parse("[cookies]\nbrowser = \"chrome\"\n").expect("読めるはず");
        let with = |cookies| {
            validate(
                file.clone(),
                EnvOverrides {
                    cookies,
                    ..EnvOverrides::default()
                },
            )
            .settings
            .cookies
        };
        assert_eq!(with(Some("safari")), spec("safari"));
        // 未設定・空はファイルの値をそのまま使う。
        for raw in [None, Some(""), Some("   ")] {
            assert_eq!(with(raw), spec("chrome"), "{raw:?}");
        }
        // none はファイルの値を無視して連携を切る。
        for raw in ["none", "NONE"] {
            assert_eq!(with(Some(raw)), None, "{raw}");
        }
    }

    #[test]
    fn parse_cookies_env_has_three_outcomes() {
        for raw in [None, Some(""), Some("   ")] {
            assert_eq!(parse_cookies_env(raw), None, "{raw:?}");
        }
        for raw in ["none", "NONE", " none "] {
            assert_eq!(parse_cookies_env(Some(raw)), Some(None), "{raw}");
        }
        assert_eq!(parse_cookies_env(Some(" firefox ")), Some(spec("firefox")));
    }

    #[test]
    fn render_writes_the_cookies_section_and_only_the_browser_spec() {
        let text = render(&Settings::default());
        assert!(text.contains("[cookies]"), "{text}");
        // 指定なしのときは書き方の例をコメントで出す。
        assert!(text.contains("# browser = \"chrome\""), "{text}");

        let settings = Settings {
            cookies: spec("chrome:Profile 1"),
            ..Settings::default()
        };
        let text = render(&settings);
        assert!(text.contains("browser = \"chrome:Profile 1\""), "{text}");
        // [cookies] セクションに入るのは browser だけ。
        let section = text
            .split("[cookies]")
            .nth(1)
            .expect("セクションがある")
            .split("\n[")
            .next()
            .expect("次のセクションまで");
        let keys: Vec<&str> = section
            .lines()
            .filter(|line| !line.trim_start().starts_with('#') && line.contains('='))
            .collect();
        assert_eq!(keys, ["browser = \"chrome:Profile 1\""], "{section}");
    }

    #[test]
    fn render_says_that_cookie_values_are_not_stored() {
        let text = render(&Settings::default());
        assert!(text.contains("cookie の値は保存しない"), "{text}");
        assert!(text.contains(crate::cookies::ENV_VAR), "{text}");
    }

    #[test]
    fn load_from_without_a_path_still_applies_the_cookies_variable() {
        let loaded = load_from(
            None,
            EnvOverrides {
                cookies: Some("chrome"),
                ..EnvOverrides::default()
            },
        );
        assert_eq!(loaded.settings.cookies, spec("chrome"));
        assert!(loaded.notice.is_none());
    }

    #[test]
    fn config_path_prefers_xdg_config_home_then_home() {
        assert_eq!(
            config_path(Some(OsStr::new("/x")), Some(OsStr::new("/h"))),
            Some(PathBuf::from("/x/tuitube/config.toml"))
        );
        assert_eq!(
            config_path(None, Some(OsStr::new("/h"))),
            Some(PathBuf::from("/h/.config/tuitube/config.toml"))
        );
        // 空文字は未設定と同じに扱う。
        assert_eq!(
            config_path(Some(OsStr::new("")), Some(OsStr::new("/h"))),
            Some(PathBuf::from("/h/.config/tuitube/config.toml"))
        );
        assert_eq!(config_path(None, None), None);
    }

    #[test]
    fn an_empty_file_yields_the_defaults_without_a_notice() {
        assert_eq!(settings_of(""), Settings::default());
        assert!(notices_of("").is_empty());
        assert_eq!(Settings::default().fps_cap, FpsCap::new(DEFAULT_FPS_CAP));
        assert_eq!(
            Settings::default().display.max_pixels(),
            Quality::Medium.max_pixels()
        );
    }

    #[test]
    fn unknown_keys_and_sections_are_ignored() {
        // 古い版が将来のキーを拒まないように。
        let text = "[display]\nfoo = 1\n\n[bar]\nx = 2\n";
        assert_eq!(settings_of(text), Settings::default());
        assert!(notices_of(text).is_empty());
    }

    #[test]
    fn a_toml_syntax_error_is_reported_with_the_line() {
        let error = parse("mode = ").expect_err("構文エラー");
        assert!(error.contains("line 1"), "{error}");
    }

    #[test]
    fn display_mode_text_is_parsed_and_rendered() {
        assert_eq!(
            settings_of("[display]\nmode = \"text\"\n").display.mode,
            DisplayMode::Text
        );
        assert!(notices_of("[display]\nmode = \"text\"\n").is_empty());

        // 往復で保たれる。
        let settings = Settings {
            display: DisplaySettings {
                mode: DisplayMode::Text,
                ..DisplaySettings::default()
            },
            ..Settings::default()
        };
        assert_eq!(settings_of(&render(&settings)), settings);
        // テンプレートのコメントに 3 つ目の選択肢が出る。
        assert!(
            render(&Settings::default()).contains("\"text\""),
            "{settings:?}"
        );
    }

    #[test]
    fn display_mode_and_quality_are_parsed_case_sensitively() {
        let settings = settings_of("[display]\nmode = \"window\"\nquality = \"high\"\n");
        assert_eq!(settings.display.mode, DisplayMode::Window);
        assert_eq!(settings.display.quality, Quality::High);
        // 大文字始まりは別の綴り。既定へ倒して notice を出す。
        let upper = "[display]\nmode = \"Window\"\n";
        assert_eq!(settings_of(upper).display.mode, DisplayMode::Embedded);
        assert_eq!(notices_of(upper).len(), 1);
    }

    #[test]
    fn an_unreadable_choice_only_affects_its_own_key() {
        // ファイル全体を既定へ倒すと、綴りの合っている他のキーまで効かなくなる。
        let text = "[display]\nmode = \"window\"\nquality = \"hi\"\nmax_frame_pixels = 300000\n\n[window]\nfocus_on = \"nevr\"\nautofit = \"640x360\"\n";
        let settings = settings_of(text);
        assert_eq!(settings.display.quality, Quality::Medium);
        assert_eq!(settings.window.focus_on, None);
        assert_eq!(settings.display.mode, DisplayMode::Window);
        assert_eq!(settings.display.max_frame_pixels, Some(300_000));
        assert_eq!(settings.window.autofit.as_deref(), Some("640x360"));

        let notices = notices_of(text);
        assert_eq!(notices.len(), 2, "{notices:?}");
        let joined = notices.join(" / ");
        // どのキーの、どの値が読めなかったかを出す。
        assert!(joined.contains("quality=\"hi\""), "{joined}");
        assert!(joined.contains("medium"), "{joined}");
        assert!(joined.contains("focus_on=\"nevr\""), "{joined}");
    }

    #[test]
    fn max_frame_pixels_overrides_quality_and_is_clamped() {
        let settings = settings_of("[display]\nquality = \"low\"\nmax_frame_pixels = 300000\n");
        assert_eq!(settings.display.max_pixels(), 300_000);

        let small = "[display]\nmax_frame_pixels = 1\n";
        assert_eq!(settings_of(small).display.max_pixels(), MIN_FRAME_PIXELS);
        assert_eq!(notices_of(small).len(), 1);

        let large = "[display]\nmax_frame_pixels = 10000000\n";
        assert_eq!(
            settings_of(large).display.max_pixels(),
            MAX_FRAME_PIXELS_LIMIT
        );
        assert_eq!(notices_of(large).len(), 1);
    }

    #[test]
    fn fps_cap_zero_disables_and_out_of_range_values_are_rounded_with_a_notice() {
        assert_eq!(settings_of("[playback]\nfps_cap = 0\n").fps_cap, None);
        assert_eq!(
            settings_of("[playback]\nfps_cap = 15\n").fps_cap,
            FpsCap::new(15)
        );
        assert!(notices_of("[playback]\nfps_cap = 15\n").is_empty());

        // 桁を打ち間違えた値を素通しすると、端末とパイプが飽和する。
        let over = "[playback]\nfps_cap = 121\n";
        assert_eq!(settings_of(over).fps_cap, FpsCap::new(MAX_FPS_CAP));
        let notice = notices_of(over).join(" / ");
        assert!(notice.contains("121") && notice.contains("120"), "{notice}");

        let negative = "[playback]\nfps_cap = -5\n";
        assert_eq!(
            settings_of(negative).fps_cap,
            FpsCap::new(DEFAULT_FPS_CAP),
            "読めない値は既定へ倒す"
        );
        let notice = notices_of(negative).join(" / ");
        assert!(notice.contains("-5") && notice.contains("15"), "{notice}");
    }

    #[test]
    fn the_environment_variable_overrides_the_file() {
        let file = parse("[playback]\nfps_cap = 30\n").expect("読めるはず");
        let env = |fps_limit| EnvOverrides {
            fps_limit,
            ..EnvOverrides::default()
        };
        assert_eq!(
            validate(file.clone(), env(Some("unlimited")))
                .settings
                .fps_cap,
            None
        );
        assert_eq!(
            validate(file.clone(), env(Some("10"))).settings.fps_cap,
            FpsCap::new(10)
        );
        // 読めない指定でファイルの値を巻き添えにしない。
        let validated = validate(file, env(Some("3O")));
        assert_eq!(validated.settings.fps_cap, FpsCap::new(30));
        assert!(
            validated.notices.join(" / ").contains("3O"),
            "{:?}",
            validated.notices
        );
    }

    #[test]
    fn the_file_value_is_kept_for_whatever_the_environment_overrode() {
        // 環境変数は一時的な指定なので、保存でファイルへ焼き付けない。
        let file = parse("[playback]\nfps_cap = 30\n\n[cookies]\nbrowser = \"chrome\"\n")
            .expect("読めるはず");
        let validated = validate(
            file,
            EnvOverrides {
                fps_limit: Some("60"),
                cookies: Some("none"),
            },
        );
        assert_eq!(validated.settings.fps_cap, FpsCap::new(60), "実行中は 60");
        assert_eq!(validated.settings.cookies, None, "実行中は連携なし");

        let overridden = validated.overridden;
        assert_eq!(overridden.keys(), ["fps_cap", "cookies.browser"]);
        let to_save = overridden.restore(&validated.settings);
        assert_eq!(to_save.fps_cap, FpsCap::new(30), "ファイルの値へ戻す");
        assert_eq!(to_save.cookies, spec("chrome"));
    }

    #[test]
    fn the_held_key_is_named_after_the_value_that_goes_back_into_the_file() {
        // 環境変数は browser 用でも、書き戻すのはファイルが持っていた file の指定。
        let (dir, path) = cookie_file("cookies-env-key");
        let file =
            parse(&format!("[cookies]\nfile = \"{}\"\n", path.display())).expect("読めるはず");
        let validated = validate(
            file,
            EnvOverrides {
                cookies: Some("chrome"),
                ..EnvOverrides::default()
            },
        );
        assert_eq!(
            validated.settings.cookies,
            spec("chrome"),
            "実行中は chrome"
        );
        assert_eq!(validated.overridden.keys(), ["cookies.file"]);
        assert_eq!(
            validated.overridden.restore(&validated.settings).cookies,
            file_source(&path)
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_saved_file_keeps_the_lines_the_variables_are_holding() {
        // 読み込み → 編集 → 保存まで通しで見る。環境変数の指定がファイルに残らない。
        let dir = temp_dir("env-round-trip");
        let path = dir.join("config.toml");
        fs::write(
            &path,
            "[playback]\nfps_cap = 30\n\n[cookies]\nbrowser = \"chrome\"\n",
        )
        .expect("書ける");

        let loaded = load_from(
            Some(&path),
            EnvOverrides {
                fps_limit: Some("60"),
                cookies: Some("none"),
            },
        );
        let mut edited = loaded.settings.clone();
        edited.search.limit = 25;
        save_to(&path, &loaded.overridden.restore(&edited)).expect("保存できる");

        let written = fs::read_to_string(&path).expect("読める");
        let reread = validate(
            parse(&written).expect("読めるはず"),
            EnvOverrides::default(),
        );
        assert_eq!(reread.settings.fps_cap, FpsCap::new(30), "{written}");
        assert_eq!(reread.settings.cookies, spec("chrome"), "{written}");
        assert_eq!(reread.settings.search.limit, 25, "編集は保存されている");
    }

    #[test]
    fn nothing_is_marked_overridden_without_the_variables() {
        let validated = validate(
            parse("[playback]\nfps_cap = 30\n").expect("読めるはず"),
            EnvOverrides::default(),
        );
        assert!(validated.overridden.is_empty());
        assert!(validated.overridden.keys().is_empty());
        // 何も上書きされていなければ、保存する値は今の設定のまま。
        assert_eq!(
            validated.overridden.restore(&validated.settings),
            validated.settings
        );
    }

    #[test]
    fn an_edited_value_still_reaches_the_file_beside_an_overridden_one() {
        // 上書きされているのは fps_cap だけ。他の行の編集は保存される。
        let validated = validate(
            parse("[playback]\nfps_cap = 30\n").expect("読めるはず"),
            EnvOverrides {
                fps_limit: Some("60"),
                ..EnvOverrides::default()
            },
        );
        let mut edited = validated.settings.clone();
        edited.search.limit = 25;
        let to_save = validated.overridden.restore(&edited);
        assert_eq!(to_save.search.limit, 25);
        assert_eq!(to_save.fps_cap, FpsCap::new(30));
    }

    #[test]
    fn the_environment_variable_is_unset_or_empty() {
        for raw in [None, Some(""), Some("   ")] {
            assert_eq!(parse_fps_limit_env(raw), (None, None), "{raw:?}");
        }
    }

    #[test]
    fn the_environment_variable_reads_an_explicit_value_without_a_notice() {
        for (raw, fps) in [("30", 30), (" 24 ", 24), ("120", MAX_FPS_CAP)] {
            assert_eq!(
                parse_fps_limit_env(Some(raw)),
                (Some(Some(fps)), None),
                "{raw}"
            );
        }
    }

    #[test]
    fn the_environment_variable_is_disabled_by_zero_or_unlimited() {
        for raw in ["0", "unlimited", "UNLIMITED"] {
            assert_eq!(parse_fps_limit_env(Some(raw)), (Some(None), None), "{raw}");
        }
    }

    #[test]
    fn the_environment_variable_clamps_a_value_above_the_maximum_and_says_so() {
        for raw in ["121", "4294967295"] {
            let (limit, notice) = parse_fps_limit_env(Some(raw));
            assert_eq!(limit, Some(Some(MAX_FPS_CAP)), "{raw} は丸めるはず");
            let notice = notice.expect("丸めた旨を出す");
            assert!(notice.contains(raw) && notice.contains("120"), "{notice}");
        }
    }

    #[test]
    fn the_environment_variable_is_ignored_when_it_cannot_be_read() {
        // 3O のような打ち間違いが黙って無視されると、効かない理由に気づけない。
        for raw in ["abc", "3O", "-5", "12.5", "99999999999999999999"] {
            let (limit, notice) = parse_fps_limit_env(Some(raw));
            assert_eq!(limit, None, "{raw} は上書きしない");
            let notice = notice.expect("読めなかった旨を出す");
            assert!(notice.contains(raw), "{notice}");
            assert!(notice.contains(FPS_LIMIT_VAR), "{notice}");
        }
    }

    #[test]
    fn window_options_require_non_empty_strings() {
        let text = "[window]\nautofit = \"\"\ngeometry = \"50%+0+0\"\n";
        let settings = settings_of(text);
        assert_eq!(settings.window.autofit, None);
        assert_eq!(settings.window.geometry.as_deref(), Some("50%+0+0"));
        assert_eq!(notices_of(text).len(), 1);
    }

    #[test]
    fn the_window_is_not_fullscreen_unless_asked() {
        assert!(!Settings::default().window.fullscreen);
        assert!(
            render(&Settings::default()).contains("# fullscreen = false\n"),
            "既定は書き出しでもコメントのまま"
        );

        let text = "[window]\nfullscreen = true\n";
        assert!(settings_of(text).window.fullscreen);
        assert_eq!(notices_of(text), Vec::<String>::new());
    }

    #[test]
    fn render_writes_the_fullscreen_flag_when_it_is_on() {
        let settings = Settings {
            window: WindowOptions {
                fullscreen: true,
                ..WindowOptions::default()
            },
            ..Settings::default()
        };
        let text = render(&settings);
        assert!(text.contains("fullscreen = true\n"), "{text}");
        assert!(!text.contains("# fullscreen"), "{text}");
        assert!(settings_of(&text).window.fullscreen);
    }

    #[test]
    fn extra_args_that_do_not_look_like_options_are_passed_with_a_notice() {
        let text = "[mpv]\nextra_args = [\"--hwdec=no\", \"foo\"]\n";
        assert_eq!(settings_of(text).extra_args, ["--hwdec=no", "foo"]);
        assert_eq!(notices_of(text).len(), 1);
    }

    #[test]
    fn render_round_trips_through_parse() {
        let default = Settings::default();
        assert_eq!(settings_of(&render(&default)), default);

        let custom = Settings {
            display: DisplaySettings {
                mode: DisplayMode::Window,
                quality: Quality::High,
                max_frame_pixels: Some(300_000),
            },
            fps_cap: None,
            window: WindowOptions {
                vo: Some("gpu-next".to_string()),
                autofit: Some("640x360".to_string()),
                geometry: Some("50%+0+0".to_string()),
                ontop: true,
                fullscreen: true,
                focus_on: Some(FocusOn::Never),
                title: Some("窓".to_string()),
            },
            extra_args: vec!["--hwdec=videotoolbox-copy".to_string()],
            cookies: spec("chrome:Profile 1"),
            ..Settings::default()
        };
        assert_eq!(settings_of(&render(&custom)), custom);
    }

    #[test]
    fn render_contains_the_comment_about_being_rewritten() {
        let text = render(&Settings::default());
        assert!(text.starts_with('#'), "{text}");
        assert!(text.contains("自分で書いたコメントは残らない"), "{text}");
    }

    #[test]
    fn load_from_a_missing_path_creates_the_template_and_says_so() {
        let dir = temp_dir("missing");
        let path = dir.join("nested/config.toml");
        let loaded = load_from(Some(&path), EnvOverrides::default());

        assert_eq!(loaded.settings, Settings::default());
        assert!(path.exists(), "親ディレクトリごと作る");
        let notice = loaded.notice.expect("作った旨を出す");
        assert!(notice.contains(&path.display().to_string()), "{notice}");
        assert_eq!(
            fs::read_to_string(&path).expect("読める"),
            render(&Settings::default())
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_display_mode_to_rewrites_only_the_mode_line() {
        let dir = temp_dir("save-mode-line");
        let path = dir.join("config.toml");
        // 手で書いたコメント・丸められる値・tuitube が持たないキーを混ぜておく。
        let before = "# 自分のメモ\n[display]\n# 表示\nmode = \"embedded\"\nquality = \"high\"\n\n[search]\nlimit = 5000\nmy_key = \"残る\"\n";
        fs::write(&path, before).expect("書ける");

        save_display_mode_to(&path, DisplayMode::Window).expect("保存できる");

        assert_eq!(
            fs::read_to_string(&path).expect("読める"),
            before.replace("\"embedded\"", "\"window\"")
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_display_mode_to_adds_the_line_when_it_is_missing() {
        let dir = temp_dir("save-mode-add");
        let path = dir.join("config.toml");
        for (before, after) in [
            (
                "[display]\nquality = \"low\"\n",
                "[display]\nmode = \"text\"\nquality = \"low\"\n",
            ),
            // mode を持つ別の節があっても [display] の側へ書く。
            (
                "[search]\nlimit = 5\n",
                "[search]\nlimit = 5\n\n[display]\nmode = \"text\"\n",
            ),
            (
                "[display]\n# mode = \"window\"\n",
                "[display]\nmode = \"text\"\n# mode = \"window\"\n",
            ),
        ] {
            fs::write(&path, before).expect("書ける");
            save_display_mode_to(&path, DisplayMode::Text).expect("保存できる");
            assert_eq!(
                fs::read_to_string(&path).expect("読める"),
                after,
                "{before}"
            );
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_display_mode_to_creates_the_template_when_there_is_no_file() {
        let dir = temp_dir("save-mode-new");
        let path = dir.join("nested/config.toml");

        save_display_mode_to(&path, DisplayMode::Text).expect("保存できる");

        let expected = Settings {
            display: DisplaySettings {
                mode: DisplayMode::Text,
                ..DisplaySettings::default()
            },
            ..Settings::default()
        };
        assert_eq!(
            fs::read_to_string(&path).expect("読める"),
            render(&expected),
            "親ディレクトリごと作る"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_display_mode_to_keeps_the_file_when_the_line_cannot_be_placed() {
        let dir = temp_dir("save-mode-dotted");
        let path = dir.join("config.toml");
        // dotted key の display は節として現れないので、足すと重複キーになる。
        let before = "display.mode = \"embedded\"\n";
        fs::write(&path, before).expect("書ける");

        let error = save_display_mode_to(&path, DisplayMode::Text).expect_err("書き換えられない");

        assert!(error.contains("display.mode"), "{error}");
        assert_eq!(fs::read_to_string(&path).expect("読める"), before);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_from_a_broken_file_falls_back_to_defaults_and_keeps_the_file() {
        let dir = temp_dir("broken");
        let path = dir.join("config.toml");
        // 構文エラーはキー単位で切り分けられないので、全体を既定値に倒す。
        let broken = "[display]\nmode = \n";
        fs::write(&path, broken).expect("書ける");
        let loaded = load_from(Some(&path), EnvOverrides::default());

        assert_eq!(loaded.settings, Settings::default());
        let notice = loaded.notice.expect("読めなかった旨を出す");
        assert!(notice.contains("既定値で動きます"), "{notice}");
        // 直せるよう、読めなかったファイルは書き換えない。
        assert_eq!(fs::read_to_string(&path).expect("読める"), broken);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_from_without_a_path_still_applies_the_environment_variable() {
        let loaded = load_from(
            None,
            EnvOverrides {
                fps_limit: Some("0"),
                ..EnvOverrides::default()
            },
        );
        assert_eq!(loaded.settings.fps_cap, None);
        assert!(loaded.notice.is_none());
    }

    #[test]
    fn search_layout_and_limit_are_read() {
        let settings = settings_of("[search]\nlayout = \"list\"\nlimit = 25\n");
        assert_eq!(settings.search.layout, LayoutMode::List);
        assert_eq!(settings.search.limit, 25);
        assert!(notices_of("[search]\nlayout = \"list\"\nlimit = 25\n").is_empty());

        assert_eq!(settings_of("").search, SearchSettings::default());
        assert_eq!(SearchSettings::default().limit, DEFAULT_SEARCH_LIMIT);
        assert_eq!(SearchSettings::default().layout, LayoutMode::Grid);
    }

    #[test]
    fn an_unreadable_search_layout_falls_back_to_grid_with_a_notice() {
        let text = "[search]\nlayout = \"tiles\"\nlimit = 5\n";
        assert_eq!(settings_of(text).search.layout, LayoutMode::Grid);
        // 綴りの合っている limit は巻き添えにしない。
        assert_eq!(settings_of(text).search.limit, 5);
        let notices = notices_of(text);
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert!(notices[0].contains("layout=\"tiles\""), "{notices:?}");
    }

    #[test]
    fn the_search_limit_upper_bound_is_1000() {
        assert_eq!(MAX_SEARCH_LIMIT, 1000);
        let text = "[search]\nlimit = 1000\n";
        assert_eq!(settings_of(text).search.limit, 1000);
        assert!(notices_of(text).is_empty(), "上限ちょうどは丸めない");
    }

    #[test]
    fn a_search_limit_outside_the_range_is_rounded_with_a_notice() {
        for (raw, expected) in [(0, MIN_SEARCH_LIMIT), (1001, MAX_SEARCH_LIMIT), (-3, 1)] {
            let text = format!("[search]\nlimit = {raw}\n");
            assert_eq!(settings_of(&text).search.limit, expected, "{text}");
            let notice = notices_of(&text).join(" / ");
            assert!(notice.contains(&raw.to_string()), "{notice}");
        }
    }

    #[test]
    fn the_search_timeout_is_read() {
        let text = "[search]\ntimeout_secs = 90\n";
        assert_eq!(settings_of(text).search.timeout, Duration::from_secs(90));
        assert!(notices_of(text).is_empty());
    }

    #[test]
    fn an_absent_search_timeout_keeps_the_previous_30_seconds() {
        assert_eq!(settings_of("").search.timeout, Duration::from_secs(30));
        assert_eq!(
            SearchSettings::default().timeout,
            crate::search::YT_DLP_TIMEOUT
        );
        // timeout_secs を知らない頃の設定ファイルも、これまでと同じ 30 秒で動く。
        assert_eq!(
            settings_of("[search]\nlayout = \"list\"\nlimit = 50\n")
                .search
                .timeout,
            Duration::from_secs(30)
        );
    }

    #[test]
    fn a_search_timeout_outside_the_range_is_rounded_with_a_notice() {
        for (raw, expected) in [
            (0, MIN_SEARCH_TIMEOUT_SECS),
            (4, MIN_SEARCH_TIMEOUT_SECS),
            (-10, MIN_SEARCH_TIMEOUT_SECS),
            (301, MAX_SEARCH_TIMEOUT_SECS),
            (100_000, MAX_SEARCH_TIMEOUT_SECS),
        ] {
            let text = format!("[search]\ntimeout_secs = {raw}\n");
            assert_eq!(
                settings_of(&text).search.timeout,
                Duration::from_secs(expected),
                "{text}"
            );
            let notice = notices_of(&text).join(" / ");
            assert!(notice.contains("[search] timeout_secs"), "{notice}");
            assert!(notice.contains(&raw.to_string()), "{notice}");
        }

        // 端ちょうどは丸めない。
        for secs in [MIN_SEARCH_TIMEOUT_SECS, MAX_SEARCH_TIMEOUT_SECS] {
            let text = format!("[search]\ntimeout_secs = {secs}\n");
            assert_eq!(settings_of(&text).search.timeout, Duration::from_secs(secs));
            assert!(notices_of(&text).is_empty(), "{text}");
        }
    }

    #[test]
    fn the_search_timeout_is_separate_from_the_thumbnail_timeout() {
        // 同じキー名が 2 つのセクションにあるので、取り違えていないか確かめる。
        let text = "[search]\ntimeout_secs = 120\n\n[thumbnails]\ntimeout_secs = 7\n";
        let settings = settings_of(text);
        assert_eq!(settings.search.timeout, Duration::from_secs(120));
        assert_eq!(settings.thumbnails.timeout, Duration::from_secs(7));
        assert!(notices_of(text).is_empty());
    }

    #[test]
    fn render_round_trips_the_search_timeout() {
        let custom = Settings {
            search: SearchSettings {
                timeout: Duration::from_secs(120),
                ..SearchSettings::default()
            },
            ..Settings::default()
        };
        let text = render(&custom);
        assert!(text.contains("timeout_secs = 120"), "{text}");
        assert_eq!(settings_of(&text), custom);

        // 既定値もそのまま書き出して読み戻せる。
        let text = render(&Settings::default());
        assert!(text.contains("timeout_secs = 30"), "{text}");
        assert!(
            text.contains(&format!(
                "{MIN_SEARCH_TIMEOUT_SECS}..={MAX_SEARCH_TIMEOUT_SECS}"
            )),
            "{text}"
        );
    }

    #[test]
    fn the_search_timeout_comment_matches_what_happens_on_a_timeout() {
        let text = render(&Settings::default());
        // 超えたときは 0 件でなくエラーが出る。探す手がかりはその文言。
        assert!(!text.contains("0 件"), "{text}");
        assert!(text.contains("検索がタイムアウトしました"), "{text}");
        // 縛るのは yt-dlp 1 回ぶんで、cookie の読み取りに失敗すると 2 回ぶん待つ。
        assert!(text.contains("2 回"), "{text}");
    }

    #[test]
    fn the_search_cache_is_off_with_a_five_minute_ttl_by_default() {
        let settings = settings_of("");
        assert!(!settings.search.cache_enabled);
        assert_eq!(
            settings.search.cache_ttl,
            Duration::from_secs(DEFAULT_SEARCH_CACHE_TTL_SECS)
        );
        // キャッシュを知らない頃の設定ファイルも、これまでどおり毎回取り直す。
        let old = settings_of("[search]\nlayout = \"list\"\nlimit = 50\n").search;
        assert!(!old.cache_enabled);
        assert_eq!(
            old.cache_ttl,
            Duration::from_secs(DEFAULT_SEARCH_CACHE_TTL_SECS)
        );
    }

    #[test]
    fn the_search_cache_settings_are_read() {
        let text = "[search]\ncache_enabled = true\ncache_ttl_secs = 60\n";
        let search = settings_of(text).search;
        assert!(search.cache_enabled);
        assert_eq!(search.cache_ttl, Duration::from_secs(60));
        assert!(notices_of(text).is_empty());
    }

    #[test]
    fn a_search_cache_ttl_outside_the_range_is_rounded_with_a_notice() {
        for (raw, expected) in [
            (0, MIN_SEARCH_CACHE_TTL_SECS),
            (9, MIN_SEARCH_CACHE_TTL_SECS),
            (-30, MIN_SEARCH_CACHE_TTL_SECS),
            (3601, MAX_SEARCH_CACHE_TTL_SECS),
            (100_000, MAX_SEARCH_CACHE_TTL_SECS),
        ] {
            let text = format!("[search]\ncache_ttl_secs = {raw}\n");
            assert_eq!(
                settings_of(&text).search.cache_ttl,
                Duration::from_secs(expected),
                "{text}"
            );
            let notice = notices_of(&text).join(" / ");
            assert!(notice.contains("[search] cache_ttl_secs"), "{notice}");
            assert!(notice.contains(&raw.to_string()), "{notice}");
        }

        // 端ちょうどは丸めない。
        for secs in [MIN_SEARCH_CACHE_TTL_SECS, MAX_SEARCH_CACHE_TTL_SECS] {
            let text = format!("[search]\ncache_ttl_secs = {secs}\n");
            assert_eq!(
                settings_of(&text).search.cache_ttl,
                Duration::from_secs(secs)
            );
            assert!(notices_of(&text).is_empty(), "{text}");
        }
    }

    #[test]
    fn render_round_trips_the_search_cache() {
        let custom = Settings {
            search: SearchSettings {
                cache_enabled: true,
                cache_ttl: Duration::from_secs(60),
                ..SearchSettings::default()
            },
            ..Settings::default()
        };
        let text = render(&custom);
        assert!(text.contains("cache_enabled = true"), "{text}");
        assert!(text.contains("cache_ttl_secs = 60"), "{text}");
        assert_eq!(settings_of(&text), custom);

        let text = render(&Settings::default());
        assert!(text.contains("cache_enabled = false"), "{text}");
        assert!(
            text.contains(&format!("cache_ttl_secs = {DEFAULT_SEARCH_CACHE_TTL_SECS}")),
            "{text}"
        );
        assert!(
            text.contains(&format!(
                "{MIN_SEARCH_CACHE_TTL_SECS}..={MAX_SEARCH_CACHE_TTL_SECS}"
            )),
            "{text}"
        );
    }

    #[test]
    fn thumbnail_settings_are_read() {
        let text = "[thumbnails]\nenabled = false\ncache_dir = \"/tmp/thumbs\"\nmax_cached = 20\ntimeout_secs = 5\n";
        let thumbnails = settings_of(text).thumbnails;
        assert!(!thumbnails.enabled);
        assert_eq!(thumbnails.cache_dir, Some(PathBuf::from("/tmp/thumbs")));
        assert_eq!(thumbnails.max_cached, 20);
        assert_eq!(thumbnails.timeout, Duration::from_secs(5));
        assert!(notices_of(text).is_empty());

        // 未指定なら既定。置き場は環境変数から決める。
        assert_eq!(settings_of("").thumbnails, ThumbnailSettings::default());
        assert_eq!(ThumbnailSettings::default().cache_dir, None);
    }

    #[test]
    fn unreadable_thumbnail_values_are_rounded_with_a_notice() {
        let text = "[thumbnails]\nmax_cached = -1\ntimeout_secs = 0\ncache_dir = \"  \"\n";
        let thumbnails = settings_of(text).thumbnails;
        assert_eq!(thumbnails.max_cached, DEFAULT_MAX_CACHED);
        assert_eq!(thumbnails.timeout, Duration::from_secs(1));
        assert_eq!(thumbnails.cache_dir, None);
        assert_eq!(notices_of(text).len(), 3, "{:?}", notices_of(text));

        let over = "[thumbnails]\ntimeout_secs = 600\n";
        assert_eq!(
            settings_of(over).thumbnails.timeout,
            Duration::from_secs(MAX_THUMB_TIMEOUT_SECS)
        );
        assert_eq!(notices_of(over).len(), 1);
    }

    #[test]
    fn a_cache_dir_starting_with_a_tilde_is_expanded_to_the_home_directory() {
        let home = std::env::var("HOME").expect("HOME");
        let text = "[thumbnails]\ncache_dir = \"~/.cache/tuitube/thumbs\"\n";
        assert_eq!(
            settings_of(text).thumbnails.cache_dir,
            Some(PathBuf::from(format!("{home}/.cache/tuitube/thumbs")))
        );
    }

    #[test]
    fn engagement_settings_are_read() {
        let text = "[engagement]\nenabled = false\nttl_secs = 3600\nmax_concurrent_requests = 5\n";
        let engagement = settings_of(text).engagement;
        assert!(!engagement.enabled);
        assert_eq!(engagement.ttl, Duration::from_secs(3_600));
        assert_eq!(engagement.max_concurrent_requests, 5);
        assert!(notices_of(text).is_empty());

        // 未指定なら既定 (1 週間・同時 3 本)。
        let defaults = settings_of("").engagement;
        assert_eq!(defaults, EngagementSettings::default());
        assert!(defaults.enabled);
        assert_eq!(
            defaults.ttl,
            Duration::from_secs(DEFAULT_ENGAGEMENT_TTL_SECS)
        );
        assert_eq!(
            defaults.max_concurrent_requests,
            DEFAULT_ENGAGEMENT_CONCURRENCY
        );
    }

    #[test]
    fn unreadable_engagement_values_are_rounded_with_a_notice() {
        let text = "[engagement]\nttl_secs = 0\nmax_concurrent_requests = 0\n";
        let engagement = settings_of(text).engagement;
        assert_eq!(engagement.ttl, Duration::from_secs(MIN_ENGAGEMENT_TTL_SECS));
        assert_eq!(
            engagement.max_concurrent_requests,
            MIN_ENGAGEMENT_CONCURRENCY
        );
        assert_eq!(notices_of(text).len(), 2, "{:?}", notices_of(text));

        let over = "[engagement]\nttl_secs = 99999999\nmax_concurrent_requests = 100\n";
        let engagement = settings_of(over).engagement;
        assert_eq!(engagement.ttl, Duration::from_secs(MAX_ENGAGEMENT_TTL_SECS));
        assert_eq!(
            engagement.max_concurrent_requests,
            MAX_ENGAGEMENT_CONCURRENCY
        );
        assert_eq!(notices_of(over).len(), 2, "{:?}", notices_of(over));
    }

    #[test]
    fn render_round_trips_the_engagement_section() {
        let custom = Settings {
            engagement: EngagementSettings {
                enabled: false,
                ttl: Duration::from_secs(3_600),
                max_concurrent_requests: 5,
            },
            ..Settings::default()
        };
        assert_eq!(settings_of(&render(&custom)), custom);

        let text = render(&Settings::default());
        assert!(text.contains("[engagement]"), "{text}");
        assert!(notices_of(&text).is_empty(), "{:?}", notices_of(&text));
    }

    #[test]
    fn categories_replace_the_defaults_when_listed() {
        let text = "[[categories]]\nlabel = \"将棋\"\nquery = \"将棋 対局\"\n\n[[categories]]\nlabel = \"おすすめ\"\nquery = \":ytrec\"\n";
        assert_eq!(
            settings_of(text).categories,
            [
                Category::new("将棋", "将棋 対局"),
                Category::new("おすすめ", ":ytrec"),
            ]
        );
        assert!(notices_of(text).is_empty());
        assert_eq!(settings_of("").categories, default_categories());
    }

    #[test]
    fn a_misspelled_feed_keyword_gets_a_notice_with_the_right_ones() {
        // ":ytwatch_later" は Feed::parse に一致せず、検索語として 0 件で返るだけになる。
        let text = "[[categories]]\nlabel = \"後で見る\"\nquery = \":ytwatch_later\"\n";
        assert_eq!(
            settings_of(text).categories,
            [Category::new("後で見る", ":ytwatch_later")]
        );
        let notice = notices_of(text).join(" / ");
        assert!(notice.contains(":ytwatch_later"), "{notice}");
        for feed in Feed::ALL {
            assert!(notice.contains(feed.keyword()), "{notice}");
        }

        // 正しいキーワードと、ただの検索語には出さない。
        for query in [":ytrec", ":ythistory", "将棋"] {
            let text = format!("[[categories]]\nlabel = \"x\"\nquery = \"{query}\"\n");
            assert!(notices_of(&text).is_empty(), "{query}");
        }
    }

    #[test]
    fn an_empty_category_list_falls_back_to_the_defaults_with_a_notice() {
        let text = "categories = []\n";
        assert_eq!(settings_of(text).categories, default_categories());
        assert_eq!(notices_of(text).len(), 1, "{:?}", notices_of(text));
    }

    #[test]
    fn a_category_without_a_label_or_query_is_dropped_with_a_notice() {
        let text = "[[categories]]\nlabel = \"将棋\"\nquery = \"将棋\"\n\n[[categories]]\nlabel = \"\"\nquery = \"x\"\n\n[[categories]]\nlabel = \"y\"\n";
        assert_eq!(
            settings_of(text).categories,
            [Category::new("将棋", "将棋")]
        );
        let notice = notices_of(text).join(" / ");
        assert!(notice.contains("2 件"), "{notice}");
    }

    #[test]
    fn render_round_trips_the_new_sections() {
        let custom = Settings {
            search: SearchSettings {
                layout: LayoutMode::List,
                limit: 30,
                timeout: Duration::from_secs(45),
                cache_enabled: true,
                cache_ttl: Duration::from_secs(45),
            },
            thumbnails: ThumbnailSettings {
                enabled: false,
                cache_dir: Some(PathBuf::from("/tmp/th")),
                max_cached: 12,
                timeout: Duration::from_secs(7),
            },
            categories: vec![Category::new("将棋", "将棋 対局")],
            ..Settings::default()
        };
        assert_eq!(settings_of(&render(&custom)), custom);

        // 既定のカテゴリは書き方の例をコメントで出す。
        let text = render(&Settings::default());
        assert!(text.contains("# [[categories]]"), "{text}");
        assert!(
            text.contains("[search]") && text.contains("[thumbnails]"),
            "{text}"
        );
        // 覚えていないと打てないフィードのキーワードも、書き戻せるよう並べる。
        for feed in Feed::ALL {
            assert!(text.contains(feed.keyword()), "{}: {text}", feed.keyword());
        }
    }

    #[test]
    fn subtitles_default_to_enabled_japanese_without_a_notice() {
        let defaults = settings_of("");
        assert!(defaults.subtitles.enabled);
        assert_eq!(defaults.subtitles.lang, SubLang::default());
        assert_eq!(defaults.subtitles, SubtitleSettings::default());
        assert!(notices_of("").is_empty());
    }

    #[test]
    fn subtitles_can_be_turned_off_and_given_a_language() {
        let text = "[subtitles]\nenabled = false\nlang = \"en\"\n";
        let settings = settings_of(text);
        assert!(!settings.subtitles.enabled);
        assert_eq!(settings.subtitles.lang.as_str(), "en");
        assert!(notices_of(text).is_empty());
    }

    #[test]
    fn an_unreadable_subtitle_language_falls_back_without_touching_other_sections() {
        // all は yt-dlp の全言語指定で、渡すと字幕トラックが 157 本載る。
        for raw in ["all", "", "ja*"] {
            let text = format!("[subtitles]\nlang = \"{raw}\"\n[search]\nlimit = 20\n");
            let settings = settings_of(&text);
            assert_eq!(settings.subtitles.lang, SubLang::default(), "{text}");
            assert!(settings.subtitles.enabled, "{text}");
            // 読めないキーだけを落とす。
            assert_eq!(settings.search.limit, 20, "{text}");

            let notice = notices_of(&text).join(" / ");
            assert!(notice.contains("[subtitles] lang"), "{notice}");
            assert!(notice.contains(SubLang::DEFAULT), "{notice}");
        }
    }

    #[test]
    fn render_round_trips_the_subtitle_section() {
        let custom = Settings {
            subtitles: SubtitleSettings {
                enabled: false,
                lang: SubLang::parse("ja,en").expect("言語コード"),
            },
            ..Settings::default()
        };
        assert_eq!(settings_of(&render(&custom)), custom);
        assert_eq!(
            settings_of(&render(&Settings::default())),
            Settings::default()
        );

        let text = render(&Settings::default());
        assert!(text.contains("[subtitles]"), "{text}");
        assert!(text.contains("enabled = true"), "{text}");
        assert!(
            text.contains(&format!("lang = \"{}\"", SubLang::DEFAULT)),
            "{text}"
        );
        // 書き方の例を出す。lang は言語コードしか受け付けない。
        assert!(text.contains("カンマ区切り"), "{text}");
    }

    #[test]
    fn save_to_replaces_atomically() {
        let dir = temp_dir("save");
        let path = dir.join("config.toml");
        fs::write(&path, "古い内容").expect("書ける");
        let settings = Settings {
            fps_cap: FpsCap::new(30),
            ..Settings::default()
        };
        save_to(&path, &settings).expect("保存できる");

        assert_eq!(
            fs::read_to_string(&path).expect("読める"),
            render(&settings)
        );
        let left: Vec<_> = fs::read_dir(&dir)
            .expect("一覧")
            .filter_map(Result::ok)
            .map(|e| e.file_name())
            .collect();
        assert_eq!(left.len(), 1, "一時ファイルが残っている: {left:?}");
        let _ = fs::remove_dir_all(&dir);
    }
}
