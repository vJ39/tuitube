//! ブラウザの YouTube ログイン cookie を yt-dlp 経由で使うための判定と文言。
//! 外部プロセスには触れない。実行は search.rs / mpv.rs が行う。

pub const ENV_VAR: &str = "TUITUBE_COOKIES_FROM_BROWSER";
/// フィードは制限しないと 167 件返ることがある (`:ytrec` 実測)。
pub const FEED_LIMIT: usize = 30;

/// yt-dlp の BROWSER[+KEYRING][:PROFILE][::CONTAINER] をそのまま保持する。
/// 対応ブラウザの一覧は持たない (yt-dlp の更新で変わるため、検証も yt-dlp に任せる)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CookieSource {
    spec: String,
}

impl CookieSource {
    pub fn from_env_value(value: Option<&str>) -> Option<Self> {
        let spec = value.unwrap_or_default().trim();
        (!spec.is_empty()).then(|| Self {
            spec: spec.to_string(),
        })
    }

    pub fn from_env() -> Option<Self> {
        Self::from_env_value(std::env::var(ENV_VAR).ok().as_deref())
    }

    pub fn spec(&self) -> &str {
        &self.spec
    }

    /// 表示用。':' '+' より前。"chrome:Profile 1" → "chrome"
    pub fn browser(&self) -> &str {
        let end = self.spec.find([':', '+']).unwrap_or(self.spec.len());
        &self.spec[..end]
    }

    /// シェルを経由しないので、引用符もエスケープも付けない。
    pub fn yt_dlp_args(&self) -> [String; 2] {
        [
            "--cookies-from-browser".to_string(),
            self.spec().to_string(),
        ]
    }

    /// 利用者の mpv.conf にある ytdl-raw-options を消さないよう追加形で渡す。
    /// -append は値を ',' で分割しないので、空白やコロン入りの spec もそのまま届く。
    pub fn mpv_arg(&self) -> String {
        format!(
            "--ytdl-raw-options-append=cookies-from-browser={}",
            self.spec()
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CookieOutcome {
    NotUsed,
    Ok,
    /// 復号できず、非ログイン相当の結果になった。yt-dlp は成功扱いで返す。
    Degraded(String),
    /// 読めず、検索そのものが実行されなかった。
    Unreadable(String),
    /// cookie は読めたがログインしていない (フィード)。
    LoginRequired,
    TimedOut,
    Unknown,
}

/// 復号に失敗したときだけ出る WARNING。exit 0 で返るので stderr を見ないと気づけない。
const DEGRADED_MARKERS: [&str; 3] = [
    "cannot decrypt v10 cookies",
    "find-generic-password failed",
    "could not be decrypted",
];

/// cookie ストアに届かなかったときの文言のうち、cookie 以外では出ないもの
/// (yt-dlp 2026.08.19 実測)。
const COOKIE_STORE_MARKERS: [&str; 4] = [
    "cookies database",
    "unsupported browser specified for cookies",
    "custom safari cookies database not found",
    "_parse_browser_specification()",
];

/// 同じく cookie ストアの失敗で出るが、OS 由来なので他の理由でも出る文言。
const PERMISSION_MARKERS: [&str; 2] = ["Operation not permitted", "Permission denied"];

const LOGIN_MARKER: &str = "Login details are needed";

/// yt-dlp の exit code と stderr から判定する。cookie を渡していない場合は呼ばない。
pub fn classify(exit_code: Option<i32>, stderr: &str) -> CookieOutcome {
    if let Some(detail) = marker_line(stderr, &DEGRADED_MARKERS) {
        return CookieOutcome::Degraded(detail);
    }
    let failed = exit_code != Some(0);
    if failed
        && let Some(detail) = find_line(stderr, |line| {
            COOKIE_STORE_MARKERS
                .iter()
                .chain(PERMISSION_MARKERS.iter())
                .any(|m| line.contains(m))
        })
    {
        return CookieOutcome::Unreadable(detail);
    }
    if stderr.contains(LOGIN_MARKER) {
        return CookieOutcome::LoginRequired;
    }
    if failed {
        return CookieOutcome::Unknown;
    }
    CookieOutcome::Ok
}

/// mpv の失敗文言から cookie ストア由来の行だけを拾う。mpv は cookie と無関係な理由でも
/// 権限エラーを出すので、その行が cookie を指しているときだけ cookie 由来とみなす。
pub fn cookie_store_failure(text: &str) -> Option<String> {
    find_line(text, |line| {
        COOKIE_STORE_MARKERS.iter().any(|m| line.contains(m))
            || (PERMISSION_MARKERS.iter().any(|m| line.contains(m))
                && line.to_lowercase().contains("cookie"))
    })
}

/// 該当行を本文だけにして返す。最初の 1 行が原因で、後続は波及した結果。
fn find_line(text: &str, matches: impl Fn(&str) -> bool) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|line| matches(line))
        .map(|line| strip_prefix(line).to_string())
}

fn marker_line(text: &str, markers: &[&str]) -> Option<String> {
    find_line(text, |line| markers.iter().any(|m| line.contains(m)))
}

fn strip_prefix(line: &str) -> &str {
    for prefix in ["yt-dlp: error: ", "ERROR: ", "WARNING: "] {
        if let Some(rest) = line.strip_prefix(prefix) {
            return rest;
        }
    }
    line
}

/// TCC に阻まれたときはダイアログすら出ないので、設定画面の場所まで書く。
const SAFARI_TCC: &str = "Safari の cookie を読めませんでした。端末アプリにフルディスクアクセスを許可してください (システム設定 → プライバシーとセキュリティ → フルディスクアクセス)。cookie 無しで検索しました";

/// 利用者向けの説明文。表示しない outcome では空になる。
pub fn describe(outcome: &CookieOutcome, source: &CookieSource) -> String {
    match outcome {
        // フルディスクアクセスの案内が要るのは Safari だけ。他のブラウザでは誤った案内になる。
        CookieOutcome::Unreadable(detail)
            if detail.contains("Operation not permitted")
                && source.browser().eq_ignore_ascii_case("safari") =>
        {
            SAFARI_TCC.to_string()
        }
        CookieOutcome::Unreadable(detail) => format!(
            "cookie を読めませんでした ({}): {detail}。cookie 無しで検索しました",
            source.browser()
        ),
        CookieOutcome::Degraded(_) => format!(
            "cookie を復号できませんでした ({})。キーチェーンのダイアログで「常に許可」を選んでください。以後は cookie 無しで動作します",
            source.browser()
        ),
        CookieOutcome::TimedOut => format!(
            "検索がタイムアウトしました ({} 秒)。cookie 連携の初回は macOS のキーチェーン許可ダイアログが別ウィンドウで出ている可能性があります。「常に許可」を選び、tuitube を再起動してください。以後は cookie 無しで動作します",
            crate::search::YT_DLP_TIMEOUT.as_secs()
        ),
        _ => String::new(),
    }
}

/// フィード名が要るので describe とは別にする。
pub fn login_required_message(feed: Feed, source: &CookieSource) -> String {
    format!(
        "{} にはログインが必要です。ブラウザ ({}) で YouTube にログインしているか確認してください",
        feed.label(),
        source.browser()
    )
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum CookieState {
    #[default]
    Off,
    /// 指定あり。まだ一度も検索で確認していない。
    Armed(CookieSource),
    Active(CookieSource),
    Suspended {
        source: CookieSource,
        reason: String,
    },
}

impl CookieState {
    pub fn from_env() -> Self {
        match CookieSource::from_env() {
            Some(source) => Self::Armed(source),
            None => Self::Off,
        }
    }

    pub fn for_search(&self) -> Option<&CookieSource> {
        match self {
            Self::Armed(source) | Self::Active(source) => Some(source),
            _ => None,
        }
    }

    /// 再生に渡すのは検索で効くと確かめた後だけ。再生側では劣化を検知できない。
    pub fn for_playback(&self) -> Option<&CookieSource> {
        match self {
            Self::Active(source) => Some(source),
            _ => None,
        }
    }

    /// 停止したら自動では戻さない。失敗のたびに yt-dlp を 2 回待たせないため。
    pub fn observe(&mut self, outcome: &CookieOutcome) {
        let Some(source) = self.for_search().cloned() else {
            return;
        };
        let armed = matches!(self, Self::Armed(_));
        match outcome {
            CookieOutcome::Ok => *self = Self::Active(source),
            CookieOutcome::Degraded(_) | CookieOutcome::Unreadable(_) => {
                let reason = describe(outcome, &source);
                *self = Self::Suspended { source, reason };
            }
            CookieOutcome::TimedOut if armed => {
                let reason = describe(outcome, &source);
                *self = Self::Suspended { source, reason };
            }
            _ => {}
        }
    }

    /// mpv 側の失敗用。
    pub fn suspend(&mut self, reason: String) {
        if let Some(source) = self.for_search().cloned() {
            *self = Self::Suspended { source, reason };
        }
    }

    pub fn label(&self) -> Option<String> {
        match self {
            Self::Off => None,
            Self::Armed(source) | Self::Active(source) => {
                Some(format!("cookies: {}", source.browser()))
            }
            Self::Suspended { source, .. } => Some(format!("cookies: {} (停止)", source.browser())),
        }
    }

    /// ログイン必須の要求を yt-dlp を起動せずに断るときの文言。
    pub fn refusal(&self, feed: Feed) -> String {
        match self {
            Self::Suspended { reason, .. } => format!(
                "{} は cookie 連携が停止中のため使えません: {reason}",
                feed.label()
            ),
            _ => format!(
                "{} には cookie 連携が必要です。{ENV_VAR} を設定してください",
                feed.label()
            ),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Feed {
    Recommended,
    History,
    Subscriptions,
    WatchLater,
}

impl Feed {
    /// 完全一致のみ。":ytfoo" のような別の文字列は検索語として扱う。
    pub fn parse(query: &str) -> Option<Feed> {
        match query.trim() {
            ":ytrec" => Some(Feed::Recommended),
            ":ythis" | ":ythistory" => Some(Feed::History),
            ":ytsubs" => Some(Feed::Subscriptions),
            ":ytwatchlater" => Some(Feed::WatchLater),
            _ => None,
        }
    }

    pub fn keyword(self) -> &'static str {
        match self {
            Feed::Recommended => ":ytrec",
            Feed::History => ":ythis",
            Feed::Subscriptions => ":ytsubs",
            Feed::WatchLater => ":ytwatchlater",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Feed::Recommended => "おすすめ",
            Feed::History => "履歴",
            Feed::Subscriptions => "登録チャンネル",
            Feed::WatchLater => "後で見る",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Search(String),
    Feed(Feed),
}

impl Target {
    pub fn for_query(query: &str) -> Target {
        let query = query.trim();
        match Feed::parse(query) {
            Some(feed) => Target::Feed(feed),
            None => Target::Search(query.to_string()),
        }
    }

    pub fn yt_dlp_url(&self) -> String {
        match self {
            Target::Search(query) => format!("ytsearch10:{query}"),
            Target::Feed(feed) => feed.keyword().to_string(),
        }
    }

    pub fn empty_message(&self) -> String {
        match self {
            Target::Search(_) => "検索結果が0件でした".to_string(),
            // `:ytrec` は未ログインでもエラーにならず 0 件で返るので、件数でなく原因を出す。
            Target::Feed(feed) => format!(
                "{} が空でした。ブラウザで YouTube にログインしているか確認してください",
                feed.label()
            ),
        }
    }

    /// `:ytrec` は yt-dlp 上は cookie 不要だが、未ログインでは 0 件になるので同じ扱いにする。
    pub fn requires_login(&self) -> bool {
        matches!(self, Target::Feed(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(spec: &str) -> CookieSource {
        CookieSource::from_env_value(Some(spec)).expect("spec")
    }

    #[test]
    fn from_env_value_trims_and_rejects_blank() {
        assert_eq!(CookieSource::from_env_value(None), None);
        assert_eq!(CookieSource::from_env_value(Some("")), None);
        assert_eq!(CookieSource::from_env_value(Some("  ")), None);
        assert_eq!(
            CookieSource::from_env_value(Some(" chrome:Profile 1 "))
                .expect("値がある")
                .spec(),
            "chrome:Profile 1"
        );
    }

    #[test]
    fn browser_is_the_part_before_profile_or_keyring() {
        assert_eq!(source("chrome:Profile 1").browser(), "chrome");
        assert_eq!(source("chrome+basictext").browser(), "chrome");
        assert_eq!(source("firefox::work").browser(), "firefox");
        assert_eq!(source("safari").browser(), "safari");
    }

    #[test]
    fn yt_dlp_args_are_two_argv_entries() {
        assert_eq!(
            source("chrome:Profile 1").yt_dlp_args(),
            [
                "--cookies-from-browser".to_string(),
                "chrome:Profile 1".to_string()
            ]
        );
    }

    #[test]
    fn mpv_arg_uses_the_append_form_verbatim() {
        assert_eq!(
            source("chrome:Profile 1").mpv_arg(),
            "--ytdl-raw-options-append=cookies-from-browser=chrome:Profile 1"
        );
    }

    #[test]
    fn classify_ok_when_exit_zero_and_no_cookie_warning() {
        assert_eq!(classify(Some(0), ""), CookieOutcome::Ok);
    }

    #[test]
    fn classify_degraded_on_keychain_warnings() {
        // キーチェーンを拒否した場合だけ exit 0 のまま結果が非ログイン相当になる。
        let stderr = "WARNING: find-generic-password failed\nWARNING: cannot decrypt v10 cookies: no key found\n";
        let outcome = classify(Some(0), stderr);
        assert!(matches!(outcome, CookieOutcome::Degraded(_)), "{outcome:?}");
        assert_eq!(
            classify(
                Some(0),
                "Extracted 0 cookies from chrome (444 could not be decrypted)"
            ),
            CookieOutcome::Degraded(
                "Extracted 0 cookies from chrome (444 could not be decrypted)".to_string()
            )
        );
    }

    #[test]
    fn classify_unreadable_for_each_observed_error() {
        let cases = [
            (
                1,
                "ERROR: could not find firefox cookies database in '/Users/x/Library/Application Support/Firefox/Profiles'",
            ),
            (
                1,
                "ERROR: could not find chrome cookies database in \"/Users/x/Library/Application Support/Google/Chrome/NoSuchProfile\"",
            ),
            (
                2,
                "yt-dlp: error: unsupported browser specified for cookies: \"arc\". Supported browsers are: brave, chrome, chromium, edge, firefox, opera, safari, vivaldi, whale",
            ),
            (
                1,
                "ERROR: [Errno 1] Operation not permitted: '/Users/x/Library/Containers/com.apple.Safari/Data/Library/Cookies/Cookies.binarycookies'",
            ),
            (
                1,
                "ERROR: [Errno 13] Permission denied: '/tmp/x.binarycookies'",
            ),
            (1, "ERROR: custom safari cookies database not found"),
            (
                1,
                "ERROR: _parse_browser_specification() missing 1 required positional argument: 'browser_name'",
            ),
        ];
        for (code, stderr) in cases {
            let outcome = classify(Some(code), stderr);
            let CookieOutcome::Unreadable(detail) = outcome else {
                panic!("Unreadable のはず: {stderr}");
            };
            // 説明に出すのは yt-dlp の本文だけ。ERROR: の飾りは落とす。
            assert!(!detail.starts_with("ERROR:"), "{detail}");
            assert!(!detail.is_empty());
        }
    }

    #[test]
    fn classify_login_required() {
        let stderr = "ERROR: [youtube:history] Login details are needed to download this content. Use --cookies-from-browser or --cookies for the authentication. See  https://github.com/yt-dlp/yt-dlp/wiki/FAQ#how-do-i-pass-cookies-to-yt-dlp  for how to manually pass cookies.";
        assert_eq!(classify(Some(1), stderr), CookieOutcome::LoginRequired);
    }

    #[test]
    fn classify_unknown_for_unrelated_failures() {
        assert_eq!(
            classify(
                Some(1),
                "ERROR: [youtube:search] Unable to download webpage: <urlopen error>"
            ),
            CookieOutcome::Unknown
        );
        // シグナル終了。
        assert_eq!(classify(None, ""), CookieOutcome::Unknown);
    }

    #[test]
    fn describe_mentions_full_disk_access_for_safari_tcc() {
        let outcome = classify(
            Some(1),
            "ERROR: [Errno 1] Operation not permitted: '/Users/x/Library/Containers/com.apple.Safari/Data/Library/Cookies/Cookies.binarycookies'",
        );
        let text = describe(&outcome, &source("safari"));
        assert!(text.contains("フルディスクアクセス"), "{text}");

        // それ以外は yt-dlp の本文をそのまま見せる。
        let other = classify(
            Some(1),
            "ERROR: could not find firefox cookies database in '/x/Profiles'",
        );
        let text = describe(&other, &source("firefox"));
        assert!(text.contains("firefox"), "{text}");
        assert!(
            text.contains("could not find firefox cookies database"),
            "{text}"
        );
        assert!(text.contains("cookie 無しで検索しました"), "{text}");
    }

    #[test]
    fn full_disk_access_is_not_suggested_for_other_browsers() {
        // chrome を指定しているのに Safari の設定を直せとは言わない。
        let outcome = classify(
            Some(1),
            "ERROR: [Errno 1] Operation not permitted: '/Users/x/Library/Application Support/Google/Chrome/Default/Cookies'",
        );
        let text = describe(&outcome, &source("chrome:Profile 1"));
        assert!(!text.contains("フルディスクアクセス"), "{text}");
        assert!(text.contains("chrome"), "{text}");
        assert!(text.contains("Operation not permitted"), "{text}");

        // Safari は大文字小文字を問わず案内する。
        let text = describe(&outcome, &source("Safari"));
        assert!(text.contains("フルディスクアクセス"), "{text}");
    }

    #[test]
    fn cookie_store_failure_ignores_permission_errors_about_other_files() {
        // mpv はストリームやファイルのオープン失敗でも同じ文言を出す。
        assert_eq!(
            cookie_store_failure("Operation not permitted: '/dev/dsp'"),
            None
        );
        assert_eq!(cookie_store_failure("Permission denied: '/x.mkv'"), None);
        assert_eq!(
            cookie_store_failure("Failed to recognize file format."),
            None
        );

        // cookie ストアを指す行だけ拾い、ERROR: の飾りは落とす。
        assert_eq!(
            cookie_store_failure("ERROR: could not find chrome cookies database in '/x'"),
            Some("could not find chrome cookies database in '/x'".to_string())
        );
        assert_eq!(
            cookie_store_failure(
                "ERROR: [Errno 13] Permission denied: '/Users/x/Library/Cookies/Cookies.binarycookies'"
            ),
            Some(
                "[Errno 13] Permission denied: '/Users/x/Library/Cookies/Cookies.binarycookies'"
                    .to_string()
            )
        );
        assert_eq!(
            cookie_store_failure(
                "yt-dlp: error: unsupported browser specified for cookies: \"arc\""
            ),
            Some("unsupported browser specified for cookies: \"arc\"".to_string())
        );
    }

    #[test]
    fn state_transitions_follow_the_table() {
        let src = source("chrome");
        let outcomes = [
            CookieOutcome::NotUsed,
            CookieOutcome::Ok,
            CookieOutcome::Degraded("d".to_string()),
            CookieOutcome::Unreadable("u".to_string()),
            CookieOutcome::LoginRequired,
            CookieOutcome::TimedOut,
            CookieOutcome::Unknown,
        ];

        // Off は何が来ても動かない。
        for outcome in &outcomes {
            let mut state = CookieState::Off;
            state.observe(outcome);
            assert_eq!(state, CookieState::Off, "{outcome:?}");
        }

        // Suspended も動かない。
        for outcome in &outcomes {
            let mut state = CookieState::Suspended {
                source: src.clone(),
                reason: "理由".to_string(),
            };
            state.observe(outcome);
            assert!(
                matches!(state, CookieState::Suspended { .. }),
                "{outcome:?}"
            );
        }

        let armed = |outcome: &CookieOutcome| {
            let mut state = CookieState::Armed(src.clone());
            state.observe(outcome);
            state
        };
        assert_eq!(armed(&CookieOutcome::Ok), CookieState::Active(src.clone()));
        for outcome in [
            CookieOutcome::Degraded("d".to_string()),
            CookieOutcome::Unreadable("u".to_string()),
            CookieOutcome::TimedOut,
        ] {
            assert!(
                matches!(armed(&outcome), CookieState::Suspended { .. }),
                "{outcome:?}"
            );
        }
        for outcome in [
            CookieOutcome::LoginRequired,
            CookieOutcome::Unknown,
            CookieOutcome::NotUsed,
        ] {
            assert_eq!(
                armed(&outcome),
                CookieState::Armed(src.clone()),
                "{outcome:?}"
            );
        }

        let active = |outcome: &CookieOutcome| {
            let mut state = CookieState::Active(src.clone());
            state.observe(outcome);
            state
        };
        for outcome in [
            CookieOutcome::Ok,
            CookieOutcome::LoginRequired,
            CookieOutcome::Unknown,
            CookieOutcome::NotUsed,
            // 一度効いた後のタイムアウトはネットワーク要因の方が濃いので止めない。
            CookieOutcome::TimedOut,
        ] {
            assert_eq!(
                active(&outcome),
                CookieState::Active(src.clone()),
                "{outcome:?}"
            );
        }
        for outcome in [
            CookieOutcome::Degraded("d".to_string()),
            CookieOutcome::Unreadable("u".to_string()),
        ] {
            assert!(
                matches!(active(&outcome), CookieState::Suspended { .. }),
                "{outcome:?}"
            );
        }

        // suspend は Armed / Active からだけ効く。
        let mut off = CookieState::Off;
        off.suspend("理由".to_string());
        assert_eq!(off, CookieState::Off);
        let mut state = CookieState::Active(src.clone());
        state.suspend("理由".to_string());
        assert_eq!(
            state,
            CookieState::Suspended {
                source: src,
                reason: "理由".to_string()
            }
        );
    }

    #[test]
    fn for_playback_is_some_only_when_active() {
        let src = source("chrome");
        assert!(CookieState::Off.for_playback().is_none());
        assert!(CookieState::Armed(src.clone()).for_playback().is_none());
        assert_eq!(CookieState::Active(src.clone()).for_playback(), Some(&src));
        assert!(
            CookieState::Suspended {
                source: src.clone(),
                reason: "理由".to_string()
            }
            .for_playback()
            .is_none()
        );

        // 検索には Armed でも渡す (最初の判定が検索で行われるため)。
        assert_eq!(CookieState::Armed(src.clone()).for_search(), Some(&src));
        assert_eq!(CookieState::Active(src.clone()).for_search(), Some(&src));
        assert!(CookieState::Off.for_search().is_none());
        assert!(
            CookieState::Suspended {
                source: src,
                reason: "理由".to_string()
            }
            .for_search()
            .is_none()
        );
    }

    #[test]
    fn label_is_none_when_off_and_marks_suspended() {
        let src = source("chrome:Profile 1");
        assert_eq!(CookieState::Off.label(), None);
        assert_eq!(
            CookieState::Armed(src.clone()).label().as_deref(),
            Some("cookies: chrome")
        );
        assert_eq!(
            CookieState::Active(src.clone()).label().as_deref(),
            Some("cookies: chrome")
        );
        assert_eq!(
            CookieState::Suspended {
                source: src,
                reason: "理由".to_string()
            }
            .label()
            .as_deref(),
            Some("cookies: chrome (停止)")
        );
    }

    #[test]
    fn feed_parse_matches_keywords_exactly() {
        assert_eq!(Feed::parse(":ytrec"), Some(Feed::Recommended));
        assert_eq!(Feed::parse(":ythis"), Some(Feed::History));
        assert_eq!(Feed::parse(":ythistory"), Some(Feed::History));
        assert_eq!(Feed::parse(":ytsubs"), Some(Feed::Subscriptions));
        assert_eq!(Feed::parse(":ytwatchlater"), Some(Feed::WatchLater));
        assert_eq!(Feed::parse(":ytfoo"), None);
        assert_eq!(Feed::parse("ytrec"), None);
        assert_eq!(Feed::parse(":ytrec の使い方"), None);
        // yt-dlp へ渡すのは History だけ短い方に寄せる。
        assert_eq!(Feed::History.keyword(), ":ythis");
    }

    #[test]
    fn target_for_query_wraps_searches_and_passes_feeds() {
        let target = Target::for_query("rust tui");
        assert_eq!(target, Target::Search("rust tui".to_string()));
        assert_eq!(target.yt_dlp_url(), "ytsearch10:rust tui");

        let target = Target::for_query(" :ytsubs ");
        assert_eq!(target, Target::Feed(Feed::Subscriptions));
        assert_eq!(target.yt_dlp_url(), ":ytsubs");
    }

    #[test]
    fn feeds_require_login_and_searches_do_not() {
        assert!(!Target::Search("q".to_string()).requires_login());
        assert!(Target::Feed(Feed::Recommended).requires_login());

        // 0 件の文言も分ける。
        assert_eq!(
            Target::Search("q".to_string()).empty_message(),
            "検索結果が0件でした"
        );
        let message = Target::Feed(Feed::Recommended).empty_message();
        assert!(message.starts_with("おすすめ"), "{message}");
        assert!(message.contains("ログイン"), "{message}");

        // 断り文句は停止中かどうかで変わる。
        let off = CookieState::Off.refusal(Feed::History);
        assert!(off.contains(ENV_VAR), "{off}");
        let suspended = CookieState::Suspended {
            source: source("chrome"),
            reason: "読めませんでした".to_string(),
        }
        .refusal(Feed::History);
        assert!(suspended.contains("停止中"), "{suspended}");
        assert!(suspended.contains("読めませんでした"), "{suspended}");

        let message = login_required_message(Feed::History, &source("chrome"));
        assert!(message.starts_with("履歴"), "{message}");
        assert!(message.contains("chrome"), "{message}");
    }
}
