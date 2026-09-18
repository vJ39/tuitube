//! 再生中の動画のコメント。取得コマンド・単一 JSON の読み取り・表示状態。

use crate::grid;
use crate::search::{YtDlp, launch_error};
use serde_json::Value;
use std::time::Duration;
use tokio::time::timeout;

/// 取得する件数。全件は動画によって数百万件に達するので、いいね数の上位だけを取る。
pub const COMMENT_LIMIT: usize = 50;
/// 1 回の取得の上限。50 件は実測 5 秒前後で返る。
pub const COMMENT_TIMEOUT: Duration = Duration::from_secs(15);

pub const UNKNOWN_AUTHOR: &str = "(author unknown)";
pub const UNREADABLE: &str = "コメントを読み取れませんでした";
pub const LOADING: &str = "コメントを取得中…";
pub const NO_COMMENTS: &str = "コメントはありません";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comment {
    pub author: String,
    pub text: String,
    pub like_count: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommentState {
    Pending,
    Ready(Vec<Comment>),
    Failed(String),
}

/// 再生中の 1 本ぶんの取得状態と表示の on/off。
#[derive(Debug, Default)]
pub struct Comments {
    video_id: String,
    /// 再生していないときは None。
    state: Option<CommentState>,
    visible: bool,
    /// 一覧の先頭に出す行。上限 50 件は 80x24 の映像領域に収まらない。
    scroll: usize,
}

impl Comments {
    /// 再生開始。前の動画のコメントも表示状態も持ち越さない。
    pub fn begin(&mut self, video_id: String) {
        self.video_id = video_id;
        self.state = Some(CommentState::Pending);
        self.visible = false;
        self.scroll = 0;
    }

    /// 取得結果を取り込む。動画が入れ替わっていれば捨てる。
    pub fn apply(&mut self, video_id: &str, comments: Result<Vec<Comment>, String>) {
        if self.state.is_none() || self.video_id != video_id {
            return;
        }
        self.state = Some(match comments {
            Ok(list) => CommentState::Ready(list),
            Err(e) => CommentState::Failed(e),
        });
    }

    pub fn state(&self) -> Option<&CommentState> {
        self.state.as_ref()
    }

    pub fn visible(&self) -> bool {
        self.visible
    }

    /// 切り替えた後の状態を返す。
    pub fn toggle(&mut self) -> bool {
        self.visible = !self.visible;
        self.visible
    }

    /// 描画に使う開始行。幅や取得結果が変わって行が減っていても末尾を越えない。
    pub fn scroll(&self, total: usize, height: usize) -> usize {
        self.scroll.min(max_scroll(total, height))
    }

    /// 一覧を delta 行ぶん送る。行が足りなければ末尾で止める。
    pub fn scroll_by(&mut self, delta: isize, total: usize, height: usize) {
        self.scroll = self
            .scroll(total, height)
            .saturating_add_signed(delta)
            .min(max_scroll(total, height));
    }

    /// 再生終了。以後に届く結果も受け取らない。
    pub fn end(&mut self) {
        self.video_id.clear();
        self.state = None;
        self.visible = false;
        self.scroll = 0;
    }
}

/// 高さに入りきらない行数だけ送れる。
pub fn max_scroll(total: usize, height: usize) -> usize {
    total.saturating_sub(height)
}

pub fn yt_dlp_args(url: &str) -> Vec<String> {
    vec![
        url.to_string(),
        "-J".to_string(),
        "--write-comments".to_string(),
        "--extractor-args".to_string(),
        format!("youtube:max_comments={COMMENT_LIMIT},all,0,0;comment_sort=top"),
    ]
}

/// `-J` は 1 動画ぶんの単一 JSON オブジェクトを返すので、検索の改行区切り
/// (search::parse_lines) とは別に読む。
pub fn parse_comments(output: &str) -> Result<Vec<Comment>, String> {
    let value: Value = serde_json::from_str(output.trim()).map_err(|_| UNREADABLE.to_string())?;
    let object = value.as_object().ok_or_else(|| UNREADABLE.to_string())?;
    let Some(list) = object.get("comments").and_then(Value::as_array) else {
        // コメント無効の動画ではキーごと出てこない。空として扱う。
        return Ok(Vec::new());
    };
    Ok(list.iter().filter_map(parse_comment).collect())
}

fn parse_comment(value: &Value) -> Option<Comment> {
    let text = value.get("text")?.as_str()?.to_string();
    let author = value
        .get("author")
        .and_then(Value::as_str)
        .unwrap_or(UNKNOWN_AUTHOR)
        .to_string();
    Some(Comment {
        author,
        text,
        like_count: value.get("like_count").and_then(Value::as_u64),
    })
}

pub async fn fetch_comments(runner: &impl YtDlp, url: &str) -> Result<Vec<Comment>, String> {
    let output = match timeout(COMMENT_TIMEOUT, runner.run(yt_dlp_args(url))).await {
        Err(_) => {
            return Err(format!(
                "コメントの取得がタイムアウトしました ({} 秒)",
                COMMENT_TIMEOUT.as_secs()
            ));
        }
        Ok(Err(e)) => return Err(launch_error(&e)),
        Ok(Ok(output)) => output,
    };
    let parsed = parse_comments(&String::from_utf8_lossy(&output.stdout));
    if output.status.success() {
        return parsed;
    }
    // 警告つきで終わっても本文が揃っていればそれを出す。
    let stderr = String::from_utf8_lossy(&output.stderr);
    parsed.map_err(|_| {
        let detail = stderr.lines().last().unwrap_or("").trim();
        format!("yt-dlp が失敗しました: {detail}")
    })
}

/// 映像領域へ出す行。作者行と本文行の 2 行を 1 件ぶんとして並べる。
pub fn display_lines(state: Option<&CommentState>, width: usize) -> Vec<String> {
    let lines = match state {
        None => return Vec::new(),
        Some(CommentState::Pending) => vec![LOADING.to_string()],
        Some(CommentState::Failed(e)) => vec![format!("コメントを取得できませんでした: {e}")],
        Some(CommentState::Ready(list)) if list.is_empty() => vec![NO_COMMENTS.to_string()],
        Some(CommentState::Ready(list)) => list.iter().flat_map(comment_lines).collect(),
    };
    lines
        .iter()
        .map(|line| grid::truncate(line, width))
        .collect()
}

fn comment_lines(comment: &Comment) -> [String; 2] {
    let header = match comment.like_count {
        Some(likes) => format!("{} (+{likes})", comment.author),
        None => comment.author.clone(),
    };
    // List は折り返さないので、改行を含む本文は 1 行へ潰す。
    let body: String = comment
        .text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    [header, format!("  {body}")]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::fixtures::{FakeYtDlp, Step, done, missing};

    const TWO: &str = r#"{"id":"abc","comments":[
        {"author":"alice","text":"first","like_count":12},
        {"author":"bob","text":"second","like_count":0}
    ]}"#;

    fn comment(author: &str, text: &str, like_count: Option<u64>) -> Comment {
        Comment {
            author: author.to_string(),
            text: text.to_string(),
            like_count,
        }
    }

    fn ready(list: Vec<Comment>) -> Comments {
        let mut comments = Comments::default();
        comments.begin("abc".to_string());
        comments.apply("abc", Ok(list));
        comments
    }

    #[test]
    fn parses_author_text_and_like_count() {
        let list = parse_comments(TWO).expect("読める");
        assert_eq!(
            list,
            [
                comment("alice", "first", Some(12)),
                comment("bob", "second", Some(0)),
            ]
        );
    }

    #[test]
    fn a_video_without_comments_is_not_an_error() {
        // コメント無効の動画では comments キーごと出てこない。
        assert_eq!(parse_comments(r#"{"id":"abc"}"#), Ok(Vec::new()));
        assert_eq!(
            parse_comments(r#"{"id":"abc","comments":null}"#),
            Ok(Vec::new())
        );
        assert_eq!(
            parse_comments(r#"{"id":"abc","comments":[]}"#),
            Ok(Vec::new())
        );
    }

    #[test]
    fn missing_optional_fields_fall_back() {
        let list = parse_comments(
            r#"{"comments":[{"text":"t"},{"author":"a","text":"t2","like_count":null}]}"#,
        )
        .expect("読める");
        assert_eq!(list[0].author, UNKNOWN_AUTHOR);
        assert_eq!(list[0].like_count, None);
        assert_eq!(list[1].like_count, None);
    }

    #[test]
    fn entries_without_text_are_skipped() {
        let list = parse_comments(r#"{"comments":[{"author":"a"},"x",{"author":"b","text":"t"}]}"#)
            .expect("読める");
        assert_eq!(list, [comment("b", "t", None)]);
    }

    #[test]
    fn broken_json_is_an_error() {
        assert_eq!(parse_comments("not json"), Err(UNREADABLE.to_string()));
        assert_eq!(parse_comments(""), Err(UNREADABLE.to_string()));
        // 配列やスカラーは -J の出力ではない。
        assert_eq!(parse_comments("[]"), Err(UNREADABLE.to_string()));
    }

    #[test]
    fn yt_dlp_args_match_the_designed_command() {
        assert_eq!(
            yt_dlp_args("https://www.youtube.com/watch?v=abc"),
            [
                "https://www.youtube.com/watch?v=abc",
                "-J",
                "--write-comments",
                "--extractor-args",
                "youtube:max_comments=50,all,0,0;comment_sort=top",
            ]
        );
    }

    #[tokio::test]
    async fn fetch_returns_the_parsed_comments() {
        let runner = FakeYtDlp::new([done(0, TWO, "")]);
        let list = fetch_comments(&runner, "https://www.youtube.com/watch?v=abc")
            .await
            .expect("取れる");
        assert_eq!(list.len(), 2);
        assert_eq!(runner.calls().len(), 1);
        assert_eq!(runner.calls()[0][0], "https://www.youtube.com/watch?v=abc");
    }

    #[tokio::test]
    async fn fetch_reports_a_missing_binary() {
        let runner = FakeYtDlp::new([missing()]);
        let error = fetch_comments(&runner, "u")
            .await
            .expect_err("起動できない");
        assert!(error.contains("yt-dlp"), "{error}");
    }

    #[tokio::test]
    async fn fetch_reports_the_last_stderr_line_when_yt_dlp_fails() {
        let runner = FakeYtDlp::new([done(1, "", "WARNING: x\nERROR: unavailable\n")]);
        let error = fetch_comments(&runner, "u").await.expect_err("失敗する");
        assert!(error.contains("ERROR: unavailable"), "{error}");
    }

    #[tokio::test]
    async fn fetch_keeps_json_that_arrived_despite_a_non_zero_exit() {
        // 警告つきで終わっても本文が揃っていれば出す。
        let runner = FakeYtDlp::new([done(1, TWO, "WARNING: something\n")]);
        assert_eq!(fetch_comments(&runner, "u").await.expect("取れる").len(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn fetch_times_out() {
        let runner = FakeYtDlp::new([Step::Hang]);
        let error = fetch_comments(&runner, "u").await.expect_err("返らない");
        assert!(error.contains("タイムアウト"), "{error}");
        assert!(error.contains("15"), "{error}");
    }

    #[test]
    fn begin_starts_pending_and_hides_the_previous_list() {
        let mut comments = ready(vec![comment("a", "t", None)]);
        comments.toggle();
        assert!(comments.visible());

        comments.begin("next".to_string());
        assert_eq!(comments.state(), Some(&CommentState::Pending));
        assert!(!comments.visible(), "前の動画の表示は持ち越さない");
    }

    #[test]
    fn apply_stores_the_list_for_the_video_that_is_playing() {
        let comments = ready(vec![comment("a", "t", Some(1))]);
        assert_eq!(
            comments.state(),
            Some(&CommentState::Ready(vec![comment("a", "t", Some(1))]))
        );
    }

    #[test]
    fn apply_keeps_the_error_when_the_fetch_failed() {
        let mut comments = Comments::default();
        comments.begin("abc".to_string());
        comments.apply("abc", Err("boom".to_string()));
        assert_eq!(
            comments.state(),
            Some(&CommentState::Failed("boom".to_string()))
        );
    }

    #[test]
    fn a_late_result_for_the_previous_video_is_dropped() {
        let mut comments = Comments::default();
        comments.begin("old".to_string());
        comments.begin("new".to_string());
        comments.apply("old", Ok(vec![comment("a", "古い", None)]));
        assert_eq!(comments.state(), Some(&CommentState::Pending));

        comments.apply("new", Ok(vec![comment("b", "新しい", None)]));
        assert_eq!(
            comments.state(),
            Some(&CommentState::Ready(vec![comment("b", "新しい", None)]))
        );
    }

    #[test]
    fn end_drops_everything_including_a_result_that_arrives_afterwards() {
        let mut comments = ready(vec![comment("a", "t", None)]);
        comments.toggle();
        comments.end();
        assert_eq!(comments.state(), None);
        assert!(!comments.visible());

        comments.apply("abc", Ok(vec![comment("a", "t", None)]));
        assert_eq!(comments.state(), None, "再生が終わった後は取り込まない");
    }

    #[test]
    fn scrolling_stops_at_the_first_and_the_last_line_that_can_be_shown() {
        let mut comments = ready(vec![comment("a", "t", None)]);
        // 10 行を 4 行の高さで見るなら 6 行ぶん送れる。
        assert_eq!(comments.scroll(10, 4), 0);
        comments.scroll_by(3, 10, 4);
        assert_eq!(comments.scroll(10, 4), 3);
        comments.scroll_by(99, 10, 4);
        assert_eq!(comments.scroll(10, 4), 6);
        comments.scroll_by(-2, 10, 4);
        assert_eq!(comments.scroll(10, 4), 4);
        comments.scroll_by(-99, 10, 4);
        assert_eq!(comments.scroll(10, 4), 0);
    }

    #[test]
    fn scroll_never_runs_past_a_list_that_got_shorter() {
        let mut comments = ready(vec![comment("a", "t", None)]);
        comments.scroll_by(6, 10, 4);
        assert_eq!(comments.scroll(10, 4), 6);
        // 幅が変わって行が減ったら末尾へ寄せる。全部入るなら先頭。
        assert_eq!(comments.scroll(5, 4), 1);
        assert_eq!(comments.scroll(4, 4), 0);
    }

    #[test]
    fn scroll_goes_back_to_the_top_for_the_next_video() {
        let mut comments = ready(vec![comment("a", "t", None)]);
        comments.scroll_by(4, 10, 2);
        comments.begin("next".to_string());
        assert_eq!(comments.scroll(10, 2), 0);

        comments.apply("next", Ok(vec![comment("a", "t", None)]));
        comments.scroll_by(4, 10, 2);
        comments.end();
        assert_eq!(comments.scroll(10, 2), 0);
    }

    #[test]
    fn toggle_returns_the_state_it_switched_to() {
        let mut comments = Comments::default();
        assert!(comments.toggle());
        assert!(comments.visible());
        assert!(!comments.toggle());
        assert!(!comments.visible());
    }

    #[test]
    fn lines_show_the_author_with_the_like_count_and_the_body() {
        let comments = ready(vec![comment("alice", "first", Some(12))]);
        assert_eq!(
            display_lines(comments.state(), 40),
            ["alice (+12)", "  first"]
        );
    }

    #[test]
    fn lines_omit_the_like_count_when_it_is_unknown() {
        let comments = ready(vec![comment("alice", "first", None)]);
        assert_eq!(display_lines(comments.state(), 40), ["alice", "  first"]);
    }

    #[test]
    fn lines_flatten_a_multi_line_body() {
        let comments = ready(vec![comment("a", "one\ntwo\r\nthree", None)]);
        assert_eq!(display_lines(comments.state(), 40)[1], "  one two three");
    }

    #[test]
    fn lines_are_truncated_to_the_width() {
        let comments = ready(vec![comment("a", "あいうえおかきくけこ", None)]);
        let lines = display_lines(comments.state(), 10);
        for line in &lines {
            assert!(crate::grid::display_width(line) <= 10, "{line}");
        }
        assert!(lines[1].ends_with('…'), "{:?}", lines[1]);
    }

    #[test]
    fn lines_say_when_there_is_nothing_to_show() {
        assert_eq!(display_lines(Some(&CommentState::Pending), 40), [LOADING]);
        assert_eq!(
            display_lines(Some(&CommentState::Ready(Vec::new())), 40),
            [NO_COMMENTS]
        );
        let failed = CommentState::Failed("boom".to_string());
        assert_eq!(display_lines(Some(&failed), 40).len(), 1);
        assert!(display_lines(Some(&failed), 40)[0].contains("boom"));
        // 再生していないときは行そのものが無い。
        assert!(display_lines(None, 40).is_empty());
    }
}
