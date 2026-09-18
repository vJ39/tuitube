//! 設定ファイル (TOML) のパス決定・読み込み・検証・テンプレート生成・保存。

use crate::display::{DisplayMode, FocusOn, FpsCap, Quality, WindowOptions};
use serde::{Deserialize, Serialize};
use std::ffi::OsStr;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

/// fps 上限を一時的に上書きする環境変数。0 か unlimited で制限を外す。
pub const FPS_LIMIT_VAR: &str = "TUITUBE_FPS_LIMIT";
/// fps 上限の既定値。kitty 出力は 1 フレームごとに画素を CPU で作るため、
/// 制限しないと再生が重くなる (実測 CPU 122% → 40%)。
pub const DEFAULT_FPS_CAP: u32 = 15;
/// 受け付ける上限値。桁を打ち間違えた値をそのまま渡すと端末とパイプが詰まる。
pub const MAX_FPS_CAP: u32 = 120;
pub const MIN_FRAME_PIXELS: u32 = 64 * 36;
pub const MAX_FRAME_PIXELS_LIMIT: u32 = 3840 * 2160;

const CONFIG_FILE: &str = "config.toml";
const APP_DIR: &str = "tuitube";

/// ファイルの生の形。全キー任意。未知のキーは無視する。
#[derive(Debug, Default, Clone, PartialEq, Deserialize, Serialize)]
pub struct RawConfig {
    pub display: Option<RawDisplay>,
    pub playback: Option<RawPlayback>,
    pub window: Option<RawWindow>,
    pub mpv: Option<RawMpv>,
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
    pub focus_on: Option<String>,
    pub title: Option<String>,
}

#[derive(Debug, Default, Clone, PartialEq, Deserialize, Serialize)]
pub struct RawPlayback {
    /// 0 は制限なし。負値・120 超は丸めて notice を出す。
    pub fps_cap: Option<i64>,
}

#[derive(Debug, Default, Clone, PartialEq, Deserialize, Serialize)]
pub struct RawMpv {
    pub extra_args: Option<Vec<String>>,
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

/// 検証済みの値。App が持つのはこれ。
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub display: DisplaySettings,
    /// None は制限なし。
    pub fps_cap: Option<FpsCap>,
    pub window: WindowOptions,
    pub extra_args: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            display: DisplaySettings::default(),
            fps_cap: FpsCap::new(DEFAULT_FPS_CAP),
            window: WindowOptions::default(),
            extra_args: Vec::new(),
        }
    }
}

/// 読み込み結果。notice は利用者に見せる 1 行。
#[derive(Debug, Clone, PartialEq)]
pub struct Loaded {
    pub settings: Settings,
    pub notice: Option<String>,
}

/// `$XDG_CONFIG_HOME/tuitube/config.toml` か `$HOME/.config/tuitube/config.toml`。
pub fn config_path(xdg_config_home: Option<&OsStr>, home: Option<&OsStr>) -> Option<PathBuf> {
    if let Some(xdg) = xdg_config_home.filter(|v| !v.is_empty()) {
        return Some(Path::new(xdg).join(APP_DIR).join(CONFIG_FILE));
    }
    let home = home.filter(|v| !v.is_empty())?;
    Some(
        Path::new(home)
            .join(".config")
            .join(APP_DIR)
            .join(CONFIG_FILE),
    )
}

pub fn parse(text: &str) -> Result<RawConfig, String> {
    toml::from_str(text).map_err(|e| e.to_string())
}

/// 検証と丸め。壊れた値で再生できなくなる方が困るので、起動は止めず notice を積む。
pub fn validate(raw: RawConfig, env_fps_limit: Option<&str>) -> (Settings, Vec<String>) {
    let mut notices = Vec::new();

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
    let (env_limit, env_notice) = parse_fps_limit_env(env_fps_limit);
    if let Some(limit) = env_limit {
        fps_cap = limit.and_then(FpsCap::new);
    }
    notices.extend(env_notice);

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

    (
        Settings {
            display,
            fps_cap,
            window,
            extra_args,
        },
        notices,
    )
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
    out.push_str("# 再生開始時の表示。\"embedded\" = TUI 内に埋め込み、\"window\" = mpv の別ウィンドウ。再生中は w で切り替え。\n");
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
    out.push_str("# 埋め込み表示の fps 上限。端末へ送るフレーム数を抑える。0 で制限なし。別ウィンドウには適用しない。\n");
    out.push_str(&format!(
        "fps_cap = {}\n",
        settings.fps_cap.map(FpsCap::get).unwrap_or(0)
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
    match window.focus_on {
        Some(focus_on) => out.push_str(&format!("focus_on = \"{}\"\n", focus_on.value())),
        None => out.push_str("# focus_on = \"never\"\n"),
    }
    out.push_str(&string_line("title", window.title.as_deref(), "tuitube"));

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

pub fn load_from(path: Option<&Path>, env_fps_limit: Option<&str>) -> Loaded {
    let Some(path) = path else {
        let (settings, notices) = validate(RawConfig::default(), env_fps_limit);
        return Loaded {
            settings,
            notice: join(notices),
        };
    };
    match fs::read_to_string(path) {
        Ok(text) => match parse(&text) {
            Ok(raw) => {
                let (settings, notices) = validate(raw, env_fps_limit);
                Loaded {
                    settings,
                    notice: join(notices),
                }
            }
            // 半端に効いた状態は原因を追いにくいので、全体を既定値に倒す。
            Err(e) => fallback(
                env_fps_limit,
                format!(
                    "{} を読めません: {}。既定値で動きます",
                    path.display(),
                    one_line(&e)
                ),
            ),
        },
        Err(e) if e.kind() == ErrorKind::NotFound => create_template(path, env_fps_limit),
        Err(e) => fallback(
            env_fps_limit,
            format!("{} を開けません: {e}。既定値で動きます", path.display()),
        ),
    }
}

fn fallback(env_fps_limit: Option<&str>, reason: String) -> Loaded {
    let (settings, mut notices) = validate(RawConfig::default(), env_fps_limit);
    notices.insert(0, reason);
    Loaded {
        settings,
        notice: join(notices),
    }
}

/// 生成に失敗しても起動は止めず、理由だけ伝える。
fn create_template(path: &Path, env_fps_limit: Option<&str>) -> Loaded {
    let (settings, mut notices) = validate(RawConfig::default(), env_fps_limit);
    notices.insert(
        0,
        match save_to(path, &Settings::default()) {
            Ok(()) => format!("{} を作成しました", path.display()),
            Err(e) => format!("{} を作成できません: {e}", path.display()),
        },
    );
    Loaded {
        settings,
        notice: join(notices),
    }
}

fn write_new(path: &Path, text: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    fs::write(path, text).map_err(|e| e.to_string())
}

/// 途中で落ちても壊れたファイルを残さないよう、一時ファイルへ書いてから置き換える。
pub fn save_to(path: &Path, settings: &Settings) -> Result<(), String> {
    let tmp = path.with_extension("toml.tmp");
    write_new(&tmp, &render(settings))?;
    fs::rename(&tmp, path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        e.to_string()
    })
}

pub fn load() -> Loaded {
    let xdg = std::env::var_os("XDG_CONFIG_HOME");
    let home = std::env::var_os("HOME");
    let path = config_path(xdg.as_deref(), home.as_deref());
    let env_fps_limit = std::env::var(FPS_LIMIT_VAR).ok();
    load_from(path.as_deref(), env_fps_limit.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display::{DisplayMode, FocusOn, FpsCap, Quality, WindowOptions};
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
        validate(parse(text).expect("読めるはず"), None).0
    }

    fn notices_of(text: &str) -> Vec<String> {
        validate(parse(text).expect("読めるはず"), None).1
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
        assert_eq!(validate(file.clone(), Some("unlimited")).0.fps_cap, None);
        assert_eq!(
            validate(file.clone(), Some("10")).0.fps_cap,
            FpsCap::new(10)
        );
        // 読めない指定でファイルの値を巻き添えにしない。
        let (settings, notices) = validate(file, Some("3O"));
        assert_eq!(settings.fps_cap, FpsCap::new(30));
        assert!(notices.join(" / ").contains("3O"), "{notices:?}");
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
                focus_on: Some(FocusOn::Never),
                title: Some("窓".to_string()),
            },
            extra_args: vec!["--hwdec=videotoolbox-copy".to_string()],
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
        let loaded = load_from(Some(&path), None);

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
    fn load_from_a_broken_file_falls_back_to_defaults_and_keeps_the_file() {
        let dir = temp_dir("broken");
        let path = dir.join("config.toml");
        // 構文エラーはキー単位で切り分けられないので、全体を既定値に倒す。
        let broken = "[display]\nmode = \n";
        fs::write(&path, broken).expect("書ける");
        let loaded = load_from(Some(&path), None);

        assert_eq!(loaded.settings, Settings::default());
        let notice = loaded.notice.expect("読めなかった旨を出す");
        assert!(notice.contains("既定値で動きます"), "{notice}");
        // 直せるよう、読めなかったファイルは書き換えない。
        assert_eq!(fs::read_to_string(&path).expect("読める"), broken);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_from_without_a_path_still_applies_the_environment_variable() {
        let loaded = load_from(None, Some("0"));
        assert_eq!(loaded.settings.fps_cap, None);
        assert!(loaded.notice.is_none());
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
