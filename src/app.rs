use crate::search::SearchResult;
use crate::seekbar::SeekBarState;
use crate::video::VideoSink;
use crossterm::event::{KeyEvent, MouseEvent};
use ratatui::layout::Rect;
use serde_json::Value;
use std::time::{Duration, Instant};

/// シークを送ってから確定値を待つ間、ポーリングの古い値を無視する時間。
pub const SEEK_HOLD: Duration = Duration::from_secs(2);
/// 相対シークは keyframes で着地がずれるので、この幅までは目標どおり着いたとみなす。
pub const SEEK_TOLERANCE_SECS: f64 = 3.0;

pub enum AppEvent {
    Key(KeyEvent),
    Mouse(MouseEvent),
    Resize {
        width: u16,
        height: u16,
    },
    // nonce identifies the search, so results of a superseded query are ignored.
    SearchDone {
        nonce: u64,
        result: Result<Vec<SearchResult>, String>,
    },
    // nonce identifies the mpv instance, so events from an already replaced player are ignored.
    MpvProperty {
        nonce: u64,
        id: u64,
        data: Option<Value>,
    },
    /// 映像が1フレーム揃った合図。これが無いと再描画はティッカー任せになる。
    VideoFrame {
        nonce: u64,
    },
    /// mpv の映像出力を読めなくなった。放っておくと mpv だけが生き残る。
    VideoError {
        nonce: u64,
        error: String,
    },
    MpvExited {
        nonce: u64,
        error: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Input,
    Results,
    Playing,
}

/// シーク送信から確定までの先行表示。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PendingSeek {
    pub target: f64,
    pub sent_at: Instant,
}

#[derive(Debug, Default, Clone)]
pub struct Playback {
    pub title: String,
    pub paused: Option<bool>,
    pub time_pos: Option<f64>,
    pub duration: Option<f64>,
    pub volume: Option<f64>,
    pub pending_seek: Option<PendingSeek>,
}

impl Playback {
    /// 送信と同時に表示だけ目標値へ進める。
    pub fn begin_seek(&mut self, target: f64, now: Instant) {
        self.time_pos = Some(target);
        self.pending_seek = Some(PendingSeek {
            target,
            sent_at: now,
        });
    }

    /// 相対シークの基準。押し続けたときに目標値が積み上がる。
    pub fn seek_base(&self) -> Option<f64> {
        self.pending_seek.map(|p| p.target).or(self.time_pos)
    }

    /// ポーリングの値を取り込む。保持時間中の古い値は捨てる。
    pub fn reconcile_time_pos(&mut self, polled: Option<f64>, now: Instant) {
        // シーク直後のポーリングは playback-restart より前に返って旧位置を寄越す。
        if let Some(pending) = self.pending_seek {
            let within_hold = now.saturating_duration_since(pending.sent_at) < SEEK_HOLD;
            let stale = polled.is_none_or(|p| (p - pending.target).abs() > SEEK_TOLERANCE_SECS);
            if within_hold && stale {
                return;
            }
        }
        self.pending_seek = None;
        self.time_pos = polled;
    }
}

pub struct App {
    pub mode: Mode,
    pub query: String,
    pub results: Vec<SearchResult>,
    pub selected: usize,
    pub searching: bool,
    pub error: Option<String>,
    /// 設定を読み替えたときなど、エラーではないが一度伝えたいこと。
    pub notice: Option<String>,
    pub playback: Playback,
    /// mpv に適用する fps 上限。None は制限なし。
    pub fps_limit: Option<u32>,
    /// 再生中だけ、mpv の kitty 出力を受け取るスロットが入る。
    pub video: Option<VideoSink>,
    /// 直近の terminal.draw() が描いた画面。マウスの当たり判定はこれで割り付ける。
    pub screen: Rect,
    pub seek_bar: SeekBarState,
    pub should_quit: bool,
}

impl Default for App {
    fn default() -> Self {
        Self {
            mode: Mode::Input,
            query: String::new(),
            results: Vec::new(),
            selected: 0,
            searching: false,
            error: None,
            notice: None,
            playback: Playback::default(),
            fps_limit: crate::mpv::FpsLimit::default().limit,
            video: None,
            screen: Rect::default(),
            seek_bar: SeekBarState::default(),
            should_quit: false,
        }
    }
}

impl App {
    pub fn select_next(&mut self) {
        if self.results.is_empty() {
            return;
        }
        self.selected = (self.selected + 1) % self.results.len();
    }

    pub fn select_prev(&mut self) {
        if self.results.is_empty() {
            return;
        }
        self.selected = (self.selected + self.results.len() - 1) % self.results.len();
    }

    pub fn selected_result(&self) -> Option<&SearchResult> {
        self.results.get(self.selected)
    }

    pub fn set_results(&mut self, results: Vec<SearchResult>) {
        self.results = results;
        self.selected = 0;
        if self.results.is_empty() {
            self.error = Some("検索結果が0件でした".to_string());
            self.mode = Mode::Input;
        } else {
            self.mode = Mode::Results;
        }
    }

    pub fn apply_property(&mut self, id: u64, data: Option<Value>) {
        match id {
            crate::mpv::REQ_TIME_POS => self
                .playback
                .reconcile_time_pos(data.and_then(|v| v.as_f64()), Instant::now()),
            crate::mpv::REQ_DURATION => self.playback.duration = data.and_then(|v| v.as_f64()),
            crate::mpv::REQ_PAUSE => self.playback.paused = data.and_then(|v| v.as_bool()),
            crate::mpv::REQ_VOLUME => self.playback.volume = data.and_then(|v| v.as_f64()),
            _ => {}
        }
    }

    /// 分岐は網羅する。モードを増やしたときの書き分け漏れをコンパイラに拾わせる。
    pub fn status_line(&self) -> String {
        let line = match self.mode {
            Mode::Playing => self.playing_status(),
            Mode::Input => self.search_status("検索したい語句を入力して Enter".to_string()),
            Mode::Results => self.search_status(format!("{} 件", self.results.len())),
        };
        // エラーが出ている行に足すと読みにくいので、そのときは譲る。
        match &self.notice {
            Some(notice) if self.error.is_none() => format!("{line}  |  {notice}"),
            _ => line,
        }
    }

    /// 再生中はエラーで再生状況を隠さず、併記する。
    fn playing_status(&self) -> String {
        let line = self.playback_line();
        match &self.error {
            Some(error) => format!("{line}  |  エラー: {error}"),
            None => line,
        }
    }

    fn search_status(&self, idle: String) -> String {
        if let Some(error) = &self.error {
            return format!("エラー: {error}");
        }
        if self.searching {
            return "検索中...".to_string();
        }
        idle
    }

    fn playback_line(&self) -> String {
        let state = match self.playback.paused {
            Some(true) => "PAUSED",
            Some(false) => "PLAYING",
            None => "状態不明",
        };
        let volume = self
            .playback
            .volume
            .map(|v| format!("  vol {v:.0}"))
            .unwrap_or_default();
        format!(
            "{state}  {}  {} / {}{volume}",
            self.playback.title,
            format_time(self.playback.time_pos),
            format_time(self.playback.duration)
        )
    }
}

pub fn format_time(seconds: Option<f64>) -> String {
    let Some(seconds) = seconds else {
        return "--:--".to_string();
    };
    let total = seconds.max(0.0).round() as u64;
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn result(id: &str) -> SearchResult {
        SearchResult {
            id: id.to_string(),
            title: format!("title {id}"),
            duration: None,
            uploader: None,
        }
    }

    #[test]
    fn formats_time() {
        assert_eq!(format_time(None), "--:--");
        assert_eq!(format_time(Some(0.0)), "00:00");
        assert_eq!(format_time(Some(75.4)), "01:15");
        assert_eq!(format_time(Some(3661.0)), "1:01:01");
        assert_eq!(format_time(Some(-3.0)), "00:00");
    }

    #[test]
    fn selection_wraps_around() {
        let mut app = App {
            results: vec![result("a"), result("b")],
            ..App::default()
        };
        app.select_next();
        assert_eq!(app.selected, 1);
        app.select_next();
        assert_eq!(app.selected, 0);
        app.select_prev();
        assert_eq!(app.selected, 1);
    }

    #[test]
    fn selection_is_noop_without_results() {
        let mut app = App::default();
        app.select_next();
        app.select_prev();
        assert_eq!(app.selected, 0);
        assert!(app.selected_result().is_none());
    }

    #[test]
    fn empty_results_set_error_and_stay_in_input() {
        let mut app = App::default();
        app.set_results(Vec::new());
        assert_eq!(app.mode, Mode::Input);
        assert!(app.error.is_some());
    }

    #[test]
    fn results_switch_to_list_mode() {
        let mut app = App {
            selected: 5,
            ..App::default()
        };
        app.set_results(vec![result("a")]);
        assert_eq!(app.mode, Mode::Results);
        assert_eq!(app.selected, 0);
        assert!(app.error.is_none());
    }

    #[test]
    fn applies_polled_properties() {
        let mut app = App::default();
        app.apply_property(crate::mpv::REQ_TIME_POS, Some(json!(12.0)));
        app.apply_property(crate::mpv::REQ_DURATION, Some(json!(300.0)));
        app.apply_property(crate::mpv::REQ_PAUSE, Some(json!(true)));
        app.apply_property(crate::mpv::REQ_VOLUME, Some(json!(80.0)));
        assert_eq!(app.playback.time_pos, Some(12.0));
        assert_eq!(app.playback.duration, Some(300.0));
        assert_eq!(app.playback.paused, Some(true));
        assert_eq!(app.playback.volume, Some(80.0));

        app.apply_property(crate::mpv::REQ_TIME_POS, None);
        assert_eq!(app.playback.time_pos, None);
    }

    #[test]
    fn pause_error_response_is_not_read_as_playing() {
        let mut app = App {
            mode: Mode::Playing,
            ..App::default()
        };
        app.apply_property(crate::mpv::REQ_PAUSE, Some(json!(true)));
        assert!(app.status_line().starts_with("PAUSED"));
        app.apply_property(crate::mpv::REQ_PAUSE, None);
        assert_eq!(app.playback.paused, None);
        assert!(!app.status_line().starts_with("PLAYING"));
    }

    #[test]
    fn the_notice_stays_visible_until_an_error_takes_the_line() {
        // 設定を読み替えた旨は、気づけるようステータス行に出し続ける。
        let mut app = App {
            mode: Mode::Results,
            results: vec![result("a")],
            notice: Some("TUITUBE_FPS_LIMIT=3O を数値として読めません".to_string()),
            ..App::default()
        };
        assert_eq!(
            app.status_line(),
            "1 件  |  TUITUBE_FPS_LIMIT=3O を数値として読めません"
        );

        // 検索中でも消えない。
        app.searching = true;
        assert!(app.status_line().starts_with("検索中..."));
        assert!(app.status_line().ends_with("読めません"));

        // 再生中も再生状況の後ろに続く。
        app.searching = false;
        app.mode = Mode::Playing;
        assert!(app.status_line().starts_with("状態不明"));
        assert!(app.status_line().ends_with("読めません"));

        // エラーが出ている行には足さない。
        app.error = Some("boom".to_string());
        assert!(app.status_line().ends_with("エラー: boom"));
    }

    #[test]
    fn the_default_fps_limit_is_the_one_the_mpv_module_defines() {
        assert_eq!(
            App::default().fps_limit,
            Some(crate::mpv::DEFAULT_FPS_LIMIT)
        );
    }

    #[test]
    fn status_line_prefers_error() {
        let app = App {
            searching: true,
            error: Some("boom".to_string()),
            ..App::default()
        };
        assert_eq!(app.status_line(), "エラー: boom");
    }

    #[test]
    fn playing_status_stays_visible_with_error() {
        let app = App {
            mode: Mode::Playing,
            error: Some("boom".to_string()),
            playback: Playback {
                title: "song".to_string(),
                paused: Some(true),
                time_pos: Some(30.0),
                duration: Some(60.0),
                volume: Some(70.0),
                ..Playback::default()
            },
            ..App::default()
        };
        let line = app.status_line();
        assert!(line.starts_with("PAUSED  song  00:30 / 01:00  vol 70"));
        assert!(line.ends_with("エラー: boom"));
    }

    #[test]
    fn optimistic_seek_survives_a_stale_poll_within_the_hold() {
        let t0 = Instant::now();
        let mut playback = Playback {
            time_pos: Some(10.0),
            ..Playback::default()
        };
        playback.begin_seek(100.0, t0);
        assert_eq!(playback.time_pos, Some(100.0));
        assert!(playback.pending_seek.is_some());

        // シーク直後のポーリングは旧位置を返しうる。
        playback.reconcile_time_pos(Some(11.0), t0 + Duration::from_millis(500));
        assert_eq!(playback.time_pos, Some(100.0));
        playback.reconcile_time_pos(None, t0 + Duration::from_millis(800));
        assert_eq!(playback.time_pos, Some(100.0));

        playback.reconcile_time_pos(Some(101.5), t0 + Duration::from_millis(900));
        assert_eq!(playback.time_pos, Some(101.5));
        assert_eq!(playback.pending_seek, None);
    }

    #[test]
    fn a_stale_poll_wins_after_the_hold_expires() {
        let t0 = Instant::now();
        let mut playback = Playback {
            time_pos: Some(10.0),
            ..Playback::default()
        };
        playback.begin_seek(100.0, t0);
        playback.reconcile_time_pos(Some(11.0), t0 + SEEK_HOLD);

        assert_eq!(playback.time_pos, Some(11.0));
        assert_eq!(playback.pending_seek, None);
    }

    #[test]
    fn relative_seeks_stack_on_the_pending_target() {
        let t0 = Instant::now();
        let mut playback = Playback {
            time_pos: Some(10.0),
            ..Playback::default()
        };
        assert_eq!(playback.seek_base(), Some(10.0));
        playback.begin_seek(15.0, t0);
        assert_eq!(playback.seek_base(), Some(15.0));

        let t1 = t0 + Duration::from_millis(100);
        playback.begin_seek(20.0, t1);
        assert_eq!(playback.time_pos, Some(20.0));
        let pending = playback.pending_seek.expect("先行更新が入っている");
        assert_eq!(pending.target, 20.0);
        assert_eq!(pending.sent_at, t1);
    }

    #[test]
    fn playing_status_without_pause_data_is_marked_unknown() {
        let app = App {
            mode: Mode::Playing,
            ..App::default()
        };
        assert!(app.status_line().starts_with("状態不明"));
    }
}
