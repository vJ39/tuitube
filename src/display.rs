//! 表示モード (TUI 内埋め込み / 別ウィンドウ) と、そこから決まる mpv の引数・切替コマンド。

use crate::mpv::{self, MpvCommand};
use crate::settings::{MAX_FPS_CAP, Settings};
use crate::video::{Geometry, MAX_FRAME_PIXELS};
use serde_json::json;

/// fps 上限フィルタに付けるラベル。再生中に外したり戻したりするために名前で指す。
pub const CAP_LABEL: &str = "tuitube-cap";
/// 別ウィンドウの題名。mpv がプロパティを展開する。
pub const DEFAULT_WINDOW_TITLE: &str = "tuitube - ${media-title}";
/// kitty VO の名前。`current-vo` がこれなら埋め込みで出ている。
const KITTY_VO: &str = "kitty";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DisplayMode {
    #[default]
    Embedded,
    Window,
}

impl DisplayMode {
    pub fn toggled(self) -> Self {
        match self {
            Self::Embedded => Self::Window,
            Self::Window => Self::Embedded,
        }
    }

    /// mpv が実際に使っている VO からモードを読む。値が来るまでは None。
    pub fn from_current_vo(vo: Option<&str>) -> Option<Self> {
        match vo? {
            KITTY_VO => Some(Self::Embedded),
            _ => Some(Self::Window),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Embedded => "埋め込み",
            Self::Window => "別ウィンドウ",
        }
    }

    /// 設定ファイルに書く名前。
    pub fn key(self) -> &'static str {
        match self {
            Self::Embedded => "embedded",
            Self::Window => "window",
        }
    }

    /// 設定ファイルの値から。綴りが違えば None (呼び出し側が既定へ倒して notice を出す)。
    pub fn from_key(key: &str) -> Option<Self> {
        [Self::Embedded, Self::Window]
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
            // 上限フィルタは GPU VO には効く意味がないので付けない。
            DisplayMode::Window => args.extend(self.window.args()),
        }
        args.extend_from_slice(&self.extra_args);
        args
    }
}

/// 埋め込み → 別ウィンドウ。fps 上限を外し、vo を変える。
pub fn to_window_commands(cap: Option<FpsCap>, window: &WindowOptions) -> Vec<MpvCommand> {
    let mut commands = Vec::new();
    if cap.is_some() {
        commands.push(FpsCap::remove_command());
    }
    commands.push(mpv::set_property("vo", json!(window.vo_value())));
    commands
}

/// 別ウィンドウ → 埋め込み。kitty VO の設定を送り、fps 上限を足し、vo を kitty に。
/// VO は生成時にしか設定を読まないので、順序を入れ替えない。
/// mode = "window" で起動したときは起動引数に --vo-kitty-* が無いため、
/// 寸法だけでなく位置・alt-screen・config-clear もここで送る。
pub fn to_embedded_commands(geometry: Geometry, cap: Option<FpsCap>) -> Vec<MpvCommand> {
    let mut commands: Vec<MpvCommand> = geometry
        .kitty_options()
        .into_iter()
        .map(|(key, value)| mpv::set_kitty_option(key, value))
        .collect();
    if let Some(cap) = cap {
        commands.push(cap.add_command());
    }
    commands.push(mpv::set_property("vo", json!(KITTY_VO)));
    commands
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::Settings;
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
    fn display_mode_is_read_back_from_current_vo() {
        assert_eq!(
            DisplayMode::from_current_vo(Some("kitty")),
            Some(DisplayMode::Embedded)
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
        assert_eq!(DisplayMode::Embedded.toggled(), DisplayMode::Window);
        assert_eq!(DisplayMode::Window.toggled(), DisplayMode::Embedded);
    }

    #[test]
    fn embedded_launch_args_contain_kitty_geometry_and_the_fps_cap() {
        let plan = LaunchPlan {
            mode: DisplayMode::Embedded,
            geometry: geometry(80, 22),
            fps_cap: Some(cap(15)),
            window: WindowOptions::default(),
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
        assert!(!bare.iter().any(|a| a.starts_with("--focus-on")));

        let full = WindowOptions {
            vo: Some("gpu-next".to_string()),
            autofit: Some("640x360".to_string()),
            geometry: Some("50%+0+0".to_string()),
            ontop: true,
            focus_on: Some(FocusOn::Never),
            title: Some("窓".to_string()),
        }
        .args();
        assert!(full.contains(&"--vo=gpu-next".to_string()));
        assert!(full.contains(&"--autofit=640x360".to_string()));
        assert!(full.contains(&"--geometry=50%+0+0".to_string()));
        assert!(full.contains(&"--ontop".to_string()));
        assert!(full.contains(&"--focus-on=never".to_string()));
        assert!(full.contains(&"--title=窓".to_string()));
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
            extra_args: extra.clone(),
        };
        let args = plan.args();
        assert_eq!(args[args.len() - 2..], extra[..]);
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

    #[test]
    fn to_window_commands_remove_the_cap_before_switching_the_vo() {
        let lines: Vec<String> = to_window_commands(Some(cap(15)), &WindowOptions::default())
            .iter()
            .map(|c| c.to_line())
            .collect();
        assert_eq!(
            lines,
            [
                "{\"command\":[\"vf\",\"remove\",\"@tuitube-cap\"]}\n",
                // 空文字は mpv の自動選択に戻す指定。
                "{\"command\":[\"set_property\",\"vo\",\"\"]}\n",
            ]
        );

        let without_cap: Vec<String> = to_window_commands(None, &WindowOptions::default())
            .iter()
            .map(|c| c.to_line())
            .collect();
        assert_eq!(
            without_cap,
            ["{\"command\":[\"set_property\",\"vo\",\"\"]}\n"]
        );

        let fixed = WindowOptions {
            vo: Some("gpu".to_string()),
            ..WindowOptions::default()
        };
        let lines: Vec<String> = to_window_commands(None, &fixed)
            .iter()
            .map(|c| c.to_line())
            .collect();
        assert_eq!(lines, ["{\"command\":[\"set_property\",\"vo\",\"gpu\"]}\n"]);
    }

    #[test]
    fn to_embedded_commands_send_geometry_then_cap_then_vo() {
        let lines: Vec<String> = to_embedded_commands(geometry(80, 22), Some(cap(15)))
            .iter()
            .map(|c| c.to_line())
            .collect();
        assert_eq!(
            lines,
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
        assert!(!lines.iter().any(|line| line.contains("\"vid\"")));

        let without_cap = to_embedded_commands(geometry(80, 22), None);
        assert_eq!(without_cap.len(), 9);
    }

    #[test]
    fn to_embedded_commands_cover_every_option_of_an_embedded_launch() {
        // 別ウィンドウ起動から w で戻したときも、埋め込み起動と同じ構成にする。
        let geometry = geometry(80, 22);
        let sent: Vec<String> = to_embedded_commands(geometry, None)
            .iter()
            .map(|c| c.to_line())
            .collect();
        for (key, _) in geometry.kitty_options() {
            assert!(
                sent.iter()
                    .any(|line| line.contains(&format!("vo-kitty-{key}"))),
                "{key} を送っていない: {sent:?}"
            );
        }
    }
}
