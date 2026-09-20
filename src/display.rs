//! 表示モード (TUI 内埋め込み / 別ウィンドウ) と、そこから決まる mpv の引数・切替コマンド。

use crate::mpv::{self, MpvCommand};
use crate::settings::{MAX_FPS_CAP, Settings};
use crate::speed::Speed;
use crate::subtitles::SubtitleLaunch;
use crate::video::{DecoderKind, Geometry, MAX_FRAME_PIXELS};
use serde_json::json;

/// fps 上限フィルタに付けるラベル。再生中に外したり戻したりするために名前で指す。
pub const CAP_LABEL: &str = "tuitube-cap";
/// 別ウィンドウの題名。mpv がプロパティを展開する。
pub const DEFAULT_WINDOW_TITLE: &str = "tuitube - ${media-title}";
/// kitty VO の名前。`current-vo` がこれなら埋め込みで出ている。
const KITTY_VO: &str = "kitty";
/// 文字ブロック VO の名前。
const TCT_VO: &str = "tct";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DisplayMode {
    #[default]
    Embedded,
    Text,
    Window,
}

impl DisplayMode {
    /// w での循環。端末内 (Text) を経由するので、途中で GUI ウィンドウが開かない。
    pub fn next(self) -> Self {
        match self {
            Self::Embedded => Self::Text,
            Self::Text => Self::Window,
            Self::Window => Self::Embedded,
        }
    }

    /// 設定画面の ← 用。next() の逆順。
    pub fn prev(self) -> Self {
        match self {
            Self::Embedded => Self::Window,
            Self::Text => Self::Embedded,
            Self::Window => Self::Text,
        }
    }

    /// mpv が実際に使っている VO からモードを読む。値が来るまでは None。
    pub fn from_current_vo(vo: Option<&str>) -> Option<Self> {
        match vo? {
            KITTY_VO => Some(Self::Embedded),
            TCT_VO => Some(Self::Text),
            _ => Some(Self::Window),
        }
    }

    /// 映像の読み取り方。別ウィンドウでは stdout に映像が来ない。
    pub fn decoder_kind(self) -> Option<DecoderKind> {
        match self {
            Self::Embedded => Some(DecoderKind::Kitty),
            Self::Text => Some(DecoderKind::Text),
            Self::Window => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Embedded => "埋め込み",
            Self::Text => "テキスト",
            Self::Window => "別ウィンドウ",
        }
    }

    /// 設定ファイルに書く名前。
    pub fn key(self) -> &'static str {
        match self {
            Self::Embedded => "embedded",
            Self::Text => "text",
            Self::Window => "window",
        }
    }

    /// 設定ファイルの値から。綴りが違えば None (呼び出し側が既定へ倒して notice を出す)。
    pub fn from_key(key: &str) -> Option<Self> {
        [Self::Embedded, Self::Text, Self::Window]
            .into_iter()
            .find(|mode| mode.key() == key)
    }
}

/// 埋め込み表示の fps 上限。1..=120。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FpsCap(u32);

impl FpsCap {
    pub fn new(fps: u32) -> Option<Self> {
        (1..=MAX_FPS_CAP).contains(&fps).then_some(Self(fps))
    }

    pub fn get(self) -> u32 {
        self.0
    }

    /// 前に通したフレームから 1/N 秒以上経ったものだけ通す上限フィルタ。
    /// fps=N は定レート変換で、上限より遅いソースのフレームを複製してしまう。
    /// +0.001 は 2/30*15 が 0.999… に落ちて 1 フレーム余計に落ちるのを防ぐ補正。
    pub fn filter_spec(self) -> String {
        format!(
            "@{CAP_LABEL}:lavfi=[select=floor((t-prev_selected_t)*{}+0.001)]",
            self.0
        )
    }

    /// --vf= は置換なので、利用者の mpv.conf にある vf を消さない --vf-append を使う。
    pub fn launch_arg(self) -> String {
        format!("--vf-append={}", self.filter_spec())
    }

    pub fn add_command(self) -> MpvCommand {
        mpv::vf_add(&self.filter_spec())
    }

    pub fn remove_command() -> MpvCommand {
        mpv::vf_remove(CAP_LABEL)
    }
}

/// 埋め込み表示の細かさ。変わるのは端末へ送るデータ量で、mpv のデコード負荷ではない。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Quality {
    Low,
    #[default]
    Medium,
    High,
    Native,
}

impl Quality {
    /// 設定画面での循環。粗い方から順に送る。
    pub fn next(self) -> Self {
        match self {
            Self::Low => Self::Medium,
            Self::Medium => Self::High,
            Self::High => Self::Native,
            Self::Native => Self::Low,
        }
    }

    /// 設定画面の ← 用。next() の逆順。4 値あるので戻せないと選び直しが遠い。
    pub fn prev(self) -> Self {
        match self {
            Self::Low => Self::Native,
            Self::Medium => Self::Low,
            Self::High => Self::Medium,
            Self::Native => Self::High,
        }
    }

    pub fn max_pixels(self) -> u32 {
        match self {
            Self::Low => 320 * 180,
            Self::Medium => MAX_FRAME_PIXELS,
            Self::High => 960 * 540,
            Self::Native => u32::MAX,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Native => "native",
        }
    }

    pub fn from_label(label: &str) -> Option<Self> {
        [Self::Low, Self::Medium, Self::High, Self::Native]
            .into_iter()
            .find(|quality| quality.label() == label)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusOn {
    Never,
    Open,
    All,
}

impl FocusOn {
    pub fn value(self) -> &'static str {
        match self {
            Self::Never => "never",
            Self::Open => "open",
            Self::All => "all",
        }
    }

    pub fn from_value(value: &str) -> Option<Self> {
        [Self::Never, Self::Open, Self::All]
            .into_iter()
            .find(|focus_on| focus_on.value() == value)
    }
}

/// 別ウィンドウ時の mpv オプション。値は検証せずそのまま渡す。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WindowOptions {
    /// None は mpv の自動選択 (起動時は --vo を渡さない、切替時は vo "")。
    pub vo: Option<String>,
    pub autofit: Option<String>,
    pub geometry: Option<String>,
    pub ontop: bool,
    pub fullscreen: bool,
    pub focus_on: Option<FocusOn>,
    pub title: Option<String>,
}

impl WindowOptions {
    pub fn args(&self) -> Vec<String> {
        let mut args = Vec::new();
        if let Some(vo) = &self.vo {
            args.push(format!("--vo={vo}"));
        }
        if let Some(autofit) = &self.autofit {
            args.push(format!("--autofit={autofit}"));
        }
        if let Some(geometry) = &self.geometry {
            args.push(format!("--geometry={geometry}"));
        }
        if self.ontop {
            args.push("--ontop".to_string());
        }
        if self.fullscreen {
            args.push("--fullscreen".to_string());
        }
        if let Some(focus_on) = self.focus_on {
            args.push(format!("--focus-on={}", focus_on.value()));
        }
        args.push(format!(
            "--title={}",
            self.title.as_deref().unwrap_or(DEFAULT_WINDOW_TITLE)
        ));
        args
    }

    /// set_property vo に渡す値。空文字は mpv の自動選択。
    pub fn vo_value(&self) -> &str {
        self.vo.as_deref().unwrap_or_default()
    }
}

/// 起動引数の元。--vo / --vo-kitty-* / --vf-append / ウィンドウ系はここから決まる。
#[derive(Debug, Clone, PartialEq)]
pub struct LaunchPlan {
    pub mode: DisplayMode,
    /// 埋め込み用。別ウィンドウで起動しても、戻るときのために持つ。
    pub geometry: Geometry,
    pub fps_cap: Option<FpsCap>,
    pub window: WindowOptions,
    /// 直前の再生から持ち越した速度。等速なら引数に出さない。
    pub speed: Speed,
    /// 字幕の要求。設定で無効なら引数を一切出さない。
    pub subtitles: SubtitleLaunch,
    /// 前回の再生位置。None なら最初から。
    pub resume_at: Option<f64>,
    /// [mpv] extra_args と cookie 連携の追加引数。
    pub extra_args: Vec<String>,
}

impl LaunchPlan {
    pub fn new(mode: DisplayMode, geometry: Geometry, settings: &Settings) -> Self {
        Self {
            mode,
            geometry,
            fps_cap: settings.fps_cap,
            window: settings.window.clone(),
            speed: Speed::NORMAL,
            // 表示するかは持ち越しの状態で決まるので、呼び出し側が差し替える。
            subtitles: SubtitleLaunch::new(&settings.subtitles, true),
            // 再開位置も選んだ動画で決まるので、呼び出し側が差し替える。
            resume_at: None,
            extra_args: settings.extra_args.clone(),
        }
    }

    /// --input-ipc-server / --log-file / --no-terminal の後ろ、URL の前に来る引数列。
    pub fn args(&self) -> Vec<String> {
        let mut args = Vec::new();
        match self.mode {
            DisplayMode::Embedded => {
                args.push(format!("--vo={KITTY_VO}"));
                args.extend(self.geometry.mpv_args());
                if let Some(cap) = self.fps_cap {
                    args.push(cap.launch_arg());
                }
            }
            DisplayMode::Text => {
                args.push(format!("--vo={TCT_VO}"));
                args.extend(self.geometry.tct_args());
                if let Some(cap) = self.fps_cap {
                    args.push(cap.launch_arg());
                }
            }
            // 上限フィルタは GPU VO には効く意味がないので付けない。
            DisplayMode::Window => args.extend(self.window.args()),
        }
        // 速度と字幕は VO と独立。表示モードによらず同じ引数で渡す。
        args.extend(self.speed.launch_arg());
        args.extend(self.subtitles.args());
        args.extend(self.resume_at.map(|secs| format!("--start={secs}")));
        args.extend_from_slice(&self.extra_args);
        args
    }
}

/// from → to の切替コマンド列。VO は生成時にしか設定を読まないので、順序を入れ替えない。
/// mode = "window" で起動したときは起動引数に VO オプションが無いため、
/// 寸法だけでなく位置・alt-screen・config-clear もここで送る。
pub fn switch_commands(
    from: DisplayMode,
    to: DisplayMode,
    geometry: Geometry,
    cap: Option<FpsCap>,
    window: &WindowOptions,
) -> Vec<MpvCommand> {
    let mut commands = Vec::new();
    match to {
        DisplayMode::Window => {
            if cap.is_some() {
                commands.push(FpsCap::remove_command());
            }
            commands.push(mpv::set_property("vo", json!(window.vo_value())));
        }
        DisplayMode::Embedded => {
            commands.extend(
                geometry
                    .kitty_options()
                    .into_iter()
                    .map(|(key, value)| mpv::set_kitty_option(key, value)),
            );
            commands.extend(restored_cap(from, cap));
            commands.push(mpv::set_property("vo", json!(KITTY_VO)));
        }
        DisplayMode::Text => {
            commands.extend(
                geometry
                    .tct_options()
                    .into_iter()
                    .map(|(key, value)| mpv::set_tct_option(key, value)),
            );
            commands.extend(restored_cap(from, cap));
            commands.push(mpv::set_property("vo", json!(TCT_VO)));
        }
    }
    commands
}

/// 上限フィルタを外すのは別ウィンドウへ行くときだけなので、足し直すのも戻るときだけ。
fn restored_cap(from: DisplayMode, cap: Option<FpsCap>) -> Option<MpvCommand> {
    (from == DisplayMode::Window)
        .then_some(cap)
        .flatten()
        .map(FpsCap::add_command)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::Settings;
    use crate::speed::Speed;
    use crate::subtitles::{SubLang, SubtitleSettings};
    use crate::video::{CellSize, Geometry, MAX_FRAME_PIXELS};
    use ratatui::layout::Rect;

    const CELL: CellSize = CellSize {
        width_px: 8,
        height_px: 16,
    };

    fn geometry(width: u16, height: u16) -> Geometry {
        Geometry::new(Rect::new(0, 0, width, height), CELL, MAX_FRAME_PIXELS)
    }

    fn cap(fps: u32) -> FpsCap {
        FpsCap::new(fps).expect("有効な上限")
    }

    #[test]
    fn fps_cap_rejects_zero_and_values_above_the_maximum() {
        assert_eq!(FpsCap::new(0), None);
        assert_eq!(FpsCap::new(121), None);
        assert_eq!(FpsCap::new(1).map(FpsCap::get), Some(1));
        assert_eq!(FpsCap::new(120).map(FpsCap::get), Some(120));
    }

    #[test]
    fn fps_cap_filter_spec_is_a_drop_only_select_with_epsilon() {
        let spec = cap(15).filter_spec();
        assert_eq!(
            spec,
            "@tuitube-cap:lavfi=[select=floor((t-prev_selected_t)*15+0.001)]"
        );
        // libavfilter 側で式が切られるので、, を含む書き方は使えない。
        assert!(!spec.contains(','), "{spec}");
    }

    #[test]
    fn fps_cap_launch_arg_appends_instead_of_replacing() {
        let arg = cap(15).launch_arg();
        // --vf= は置換なので、利用者の mpv.conf の vf 設定を消してしまう。
        assert!(arg.starts_with("--vf-append="), "{arg}");
        assert!(arg.ends_with(&cap(15).filter_spec()), "{arg}");
    }

    #[test]
    fn fps_cap_add_and_remove_commands_share_the_label() {
        assert_eq!(
            cap(15).add_command().to_line(),
            "{\"command\":[\"vf\",\"add\",\"@tuitube-cap:lavfi=[select=floor((t-prev_selected_t)*15+0.001)]\"]}\n"
        );
        assert_eq!(
            FpsCap::remove_command().to_line(),
            "{\"command\":[\"vf\",\"remove\",\"@tuitube-cap\"]}\n"
        );
    }

    #[test]
    fn quality_budgets_are_ordered_and_medium_is_the_current_constant() {
        assert!(Quality::Low.max_pixels() < Quality::Medium.max_pixels());
        assert!(Quality::Medium.max_pixels() < Quality::High.max_pixels());
        assert!(Quality::High.max_pixels() < Quality::Native.max_pixels());
        assert_eq!(Quality::Medium.max_pixels(), MAX_FRAME_PIXELS);
        assert_eq!(Quality::default(), Quality::Medium);
    }

    #[test]
    fn quality_and_display_mode_step_back_the_way_they_came() {
        // 設定画面の ← 用。next() を 1 回ぶん取り消せる。
        for quality in [
            Quality::Low,
            Quality::Medium,
            Quality::High,
            Quality::Native,
        ] {
            assert_eq!(quality.next().prev(), quality, "{quality:?}");
            assert_eq!(quality.prev().next(), quality, "{quality:?}");
        }
        assert_eq!(Quality::Low.prev(), Quality::Native, "端では巻き戻る");

        for mode in [
            DisplayMode::Embedded,
            DisplayMode::Text,
            DisplayMode::Window,
        ] {
            assert_eq!(mode.next().prev(), mode, "{mode:?}");
        }
        assert_eq!(DisplayMode::Embedded.prev(), DisplayMode::Window);
    }

    #[test]
    fn quality_cycles_from_the_coarsest_to_the_finest_and_back() {
        assert_eq!(Quality::Low.next(), Quality::Medium);
        assert_eq!(Quality::Medium.next(), Quality::High);
        assert_eq!(Quality::High.next(), Quality::Native);
        assert_eq!(Quality::Native.next(), Quality::Low);

        // 4 回で一周する。どの値からでも全部を選べる。
        let mut quality = Quality::default();
        let mut seen = Vec::new();
        for _ in 0..4 {
            seen.push(quality);
            quality = quality.next();
        }
        assert_eq!(quality, Quality::default());
        assert_eq!(seen.len(), 4);
        for value in [
            Quality::Low,
            Quality::Medium,
            Quality::High,
            Quality::Native,
        ] {
            assert!(seen.contains(&value), "{} が出ない", value.label());
        }
    }

    #[test]
    fn display_mode_is_read_back_from_current_vo() {
        assert_eq!(
            DisplayMode::from_current_vo(Some("kitty")),
            Some(DisplayMode::Embedded)
        );
        assert_eq!(
            DisplayMode::from_current_vo(Some("tct")),
            Some(DisplayMode::Text)
        );
        for vo in ["gpu-next", "gpu"] {
            assert_eq!(
                DisplayMode::from_current_vo(Some(vo)),
                Some(DisplayMode::Window),
                "{vo}"
            );
        }
        assert_eq!(DisplayMode::from_current_vo(None), None);
        assert_eq!(DisplayMode::default(), DisplayMode::Embedded);
    }

    #[test]
    fn display_mode_cycles_embedded_text_window() {
        assert_eq!(DisplayMode::Embedded.next(), DisplayMode::Text);
        assert_eq!(DisplayMode::Text.next(), DisplayMode::Window);
        assert_eq!(DisplayMode::Window.next(), DisplayMode::Embedded);
    }

    #[test]
    fn display_mode_keys_and_labels_cover_text() {
        assert_eq!(DisplayMode::from_key("text"), Some(DisplayMode::Text));
        assert_eq!(DisplayMode::Text.key(), "text");
        assert_eq!(DisplayMode::Text.label(), "テキスト");
        // 大文字始まりは別の綴り。
        assert_eq!(DisplayMode::from_key("Text"), None);
    }

    #[test]
    fn decoder_kind_is_none_only_for_the_window() {
        assert_eq!(
            DisplayMode::Embedded.decoder_kind(),
            Some(DecoderKind::Kitty)
        );
        assert_eq!(DisplayMode::Text.decoder_kind(), Some(DecoderKind::Text));
        assert_eq!(DisplayMode::Window.decoder_kind(), None);
    }

    #[test]
    fn text_launch_args_use_tct_with_the_cell_size_and_the_fps_cap() {
        let plan = LaunchPlan {
            mode: DisplayMode::Text,
            geometry: geometry(80, 22),
            fps_cap: Some(cap(15)),
            window: WindowOptions::default(),
            speed: Speed::NORMAL,
            subtitles: SubtitleLaunch::Disabled,
            resume_at: None,
            extra_args: Vec::new(),
        };
        let args = plan.args();

        assert!(args.contains(&"--vo=tct".to_string()), "{args:?}");
        assert!(args.contains(&"--vo-tct-width=80".to_string()), "{args:?}");
        assert!(args.contains(&"--vo-tct-height=22".to_string()), "{args:?}");
        assert!(args.contains(&cap(15).launch_arg()), "{args:?}");
        assert!(
            !args.iter().any(|a| a.starts_with("--vo-kitty-")),
            "{args:?}"
        );
        assert!(!args.iter().any(|a| a.starts_with("--title")), "{args:?}");
    }

    #[test]
    fn launch_args_carry_the_speed_only_when_it_is_not_normal() {
        let extra = vec!["--hwdec=no".to_string()];
        let plan = LaunchPlan {
            mode: DisplayMode::Embedded,
            geometry: geometry(80, 22),
            fps_cap: Some(cap(15)),
            window: WindowOptions::default(),
            speed: Speed::from_tenths(15).expect("1.5x"),
            subtitles: SubtitleLaunch::Disabled,
            resume_at: None,
            extra_args: extra.clone(),
        };
        let args = plan.args();
        let at = args
            .iter()
            .position(|a| a == "--speed=1.5")
            .unwrap_or_else(|| panic!("--speed がない: {args:?}"));
        // 利用者の指定 (extra_args) が後から上書きできる位置に置く。
        assert_eq!(args[at + 1..], extra[..]);

        let normal = LaunchPlan {
            speed: Speed::NORMAL,
            ..plan
        };
        assert!(
            !normal.args().iter().any(|a| a.starts_with("--speed")),
            "{:?}",
            normal.args()
        );
    }

    #[test]
    fn plan_starts_at_normal_speed() {
        let plan = LaunchPlan::new(
            DisplayMode::Embedded,
            geometry(80, 22),
            &Settings::default(),
        );
        assert_eq!(plan.speed, Speed::NORMAL);
    }

    #[test]
    fn embedded_launch_args_contain_kitty_geometry_and_the_fps_cap() {
        let plan = LaunchPlan {
            mode: DisplayMode::Embedded,
            geometry: geometry(80, 22),
            fps_cap: Some(cap(15)),
            window: WindowOptions::default(),
            speed: Speed::NORMAL,
            subtitles: SubtitleLaunch::Disabled,
            resume_at: None,
            extra_args: Vec::new(),
        };
        let args = plan.args();

        assert!(args.contains(&"--vo=kitty".to_string()));
        for expected in geometry(80, 22).mpv_args() {
            assert!(args.contains(&expected), "{expected} がない: {args:?}");
        }
        assert!(args.contains(&cap(15).launch_arg()));
        assert!(!args.iter().any(|a| a.starts_with("--autofit")), "{args:?}");
        assert!(!args.iter().any(|a| a.starts_with("--title")), "{args:?}");
    }

    #[test]
    fn window_launch_args_have_no_kitty_options_and_no_fps_cap() {
        let plan = LaunchPlan {
            mode: DisplayMode::Window,
            geometry: geometry(80, 22),
            fps_cap: Some(cap(15)),
            window: WindowOptions::default(),
            speed: Speed::NORMAL,
            subtitles: SubtitleLaunch::Disabled,
            resume_at: None,
            extra_args: Vec::new(),
        };
        let args = plan.args();

        // VO は mpv の自動選択に任せるので --vo 自体を渡さない。
        assert!(!args.iter().any(|a| a.starts_with("--vo")), "{args:?}");
        assert!(
            !args.iter().any(|a| a.starts_with("--vf-append")),
            "{args:?}"
        );
        assert!(
            args.iter().any(|a| a.starts_with("--title=")),
            "既定の題名がない: {args:?}"
        );

        let fixed = LaunchPlan {
            window: WindowOptions {
                vo: Some("gpu".to_string()),
                ..WindowOptions::default()
            },
            ..plan
        };
        assert!(fixed.args().contains(&"--vo=gpu".to_string()));
    }

    #[test]
    fn window_options_pass_through_only_what_is_set() {
        let bare = WindowOptions::default().args();
        assert!(!bare.iter().any(|a| a.starts_with("--autofit")));
        assert!(!bare.iter().any(|a| a.starts_with("--geometry")));
        assert!(!bare.iter().any(|a| a == "--ontop"));
        assert!(!bare.iter().any(|a| a == "--fullscreen"));
        assert!(!bare.iter().any(|a| a.starts_with("--focus-on")));

        let full = WindowOptions {
            vo: Some("gpu-next".to_string()),
            autofit: Some("640x360".to_string()),
            geometry: Some("50%+0+0".to_string()),
            ontop: true,
            fullscreen: true,
            focus_on: Some(FocusOn::Never),
            title: Some("窓".to_string()),
        }
        .args();
        assert!(full.contains(&"--vo=gpu-next".to_string()));
        assert!(full.contains(&"--autofit=640x360".to_string()));
        assert!(full.contains(&"--geometry=50%+0+0".to_string()));
        assert!(full.contains(&"--ontop".to_string()));
        assert!(full.contains(&"--fullscreen".to_string()));
        assert!(full.contains(&"--focus-on=never".to_string()));
        assert!(full.contains(&"--title=窓".to_string()));
    }

    #[test]
    fn fullscreen_is_a_bare_flag_and_is_independent_of_ontop() {
        let only_fullscreen = WindowOptions {
            fullscreen: true,
            ..WindowOptions::default()
        }
        .args();
        assert!(only_fullscreen.contains(&"--fullscreen".to_string()));
        assert!(!only_fullscreen.iter().any(|a| a == "--ontop"));
        assert!(
            !only_fullscreen
                .iter()
                .any(|a| a.starts_with("--fullscreen=")),
            "値は付けない: {only_fullscreen:?}"
        );

        let only_ontop = WindowOptions {
            ontop: true,
            ..WindowOptions::default()
        }
        .args();
        assert!(!only_ontop.iter().any(|a| a == "--fullscreen"));
    }

    #[test]
    fn switching_to_the_window_does_not_carry_the_fullscreen_setting() {
        let full = WindowOptions {
            fullscreen: true,
            ..WindowOptions::default()
        };
        let lines: Vec<String> = switch_commands(
            DisplayMode::Embedded,
            DisplayMode::Window,
            geometry(80, 22),
            None,
            &full,
        )
        .iter()
        .map(|c| c.to_line())
        .collect();
        assert_eq!(lines, ["{\"command\":[\"set_property\",\"vo\",\"\"]}\n"]);
    }

    #[test]
    fn extra_args_come_last_in_the_plan() {
        let extra = vec![
            "--hwdec=videotoolbox-copy".to_string(),
            "--ytdl-raw-options-append=cookies-from-browser=chrome".to_string(),
        ];
        let plan = LaunchPlan {
            mode: DisplayMode::Embedded,
            geometry: geometry(80, 22),
            fps_cap: Some(cap(15)),
            window: WindowOptions::default(),
            speed: Speed::NORMAL,
            subtitles: SubtitleLaunch::Disabled,
            resume_at: None,
            extra_args: extra.clone(),
        };
        let args = plan.args();
        assert_eq!(args[args.len() - 2..], extra[..]);
    }

    #[test]
    fn launch_args_carry_the_resume_position_only_when_set() {
        let plan = LaunchPlan {
            mode: DisplayMode::Embedded,
            geometry: geometry(80, 22),
            fps_cap: Some(cap(15)),
            window: WindowOptions::default(),
            speed: Speed::NORMAL,
            subtitles: SubtitleLaunch::Disabled,
            resume_at: Some(42.5),
            extra_args: Vec::new(),
        };
        assert!(
            plan.args().contains(&"--start=42.5".to_string()),
            "{:?}",
            plan.args()
        );

        let from_the_start = LaunchPlan {
            resume_at: None,
            ..plan
        };
        assert!(
            !from_the_start
                .args()
                .iter()
                .any(|a| a.starts_with("--start")),
            "{:?}",
            from_the_start.args()
        );
    }

    #[test]
    fn the_resume_position_comes_after_the_subtitles_and_before_the_extra_args() {
        let extra = vec!["--hwdec=no".to_string()];
        let plan = LaunchPlan {
            mode: DisplayMode::Embedded,
            geometry: geometry(80, 22),
            fps_cap: Some(cap(15)),
            window: WindowOptions::default(),
            speed: Speed::NORMAL,
            subtitles: SubtitleLaunch::Disabled,
            resume_at: Some(10.0),
            extra_args: extra.clone(),
        };
        let args = plan.args();
        let at = args
            .iter()
            .position(|a| a == "--start=10")
            .unwrap_or_else(|| panic!("--start がない: {args:?}"));
        assert_eq!(args[at + 1..], extra[..]);
    }

    #[test]
    fn plan_takes_the_budget_and_extras_from_the_settings() {
        let settings = Settings::default();
        let plan = LaunchPlan::new(DisplayMode::Window, geometry(80, 22), &settings);
        assert_eq!(plan.mode, DisplayMode::Window);
        assert_eq!(plan.fps_cap, settings.fps_cap);
        assert_eq!(plan.window, settings.window);
        assert_eq!(plan.extra_args, settings.extra_args);
    }

    fn subtitle_settings(enabled: bool) -> Settings {
        Settings {
            subtitles: SubtitleSettings {
                enabled,
                lang: SubLang::parse("ja").expect("言語コード"),
            },
            ..Settings::default()
        }
    }

    #[test]
    fn the_plan_takes_the_subtitle_request_from_the_settings() {
        let plan = LaunchPlan::new(
            DisplayMode::Embedded,
            geometry(80, 22),
            &subtitle_settings(true),
        );
        assert_eq!(
            plan.subtitles,
            SubtitleLaunch::new(&subtitle_settings(true).subtitles, true)
        );

        let off = LaunchPlan::new(
            DisplayMode::Embedded,
            geometry(80, 22),
            &subtitle_settings(false),
        );
        assert_eq!(off.subtitles, SubtitleLaunch::Disabled);
    }

    #[test]
    fn subtitle_args_come_after_the_speed_and_before_the_extra_args() {
        let extra = vec!["--hwdec=no".to_string()];
        let plan = LaunchPlan {
            mode: DisplayMode::Embedded,
            geometry: geometry(80, 22),
            fps_cap: Some(cap(15)),
            window: WindowOptions::default(),
            speed: Speed::from_tenths(15).expect("1.5x"),
            subtitles: SubtitleLaunch::new(&subtitle_settings(true).subtitles, true),
            resume_at: None,
            extra_args: extra.clone(),
        };
        let args = plan.args();
        let speed_at = args
            .iter()
            .position(|a| a == "--speed=1.5")
            .unwrap_or_else(|| panic!("--speed がない: {args:?}"));
        let subs = plan.subtitles.args();
        assert_eq!(args[speed_at + 1..speed_at + 1 + subs.len()], subs[..]);
        // 利用者の指定が後勝ちで上書きできる位置を保つ。
        assert_eq!(args[args.len() - extra.len()..], extra[..]);
    }

    #[test]
    fn a_disabled_subtitle_plan_mentions_no_subtitle_option() {
        let plan = LaunchPlan::new(
            DisplayMode::Embedded,
            geometry(80, 22),
            &subtitle_settings(false),
        );
        for key in [
            "slang",
            "sub-langs",
            "sid",
            "sub-visibility",
            "write-auto-subs",
        ] {
            assert!(
                !plan.args().iter().any(|a| a.contains(key)),
                "{key} が入っている: {:?}",
                plan.args()
            );
        }
    }

    #[test]
    fn subtitle_args_are_the_same_in_every_display_mode() {
        // 字幕は VO と独立。埋め込みでも別ウィンドウでも同じ引数で渡す。
        let subs = SubtitleLaunch::new(&subtitle_settings(true).subtitles, false);
        for mode in [
            DisplayMode::Embedded,
            DisplayMode::Text,
            DisplayMode::Window,
        ] {
            let plan = LaunchPlan {
                subtitles: subs.clone(),
                ..LaunchPlan::new(mode, geometry(80, 22), &subtitle_settings(true))
            };
            let args = plan.args();
            for expected in subs.args() {
                assert!(args.contains(&expected), "{expected} がない: {args:?}");
            }
        }
    }

    #[test]
    fn subtitle_args_and_the_cookie_argument_both_survive() {
        let cookie = "--ytdl-raw-options-append=cookies-from-browser=chrome".to_string();
        let plan = LaunchPlan {
            extra_args: vec![cookie.clone()],
            ..LaunchPlan::new(
                DisplayMode::Embedded,
                geometry(80, 22),
                &subtitle_settings(true),
            )
        };
        let args = plan.args();
        let cookie_at = args
            .iter()
            .position(|a| a == &cookie)
            .unwrap_or_else(|| panic!("cookie がない: {args:?}"));
        let langs_at = args
            .iter()
            .position(|a| a.starts_with("--ytdl-raw-options-append=sub-langs="))
            .unwrap_or_else(|| panic!("sub-langs がない: {args:?}"));
        // -append は key-value の追加なので両方そのまま yt-dlp へ届く。
        assert!(langs_at < cookie_at, "{args:?}");
    }

    fn switch(from: DisplayMode, to: DisplayMode, cap: Option<FpsCap>) -> Vec<String> {
        switch_commands(from, to, geometry(80, 22), cap, &WindowOptions::default())
            .iter()
            .map(|c| c.to_line())
            .collect()
    }

    #[test]
    fn switch_to_the_window_removes_the_cap_from_either_terminal_mode() {
        for from in [DisplayMode::Embedded, DisplayMode::Text] {
            assert_eq!(
                switch(from, DisplayMode::Window, Some(cap(15))),
                [
                    "{\"command\":[\"vf\",\"remove\",\"@tuitube-cap\"]}\n",
                    // 空文字は mpv の自動選択に戻す指定。
                    "{\"command\":[\"set_property\",\"vo\",\"\"]}\n",
                ],
                "{from:?}"
            );
            assert_eq!(
                switch(from, DisplayMode::Window, None),
                ["{\"command\":[\"set_property\",\"vo\",\"\"]}\n"],
                "{from:?}"
            );
        }

        let fixed = WindowOptions {
            vo: Some("gpu".to_string()),
            ..WindowOptions::default()
        };
        let lines: Vec<String> = switch_commands(
            DisplayMode::Embedded,
            DisplayMode::Window,
            geometry(80, 22),
            None,
            &fixed,
        )
        .iter()
        .map(|c| c.to_line())
        .collect();
        assert_eq!(lines, ["{\"command\":[\"set_property\",\"vo\",\"gpu\"]}\n"]);
    }

    #[test]
    fn switch_to_embedded_from_the_window_matches_the_current_sequence() {
        assert_eq!(
            switch(DisplayMode::Window, DisplayMode::Embedded, Some(cap(15))),
            [
                // 設定は VO の生成前に入っていなければ読まれない。
                "{\"command\":[\"set_property\",\"vo-kitty-cols\",80]}\n",
                "{\"command\":[\"set_property\",\"vo-kitty-rows\",22]}\n",
                "{\"command\":[\"set_property\",\"vo-kitty-width\",640]}\n",
                "{\"command\":[\"set_property\",\"vo-kitty-height\",352]}\n",
                // 別ウィンドウ起動では起動引数に無いので、ここで送る。
                "{\"command\":[\"set_property\",\"vo-kitty-left\",1]}\n",
                "{\"command\":[\"set_property\",\"vo-kitty-top\",1]}\n",
                "{\"command\":[\"set_property\",\"vo-kitty-alt-screen\",false]}\n",
                "{\"command\":[\"set_property\",\"vo-kitty-config-clear\",false]}\n",
                "{\"command\":[\"vf\",\"add\",\"@tuitube-cap:lavfi=[select=floor((t-prev_selected_t)*15+0.001)]\"]}\n",
                "{\"command\":[\"set_property\",\"vo\",\"kitty\"]}\n",
            ]
        );
        // vo の変更が VO を作り直すので、映像トラックの入れ直しは要らない。
        assert!(
            !switch(DisplayMode::Window, DisplayMode::Embedded, Some(cap(15)))
                .iter()
                .any(|line| line.contains("\"vid\""))
        );
        assert_eq!(
            switch(DisplayMode::Window, DisplayMode::Embedded, None).len(),
            9
        );
    }

    #[test]
    fn switch_to_embedded_covers_every_option_of_an_embedded_launch() {
        // 別ウィンドウ起動から w で戻したときも、埋め込み起動と同じ構成にする。
        let sent = switch(DisplayMode::Window, DisplayMode::Embedded, None);
        for (key, _) in geometry(80, 22).kitty_options() {
            assert!(
                sent.iter()
                    .any(|line| line.contains(&format!("vo-kitty-{key}"))),
                "{key} を送っていない: {sent:?}"
            );
        }
    }

    #[test]
    fn switch_to_embedded_from_text_sends_the_kitty_options_and_keeps_the_cap() {
        let lines = switch(DisplayMode::Text, DisplayMode::Embedded, Some(cap(15)));
        // 上限フィルタは外していないので足し直さない。
        assert!(
            !lines.iter().any(|line| line.contains("\"vf\"")),
            "{lines:?}"
        );
        assert_eq!(lines.len(), 9);
        assert_eq!(
            lines.last().map(String::as_str),
            Some("{\"command\":[\"set_property\",\"vo\",\"kitty\"]}\n")
        );
    }

    #[test]
    fn switch_to_text_from_embedded_sends_tct_size_then_vo_without_touching_the_cap() {
        assert_eq!(
            switch(DisplayMode::Embedded, DisplayMode::Text, Some(cap(15))),
            [
                "{\"command\":[\"set_property\",\"vo-tct-width\",80]}\n",
                "{\"command\":[\"set_property\",\"vo-tct-height\",22]}\n",
                "{\"command\":[\"set_property\",\"vo\",\"tct\"]}\n",
            ]
        );
    }

    #[test]
    fn switch_to_text_from_the_window_adds_the_cap_back() {
        assert_eq!(
            switch(DisplayMode::Window, DisplayMode::Text, Some(cap(15))),
            [
                "{\"command\":[\"set_property\",\"vo-tct-width\",80]}\n",
                "{\"command\":[\"set_property\",\"vo-tct-height\",22]}\n",
                "{\"command\":[\"vf\",\"add\",\"@tuitube-cap:lavfi=[select=floor((t-prev_selected_t)*15+0.001)]\"]}\n",
                "{\"command\":[\"set_property\",\"vo\",\"tct\"]}\n",
            ]
        );
        // 上限を設けていなければ足すものは無い。
        assert_eq!(
            switch(DisplayMode::Window, DisplayMode::Text, None).len(),
            3
        );
    }
}
