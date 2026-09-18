//! 字幕 (YouTube の自動生成字幕) の設定・起動引数・表示状態。外部プロセスには触れない。

use crate::mpv::{self, MpvCommand};
use serde_json::{Value, json};
use std::time::{Duration, Instant};

/// sid の確定を待つ時間。遅延選択で字幕ファイルを開くのに実測 約2秒かかる。
/// 要求してからと、動画の長さが取れてからの両方でこれだけ待つ。
pub const SELECT_GRACE: Duration = Duration::from_secs(3);

/// 動画の長さが取れないまま「字幕なし」と言い出すまでの時間。
/// ライブ配信のように duration が来ない再生でも、いつまでも「字幕...」で止めない。
/// 長さの取得自体に実測 12〜14 秒かかるので、それに SELECT_GRACE 分を足した長さにする。
pub const LOAD_GRACE: Duration = Duration::from_secs(20);

/// yt-dlp の sub-langs と mpv の --slang に同じ文字列で渡す言語指定。
/// 受け付けるのは言語コードのカンマ区切りだけ (yt-dlp の正規表現記法や all は --slang が解釈できない)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubLang(String);

impl SubLang {
    pub const DEFAULT: &'static str = "ja-orig,ja";

    /// 空・all・言語コードでない要素があれば None。呼び出し側が既定へ倒して notice を出す。
    pub fn parse(value: &str) -> Option<Self> {
        let codes: Vec<&str> = value.split(',').map(str::trim).collect();
        codes
            .iter()
            .all(|code| is_language_code(code))
            .then(|| Self(codes.join(",")))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// 表示用。先頭の言語コード。
    pub fn primary(&self) -> &str {
        self.0.split(',').next().unwrap_or_default()
    }
}

impl Default for SubLang {
    fn default() -> Self {
        Self(Self::DEFAULT.to_string())
    }
}

/// `^[A-Za-z][A-Za-z0-9-]*$`。all は yt-dlp の全言語指定で、渡すとトラックが 157 本載る。
fn is_language_code(code: &str) -> bool {
    if code.eq_ignore_ascii_case("all") {
        return false;
    }
    let mut chars = code.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// 検証済みの [subtitles]。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubtitleSettings {
    pub enabled: bool,
    pub lang: SubLang,
}

impl Default for SubtitleSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            lang: SubLang::default(),
        }
    }
}

/// 起動引数の元。再生開始時に字幕を出すかどうかまで含む。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum SubtitleLaunch {
    /// 設定で無効。字幕の引数を一切渡さない。
    #[default]
    Disabled,
    Requested {
        lang: SubLang,
        shown: bool,
    },
}

impl SubtitleLaunch {
    pub fn new(settings: &SubtitleSettings, shown: bool) -> Self {
        if !settings.enabled {
            return Self::Disabled;
        }
        Self::Requested {
            lang: settings.lang.clone(),
            shown,
        }
    }

    /// write-auto-subs は sub-langs とセットで渡す
    /// (単独だと ytdl_hook の既定 all が残り字幕トラックが 157 本載る)。
    pub fn args(&self) -> Vec<String> {
        let Self::Requested { lang, shown } = self else {
            return Vec::new();
        };
        let lang = lang.as_str();
        let mut args = vec![
            "--ytdl-raw-options-append=write-auto-subs=".to_string(),
            format!("--ytdl-raw-options-append=sub-langs={lang}"),
            format!("--slang={lang}"),
            // 利用者の mpv.conf に sub-visibility=no があると、選んでも何も出ない。
            "--sub-visibility=yes".to_string(),
        ];
        if !shown {
            args.push("--sid=no".to_string());
        }
        args
    }
}

/// 再生中の字幕の状態。表示したいかは動画をまたいで持ち越す。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SubtitleState {
    wanted: bool,
    /// ポーリングした sid。数値なら選ばれている。
    selected: Option<u64>,
    /// 選ばれているトラックの言語 (current-tracks/sub/lang)。
    /// --slang の先頭が選ばれるとは限らないので、表示にはこちらを使う。
    selected_lang: Option<String>,
    /// 表示を要求した時刻。ここから SELECT_GRACE の間は「字幕なし」と言わない。
    requested_at: Option<Instant>,
    /// 動画の長さが取れた時刻。「字幕なし」を言い出す起点。
    loaded_at: Option<Instant>,
}

impl SubtitleState {
    pub fn from_settings(settings: &SubtitleSettings) -> Self {
        Self {
            wanted: settings.enabled,
            ..Self::default()
        }
    }

    pub fn wanted(&self) -> bool {
        self.wanted
    }

    /// 押されたときの行き先。送信に成功するまで状態は動かさない (set_speed と同じ形)。
    /// Err は出せない理由 (設定で無効)。
    pub fn toggle_command(
        &self,
        settings: &SubtitleSettings,
    ) -> Result<(bool, MpvCommand), String> {
        if !settings.enabled {
            return Err(disabled_notice());
        }
        Ok(if self.wanted {
            (false, hide_command())
        } else {
            (true, show_command())
        })
    }

    /// 送信できたときに確定させる。
    pub fn set_wanted(&mut self, wanted: bool, now: Instant) {
        self.wanted = wanted;
        self.forget_selection();
        self.requested_at = wanted.then_some(now);
    }

    /// 新しい mpv を起動したとき。選択と長さを捨て、要求時刻を打ち直す。
    pub fn begin_playback(&mut self, now: Instant) {
        self.forget_selection();
        self.loaded_at = None;
        self.requested_at = self.wanted.then_some(now);
    }

    fn forget_selection(&mut self) {
        self.selected = None;
        self.selected_lang = None;
    }

    /// 応答が取れなかったとき (property unavailable) は触らない。
    /// 一度の欠測で選択を捨てると「字幕なし」へ落ちてしまう (speed と同じ扱い)。
    pub fn observe_sid(&mut self, data: Option<&Value>) {
        let Some(value) = data else {
            return;
        };
        self.selected = value.as_u64();
    }

    /// current-tracks/sub/lang。選ばれていない間は取れないので、取れた値だけ控える。
    pub fn observe_sub_lang(&mut self, data: Option<&Value>) {
        if let Some(lang) = data.and_then(Value::as_str) {
            self.selected_lang = Some(lang.to_string());
        }
    }

    /// loaded は再生する動画の長さが取れたか (playback.duration.is_some())。
    pub fn observe_loaded(&mut self, loaded: bool, now: Instant) {
        if loaded {
            self.loaded_at.get_or_insert(now);
        }
    }

    pub fn status(&self, settings: &SubtitleSettings, now: Instant) -> SubtitleStatus {
        if !settings.enabled {
            return SubtitleStatus::Disabled;
        }
        if !self.wanted {
            return SubtitleStatus::Off;
        }
        if self.selected.is_some() {
            return SubtitleStatus::Shown;
        }
        if self.waited_long_enough(now) {
            SubtitleStatus::Missing
        } else {
            SubtitleStatus::Loading
        }
    }

    /// 「字幕なし」と言ってよいか。要求からと長さが取れてからの両方で猶予を過ぎたときだけ。
    /// 長さが来ない再生は要求から LOAD_GRACE で打ち切る。
    fn waited_long_enough(&self, now: Instant) -> bool {
        let Some(requested_at) = self.requested_at else {
            return false;
        };
        let waited = now.saturating_duration_since(requested_at);
        match self.loaded_at {
            Some(loaded_at) => {
                waited >= SELECT_GRACE && now.saturating_duration_since(loaded_at) >= SELECT_GRACE
            }
            None => waited >= LOAD_GRACE,
        }
    }

    /// ステータス行の印。非表示・無効のときは None。
    pub fn marker(&self, settings: &SubtitleSettings, now: Instant) -> Option<String> {
        match self.status(settings, now) {
            SubtitleStatus::Shown => Some(format!("字幕{}", self.shown_lang(settings))),
            SubtitleStatus::Loading => Some("字幕...".to_string()),
            SubtitleStatus::Missing => Some("字幕なし".to_string()),
            SubtitleStatus::Off | SubtitleStatus::Disabled => None,
        }
    }

    /// 出ている字幕の言語。mpv から取れるまでは設定の先頭で代用する。
    fn shown_lang<'a>(&'a self, settings: &'a SubtitleSettings) -> &'a str {
        self.selected_lang
            .as_deref()
            .unwrap_or_else(|| settings.lang.primary())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubtitleStatus {
    /// 設定で無効。
    Disabled,
    /// 利用者が消している。
    Off,
    /// 要求したが sid がまだ確定しない。
    Loading,
    Shown,
    /// 要求したのに、この動画には指定言語の字幕が無い。
    Missing,
}

/// 表示する / 隠すコマンド。--slang を起動引数に入れてあるので auto で同じ言語へ戻る。
pub fn show_command() -> MpvCommand {
    mpv::set_property("sid", json!("auto"))
}

pub fn hide_command() -> MpvCommand {
    mpv::set_property("sid", json!("no"))
}

/// 切り替えた直後の知らせ。
pub fn toggle_notice(wanted: bool, lang: &SubLang) -> String {
    if wanted {
        format!("字幕を出します ({})", lang.primary())
    } else {
        "字幕を消しました".to_string()
    }
}

pub fn disabled_notice() -> String {
    "[subtitles] enabled が false です。設定ファイルを直して再生し直すと字幕を出せます".to_string()
}

/// 指定した言語は 1 つも選ばれなかったので、先頭だけでなく列全体を出す。
pub fn missing_notice(lang: &SubLang) -> String {
    format!("この動画に {} の字幕がありません", lang.as_str())
}

pub fn text_mode_notice() -> String {
    "テキスト表示では映像に字幕が出ません".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn lang(value: &str) -> SubLang {
        SubLang::parse(value).unwrap_or_else(|| panic!("{value} は言語コード"))
    }

    fn settings(enabled: bool, value: &str) -> SubtitleSettings {
        SubtitleSettings {
            enabled,
            lang: lang(value),
        }
    }

    #[test]
    fn a_language_is_a_comma_separated_list_of_codes() {
        for value in ["ja", "ja-orig", "ja,en", "pt-BR", "zh-Hans", "ja-orig,ja"] {
            assert_eq!(lang(value).as_str(), value, "{value}");
        }
        // 前後と要素間の空白は落として、そのまま yt-dlp へ渡せる形にする。
        assert_eq!(lang(" ja , en ").as_str(), "ja,en");
    }

    #[test]
    fn a_language_rejects_what_slang_cannot_read() {
        // all は yt-dlp の全言語指定で、渡すと字幕トラックが 157 本載る (実測)。
        for value in [
            "",
            "  ",
            "all",
            "ALL",
            "ja*",
            "ja.en",
            "-live_chat",
            ",ja",
            "ja,,en",
            "ja,",
            "1ja",
        ] {
            assert_eq!(SubLang::parse(value), None, "{value}");
        }
    }

    #[test]
    fn the_primary_language_is_the_first_code() {
        assert_eq!(lang("ja-orig,ja").primary(), "ja-orig");
        assert_eq!(lang("ja").primary(), "ja");
        assert_eq!(SubLang::default().as_str(), SubLang::DEFAULT);
        assert_eq!(SubLang::DEFAULT, "ja-orig,ja");
    }

    #[test]
    fn subtitles_are_enabled_by_default() {
        let defaults = SubtitleSettings::default();
        assert!(defaults.enabled);
        assert_eq!(defaults.lang, SubLang::default());
    }

    #[test]
    fn a_disabled_launch_passes_no_subtitle_arguments() {
        assert_eq!(SubtitleLaunch::default(), SubtitleLaunch::Disabled);
        assert!(SubtitleLaunch::Disabled.args().is_empty());
        assert_eq!(
            SubtitleLaunch::new(&settings(false, "ja"), true),
            SubtitleLaunch::Disabled
        );
    }

    #[test]
    fn a_shown_launch_asks_for_the_language_without_touching_sid() {
        let args = SubtitleLaunch::new(&settings(true, "ja"), true).args();
        assert_eq!(
            args,
            [
                "--ytdl-raw-options-append=write-auto-subs=",
                "--ytdl-raw-options-append=sub-langs=ja",
                "--slang=ja",
                "--sub-visibility=yes",
            ]
        );
        assert!(!args.iter().any(|a| a.starts_with("--sid")), "{args:?}");
    }

    #[test]
    fn a_hidden_launch_adds_sid_no_at_the_end() {
        let args = SubtitleLaunch::new(&settings(true, "ja"), false).args();
        assert_eq!(args.last().map(String::as_str), Some("--sid=no"));
        // 非表示でもトラックは用意させる。再生中に sid auto で出せる。
        assert!(
            args.contains(&"--ytdl-raw-options-append=write-auto-subs=".to_string()),
            "{args:?}"
        );
    }

    #[test]
    fn write_auto_subs_never_goes_without_sub_langs() {
        // 単独で渡すと ytdl_hook の既定 --sub-langs all が残り、字幕トラックが 157 本載る。
        for shown in [true, false] {
            let args = SubtitleLaunch::new(&settings(true, "ja,en"), shown).args();
            let has_auto = args
                .iter()
                .any(|a| a == "--ytdl-raw-options-append=write-auto-subs=");
            let has_langs = args
                .iter()
                .any(|a| a.starts_with("--ytdl-raw-options-append=sub-langs="));
            assert_eq!(has_auto, has_langs, "{args:?}");
            assert!(has_auto, "{args:?}");
        }
    }

    #[test]
    fn a_multi_language_value_stays_in_one_argument() {
        // -append は値を , で分割しないので、優先順のリストがそのまま届く。
        let args = SubtitleLaunch::new(&settings(true, "ja-orig,ja"), true).args();
        assert!(
            args.contains(&"--ytdl-raw-options-append=sub-langs=ja-orig,ja".to_string()),
            "{args:?}"
        );
        assert!(args.contains(&"--slang=ja-orig,ja".to_string()), "{args:?}");
    }

    #[test]
    fn show_and_hide_set_sid_by_name() {
        // 数値の sid は無いトラックでも success が返り、状態だけ黙って外れる (実測)。
        assert_eq!(
            show_command().to_line(),
            "{\"command\":[\"set_property\",\"sid\",\"auto\"]}\n"
        );
        assert_eq!(
            hide_command().to_line(),
            "{\"command\":[\"set_property\",\"sid\",\"no\"]}\n"
        );
    }

    #[test]
    fn a_disabled_setting_has_nothing_to_toggle() {
        let state = SubtitleState::from_settings(&settings(false, "ja"));
        assert!(!state.wanted());
        let reason = state
            .toggle_command(&settings(false, "ja"))
            .expect_err("出せない");
        assert!(reason.contains("[subtitles] enabled"), "{reason}");
    }

    #[test]
    fn toggling_picks_the_command_without_changing_the_state() {
        let config = settings(true, "ja");
        let state = SubtitleState::from_settings(&config);
        assert!(state.wanted(), "enabled なら最初から出す");

        let (wanted, command) = state.toggle_command(&config).expect("出せる");
        assert!(!wanted);
        assert_eq!(command.to_line(), hide_command().to_line());
        // 送信できるまでは状態を動かさない。
        assert!(state.wanted());

        let mut off = state.clone();
        off.set_wanted(false, Instant::now());
        let (wanted, command) = off.toggle_command(&config).expect("出せる");
        assert!(wanted);
        assert_eq!(command.to_line(), show_command().to_line());
    }

    #[test]
    fn setting_wanted_records_when_it_was_asked_for() {
        let config = settings(true, "ja");
        let now = Instant::now();
        let mut state = SubtitleState::default();
        assert!(!state.wanted());

        state.set_wanted(true, now);
        state.observe_loaded(true, now);
        assert!(state.wanted());
        // 要求した直後は猶予の内側なので、まだ「字幕なし」とは言わない。
        assert_eq!(
            state.status(&config, now + SELECT_GRACE - Duration::from_millis(1)),
            SubtitleStatus::Loading
        );
        assert_eq!(
            state.status(&config, now + SELECT_GRACE),
            SubtitleStatus::Missing
        );

        state.set_wanted(false, now);
        assert_eq!(state.status(&config, now), SubtitleStatus::Off);
    }

    #[test]
    fn the_polled_sid_is_a_number_only_while_a_track_is_selected() {
        let config = settings(true, "ja");
        let now = Instant::now();
        let mut state = SubtitleState::from_settings(&config);
        state.set_wanted(true, now);
        state.observe_loaded(true, now);

        state.observe_sid(Some(&json!(1)));
        assert_eq!(state.status(&config, now), SubtitleStatus::Shown);

        for data in [json!(false), json!("auto")] {
            state.observe_sid(Some(&json!(1)));
            state.observe_sid(Some(&data));
            assert_eq!(
                state.status(&config, now),
                SubtitleStatus::Loading,
                "{data:?}"
            );
        }
    }

    #[test]
    fn a_poll_that_came_back_empty_leaves_the_selection_alone() {
        // property unavailable は「選ばれていない」ではない。取れないときは触らない。
        let config = settings(true, "ja");
        let now = Instant::now();
        let mut state = SubtitleState::from_settings(&config);
        state.set_wanted(true, now);
        state.observe_loaded(true, now);
        state.observe_sid(Some(&json!(1)));

        state.observe_sid(None);
        assert_eq!(
            state.status(&config, now + SELECT_GRACE),
            SubtitleStatus::Shown
        );
    }

    #[test]
    fn the_status_covers_every_reason_for_not_showing_a_subtitle() {
        let now = Instant::now();
        let late = now + SELECT_GRACE;
        let mut state = SubtitleState::from_settings(&settings(true, "ja"));
        state.set_wanted(true, now);

        // 設定で切っていれば、持ち越した要求より設定が勝つ。
        assert_eq!(
            state.status(&settings(false, "ja"), late),
            SubtitleStatus::Disabled
        );
        // 動画の長さが取れるまでは、字幕が無いとは判定しない。
        assert_eq!(
            state.status(&settings(true, "ja"), late),
            SubtitleStatus::Loading
        );

        state.observe_loaded(true, now);
        assert_eq!(
            state.status(&settings(true, "ja"), late),
            SubtitleStatus::Missing
        );

        state.observe_sid(Some(&json!(1)));
        assert_eq!(
            state.status(&settings(true, "ja"), late),
            SubtitleStatus::Shown
        );
    }

    #[test]
    fn the_wait_for_a_missing_subtitle_starts_when_the_length_arrives() {
        // 長さが取れるまで実測 12 秒かかる。要求時刻から数えると猶予を使い切ってしまう。
        let config = settings(true, "ja");
        let now = Instant::now();
        let loaded = now + Duration::from_secs(12);
        let mut state = SubtitleState::from_settings(&config);
        state.set_wanted(true, now);

        assert_eq!(state.status(&config, loaded), SubtitleStatus::Loading);
        state.observe_loaded(true, loaded);
        assert_eq!(
            state.status(&config, loaded + SELECT_GRACE - Duration::from_millis(1)),
            SubtitleStatus::Loading
        );
        assert_eq!(
            state.status(&config, loaded + SELECT_GRACE),
            SubtitleStatus::Missing
        );
    }

    #[test]
    fn a_playback_without_a_length_still_stops_saying_loading() {
        // ライブ配信などで duration が来ない再生。いつまでも「字幕...」で止めない。
        let config = settings(true, "ja");
        let now = Instant::now();
        let mut state = SubtitleState::from_settings(&config);
        state.begin_playback(now);
        state.observe_loaded(false, now + Duration::from_secs(30));

        assert_eq!(
            state.status(&config, now + LOAD_GRACE - Duration::from_millis(1)),
            SubtitleStatus::Loading
        );
        assert_eq!(
            state.status(&config, now + LOAD_GRACE),
            SubtitleStatus::Missing
        );
    }

    #[test]
    fn asking_again_after_the_length_arrived_gets_its_own_grace() {
        // 長さが取れた後に s で出し直したとき、sid の確定を待たずに「なし」と言わない。
        let config = settings(true, "ja");
        let now = Instant::now();
        let mut state = SubtitleState::from_settings(&config);
        state.observe_loaded(true, now);

        let asked = now + Duration::from_secs(300);
        state.set_wanted(true, asked);
        assert_eq!(state.status(&config, asked), SubtitleStatus::Loading);
        assert_eq!(
            state.status(&config, asked + SELECT_GRACE),
            SubtitleStatus::Missing
        );
    }

    #[test]
    fn a_new_playback_keeps_the_wish_and_drops_the_selection() {
        let config = settings(true, "ja");
        let now = Instant::now();
        let mut state = SubtitleState::from_settings(&config);
        state.set_wanted(true, now);
        state.observe_loaded(true, now);
        state.observe_sid(Some(&json!(1)));

        let next = now + Duration::from_secs(60);
        state.begin_playback(next);
        assert!(state.wanted(), "表示は動画をまたいで持ち越す");
        assert_eq!(
            state.status(&config, next),
            SubtitleStatus::Loading,
            "前の動画の選択は捨てる"
        );
        assert_eq!(
            state.status(&config, next + SELECT_GRACE),
            SubtitleStatus::Loading,
            "前の動画の長さも捨てる"
        );

        let mut off = SubtitleState::default();
        off.begin_playback(now);
        assert_eq!(
            off.status(&config, now + SELECT_GRACE * 2),
            SubtitleStatus::Off
        );
    }

    #[test]
    fn the_marker_names_the_language_only_while_it_is_wanted() {
        let config = settings(true, "ja-orig,ja");
        let now = Instant::now();
        let late = now + SELECT_GRACE;
        let mut state = SubtitleState::from_settings(&config);
        state.set_wanted(true, now);

        assert_eq!(state.marker(&config, late), Some("字幕...".to_string()));

        state.observe_loaded(true, now);
        assert_eq!(state.marker(&config, late), Some("字幕なし".to_string()));

        // まだ言語が取れていない間は設定の先頭で代用する。
        state.observe_sid(Some(&json!(1)));
        assert_eq!(state.marker(&config, late), Some("字幕ja-orig".to_string()));

        // 消しているときと設定で切っているときは桁を使わない。
        state.set_wanted(false, now);
        assert_eq!(state.marker(&config, late), None);
        assert_eq!(state.marker(&settings(false, "ja"), late), None);
    }

    #[test]
    fn the_marker_names_the_track_that_mpv_actually_chose() {
        // lang="ja-orig,ja" でも ja が選ばれることがある (実測)。印は選ばれた方を出す。
        let config = settings(true, "ja-orig,ja");
        let now = Instant::now();
        let mut state = SubtitleState::from_settings(&config);
        state.set_wanted(true, now);
        state.observe_sid(Some(&json!(1)));
        state.observe_sub_lang(Some(&json!("ja")));
        assert_eq!(state.marker(&config, now), Some("字幕ja".to_string()));

        // 選択が外れている間は取れない。前の言語を引きずらない。
        state.observe_sub_lang(None);
        state.set_wanted(false, now);
        state.set_wanted(true, now);
        state.observe_sid(Some(&json!(2)));
        assert_eq!(state.marker(&config, now), Some("字幕ja-orig".to_string()));
    }

    #[test]
    fn the_notices_say_what_happened_and_why() {
        let ja = lang("ja-orig,ja");
        assert_eq!(toggle_notice(true, &ja), "字幕を出します (ja-orig)");
        assert_eq!(toggle_notice(false, &ja), "字幕を消しました");
        assert!(disabled_notice().contains("[subtitles] enabled"));
        // 先頭だけだと「ja なら出るのか」と読めてしまう。指定した列をそのまま出す。
        assert_eq!(
            missing_notice(&ja),
            "この動画に ja-orig,ja の字幕がありません"
        );
        assert!(text_mode_notice().contains("テキスト表示"));
    }
}
