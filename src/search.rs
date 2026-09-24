use crate::cookies::{
    CHANNEL_LIMIT, CookieOutcome, CookieSource, FEED_LIMIT, PLAYLIST_LIMIT, PLAYLISTS_URL, Target,
    classify,
};
use serde_json::Value;
use std::future::Future;
use std::io::ErrorKind;
use std::process::Output;
use std::time::Duration;
use tokio::process::Command;

/// [search] timeout_secs を書いていないときの、yt-dlp 1 回ぶんの上限。
/// cookie 付きの失敗から再試行すると最大 2 回分待つ。
pub const YT_DLP_TIMEOUT: Duration = Duration::from_secs(30);
/// 1 本ぶんのチャンネル引きの上限。実測 3〜4 秒で返る。
pub const CHANNEL_LOOKUP_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, PartialEq)]
pub struct SearchResult {
    pub id: String,
    pub title: String,
    pub duration: Option<f64>,
    pub uploader: Option<String>,
    /// チャンネルへ移るための UC... 形式の ID。チャンネルタブ経由の行では入らない。
    pub channel_id: Option<String>,
    /// 配信中か。--flat-playlist でも返るので、どの画面でも行ごとに判定できる。
    pub is_live: bool,
}

impl SearchResult {
    pub fn url(&self) -> String {
        format!("https://www.youtube.com/watch?v={}", self.id)
    }
}

pub fn parse_lines(output: &str) -> Vec<SearchResult> {
    output.lines().filter_map(parse_line).collect()
}

/// Target ごとのパース。検索結果ページはチャンネルの行も混ぜて返すので動画だけ残す。
/// Feed (後で見る・履歴・登録チャンネル・おすすめ) は YouTube 側の返却順に追加順・
/// 視聴順等の意味があるので並べ替えない。それ以外は公開日の新しい順に並べ替える。
pub fn parse_target_lines(target: &Target, output: &str) -> Vec<SearchResult> {
    let mut lines: Vec<ParsedLine> = output
        .lines()
        .filter_map(parse_entry)
        .filter(|line| !matches!(target, Target::Search(_)) || line.is_video)
        .collect();
    // timestamp は動画自体の公開日で、プレイリストへの追加日ではない。Feed でこれを
    // 使うと YouTube 側の意味のある返却順を壊すので、Feed だけは並べ替えない。
    // 日付が無い行同士は元の順序を保つ (安定ソート)。
    if !matches!(target, Target::Feed(_)) {
        lines.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
    }
    lines.into_iter().map(|line| line.result).collect()
}

/// 1 行ぶんの読み取り結果。timestamp と ie_key は並べ替えと選り分けにしか使わないので、
/// SearchResult には載せずここでだけ持つ。
struct ParsedLine {
    result: SearchResult,
    /// approximate_date で入る公開日時。履歴等では入らない。
    timestamp: Option<i64>,
    /// ie_key が "Youtube" か。チャンネルの行は "YoutubeTab" で返る。
    is_video: bool,
}

fn parse_entry(line: &str) -> Option<ParsedLine> {
    let value = parse_value(line)?;
    Some(ParsedLine {
        result: result_from(&value)?,
        timestamp: value.get("timestamp").and_then(Value::as_i64),
        is_video: value.get("ie_key").and_then(Value::as_str) == Some("Youtube"),
    })
}

fn parse_line(line: &str) -> Option<SearchResult> {
    result_from(&parse_value(line)?)
}

fn parse_value(line: &str) -> Option<Value> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    serde_json::from_str(line).ok()
}

fn result_from(value: &Value) -> Option<SearchResult> {
    let id = value.get("id")?.as_str()?.to_string();
    let title = value
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("(title unknown)")
        .to_string();
    let duration = value.get("duration").and_then(Value::as_f64);
    // フィードの行は uploader を欠くことがあるので channel でも拾う。
    let uploader = value
        .get("uploader")
        .and_then(Value::as_str)
        .or_else(|| value.get("channel").and_then(Value::as_str))
        .map(str::to_string);
    let channel_id = value
        .get("channel_id")
        .and_then(Value::as_str)
        .map(str::to_string);
    let is_live = value
        .get("is_live")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Some(SearchResult {
        id,
        title,
        duration,
        uploader,
        channel_id,
        is_live,
    })
}

/// プレイリスト一覧の 1 件。一覧の行は uploader が固定文字列で duration も無いので、
/// 動画用の SearchResult とは分ける。
#[derive(Debug, Clone, PartialEq)]
pub struct PlaylistEntry {
    pub id: String,
    pub title: String,
}

pub fn parse_playlist_lines(output: &str) -> Vec<PlaylistEntry> {
    output.lines().filter_map(parse_playlist_line).collect()
}

fn parse_playlist_line(line: &str) -> Option<PlaylistEntry> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let value: Value = serde_json::from_str(line).ok()?;
    let id = value.get("id")?.as_str()?.to_string();
    let title = value
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("(title unknown)")
        .to_string();
    Some(PlaylistEntry { id, title })
}

/// 本番は tokio Command、テストは台本どおりの Output を返す偽物。
pub trait YtDlp {
    fn run(&self, args: Vec<String>) -> impl Future<Output = std::io::Result<Output>> + Send;
}

pub struct RealYtDlp;

impl YtDlp for RealYtDlp {
    fn run(&self, args: Vec<String>) -> impl Future<Output = std::io::Result<Output>> + Send {
        // 検索中にアプリを終了しても yt-dlp を孤児にしない。
        Command::new("yt-dlp")
            .args(args)
            .kill_on_drop(true)
            .output()
    }
}

pub fn yt_dlp_args(target: &Target, cookies: Option<&CookieSource>, limit: usize) -> Vec<String> {
    let mut args = vec![
        target.yt_dlp_url(),
        "--flat-playlist".to_string(),
        "--dump-json".to_string(),
        // 行に公開日を入れさせる。付けないと timestamp が常に null で並べ替えられない。
        "--extractor-args".to_string(),
        "youtubetab:approximate_date".to_string(),
        "--playlist-end".to_string(),
    ];
    args.push(match target {
        Target::Search(_) => limit.to_string(),
        Target::Feed(_) => FEED_LIMIT.to_string(),
        Target::Channel { .. } => CHANNEL_LIMIT.to_string(),
        Target::Playlist(_) => PLAYLIST_LIMIT.to_string(),
        Target::Video(_) => "1".to_string(),
    });
    if let Some(cookies) = cookies {
        args.extend(cookies.yt_dlp_args());
    }
    args
}

/// プレイリスト一覧を取る引数。返る行は動画でないので parse_playlist_lines で読む。
pub fn playlists_args(cookies: Option<&CookieSource>) -> Vec<String> {
    let mut args = vec![
        PLAYLISTS_URL.to_string(),
        "--flat-playlist".to_string(),
        "--dump-json".to_string(),
    ];
    if let Some(cookies) = cookies {
        args.extend(cookies.yt_dlp_args());
    }
    args
}

/// 引き当てたチャンネル。履歴タブの行は名前も持たないので、引いた行から一緒に拾う。
#[derive(Debug, Clone, PartialEq)]
pub struct ChannelRef {
    pub id: String,
    pub uploader: Option<String>,
}

/// 1 本だけを取る引数。--flat-playlist を付けないので channel_id まで入った行が返る。
pub fn channel_lookup_args(url: &str) -> Vec<String> {
    vec![url.to_string(), "--dump-json".to_string()]
}

/// 選択中の 1 本からチャンネルを引く。フィードの行は channel_id を欠くので、
/// チャンネルへ移る直前にここで補う。取れたが持っていない動画は Ok(None)。
pub async fn fetch_channel(runner: &impl YtDlp, url: &str) -> Result<Option<ChannelRef>, String> {
    let args = channel_lookup_args(url);
    let output = match tokio::time::timeout(CHANNEL_LOOKUP_TIMEOUT, runner.run(args)).await {
        Err(_) => {
            return Err(format!(
                "チャンネル情報の取得がタイムアウトしました ({} 秒)",
                CHANNEL_LOOKUP_TIMEOUT.as_secs()
            ));
        }
        Ok(Err(e)) => return Err(launch_error(&e)),
        Ok(Ok(output)) => output,
    };
    let found = parse_lines(&String::from_utf8_lossy(&output.stdout))
        .into_iter()
        .next()
        .and_then(|result| {
            let uploader = result.uploader;
            result.channel_id.map(|id| ChannelRef { id, uploader })
        });
    // 警告つきで終わっても行が揃っていればそれを使う。
    if found.is_none() && !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("yt-dlp が失敗しました: {}", stderr_detail(&stderr)));
    }
    Ok(found)
}

/// プレイリスト一覧を取る。返る行が動画でないので run_search とは別経路。
pub async fn fetch_playlists(
    runner: &impl YtDlp,
    cookies: Option<&CookieSource>,
    timeout: Duration,
) -> Result<Vec<PlaylistEntry>, String> {
    let args = playlists_args(cookies);
    let output = match tokio::time::timeout(timeout, runner.run(args)).await {
        Err(_) => {
            return Err(format!(
                "プレイリスト一覧の取得がタイムアウトしました ({} 秒)",
                timeout.as_secs()
            ));
        }
        Ok(Err(e)) => return Err(launch_error(&e)),
        Ok(Ok(output)) => output,
    };
    let entries = parse_playlist_lines(&String::from_utf8_lossy(&output.stdout));
    // 警告つきで終わっても行が揃っていればそれを使う。
    if entries.is_empty() && !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("yt-dlp が失敗しました: {}", stderr_detail(&stderr)));
    }
    Ok(entries)
}

#[derive(Debug)]
pub struct SearchReport {
    pub results: Result<Vec<SearchResult>, String>,
    pub outcome: CookieOutcome,
    /// cookie 無しで再実行した。
    pub fell_back: bool,
    /// この検索を待った上限。結果が届くまでに設定を変えられても、
    /// 文言の秒数はこちらを使う。
    pub timeout: Duration,
    /// プレイリストを取ったとき、yt-dlp が各行に入れるプレイリストの名前。
    pub playlist_title: Option<String>,
}

struct Attempt {
    results: Result<Vec<SearchResult>, String>,
    outcome: CookieOutcome,
    playlist_title: Option<String>,
}

pub async fn run_search(
    runner: &impl YtDlp,
    target: &Target,
    cookies: Option<&CookieSource>,
    limit: usize,
    timeout: Duration,
) -> SearchReport {
    let first = attempt(runner, target, cookies, limit, timeout).await;
    // 読めないときは検索そのものが実行されないので、同じ target を cookie 無しで出し直す。
    if let CookieOutcome::Unreadable(_) = &first.outcome {
        let retry = attempt(runner, target, None, limit, timeout).await;
        return SearchReport {
            results: retry.results,
            outcome: first.outcome,
            fell_back: true,
            timeout,
            playlist_title: retry.playlist_title,
        };
    }
    SearchReport {
        results: first.results,
        outcome: first.outcome,
        fell_back: false,
        timeout,
        playlist_title: first.playlist_title,
    }
}

async fn attempt(
    runner: &impl YtDlp,
    target: &Target,
    cookies: Option<&CookieSource>,
    limit: usize,
    timeout: Duration,
) -> Attempt {
    let used_cookies = cookies.is_some();
    let args = yt_dlp_args(target, cookies, limit);
    let output = match tokio::time::timeout(timeout, runner.run(args)).await {
        Err(_) => {
            return Attempt {
                results: Err(format!(
                    "検索がタイムアウトしました ({} 秒)",
                    timeout.as_secs()
                )),
                outcome: outcome_of(used_cookies, CookieOutcome::TimedOut),
                playlist_title: None,
            };
        }
        Ok(Err(e)) => {
            return Attempt {
                results: Err(launch_error(&e)),
                outcome: outcome_of(used_cookies, CookieOutcome::Unknown),
                playlist_title: None,
            };
        }
        Ok(Ok(output)) => output,
    };

    let stderr = String::from_utf8_lossy(&output.stderr);
    let outcome = if used_cookies {
        classify(output.status.code(), &stderr)
    } else {
        CookieOutcome::NotUsed
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let results = parse_target_lines(target, &stdout);
    let results = if results.is_empty() && !output.status.success() {
        Err(format!("yt-dlp が失敗しました: {}", stderr_detail(&stderr)))
    } else {
        Ok(results)
    };
    Attempt {
        results,
        outcome,
        playlist_title: playlist_title_of(&stdout),
    }
}

/// 最初に見つかった playlist_title。プレイリストの各行に同じ名前が入る。
fn playlist_title_of(output: &str) -> Option<String> {
    output
        .lines()
        .filter_map(parse_value)
        .find_map(|value| value.get("playlist_title")?.as_str().map(str::to_string))
}

/// 失敗の理由を伝える一行。yt-dlp は原因を ERROR 行に書き、その後ろに警告が続くことが
/// あるので、ERROR 行があればそちらを採る。受け取る側はこの一行で原因を見分ける。
fn stderr_detail(stderr: &str) -> &str {
    let mut last = "";
    let mut error = "";
    for line in stderr.lines().map(str::trim).filter(|l| !l.is_empty()) {
        last = line;
        if line.starts_with("ERROR:") {
            error = line;
        }
    }
    if error.is_empty() { last } else { error }
}

/// yt-dlp が「そのタブは無い」と言って落ちたか。配信タブを持たないチャンネルの
/// `/streams` がこれに当たる。起動失敗・通信失敗・タイムアウトを一緒に飲み込まないよう、
/// 一覧が無いだけの失敗はここでだけ見分ける。
pub fn is_missing_tab(error: &str) -> bool {
    let error = error.to_ascii_lowercase();
    error.contains("does not have a") && error.contains("tab")
}

fn outcome_of(used_cookies: bool, outcome: CookieOutcome) -> CookieOutcome {
    if used_cookies {
        outcome
    } else {
        CookieOutcome::NotUsed
    }
}

pub fn launch_error(e: &std::io::Error) -> String {
    if e.kind() == ErrorKind::NotFound {
        "yt-dlp が見つかりません (PATH を確認してください)".to_string()
    } else {
        format!("yt-dlp の起動に失敗しました: {e}")
    }
}

#[cfg(test)]
pub(crate) mod fixtures {
    use super::*;
    use std::collections::VecDeque;
    use std::os::unix::process::ExitStatusExt;
    use std::sync::{Arc, Mutex};

    pub enum Step {
        Done(std::io::Result<Output>),
        /// 応答が返らない状況 (キーチェーンのダイアログ待ち等)。
        Hang,
    }

    pub fn done(code: i32, stdout: &str, stderr: &str) -> Step {
        Step::Done(Ok(Output {
            // ExitStatus は生の wait ステータスから作る (下位 8 bit はシグナル用)。
            status: ExitStatusExt::from_raw(code << 8),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }))
    }

    /// yt-dlp そのものが PATH に無い。
    pub fn missing() -> Step {
        Step::Done(Err(std::io::Error::from(ErrorKind::NotFound)))
    }

    /// 起動せず台本どおりに振る舞う yt-dlp。呼ばれた引数を溜める。
    /// 溜め込み先を Arc で持つのは、spawn したタスクへ渡した後も呼び出しを読むため。
    #[derive(Clone, Default)]
    pub struct FakeYtDlp {
        script: Arc<Mutex<VecDeque<Step>>>,
        calls: Arc<Mutex<Vec<Vec<String>>>>,
    }

    impl FakeYtDlp {
        pub fn new(steps: impl IntoIterator<Item = Step>) -> Self {
            Self {
                script: Arc::new(Mutex::new(steps.into_iter().collect())),
                calls: Arc::new(Mutex::new(Vec::new())),
            }
        }

        pub fn calls(&self) -> Vec<Vec<String>> {
            self.calls.lock().expect("lock").clone()
        }
    }

    impl YtDlp for FakeYtDlp {
        fn run(&self, args: Vec<String>) -> impl Future<Output = std::io::Result<Output>> + Send {
            self.calls.lock().expect("lock").push(args);
            let step = self.script.lock().expect("lock").pop_front();
            async move {
                match step {
                    Some(Step::Done(result)) => result,
                    Some(Step::Hang) => std::future::pending().await,
                    None => panic!("台本に無い呼び出し"),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cookies::{CHANNEL_LIMIT, ChannelTab, Feed};
    use crate::search::fixtures::{FakeYtDlp, Step, done, missing};

    const LINE_FULL: &str = r#"{"ie_key":"Youtube","id":"abc123","title":"Rust TUI tutorial","duration":612.0,"uploader":"someone"}"#;

    fn source(spec: &str) -> CookieSource {
        CookieSource::from_spec(Some(spec)).expect("spec")
    }

    fn has_cookie_flag(args: &[String]) -> bool {
        args.iter().any(|a| a == "--cookies-from-browser")
    }

    /// --playlist-end に渡した件数。
    fn playlist_end(args: &[String]) -> Option<String> {
        let at = args.iter().position(|a| a == "--playlist-end")?;
        args.get(at + 1).cloned()
    }

    #[test]
    fn parses_full_lines() {
        let out = format!("{LINE_FULL}\n{LINE_FULL}\n");
        let results = parse_lines(&out);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].id, "abc123");
        assert_eq!(results[0].title, "Rust TUI tutorial");
        assert_eq!(results[0].duration, Some(612.0));
        assert_eq!(results[0].uploader.as_deref(), Some("someone"));
    }

    #[test]
    fn missing_optional_fields_become_none() {
        let results = parse_lines(r#"{"id":"xyz","title":"no meta"}"#);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].duration, None);
        assert_eq!(results[0].uploader, None);
    }

    #[test]
    fn null_duration_and_missing_title() {
        let results = parse_lines(r#"{"id":"xyz","duration":null,"uploader":null}"#);
        assert_eq!(results[0].title, "(title unknown)");
        assert_eq!(results[0].duration, None);
        assert_eq!(results[0].uploader, None);
    }

    #[test]
    fn skips_blank_and_broken_lines() {
        let out = format!("\n  \n{LINE_FULL}\nnot json\n{{\"title\":\"no id\"}}\n\n");
        let results = parse_lines(&out);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "abc123");
    }

    #[test]
    fn empty_output_yields_no_results() {
        assert!(parse_lines("").is_empty());
    }

    #[test]
    fn ignores_unrelated_fields_of_real_output() {
        let line = r#"{"_type":"url","ie_key":"Youtube","id":"awX7DUp-r14","url":"https://www.youtube.com/watch?v=awX7DUp-r14","title":"Rust TUI Tutorial: Ratatui, Multithreading, and Responsiveness","duration":2404.0,"channel_id":"UCxxx","uploader":"Green Tea Coding","uploader_id":"@greenteacoding","view_count":16224,"thumbnails":[{"url":"https://i.ytimg.com/vi/awX7DUp-r14/hq720.jpg","height":404}],"playlist":"rust tui","epoch":1758100000}"#;
        let results = parse_lines(line);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "awX7DUp-r14");
        assert_eq!(results[0].duration, Some(2404.0));
        assert_eq!(results[0].uploader.as_deref(), Some("Green Tea Coding"));
    }

    #[test]
    fn builds_watch_url() {
        let results = parse_lines(LINE_FULL);
        assert_eq!(results[0].url(), "https://www.youtube.com/watch?v=abc123");
    }

    #[test]
    fn parse_line_reads_the_channel_id() {
        let line = r#"{"id":"a","title":"t","channel_id":"UCabc","uploader":"Up"}"#;
        assert_eq!(parse_lines(line)[0].channel_id.as_deref(), Some("UCabc"));
        // チャンネルタブ経由の行は channel_id を欠く。c キーは無反応になるだけ。
        let without = r#"{"id":"a","title":"t"}"#;
        assert_eq!(parse_lines(without)[0].channel_id, None);
        let null = r#"{"id":"a","title":"t","channel_id":null}"#;
        assert_eq!(parse_lines(null)[0].channel_id, None);
    }

    #[test]
    fn parse_line_reads_the_live_flag() {
        let live = r#"{"id":"a","title":"t","is_live":true}"#;
        assert!(parse_lines(live)[0].is_live);
        // 欠け・null・false はどれもライブでない扱いにする。
        for not_live in [
            r#"{"id":"a","title":"t","is_live":false}"#,
            r#"{"id":"a","title":"t","is_live":null}"#,
            r#"{"id":"a","title":"t"}"#,
        ] {
            assert!(!parse_lines(not_live)[0].is_live, "{not_live}");
        }
    }

    /// 検索結果ページの動画の行。approximate_date を付けると timestamp が入る。
    fn video_line(id: &str, timestamp: Option<i64>) -> String {
        match timestamp {
            Some(timestamp) => format!(
                r#"{{"ie_key":"Youtube","id":"{id}","title":"{id}","timestamp":{timestamp}}}"#
            ),
            None => format!(r#"{{"ie_key":"Youtube","id":"{id}","title":"{id}"}}"#),
        }
    }

    /// 検索結果ページは動画の間にチャンネルの行を混ぜて返す。
    const CHANNEL_ROW: &str = r#"{"_type":"url","ie_key":"YoutubeTab","id":"UCabc","title":"Some Channel","url":"https://www.youtube.com/channel/UCabc"}"#;

    fn ids(results: &[SearchResult]) -> Vec<&str> {
        results.iter().map(|r| r.id.as_str()).collect()
    }

    fn all_targets() -> [Target; 4] {
        [
            Target::Search("q".to_string()),
            Target::Feed(Feed::Recommended),
            Target::Channel {
                id: "UCabc".to_string(),
                tab: ChannelTab::Videos,
            },
            Target::Playlist("PLabc123".to_string()),
        ]
    }

    #[test]
    fn a_search_drops_the_channel_rows_of_the_results_page() {
        let out = format!(
            "{}\n{CHANNEL_ROW}\n{}\n",
            video_line("v1", Some(3)),
            video_line("v2", Some(2))
        );
        let results = parse_target_lines(&Target::Search("q".to_string()), &out);
        assert_eq!(ids(&results), ["v1", "v2"]);
    }

    #[test]
    fn the_other_targets_keep_every_row_they_are_given() {
        // チャンネル・プレイリスト・フィードの行は ie_key を欠くことがあるので選り分けない。
        let out = format!(
            "{}\n{}\n",
            r#"{"id":"v1","title":"t","timestamp":2}"#, r#"{"id":"v2","title":"t"}"#
        );
        for target in all_targets().into_iter().skip(1) {
            assert_eq!(
                ids(&parse_target_lines(&target, &out)),
                ["v1", "v2"],
                "{target:?}"
            );
        }
        // 検索では ie_key の無い行は動画と見なさない。
        assert!(parse_target_lines(&Target::Search("q".to_string()), &out).is_empty());
    }

    #[test]
    fn results_come_back_newest_first() {
        let out = format!(
            "{}\n{}\n{}\n",
            video_line("old", Some(1_600_000_000)),
            video_line("new", Some(1_750_000_000)),
            video_line("mid", Some(1_700_000_000))
        );
        // Feed は並べ替えない (feed_rows_keep_the_youtube_returned_order で検証)。
        for target in all_targets()
            .into_iter()
            .filter(|target| !matches!(target, Target::Feed(_)))
        {
            assert_eq!(
                ids(&parse_target_lines(&target, &out)),
                ["new", "mid", "old"],
                "{target:?}"
            );
        }
    }

    #[test]
    fn feed_rows_keep_the_youtube_returned_order() {
        // 後で見る・履歴・登録チャンネル・おすすめは YouTube 側の返却順に追加順・視聴順等の
        // 意味があるので、公開日 (timestamp) では並べ替えない。
        let out = format!(
            "{}\n{}\n{}\n",
            video_line("old", Some(1_600_000_000)),
            video_line("new", Some(1_750_000_000)),
            video_line("mid", Some(1_700_000_000))
        );
        for feed in [
            Feed::Recommended,
            Feed::History,
            Feed::Subscriptions,
            Feed::WatchLater,
        ] {
            let target = Target::Feed(feed);
            assert_eq!(
                ids(&parse_target_lines(&target, &out)),
                ["old", "new", "mid"],
                "{target:?}"
            );
        }
    }

    #[test]
    fn rows_without_a_date_keep_their_order_after_the_dated_ones() {
        // 日付の無い行同士は元の順序を保つ (安定ソート)。
        let target = Target::Channel {
            id: "UCabc".to_string(),
            tab: ChannelTab::Videos,
        };
        let out = format!(
            "{}\n{}\n{}\n",
            video_line("a", None),
            video_line("dated", Some(1_700_000_000)),
            video_line("b", None)
        );
        let results = parse_target_lines(&target, &out);
        assert_eq!(ids(&results), ["dated", "a", "b"]);
        // 全部日付が無ければ元の順のまま。
        let out = format!("{}\n{}\n", video_line("a", None), video_line("b", None));
        let results = parse_target_lines(&target, &out);
        assert_eq!(ids(&results), ["a", "b"]);
    }

    #[test]
    fn parse_target_lines_reads_the_same_fields_as_the_line_parser() {
        // 日付順に並べ替えても、行から読む中身は変わらない。
        assert_eq!(
            parse_target_lines(&Target::Search("q".to_string()), LINE_FULL),
            parse_lines(LINE_FULL)
        );
    }

    #[tokio::test]
    async fn run_search_hands_back_the_newest_first() {
        let out = format!(
            "{}\n{}\n",
            video_line("old", Some(1_600_000_000)),
            video_line("new", Some(1_750_000_000))
        );
        let runner = FakeYtDlp::new([done(0, &out, "")]);
        let report = run_search(
            &runner,
            &Target::Search("q".to_string()),
            None,
            10,
            YT_DLP_TIMEOUT,
        )
        .await;
        let results = report.results.expect("結果は返る");
        assert_eq!(ids(&results), ["new", "old"]);
    }

    const ONE_VIDEO: &str = r#"{"id":"id1","title":"t","channel_id":"UCfeed","uploader":"Up"}"#;

    #[test]
    fn channel_lookup_args_take_one_video_without_flat_playlist() {
        // --flat-playlist を付けないから channel_id まで入った 1 行が返る。
        assert_eq!(
            channel_lookup_args("https://www.youtube.com/watch?v=id1"),
            ["https://www.youtube.com/watch?v=id1", "--dump-json"]
        );
    }

    fn channel(id: &str, uploader: Option<&str>) -> ChannelRef {
        ChannelRef {
            id: id.to_string(),
            uploader: uploader.map(str::to_string),
        }
    }

    #[tokio::test]
    async fn fetch_channel_reads_the_channel_of_a_single_video() {
        let runner = FakeYtDlp::new([done(0, ONE_VIDEO, "")]);
        let url = "https://www.youtube.com/watch?v=id1";
        assert_eq!(
            fetch_channel(&runner, url).await,
            Ok(Some(channel("UCfeed", Some("Up"))))
        );
        assert_eq!(runner.calls(), [channel_lookup_args(url)]);
    }

    #[tokio::test]
    async fn fetch_channel_keeps_the_name_from_the_line_it_read() {
        // 履歴タブの行は名前を持たないので、引いた行の uploader が唯一の出どころ。
        let line = r#"{"id":"id1","title":"t","channel_id":"UCfeed","channel":"Some Channel"}"#;
        let runner = FakeYtDlp::new([done(0, line, "")]);
        assert_eq!(
            fetch_channel(&runner, "u").await,
            Ok(Some(channel("UCfeed", Some("Some Channel"))))
        );
        // 引いた行にも無ければ名前は付かない。
        let runner = FakeYtDlp::new([done(0, r#"{"id":"id1","channel_id":"UCfeed"}"#, "")]);
        assert_eq!(
            fetch_channel(&runner, "u").await,
            Ok(Some(channel("UCfeed", None)))
        );
    }

    #[tokio::test]
    async fn fetch_channel_is_none_when_the_video_has_no_channel() {
        let runner = FakeYtDlp::new([done(0, r#"{"id":"id1","title":"t"}"#, "")]);
        assert_eq!(fetch_channel(&runner, "u").await, Ok(None));
        // 何も出てこなかったときも失敗ではない。
        let runner = FakeYtDlp::new([done(0, "", "")]);
        assert_eq!(fetch_channel(&runner, "u").await, Ok(None));
    }

    #[tokio::test]
    async fn fetch_channel_reports_the_error_line_when_yt_dlp_fails() {
        let runner = FakeYtDlp::new([done(
            1,
            "",
            "WARNING: falling back\nERROR: Video unavailable\nWARNING: cache\n",
        )]);
        let error = fetch_channel(&runner, "u").await.expect_err("失敗する");
        assert!(error.contains("Video unavailable"), "{error}");
    }

    #[tokio::test]
    async fn fetch_channel_keeps_json_that_arrived_despite_a_non_zero_exit() {
        let runner = FakeYtDlp::new([done(1, ONE_VIDEO, "WARNING: something\n")]);
        assert_eq!(
            fetch_channel(&runner, "u").await,
            Ok(Some(channel("UCfeed", Some("Up"))))
        );
    }

    #[tokio::test]
    async fn fetch_channel_reports_a_missing_binary() {
        let runner = FakeYtDlp::new([missing()]);
        let error = fetch_channel(&runner, "u").await.expect_err("起動できない");
        assert!(error.contains("yt-dlp"), "{error}");
    }

    #[tokio::test(start_paused = true)]
    async fn fetch_channel_times_out() {
        let runner = FakeYtDlp::new([Step::Hang]);
        let error = fetch_channel(&runner, "u").await.expect_err("返らない");
        assert!(error.contains("タイムアウト"), "{error}");
        assert!(error.contains("15"), "{error}");
    }

    #[test]
    fn yt_dlp_args_limit_channel_tabs() {
        let args = yt_dlp_args(
            &Target::Channel {
                id: "UCabc".to_string(),
                tab: ChannelTab::Shorts,
            },
            None,
            10,
        );
        assert_eq!(
            args,
            [
                "https://www.youtube.com/channel/UCabc/shorts",
                "--flat-playlist",
                "--dump-json",
                "--extractor-args",
                "youtubetab:approximate_date",
                "--playlist-end",
                &CHANNEL_LIMIT.to_string(),
            ]
        );
    }

    #[test]
    fn yt_dlp_args_pass_cookies_to_a_channel_when_they_are_set() {
        // チャンネルは公開情報だが、cookie を切り分けるのは検索と同じ扱いにする。
        let args = yt_dlp_args(
            &Target::Channel {
                id: "UCabc".to_string(),
                tab: ChannelTab::Videos,
            },
            Some(&source("chrome")),
            10,
        );
        assert_eq!(
            args[args.len() - 2..],
            ["--cookies-from-browser".to_string(), "chrome".to_string()]
        );
    }

    #[test]
    fn parse_line_falls_back_to_channel_when_uploader_is_missing() {
        let line = r#"{"id":"a","title":"t","channel":"Some Channel"}"#;
        assert_eq!(
            parse_lines(line)[0].uploader.as_deref(),
            Some("Some Channel")
        );
        // uploader が入っていればそちらを優先する。
        let both = r#"{"id":"a","title":"t","uploader":"Up","channel":"Ch"}"#;
        assert_eq!(parse_lines(both)[0].uploader.as_deref(), Some("Up"));
        // 履歴の行は両方とも無いので埋まらない。
        let neither = r#"{"id":"a","title":"t"}"#;
        assert_eq!(parse_lines(neither)[0].uploader, None);
    }

    #[test]
    fn yt_dlp_args_without_cookies_match_the_current_command() {
        assert_eq!(
            yt_dlp_args(&Target::Search("q".to_string()), None, 10),
            [
                "ytsearch1000:q",
                "--flat-playlist",
                "--dump-json",
                "--extractor-args",
                "youtubetab:approximate_date",
                "--playlist-end",
                "10",
            ]
        );
    }

    #[test]
    fn yt_dlp_args_append_cookie_flags_after_the_fixed_part() {
        let args = yt_dlp_args(
            &Target::Search("q".to_string()),
            Some(&source("chrome:P 1")),
            10,
        );
        assert_eq!(
            args[args.len() - 2..],
            [
                "--cookies-from-browser".to_string(),
                "chrome:P 1".to_string()
            ]
        );
    }

    #[test]
    fn yt_dlp_args_pass_a_cookie_file_with_the_cookies_flag() {
        let file =
            CookieSource::from_file(Some(std::path::Path::new("/tmp/cookies.txt"))).expect("path");
        let args = yt_dlp_args(&Target::Search("q".to_string()), Some(&file), 10);
        assert_eq!(
            args[args.len() - 2..],
            ["--cookies".to_string(), "/tmp/cookies.txt".to_string()]
        );
    }

    #[test]
    fn yt_dlp_args_take_the_result_count_from_the_setting() {
        // URL の ytsearchN の N は固定の上限で、実際の件数は --playlist-end で絞る。
        let args = yt_dlp_args(&Target::Search("q".to_string()), None, 25);
        assert_eq!(args[0], "ytsearch1000:q");
        assert_eq!(playlist_end(&args), Some("25".to_string()));
    }

    #[test]
    fn yt_dlp_args_always_ask_for_approximate_dates() {
        // 公開日が行に乗らないと日付順に並べ替えられないので、どの種別にも付ける。
        for target in all_targets() {
            let args = yt_dlp_args(&target, None, 10);
            let at = args
                .iter()
                .position(|a| a == "--extractor-args")
                .unwrap_or_else(|| panic!("{target:?} に --extractor-args がある"));
            assert_eq!(args[at + 1], "youtubetab:approximate_date", "{target:?}");
        }
    }

    /// プレイリスト一覧の行。uploader は固定文字列で、duration/channel_id は null で返る。
    const PLAYLIST_LINE: &str = r#"{"_type":"url","ie_key":"YoutubeTab","id":"PLabc123","url":"https://www.youtube.com/playlist?list=PLabc123","title":"作業用BGM","uploader":"View full playlist","duration":null,"channel_id":null}"#;

    fn entry(id: &str, title: &str) -> PlaylistEntry {
        PlaylistEntry {
            id: id.to_string(),
            title: title.to_string(),
        }
    }

    #[test]
    fn parses_playlist_entries() {
        let out = format!("{PLAYLIST_LINE}\n{PLAYLIST_LINE}\n");
        let entries = parse_playlist_lines(&out);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0], entry("PLabc123", "作業用BGM"));
    }

    #[test]
    fn playlist_entries_do_not_need_the_video_fields() {
        // uploader は "View full playlist" 固定、duration/channel_id は null なので読まない。
        assert_eq!(
            parse_playlist_lines(r#"{"id":"PLabc123","title":"作業用BGM"}"#)[0],
            entry("PLabc123", "作業用BGM")
        );
        // 名前が無い行も一覧には出す。
        assert_eq!(
            parse_playlist_lines(r#"{"id":"PLabc123"}"#)[0],
            entry("PLabc123", "(title unknown)")
        );
    }

    #[test]
    fn special_playlists_are_ordinary_entries() {
        let out = format!(
            "{}\n{}\n",
            r#"{"id":"LL","title":"Liked videos","uploader":"View full playlist"}"#,
            r#"{"id":"WL","title":"Watch later","uploader":"View full playlist"}"#
        );
        assert_eq!(
            parse_playlist_lines(&out),
            [entry("LL", "Liked videos"), entry("WL", "Watch later")]
        );
    }

    #[test]
    fn skips_blank_and_broken_playlist_lines() {
        let out = format!("\n  \n{PLAYLIST_LINE}\nnot json\n{{\"title\":\"no id\"}}\n\n");
        let entries = parse_playlist_lines(&out);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, "PLabc123");
        assert!(parse_playlist_lines("").is_empty());
    }

    #[test]
    fn playlists_args_ask_the_playlists_feed_for_a_flat_list() {
        assert_eq!(
            playlists_args(None),
            [PLAYLISTS_URL, "--flat-playlist", "--dump-json"]
        );
        // cookie は検索と同じく後ろに足す (自分の一覧なので cookie が無いと空で返る)。
        let args = playlists_args(Some(&source("chrome")));
        assert_eq!(
            args[args.len() - 2..],
            ["--cookies-from-browser".to_string(), "chrome".to_string()]
        );
    }

    #[test]
    fn yt_dlp_args_ask_for_one_video() {
        let target = Target::Video("jNQXAC9IVRw".to_string());
        let args = yt_dlp_args(&target, Some(&source("chrome")), 10);
        assert_eq!(args[0], "https://www.youtube.com/watch?v=jNQXAC9IVRw");
        let end = args
            .iter()
            .position(|a| a == "--playlist-end")
            .expect("件数");
        assert_eq!(args[end + 1], "1");
        assert!(has_cookie_flag(&args), "検索と同じく cookie を渡す");
    }

    /// 動画 1 本を指したときの yt-dlp の行 (実測を縮めたもの)。検索結果の行と違い ie_key が無い。
    const SINGLE_VIDEO_LINE: &str = r#"{"_type":"video","extractor_key":"Youtube","id":"jNQXAC9IVRw","title":"Me at the zoo","duration":19,"uploader":"jawed","channel_id":"UC4QobU6STFB0P71PMvOGN5A","live_status":"not_live","timestamp":1114313512}"#;

    #[test]
    fn a_single_video_line_is_read_as_one_result() {
        let results =
            parse_target_lines(&Target::Video("jNQXAC9IVRw".to_string()), SINGLE_VIDEO_LINE);
        assert_eq!(results.len(), 1, "ie_key が無くても落とさない");
        assert_eq!(results[0].id, "jNQXAC9IVRw");
        assert_eq!(results[0].title, "Me at the zoo");
        assert_eq!(
            results[0].channel_id.as_deref(),
            Some("UC4QobU6STFB0P71PMvOGN5A")
        );
    }

    #[tokio::test]
    async fn run_search_picks_up_the_playlist_title() {
        let out = concat!(
            r#"{"_type":"url","ie_key":"Youtube","id":"v1","title":"a","playlist_title":"Popular Music Videos"}"#,
            "\n",
            r#"{"_type":"url","ie_key":"Youtube","id":"v2","title":"b","playlist_title":"Popular Music Videos"}"#,
            "\n"
        );
        let runner = FakeYtDlp::new([done(0, out, "")]);
        let report = run_search(
            &runner,
            &Target::Playlist("PLabc".to_string()),
            None,
            10,
            YT_DLP_TIMEOUT,
        )
        .await;

        assert_eq!(
            report.playlist_title.as_deref(),
            Some("Popular Music Videos")
        );
        assert_eq!(report.results.expect("結果").len(), 2);
    }

    #[test]
    fn yt_dlp_args_limit_a_playlist() {
        let target = Target::Playlist("PLabc123".to_string());
        assert_eq!(
            yt_dlp_args(&target, None, 10),
            [
                "https://www.youtube.com/playlist?list=PLabc123",
                "--flat-playlist",
                "--dump-json",
                "--extractor-args",
                "youtubetab:approximate_date",
                "--playlist-end",
                &PLAYLIST_LIMIT.to_string(),
            ]
        );
        let args = yt_dlp_args(&target, Some(&source("chrome")), 10);
        assert!(has_cookie_flag(&args));
    }

    #[test]
    fn a_playlist_page_is_read_by_the_video_parser() {
        // プレイリストの中身は通常の動画検索と同じ形で返るので、既存の parse_lines を使う。
        let line = r#"{"_type":"url","ie_key":"Youtube","id":"awX7DUp-r14","title":"Rust TUI Tutorial","duration":2404.0,"channel_id":"UCxxx","uploader":"Green Tea Coding"}"#;
        let results = parse_lines(line);
        assert_eq!(
            results,
            [SearchResult {
                id: "awX7DUp-r14".to_string(),
                title: "Rust TUI Tutorial".to_string(),
                duration: Some(2404.0),
                uploader: Some("Green Tea Coding".to_string()),
                channel_id: Some("UCxxx".to_string()),
                is_live: false,
            }]
        );
    }

    const PLAYLISTS_TIMEOUT: Duration = Duration::from_secs(20);

    #[tokio::test]
    async fn fetch_playlists_reads_the_playlists_feed() {
        let runner = FakeYtDlp::new([done(0, PLAYLIST_LINE, "")]);
        assert_eq!(
            fetch_playlists(&runner, None, PLAYLISTS_TIMEOUT).await,
            Ok(vec![entry("PLabc123", "作業用BGM")])
        );
        assert_eq!(runner.calls(), [playlists_args(None)]);
    }

    #[tokio::test]
    async fn fetch_playlists_passes_the_cookie_along() {
        let runner = FakeYtDlp::new([done(0, "", "")]);
        let cookies = source("chrome");
        assert_eq!(
            fetch_playlists(&runner, Some(&cookies), PLAYLISTS_TIMEOUT).await,
            Ok(vec![])
        );
        assert!(has_cookie_flag(&runner.calls()[0]));
    }

    #[tokio::test]
    async fn an_empty_playlists_feed_is_not_a_failure() {
        // cookie が無い・1 件も作っていない場合は、失敗せず空で返る。
        let runner = FakeYtDlp::new([done(0, "", "")]);
        assert_eq!(
            fetch_playlists(&runner, None, PLAYLISTS_TIMEOUT).await,
            Ok(vec![])
        );
    }

    #[tokio::test]
    async fn fetch_playlists_reports_the_error_line_when_yt_dlp_fails() {
        let runner = FakeYtDlp::new([done(1, "", "ERROR: Sign in to confirm\nWARNING: cache\n")]);
        let error = fetch_playlists(&runner, None, PLAYLISTS_TIMEOUT)
            .await
            .expect_err("失敗する");
        assert!(error.contains("Sign in to confirm"), "{error}");
    }

    #[tokio::test]
    async fn fetch_playlists_keeps_json_that_arrived_despite_a_non_zero_exit() {
        let runner = FakeYtDlp::new([done(1, PLAYLIST_LINE, "WARNING: something\n")]);
        assert_eq!(
            fetch_playlists(&runner, None, PLAYLISTS_TIMEOUT).await,
            Ok(vec![entry("PLabc123", "作業用BGM")])
        );
    }

    #[tokio::test]
    async fn fetch_playlists_reports_a_missing_binary() {
        let runner = FakeYtDlp::new([missing()]);
        let error = fetch_playlists(&runner, None, PLAYLISTS_TIMEOUT)
            .await
            .expect_err("起動できない");
        assert!(error.contains("yt-dlp"), "{error}");
    }

    #[tokio::test(start_paused = true)]
    async fn fetch_playlists_times_out() {
        let runner = FakeYtDlp::new([Step::Hang]);
        let error = fetch_playlists(&runner, None, PLAYLISTS_TIMEOUT)
            .await
            .expect_err("返らない");
        assert!(error.contains("タイムアウト"), "{error}");
        assert!(error.contains("20"), "{error}");
    }

    #[test]
    fn yt_dlp_args_limit_feeds() {
        let args = yt_dlp_args(
            &Target::Feed(Feed::Recommended),
            Some(&source("chrome")),
            10,
        );
        assert_eq!(
            args,
            [
                ":ytrec",
                "--flat-playlist",
                "--dump-json",
                "--extractor-args",
                "youtubetab:approximate_date",
                "--playlist-end",
                "30",
                "--cookies-from-browser",
                "chrome",
            ]
        );
        // フィードの上限は設定の件数では変わらない。
        let args = yt_dlp_args(&Target::Feed(Feed::Recommended), None, 25);
        assert_eq!(playlist_end(&args), Some(FEED_LIMIT.to_string()));
    }

    #[tokio::test]
    async fn run_search_retries_without_cookies_when_the_store_is_unreadable() {
        let runner = FakeYtDlp::new([
            done(
                1,
                "",
                "ERROR: could not find firefox cookies database in '/x/Profiles'",
            ),
            done(0, LINE_FULL, ""),
        ]);
        let report = run_search(
            &runner,
            &Target::Search("q".to_string()),
            Some(&source("firefox")),
            10,
            YT_DLP_TIMEOUT,
        )
        .await;

        let calls = runner.calls();
        assert_eq!(calls.len(), 2);
        assert!(has_cookie_flag(&calls[0]));
        assert!(!has_cookie_flag(&calls[1]));
        assert_eq!(report.results.expect("2 回目の結果").len(), 1);
        assert!(report.fell_back);
        assert!(matches!(report.outcome, CookieOutcome::Unreadable(_)));
    }

    #[tokio::test]
    async fn a_failure_is_reported_with_the_error_line_not_a_trailing_warning() {
        // 受け取る側はこの一行で原因を見分けるので、後ろの警告で押し流さない。
        let runner = FakeYtDlp::new([done(
            1,
            "",
            "WARNING: falling back\nERROR: [youtube:tab] UCabc: This channel does not have a streams tab\nWARNING: unable to write cache\n",
        )]);
        let report = run_search(
            &runner,
            &Target::Channel {
                id: "UCabc".to_string(),
                tab: crate::cookies::ChannelTab::Streams,
            },
            None,
            10,
            YT_DLP_TIMEOUT,
        )
        .await;

        let error = report.results.expect_err("結果は返らない");
        assert!(error.contains("does not have a streams tab"), "{error}");
        assert!(is_missing_tab(&error), "{error}");
    }

    #[test]
    fn only_a_missing_tab_counts_as_a_missing_tab() {
        assert!(is_missing_tab(
            "yt-dlp が失敗しました: ERROR: [youtube:tab] UCabc: This channel does not have a streams tab"
        ));
        // 大文字小文字は yt-dlp の版で変わりうる。
        assert!(is_missing_tab("This Channel Does Not Have A Shorts Tab"));
        for other in [
            "yt-dlp が見つかりません (PATH を確認してください)",
            "検索がタイムアウトしました (30 秒)",
            "yt-dlp が失敗しました: ERROR: [youtube:tab] Unable to download webpage",
            "yt-dlp が失敗しました: ERROR: [youtube:tab] UCabc: This channel does not exist",
        ] {
            assert!(!is_missing_tab(other), "{other}");
        }
    }

    #[test]
    fn a_failure_without_an_error_line_still_says_something() {
        assert_eq!(stderr_detail("WARNING: a\nWARNING: b\n"), "WARNING: b");
        assert_eq!(stderr_detail(""), "");
    }

    #[tokio::test]
    async fn run_search_does_not_retry_on_unrelated_failure() {
        let runner = FakeYtDlp::new([done(
            1,
            "",
            "ERROR: [youtube:search] Unable to download webpage",
        )]);
        let report = run_search(
            &runner,
            &Target::Search("q".to_string()),
            Some(&source("chrome")),
            10,
            YT_DLP_TIMEOUT,
        )
        .await;

        assert_eq!(runner.calls().len(), 1);
        assert!(report.results.is_err());
        assert_eq!(report.outcome, CookieOutcome::Unknown);
        assert!(!report.fell_back);
    }

    #[tokio::test]
    async fn run_search_keeps_results_but_reports_degradation() {
        let runner = FakeYtDlp::new([done(
            0,
            LINE_FULL,
            "WARNING: find-generic-password failed\nWARNING: cannot decrypt v10 cookies: no key found\n",
        )]);
        let report = run_search(
            &runner,
            &Target::Search("q".to_string()),
            Some(&source("chrome")),
            10,
            YT_DLP_TIMEOUT,
        )
        .await;

        assert_eq!(runner.calls().len(), 1);
        assert_eq!(report.results.expect("結果は返る").len(), 1);
        assert!(matches!(report.outcome, CookieOutcome::Degraded(_)));
        assert!(!report.fell_back);
    }

    #[tokio::test]
    async fn run_search_reports_login_required_without_retry() {
        let runner = FakeYtDlp::new([done(
            1,
            "",
            "ERROR: [youtube:history] Login details are needed to download this content.",
        )]);
        let report = run_search(
            &runner,
            &Target::Feed(Feed::History),
            Some(&source("safari")),
            10,
            YT_DLP_TIMEOUT,
        )
        .await;

        assert_eq!(runner.calls().len(), 1);
        assert_eq!(report.outcome, CookieOutcome::LoginRequired);
        assert!(!report.fell_back);
    }

    #[tokio::test]
    async fn run_search_reports_not_used_without_cookies() {
        let runner = FakeYtDlp::new([done(
            1,
            "",
            "ERROR: could not find chrome cookies database in '/x'",
        )]);
        let report = run_search(
            &runner,
            &Target::Search("q".to_string()),
            None,
            10,
            YT_DLP_TIMEOUT,
        )
        .await;

        // cookie を渡していない実行の stderr は cookie の判定材料にしない。
        assert_eq!(runner.calls().len(), 1);
        assert_eq!(report.outcome, CookieOutcome::NotUsed);
        assert!(!report.fell_back);
    }

    #[tokio::test(start_paused = true)]
    async fn run_search_times_out_per_attempt() {
        let runner = FakeYtDlp::new([Step::Hang]);
        let report = run_search(
            &runner,
            &Target::Search("q".to_string()),
            Some(&source("chrome")),
            10,
            YT_DLP_TIMEOUT,
        )
        .await;

        assert_eq!(runner.calls().len(), 1, "タイムアウトでは再試行しない");
        let error = report.results.expect_err("結果は返らない");
        assert!(error.contains("タイムアウト"), "{error}");
        assert!(error.contains("30 秒"), "{error}");
        assert_eq!(report.outcome, CookieOutcome::TimedOut);
    }

    #[tokio::test]
    async fn run_search_says_why_yt_dlp_could_not_be_started() {
        let denied = Step::Done(Err(std::io::Error::from(ErrorKind::PermissionDenied)));
        for (step, expected) in [
            (missing(), "yt-dlp が見つかりません"),
            (denied, "yt-dlp の起動に失敗しました"),
        ] {
            let runner = FakeYtDlp::new([step]);
            let report = run_search(
                &runner,
                &Target::Search("q".to_string()),
                Some(&source("chrome")),
                10,
                YT_DLP_TIMEOUT,
            )
            .await;

            assert_eq!(runner.calls().len(), 1, "起動できないものは出し直さない");
            let error = report.results.expect_err("結果は返らない");
            assert!(error.starts_with(expected), "{error}");
            assert_eq!(report.outcome, CookieOutcome::Unknown);
            assert!(!report.fell_back);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn run_search_waits_for_the_timeout_it_is_given() {
        for secs in [5, 90, 300] {
            let runner = FakeYtDlp::new([Step::Hang]);
            let started = tokio::time::Instant::now();
            let report = run_search(
                &runner,
                &Target::Search("q".to_string()),
                None,
                10,
                Duration::from_secs(secs),
            )
            .await;

            assert_eq!(started.elapsed(), Duration::from_secs(secs), "{secs} 秒");
            let error = report.results.expect_err("結果は返らない");
            assert!(error.contains(&format!("{secs} 秒")), "{error}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn the_default_timeout_is_still_30_seconds() {
        assert_eq!(YT_DLP_TIMEOUT, Duration::from_secs(30));
        let runner = FakeYtDlp::new([Step::Hang]);
        let started = tokio::time::Instant::now();
        let _ = run_search(
            &runner,
            &Target::Search("q".to_string()),
            None,
            10,
            YT_DLP_TIMEOUT,
        )
        .await;
        assert_eq!(started.elapsed(), Duration::from_secs(30));
    }

    #[tokio::test(start_paused = true)]
    async fn the_retry_without_cookies_gets_the_same_timeout() {
        let runner = FakeYtDlp::new([
            done(
                1,
                "",
                "ERROR: could not find chrome cookies database in '/x'",
            ),
            Step::Hang,
        ]);
        let report = run_search(
            &runner,
            &Target::Search("q".to_string()),
            Some(&source("chrome")),
            10,
            Duration::from_secs(45),
        )
        .await;

        assert_eq!(runner.calls().len(), 2);
        let error = report.results.expect_err("2 回目もタイムアウト");
        assert!(error.contains("45 秒"), "{error}");
    }

    #[tokio::test(start_paused = true)]
    async fn the_report_carries_the_timeout_it_was_given() {
        // 受け取る側は、届いた時点の設定でなくこの値で文言を作る。
        let runner = FakeYtDlp::new([Step::Hang]);
        let report = run_search(
            &runner,
            &Target::Search("q".to_string()),
            None,
            10,
            Duration::from_secs(90),
        )
        .await;
        assert_eq!(report.timeout, Duration::from_secs(90));

        // 成功したときも同じ値が入る。
        let runner = FakeYtDlp::new([done(0, LINE_FULL, "")]);
        let report = run_search(
            &runner,
            &Target::Search("q".to_string()),
            None,
            10,
            Duration::from_secs(15),
        )
        .await;
        assert_eq!(report.timeout, Duration::from_secs(15));

        // cookie 無しで出し直した報告にも入る。
        let runner = FakeYtDlp::new([
            done(
                1,
                "",
                "ERROR: could not find chrome cookies database in '/x'",
            ),
            done(0, LINE_FULL, ""),
        ]);
        let report = run_search(
            &runner,
            &Target::Search("q".to_string()),
            Some(&source("chrome")),
            10,
            Duration::from_secs(45),
        )
        .await;
        assert!(report.fell_back);
        assert_eq!(report.timeout, Duration::from_secs(45));
    }
}
