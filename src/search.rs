use crate::cookies::{CookieOutcome, CookieSource, FEED_LIMIT, Target, classify};
use serde_json::Value;
use std::future::Future;
use std::io::ErrorKind;
use std::process::Output;
use std::time::Duration;
use tokio::process::Command;
use tokio::time::timeout;

/// yt-dlp 1 回の実行ごとの上限。cookie 付きの失敗から再試行すると最大 2 回分待つ。
pub const YT_DLP_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq)]
pub struct SearchResult {
    pub id: String,
    pub title: String,
    pub duration: Option<f64>,
    pub uploader: Option<String>,
}

impl SearchResult {
    pub fn url(&self) -> String {
        format!("https://www.youtube.com/watch?v={}", self.id)
    }
}

pub fn parse_lines(output: &str) -> Vec<SearchResult> {
    output.lines().filter_map(parse_line).collect()
}

fn parse_line(line: &str) -> Option<SearchResult> {
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
    let duration = value.get("duration").and_then(Value::as_f64);
    // フィードの行は uploader を欠くことがあるので channel でも拾う。
    let uploader = value
        .get("uploader")
        .and_then(Value::as_str)
        .or_else(|| value.get("channel").and_then(Value::as_str))
        .map(str::to_string);
    Some(SearchResult {
        id,
        title,
        duration,
        uploader,
    })
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
        target.yt_dlp_url(limit),
        "--flat-playlist".to_string(),
        "--dump-json".to_string(),
    ];
    if let Target::Feed(_) = target {
        args.push("--playlist-end".to_string());
        args.push(FEED_LIMIT.to_string());
    }
    if let Some(cookies) = cookies {
        args.extend(cookies.yt_dlp_args());
    }
    args
}

#[derive(Debug)]
pub struct SearchReport {
    pub results: Result<Vec<SearchResult>, String>,
    pub outcome: CookieOutcome,
    /// cookie 無しで再実行した。
    pub fell_back: bool,
}

struct Attempt {
    results: Result<Vec<SearchResult>, String>,
    outcome: CookieOutcome,
}

pub async fn run_search(
    runner: &impl YtDlp,
    target: &Target,
    cookies: Option<&CookieSource>,
    limit: usize,
) -> SearchReport {
    let first = attempt(runner, target, cookies, limit).await;
    // 読めないときは検索そのものが実行されないので、同じ target を cookie 無しで出し直す。
    if let CookieOutcome::Unreadable(_) = &first.outcome {
        let retry = attempt(runner, target, None, limit).await;
        return SearchReport {
            results: retry.results,
            outcome: first.outcome,
            fell_back: true,
        };
    }
    SearchReport {
        results: first.results,
        outcome: first.outcome,
        fell_back: false,
    }
}

async fn attempt(
    runner: &impl YtDlp,
    target: &Target,
    cookies: Option<&CookieSource>,
    limit: usize,
) -> Attempt {
    let used_cookies = cookies.is_some();
    let args = yt_dlp_args(target, cookies, limit);
    let output = match timeout(YT_DLP_TIMEOUT, runner.run(args)).await {
        Err(_) => {
            return Attempt {
                results: Err(format!(
                    "検索がタイムアウトしました ({} 秒)",
                    YT_DLP_TIMEOUT.as_secs()
                )),
                outcome: outcome_of(used_cookies, CookieOutcome::TimedOut),
            };
        }
        Ok(Err(e)) => {
            return Attempt {
                results: Err(launch_error(&e)),
                outcome: outcome_of(used_cookies, CookieOutcome::Unknown),
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
    let results = parse_lines(&String::from_utf8_lossy(&output.stdout));
    let results = if results.is_empty() && !output.status.success() {
        let detail = stderr.lines().last().unwrap_or("").trim();
        Err(format!("yt-dlp が失敗しました: {detail}"))
    } else {
        Ok(results)
    };
    Attempt { results, outcome }
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
    use crate::cookies::Feed;
    use crate::search::fixtures::{FakeYtDlp, Step, done};

    const LINE_FULL: &str =
        r#"{"id":"abc123","title":"Rust TUI tutorial","duration":612.0,"uploader":"someone"}"#;

    fn source(spec: &str) -> CookieSource {
        CookieSource::from_spec(Some(spec)).expect("spec")
    }

    fn has_cookie_flag(args: &[String]) -> bool {
        args.iter().any(|a| a == "--cookies-from-browser")
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
            ["ytsearch10:q", "--flat-playlist", "--dump-json"]
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
            args,
            [
                "ytsearch10:q",
                "--flat-playlist",
                "--dump-json",
                "--cookies-from-browser",
                "chrome:P 1",
            ]
        );
    }

    #[test]
    fn yt_dlp_args_take_the_result_count_from_the_setting() {
        let args = yt_dlp_args(&Target::Search("q".to_string()), None, 25);
        assert_eq!(args[0], "ytsearch25:q");
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
                "--playlist-end",
                "30",
                "--cookies-from-browser",
                "chrome",
            ]
        );
        assert!(
            !yt_dlp_args(&Target::Search("q".to_string()), None, 10)
                .iter()
                .any(|a| a == "--playlist-end")
        );
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
        let report = run_search(&runner, &Target::Search("q".to_string()), None, 10).await;

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
        )
        .await;

        assert_eq!(runner.calls().len(), 1, "タイムアウトでは再試行しない");
        let error = report.results.expect_err("結果は返らない");
        assert!(error.contains("タイムアウト"), "{error}");
        assert_eq!(report.outcome, CookieOutcome::TimedOut);
    }
}
