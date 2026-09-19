//! YouTube ログイン cookie を yt-dlp 経由で使うための判定と文言。
//! 外部プロセスには触れない。実行は search.rs / mpv.rs が行う。

use std::path::{Path, PathBuf};
use std::time::Duration;

pub const ENV_VAR: &str = "TUITUBE_COOKIES_FROM_BROWSER";
/// フィードは制限しないと 167 件返ることがある (`:ytrec` 実測)。
pub const FEED_LIMIT: usize = 30;

/// cookie の渡し方。対応ブラウザの一覧も cookies.txt の中身も検証しない
/// (どちらも yt-dlp の更新で変わるため、検証は yt-dlp に任せる)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CookieSource {
    /// yt-dlp の BROWSER[+KEYRING][:PROFILE][::CONTAINER]。
    Browser(String),
    /// エクスポートした cookies.txt (Netscape 形式) のパス。
    File(PathBuf),
}

impl CookieSource {
    /// 設定ファイルの [cookies] browser も環境変数も、同じ spec 文字列を渡す。
    pub fn from_spec(value: Option<&str>) -> Option<Self> {
        let spec = value.unwrap_or_default().trim();
        (!spec.is_empty()).then(|| Self::Browser(spec.to_string()))
    }

    /// 設定ファイルの [cookies] file から。存在の確認は settings 側で済ませておく。
    pub fn from_file(path: Option<&Path>) -> Option<Self> {
        let path = path?;
        (!path.as_os_str().is_empty()).then(|| Self::File(path.to_path_buf()))
    }

    pub fn spec(&self) -> Option<&str> {
        match self {
            Self::Browser(spec) => Some(spec),
            Self::File(_) => None,
        }
    }

    pub fn file(&self) -> Option<&Path> {
        match self {
            Self::Browser(_) => None,
            Self::File(path) => Some(path),
        }
    }

    /// 表示用。':' '+' より前。"chrome:Profile 1" → "chrome"
    pub fn browser(&self) -> Option<&str> {
        let spec = self.spec()?;
        let end = spec.find([':', '+']).unwrap_or(spec.len());
        Some(&spec[..end])
    }

    /// 文言に出す名前。ブラウザ名か、cookies.txt のファイル名。
    pub fn label(&self) -> String {
        match self {
            Self::Browser(_) => self.browser().unwrap_or_default().to_string(),
            Self::File(path) => path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string()),
        }
    }

    /// シェルを経由しないので、引用符もエスケープも付けない。
    pub fn yt_dlp_args(&self) -> [String; 2] {
        match self {
            Self::Browser(spec) => ["--cookies-from-browser".to_string(), spec.clone()],
            Self::File(path) => ["--cookies".to_string(), path.display().to_string()],
        }
    }

    /// 利用者の mpv.conf にある ytdl-raw-options を消さないよう追加形で渡す。
    /// -append は値を ',' で分割しないので、空白やコロン入りの値もそのまま届く。
    pub fn mpv_arg(&self) -> String {
        match self {
            Self::Browser(spec) => {
                format!("--ytdl-raw-options-append=cookies-from-browser={spec}")
            }
            Self::File(path) => {
                format!("--ytdl-raw-options-append=cookies={}", path.display())
            }
        }
    }

    fn is_safari(&self) -> bool {
        self.browser()
            .is_some_and(|browser| browser.eq_ignore_ascii_case("safari"))
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

/// mpv の失敗文言から cookie 由来の行だけを拾う。mpv は cookie と無関係な理由でも
/// 権限エラーを出すので、その行が cookie を指しているときだけ cookie 由来とみなす。
pub fn cookie_store_failure(text: &str, source: &CookieSource) -> Option<String> {
    find_line(text, |line| {
        COOKIE_STORE_MARKERS.iter().any(|m| line.contains(m))
            || (PERMISSION_MARKERS.iter().any(|m| line.contains(m)) && names_cookies(line, source))
    })
}

/// browser 方式は cookie ストアのパスに必ず "Cookies" が入るが、file 方式の名前は
/// 利用者任せなので (例 yt.txt)、指定したパスと一致するかも見る。
fn names_cookies(line: &str, source: &CookieSource) -> bool {
    if line.to_lowercase().contains("cookie") {
        return true;
    }
    source
        .file()
        .is_some_and(|path| line.contains(&path.display().to_string()))
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
/// timeout は実際に待った上限 ([search] timeout_secs)。案内の秒数に出す。
pub fn describe(outcome: &CookieOutcome, source: &CookieSource, timeout: Duration) -> String {
    match outcome {
        // フルディスクアクセスの案内が要るのは Safari だけ。他では誤った案内になる。
        CookieOutcome::Unreadable(detail)
            if detail.contains("Operation not permitted") && source.is_safari() =>
        {
            SAFARI_TCC.to_string()
        }
        CookieOutcome::Unreadable(detail) => format!(
            "cookie を読めませんでした ({}): {detail}。cookie 無しで検索しました",
            source.label()
        ),
        CookieOutcome::Degraded(_) => format!(
            "cookie を復号できませんでした ({})。キーチェーンのダイアログで「常に許可」を選んでください。以後は cookie 無しで動作します",
            source.label()
        ),
        CookieOutcome::TimedOut => format!(
            "検索がタイムアウトしました ({} 秒)。cookie 連携の初回は macOS のキーチェーン許可ダイアログが別ウィンドウで出ている可能性があります。「常に許可」を選び、tuitube を再起動してください。以後は cookie 無しで動作します",
            timeout.as_secs()
        ),
        _ => String::new(),
    }
}

/// フィード名が要るので describe とは別にする。
pub fn login_required_message(feed: Feed, source: &CookieSource) -> String {
    format!(
        "{} にはログインが必要です。{} の cookie が YouTube にログイン済みか確認してください",
        feed.label(),
        source.label()
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
    pub fn from_source(source: Option<CookieSource>) -> Self {
        match source {
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
    pub fn observe(&mut self, outcome: &CookieOutcome, timeout: Duration) {
        let Some(source) = self.for_search().cloned() else {
            return;
        };
        let armed = matches!(self, Self::Armed(_));
        match outcome {
            CookieOutcome::Ok => *self = Self::Active(source),
            CookieOutcome::Degraded(_) | CookieOutcome::Unreadable(_) => {
                let reason = describe(outcome, &source, timeout);
                *self = Self::Suspended { source, reason };
            }
            CookieOutcome::TimedOut if armed => {
                let reason = describe(outcome, &source, timeout);
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
                Some(format!("cookies: {}", source.label()))
            }
            Self::Suspended { source, .. } => Some(format!("cookies: {} (停止)", source.label())),
        }
    }

    /// ログイン必須の要求を yt-dlp を起動せずに断るときの文言。
    /// ステータス行は 1 行しかないので、80 桁に収まる長さにする。
    /// 環境変数での指定は設定ファイルの [cookies] のコメントに書いてある。
    pub fn refusal(&self, feed: Feed) -> String {
        match self {
            Self::Suspended { reason, .. } => {
                format!("{} は cookie 連携が停止中: {reason}", feed.label())
            }
            // ブラウザを置けない環境からも使うので、file 方式も必ず併記する。
            _ => format!(
                "{} には cookie 連携が必要です ([cookies] browser か file)",
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
    /// タブの既定の並びと、設定ファイルの案内が使う一覧。
    pub const ALL: [Feed; 4] = [
        Feed::Recommended,
        Feed::History,
        Feed::Subscriptions,
        Feed::WatchLater,
    ];

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

    /// limit は [search] limit。フィードは件数を --playlist-end で渡すのでここでは使わない。
    pub fn yt_dlp_url(&self, limit: usize) -> String {
        match self {
            Target::Search(query) => format!("ytsearch{limit}:{query}"),
            Target::Feed(feed) => feed.keyword().to_string(),
        }
    }

    /// 確認先は cookie の出どころで変わる。file 方式ではブラウザのログイン状態は
    /// 関係がなく、見るのは cookies.txt の中身。
    pub fn empty_message(&self, source: Option<&CookieSource>) -> String {
        match self {
            Target::Search(_) => "検索結果が0件でした".to_string(),
            // `:ytrec` は未ログインでもエラーにならず 0 件で返るので、件数でなく原因を出す。
            Target::Feed(feed) => match source {
                Some(source) => format!(
                    "{} が空でした。{} の cookie が YouTube にログイン済みか確認してください",
                    feed.label(),
                    source.label()
                ),
                None => format!(
                    "{} が空でした。cookie が YouTube にログイン済みか確認してください",
                    feed.label()
                ),
            },
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
    use std::time::Duration;

    /// 秒数そのものを確かめる以外のテストで渡す上限。
    const TIMEOUT: Duration = crate::search::YT_DLP_TIMEOUT;

    fn source(spec: &str) -> CookieSource {
        CookieSource::from_spec(Some(spec)).expect("spec")
    }

    fn file_source(path: &str) -> CookieSource {
        CookieSource::from_file(Some(Path::new(path))).expect("path")
    }

    #[test]
    fn from_spec_trims_and_rejects_blank() {
        assert_eq!(CookieSource::from_spec(None), None);
        assert_eq!(CookieSource::from_spec(Some("")), None);
        assert_eq!(CookieSource::from_spec(Some("  ")), None);
        assert_eq!(
            CookieSource::from_spec(Some(" chrome:Profile 1 "))
                .expect("値がある")
                .spec(),
            Some("chrome:Profile 1")
        );
    }

    #[test]
    fn from_file_rejects_an_empty_path() {
        assert_eq!(CookieSource::from_file(None), None);
        assert_eq!(CookieSource::from_file(Some(Path::new(""))), None);
        assert_eq!(
            CookieSource::from_file(Some(Path::new("/tmp/cookies.txt"))),
            Some(CookieSource::File(PathBuf::from("/tmp/cookies.txt")))
        );
    }

    #[test]
    fn a_source_carries_either_a_browser_spec_or_a_path() {
        let browser = source("chrome:Profile 1");
        assert_eq!(browser.spec(), Some("chrome:Profile 1"));
        assert_eq!(browser.file(), None);

        let file = file_source("/tmp/cookies.txt");
        assert_eq!(file.spec(), None);
        assert_eq!(file.browser(), None);
        assert_eq!(file.file(), Some(Path::new("/tmp/cookies.txt")));
    }

    #[test]
    fn yt_dlp_args_for_a_file_pass_the_path_to_the_cookies_flag() {
        assert_eq!(
            file_source("/tmp/my cookies.txt").yt_dlp_args(),
            ["--cookies".to_string(), "/tmp/my cookies.txt".to_string()]
        );
    }

    #[test]
    fn mpv_arg_for_a_file_uses_the_cookies_option() {
        assert_eq!(
            file_source("/tmp/my cookies.txt").mpv_arg(),
            "--ytdl-raw-options-append=cookies=/tmp/my cookies.txt"
        );
    }

    #[test]
    fn a_file_source_is_labelled_by_its_file_name() {
        assert_eq!(
            file_source("/home/u/.config/tuitube/cookies.txt").label(),
            "cookies.txt"
        );
        // ファイル名を取れない指定は、そのままの綴りで出す。
        assert_eq!(file_source("/").label(), "/");
        assert_eq!(source("chrome:Profile 1").label(), "chrome");

        let file = file_source("/tmp/cookies.txt");
        assert_eq!(
            CookieState::Armed(file.clone()).label().as_deref(),
            Some("cookies: cookies.txt")
        );
        assert_eq!(
            CookieState::Suspended {
                source: file,
                reason: "理由".to_string()
            }
            .label()
            .as_deref(),
            Some("cookies: cookies.txt (停止)")
        );
    }

    #[test]
    fn full_disk_access_is_not_suggested_for_a_cookie_file() {
        // TCC はブラウザの cookie ストアの話で、自分で置いたファイルには関係がない。
        let outcome = classify(
            Some(1),
            "ERROR: [Errno 1] Operation not permitted: '/tmp/cookies.txt'",
        );
        let text = describe(&outcome, &file_source("/tmp/cookies.txt"), TIMEOUT);
        assert!(!text.contains("フルディスクアクセス"), "{text}");
        assert!(text.contains("cookies.txt"), "{text}");
        assert!(text.contains("Operation not permitted"), "{text}");
    }

    #[test]
    fn a_file_source_names_the_file_when_login_is_required() {
        let message = login_required_message(Feed::History, &file_source("/tmp/cookies.txt"));
        assert!(message.starts_with("履歴"), "{message}");
        assert!(message.contains("cookies.txt"), "{message}");
    }

    #[test]
    fn an_empty_feed_points_at_whichever_cookie_source_is_in_use() {
        let feed = Target::Feed(Feed::Subscriptions);

        // file 方式ではブラウザのログイン状態は関係がない。見るのは cookies.txt。
        let message = feed.empty_message(Some(&file_source("/tmp/cookies.txt")));
        assert!(message.contains("cookies.txt"), "{message}");
        assert!(!message.contains("ブラウザ"), "{message}");

        let message = feed.empty_message(Some(&source("chrome:Profile 1")));
        assert!(message.contains("chrome"), "{message}");
    }

    #[test]
    fn from_source_arms_when_present_and_is_off_otherwise() {
        let src = source("chrome");
        assert_eq!(
            CookieState::from_source(Some(src.clone())),
            CookieState::Armed(src)
        );
        assert_eq!(CookieState::from_source(None), CookieState::Off);
    }

    #[test]
    fn refusal_names_the_config_key_and_fits_one_status_row() {
        for feed in Feed::ALL {
            let off = CookieState::Off.refusal(feed);
            assert!(off.contains("[cookies] browser"), "{off}");
            // ブラウザを置けない環境の利用者にも設定先を伝える。
            assert!(off.contains("file"), "{off}");
            assert!(off.contains(feed.label()), "{off}");
            // ステータス行は "エラー: " を足して 80 桁に描く。切れると設定先が読めない。
            let width = crate::grid::display_width(&format!("エラー: {off}"));
            assert!(width <= 80, "{width} 桁: {off}");
        }
    }

    #[test]
    fn browser_is_the_part_before_profile_or_keyring() {
        assert_eq!(source("chrome:Profile 1").browser(), Some("chrome"));
        assert_eq!(source("chrome+basictext").browser(), Some("chrome"));
        assert_eq!(source("firefox::work").browser(), Some("firefox"));
        assert_eq!(source("safari").browser(), Some("safari"));
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
        let text = describe(&outcome, &source("safari"), TIMEOUT);
        assert!(text.contains("フルディスクアクセス"), "{text}");

        // それ以外は yt-dlp の本文をそのまま見せる。
        let other = classify(
            Some(1),
            "ERROR: could not find firefox cookies database in '/x/Profiles'",
        );
        let text = describe(&other, &source("firefox"), TIMEOUT);
        assert!(text.contains("firefox"), "{text}");
        assert!(
            text.contains("could not find firefox cookies database"),
            "{text}"
        );
        assert!(text.contains("cookie 無しで検索しました"), "{text}");
    }

    #[test]
    fn the_timeout_notice_names_the_seconds_that_were_actually_used() {
        for secs in [5, 30, 120, 300] {
            let text = describe(
                &CookieOutcome::TimedOut,
                &source("chrome"),
                Duration::from_secs(secs),
            );
            assert!(text.contains(&format!("{secs} 秒")), "{text}");
            assert!(text.contains("キーチェーン"), "{text}");
        }
    }

    #[test]
    fn suspending_on_a_timeout_keeps_the_seconds_in_the_reason() {
        let mut state = CookieState::Armed(source("chrome"));
        state.observe(&CookieOutcome::TimedOut, Duration::from_secs(120));
        let CookieState::Suspended { reason, .. } = state else {
            panic!("停止する");
        };
        assert!(reason.contains("120 秒"), "{reason}");
    }

    #[test]
    fn full_disk_access_is_not_suggested_for_other_browsers() {
        // chrome を指定しているのに Safari の設定を直せとは言わない。
        let outcome = classify(
            Some(1),
            "ERROR: [Errno 1] Operation not permitted: '/Users/x/Library/Application Support/Google/Chrome/Default/Cookies'",
        );
        let text = describe(&outcome, &source("chrome:Profile 1"), TIMEOUT);
        assert!(!text.contains("フルディスクアクセス"), "{text}");
        assert!(text.contains("chrome"), "{text}");
        assert!(text.contains("Operation not permitted"), "{text}");

        // Safari は大文字小文字を問わず案内する。
        let text = describe(&outcome, &source("Safari"), TIMEOUT);
        assert!(text.contains("フルディスクアクセス"), "{text}");
    }

    #[test]
    fn cookie_store_failure_ignores_permission_errors_about_other_files() {
        let chrome = source("chrome");
        // mpv はストリームやファイルのオープン失敗でも同じ文言を出す。
        assert_eq!(
            cookie_store_failure("Operation not permitted: '/dev/dsp'", &chrome),
            None
        );
        assert_eq!(
            cookie_store_failure("Permission denied: '/x.mkv'", &chrome),
            None
        );
        assert_eq!(
            cookie_store_failure("Failed to recognize file format.", &chrome),
            None
        );

        // cookie ストアを指す行だけ拾い、ERROR: の飾りは落とす。
        assert_eq!(
            cookie_store_failure(
                "ERROR: could not find chrome cookies database in '/x'",
                &chrome
            ),
            Some("could not find chrome cookies database in '/x'".to_string())
        );
        assert_eq!(
            cookie_store_failure(
                "ERROR: [Errno 13] Permission denied: '/Users/x/Library/Cookies/Cookies.binarycookies'",
                &chrome
            ),
            Some(
                "[Errno 13] Permission denied: '/Users/x/Library/Cookies/Cookies.binarycookies'"
                    .to_string()
            )
        );
        assert_eq!(
            cookie_store_failure(
                "yt-dlp: error: unsupported browser specified for cookies: \"arc\"",
                &chrome
            ),
            Some("unsupported browser specified for cookies: \"arc\"".to_string())
        );
    }

    #[test]
    fn a_cookie_file_is_recognised_by_its_path_whatever_it_is_named() {
        // file 方式の名前は利用者任せで、"cookie" が入るとは限らない。
        let file = file_source("/home/ubuntu/.config/tuitube/yt.txt");
        assert_eq!(
            cookie_store_failure(
                "ERROR: [Errno 13] Permission denied: '/home/ubuntu/.config/tuitube/yt.txt'",
                &file
            ),
            Some("[Errno 13] Permission denied: '/home/ubuntu/.config/tuitube/yt.txt'".to_string())
        );
        // 別のファイルの権限エラーは cookie 由来にしない。
        assert_eq!(
            cookie_store_failure("Permission denied: '/home/ubuntu/movie.mkv'", &file),
            None
        );
        // browser 方式では同じ行を cookie 由来と判定しない (パスの一致が無い)。
        assert_eq!(
            cookie_store_failure(
                "ERROR: [Errno 13] Permission denied: '/home/ubuntu/.config/tuitube/yt.txt'",
                &source("chrome")
            ),
            None
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
            state.observe(outcome, TIMEOUT);
            assert_eq!(state, CookieState::Off, "{outcome:?}");
        }

        // Suspended も動かない。
        for outcome in &outcomes {
            let mut state = CookieState::Suspended {
                source: src.clone(),
                reason: "理由".to_string(),
            };
            state.observe(outcome, TIMEOUT);
            assert!(
                matches!(state, CookieState::Suspended { .. }),
                "{outcome:?}"
            );
        }

        let armed = |outcome: &CookieOutcome| {
            let mut state = CookieState::Armed(src.clone());
            state.observe(outcome, TIMEOUT);
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
            state.observe(outcome, TIMEOUT);
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

        // ALL はタブの並びと設定ファイルの案内が使うので、parse と往復する。
        for feed in Feed::ALL {
            assert_eq!(Feed::parse(feed.keyword()), Some(feed), "{}", feed.label());
        }
    }

    #[test]
    fn target_for_query_wraps_searches_and_passes_feeds() {
        let target = Target::for_query("rust tui");
        assert_eq!(target, Target::Search("rust tui".to_string()));
        assert_eq!(target.yt_dlp_url(10), "ytsearch10:rust tui");
        // 件数は設定から渡る。
        assert_eq!(target.yt_dlp_url(25), "ytsearch25:rust tui");

        let target = Target::for_query(" :ytsubs ");
        assert_eq!(target, Target::Feed(Feed::Subscriptions));
        assert_eq!(target.yt_dlp_url(10), ":ytsubs");
        assert_eq!(target.yt_dlp_url(25), ":ytsubs");
    }

    #[test]
    fn feeds_require_login_and_searches_do_not() {
        assert!(!Target::Search("q".to_string()).requires_login());
        assert!(Target::Feed(Feed::Recommended).requires_login());

        // 0 件の文言も分ける。
        assert_eq!(
            Target::Search("q".to_string()).empty_message(None),
            "検索結果が0件でした"
        );
        let message = Target::Feed(Feed::Recommended).empty_message(None);
        assert!(message.starts_with("おすすめ"), "{message}");
        assert!(message.contains("ログイン"), "{message}");

        // 断り文句は停止中かどうかで変わる。
        let off = CookieState::Off.refusal(Feed::History);
        assert!(off.contains("[cookies] browser"), "{off}");
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
