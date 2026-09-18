use crate::category::Tabs;
use crate::cookies::{CookieState, Target};
use crate::display::DisplayMode;
use crate::mpv::MpvCommand;
use crate::rgb::RgbImage;
use crate::search::{SearchReport, SearchResult};
use crate::seekbar::SeekBarState;
use crate::settings::Settings;
use crate::speed::{Polled, Speed};
use crate::thumbs::Thumbs;
use crate::video::VideoSink;
use crossterm::event::{KeyEvent, MouseEvent};
use ratatui::layout::Rect;
use serde_json::Value;
use std::time::{Duration, Instant};

/// シークを送ってから確定値を待つ間、ポーリングの古い値を無視する時間。
pub const SEEK_HOLD: Duration = Duration::from_secs(2);
/// 相対シークは keyframes で着地がずれるので、この幅までは目標どおり着いたとみなす。
pub const SEEK_TOLERANCE_SECS: f64 = 3.0;
/// 速度を送ってから確定値を待つ間、ポーリングの古い値を無視する時間。
pub const SPEED_HOLD: Duration = Duration::from_secs(2);

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
        target: Target,
        report: SearchReport,
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
    /// サムネイルのデコードが終わった。nonce は検索と共用で、
    /// 検索が入れ替わっていれば古い画像として捨てる。
    ThumbsReady {
        nonce: u64,
        target_px: (u32, u32),
        images: Vec<(String, Result<RgbImage, ()>)>,
        /// 一度だけ伝える事情 (保存先を作れない等)。
        notice: Option<String>,
        /// 以後サムネイル取得を止める理由 (curl が無い)。
        disable: Option<String>,
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
    /// mpv が実際に使っている VO。要求した表示モードが通ったかはこれで見る。
    pub current_vo: Option<String>,
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
    /// ブラウザ cookie 連携の状態。検索・再生・表示がここを見る。
    pub cookies: CookieState,
    pub playback: Playback,
    /// 起動時に読んだ設定。
    pub settings: Settings,
    /// 要求中の表示モード。実際にどちらで出ているかは playback.current_vo。
    pub display: DisplayMode,
    /// 再生速度。動画をまたいで持ち越すので Playback でなく App が持つ。
    pub speed: Speed,
    /// 速度を送った時刻。ここから SPEED_HOLD の間は、食い違うポーリング値を捨てる。
    pub speed_sent_at: Option<Instant>,
    /// 再生中だけ、mpv の kitty 出力を受け取るスロットが入る。
    pub video: Option<VideoSink>,
    /// 直近の terminal.draw() が描いた画面。マウスの当たり判定はこれで割り付ける。
    pub screen: Rect,
    pub seek_bar: SeekBarState,
    pub should_quit: bool,
    /// 擬似カテゴリタブ。results / selected / scroll はここの写し。
    pub tabs: Tabs,
    pub thumbs: Thumbs,
    /// 格子の先頭表示位置 (項目インデックス)。描画のたびに列数で丸める。
    pub scroll: usize,
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
            cookies: CookieState::default(),
            playback: Playback::default(),
            settings: Settings::default(),
            display: DisplayMode::default(),
            speed: Speed::NORMAL,
            speed_sent_at: None,
            video: None,
            screen: Rect::default(),
            seek_bar: SeekBarState::default(),
            should_quit: false,
            tabs: Tabs::default(),
            thumbs: Thumbs::default(),
            scroll: 0,
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

    pub fn set_results(&mut self, results: Vec<SearchResult>, target: &Target) {
        let state = self.tabs.state_mut();
        state.results = results;
        state.selected = 0;
        state.scroll = 0;
        // 0 件でも読み込み済みにする。戻るたびに同じ検索を投げ直さないため。
        state.loaded = true;
        self.sync_from_tab();
        if self.results.is_empty() {
            self.error = Some(target.empty_message());
            self.mode = Mode::Input;
        } else {
            self.mode = Mode::Results;
        }
    }

    /// 今のタブの内容を App 側の写しへ取り込む。サムネイルの状態表も入れ替える。
    pub fn sync_from_tab(&mut self) {
        let state = self.tabs.state();
        self.results = state.results.clone();
        self.selected = state.selected;
        self.scroll = state.scroll;
        let ids = self.result_ids();
        self.thumbs.reset(&ids);
        self.thumbs.mark_dirty();
    }

    /// 画面から離れる前に、タブへ選択位置を書き戻す。
    pub fn store_to_tab(&mut self) {
        let (selected, scroll) = (self.selected, self.scroll);
        let state = self.tabs.state_mut();
        state.selected = selected;
        state.scroll = scroll;
    }

    pub fn result_ids(&self) -> Vec<String> {
        self.results.iter().map(|r| r.id.clone()).collect()
    }

    /// ポーリングの応答を取り込む。mpv へ送り返すものがあれば返す。
    pub fn apply_property(&mut self, id: u64, data: Option<Value>) -> Option<MpvCommand> {
        match id {
            crate::mpv::REQ_TIME_POS => self
                .playback
                .reconcile_time_pos(data.and_then(|v| v.as_f64()), Instant::now()),
            crate::mpv::REQ_DURATION => self.playback.duration = data.and_then(|v| v.as_f64()),
            crate::mpv::REQ_PAUSE => self.playback.paused = data.and_then(|v| v.as_bool()),
            crate::mpv::REQ_VOLUME => self.playback.volume = data.and_then(|v| v.as_f64()),
            crate::mpv::REQ_CURRENT_VO => {
                self.playback.current_vo =
                    data.as_ref().and_then(|v| v.as_str()).map(str::to_string);
            }
            // 取れないときは触らない。mpv 側の値だけが正とは限らない。
            crate::mpv::REQ_SPEED => {
                if let Some(value) = data.and_then(|v| v.as_f64()) {
                    return self.reconcile_speed(Polled::from_f64(value), Instant::now());
                }
            }
            _ => {}
        }
        None
    }

    /// 自分で決めた速度を控える。mpv が確定するまでのポーリング値はこれで弾く。
    pub fn set_speed_sent(&mut self, speed: Speed, now: Instant) {
        self.speed = speed;
        self.speed_sent_at = Some(now);
    }

    /// ポーリングの速度を取り込む。保持時間中の古い値は捨てる。
    /// mpv 側が範囲外なら、丸めた値を送り返して表示と実際の再生を揃える。
    fn reconcile_speed(&mut self, polled: Polled, now: Instant) -> Option<MpvCommand> {
        // 送信前に発行された get_property の応答は、後から古い値を寄越す。
        if let Some(sent_at) = self.speed_sent_at {
            let within_hold = now.saturating_duration_since(sent_at) < SPEED_HOLD;
            if within_hold && polled.speed != self.speed {
                return None;
            }
        }
        if polled.clamped {
            self.set_speed_sent(polled.speed, now);
            return Some(polled.speed.command());
        }
        self.speed_sent_at = None;
        self.speed = polled.speed;
        None
    }

    /// 分岐は網羅する。モードを増やしたときの書き分け漏れをコンパイラに拾わせる。
    pub fn status_line(&self) -> String {
        let line = match self.mode {
            Mode::Playing => self.playing_status(),
            Mode::Input => self.search_status("検索したい語句を入力して Enter".to_string()),
            Mode::Results => self.search_status(self.results_status()),
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

    /// 格子のタイトルは 18 桁ほどで切れるので、選択中の完全なタイトルはここに出す。
    fn results_status(&self) -> String {
        let count = format!("{} 件", self.results.len());
        if self.thumbs.is_fetching() {
            return format!("{count}  |  サムネイル取得中...");
        }
        match self.selected_result() {
            Some(result) => format!("{count}  |  {}", result.title),
            None => count,
        }
    }

    fn search_status(&self, idle: String) -> String {
        if let Some(error) = &self.error {
            return format!("エラー: {error}");
        }
        let line = if self.searching {
            "検索中...".to_string()
        } else {
            idle
        };
        // cookie 連携は GUI 側の事情で黙って効かなくなるので、常に現状を出しておく。
        match self.cookies.label() {
            Some(label) => format!("{line}  |  {label}"),
            None => line,
        }
    }

    /// 要求と mpv の実際が食い違う間は切替中と出す。切替は数秒かかることがある。
    pub fn display_label(&self) -> String {
        let fps = self
            .settings
            .fps_cap
            .map(|cap| format!(" {}fps", cap.get()))
            .unwrap_or_default();
        let detail = match self.display {
            DisplayMode::Embedded => format!("{fps} {}", self.settings.display.quality.label()),
            // 画質はピクセル予算なので、文字ブロックでは効かない。
            DisplayMode::Text => fps,
            DisplayMode::Window => String::new(),
        };
        let actual = DisplayMode::from_current_vo(self.playback.current_vo.as_deref());
        let transition = match actual {
            Some(actual) if actual != self.display => " (切替中)",
            _ => "",
        };
        format!("[{}{detail}{transition}]", self.display.label())
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
            "{state}  {}  {} / {}{volume}  {}  {}",
            self.playback.title,
            format_time(self.playback.time_pos),
            format_time(self.playback.duration),
            self.speed.label(),
            self.display_label()
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
    use crate::cookies::{CookieSource, Feed};
    use crate::speed::Speed;
    use serde_json::json;

    fn search_target() -> Target {
        Target::Search("q".to_string())
    }

    fn source() -> CookieSource {
        CookieSource::from_spec(Some("chrome")).expect("spec")
    }

    fn result(id: &str) -> SearchResult {
        SearchResult {
            id: id.to_string(),
            title: format!("title {id}"),
            duration: None,
            uploader: None,
        }
    }

    /// 送り返しが要らない取り込み。返り値まで込みで確かめる。
    fn poll(app: &mut App, id: u64, data: Option<Value>) {
        assert_eq!(app.apply_property(id, data), None, "送り返しは出ない");
    }

    fn polled(speed: Speed) -> crate::speed::Polled {
        crate::speed::Polled {
            speed,
            clamped: false,
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
        app.set_results(Vec::new(), &search_target());
        assert_eq!(app.mode, Mode::Input);
        assert!(app.error.is_some());
    }

    #[test]
    fn the_results_status_shows_the_full_title_of_the_selection() {
        // 格子ではタイトルが切り詰められるので、完全なタイトルはここでしか読めない。
        let mut app = App {
            mode: Mode::Results,
            results: vec![result("a"), result("b")],
            selected: 1,
            ..App::default()
        };
        assert_eq!(app.status_line(), "2 件  |  title b");

        app.thumbs.set_fetching(true);
        assert_eq!(app.status_line(), "2 件  |  サムネイル取得中...");

        // 0 件なら件数だけ。
        let app = App {
            mode: Mode::Results,
            ..App::default()
        };
        assert_eq!(app.status_line(), "0 件");
    }

    #[test]
    fn set_results_writes_through_to_the_current_tab() {
        let mut app = App {
            selected: 5,
            scroll: 8,
            ..App::default()
        };
        app.set_results(vec![result("a"), result("b")], &search_target());

        assert_eq!(app.tabs.state().results.len(), 2);
        assert!(app.tabs.state().loaded, "戻ったときに再検索しない");
        assert_eq!(app.selected, 0);
        assert_eq!(app.scroll, 0);
        // 0 件でも読み込み済みにする。
        app.set_results(Vec::new(), &search_target());
        assert!(app.tabs.state().loaded);
    }

    #[test]
    fn storing_and_syncing_moves_the_selection_between_tabs() {
        let mut app = App::default();
        app.set_results(vec![result("a"), result("b")], &search_target());
        app.selected = 1;
        app.scroll = 4;
        app.store_to_tab();

        app.tabs.next();
        app.sync_from_tab();
        assert!(app.results.is_empty());
        assert_eq!(app.selected, 0);
        assert_eq!(app.scroll, 0);

        app.tabs.prev();
        app.sync_from_tab();
        assert_eq!(app.results.len(), 2);
        assert_eq!(app.selected, 1);
        assert_eq!(app.scroll, 4);
        assert_eq!(app.result_ids(), ["a", "b"]);
    }

    #[test]
    fn results_switch_to_list_mode() {
        let mut app = App {
            selected: 5,
            ..App::default()
        };
        app.set_results(vec![result("a")], &search_target());
        assert_eq!(app.mode, Mode::Results);
        assert_eq!(app.selected, 0);
        assert!(app.error.is_none());
    }

    #[test]
    fn empty_search_results_keep_the_current_message() {
        let mut app = App::default();
        app.set_results(Vec::new(), &search_target());
        assert_eq!(app.error.as_deref(), Some("検索結果が0件でした"));
    }

    #[test]
    fn empty_feed_results_explain_the_login_requirement() {
        // フィードが空なのは件数の問題ではなく、ログインが効いていない疑いが濃い。
        let mut app = App::default();
        app.set_results(Vec::new(), &Target::Feed(Feed::Recommended));
        let error = app.error.expect("理由を出す");
        assert!(error.starts_with("おすすめ"), "{error}");
        assert!(error.contains("ログイン"), "{error}");
    }

    #[test]
    fn idle_and_searching_status_show_the_cookie_label() {
        let mut app = App {
            cookies: CookieState::Armed(source()),
            ..App::default()
        };
        assert!(
            app.status_line().ends_with("cookies: chrome"),
            "{}",
            app.status_line()
        );
        assert!(app.status_line().starts_with("検索したい語句"));

        app.searching = true;
        assert!(app.status_line().starts_with("検索中..."));
        assert!(app.status_line().ends_with("cookies: chrome"));

        // 停止中はその旨まで出す。
        app.searching = false;
        app.cookies = CookieState::Suspended {
            source: source(),
            reason: "読めませんでした".to_string(),
        };
        assert!(app.status_line().ends_with("cookies: chrome (停止)"));

        // 環境変数が無ければ従来どおり何も足さない。
        app.cookies = CookieState::Off;
        assert_eq!(app.status_line(), "検索したい語句を入力して Enter");
    }

    #[test]
    fn notice_shows_when_there_is_no_error_and_error_wins() {
        let mut app = App {
            cookies: CookieState::Armed(source()),
            notice: Some("cookie を読めませんでした".to_string()),
            ..App::default()
        };
        let line = app.status_line();
        assert!(line.contains("cookies: chrome"), "{line}");
        assert!(line.ends_with("cookie を読めませんでした"), "{line}");

        app.error = Some("boom".to_string());
        assert_eq!(app.status_line(), "エラー: boom");
    }

    #[test]
    fn applies_polled_properties() {
        let mut app = App::default();
        poll(&mut app, crate::mpv::REQ_TIME_POS, Some(json!(12.0)));
        poll(&mut app, crate::mpv::REQ_DURATION, Some(json!(300.0)));
        poll(&mut app, crate::mpv::REQ_PAUSE, Some(json!(true)));
        poll(&mut app, crate::mpv::REQ_VOLUME, Some(json!(80.0)));
        assert_eq!(app.playback.time_pos, Some(12.0));
        assert_eq!(app.playback.duration, Some(300.0));
        assert_eq!(app.playback.paused, Some(true));
        assert_eq!(app.playback.volume, Some(80.0));

        poll(&mut app, crate::mpv::REQ_TIME_POS, None);
        assert_eq!(app.playback.time_pos, None);
    }

    #[test]
    fn speed_is_applied_from_the_poll_and_rounded() {
        let mut app = App::default();
        assert_eq!(app.speed, Speed::NORMAL);

        // mpv ウィンドウ側の × 1.1 は 0.1 刻みに乗らない値で返る。
        poll(&mut app, crate::mpv::REQ_SPEED, Some(json!(2.75)));
        assert_eq!(app.speed, Speed::from_tenths(28).expect("2.8x"));

        // 値が取れないときは触らない。
        poll(&mut app, crate::mpv::REQ_SPEED, None);
        assert_eq!(app.speed, Speed::from_tenths(28).expect("2.8x"));
    }

    #[test]
    fn a_speed_outside_the_range_is_sent_back_so_the_display_matches_the_playback() {
        // mpv ウィンドウ側で 8.0 にされた状態。表示を 4.0x にするだけでは実再生が 8 倍のまま。
        let mut app = App::default();
        assert_eq!(
            app.apply_property(crate::mpv::REQ_SPEED, Some(json!(8.0))),
            Some(Speed::MAX.command())
        );
        assert_eq!(app.speed, Speed::MAX);

        // 送り返した値が mpv から返ってくれば、それ以上は送らない。
        poll(&mut app, crate::mpv::REQ_SPEED, Some(json!(4.0)));
        assert_eq!(app.speed, Speed::MAX);
    }

    #[test]
    fn a_stale_speed_poll_within_the_hold_does_not_roll_back_the_local_value() {
        // 1.2x で問い合わせ → 応答が届く前に ] を処理 → 1.2 が後から届く、という順番。
        let t0 = Instant::now();
        let mut app = App::default();
        app.set_speed_sent(Speed::from_tenths(13).expect("1.3x"), t0);

        assert_eq!(
            app.reconcile_speed(
                polled(Speed::from_tenths(12).expect("1.2x")),
                t0 + Duration::from_millis(500)
            ),
            None
        );
        assert_eq!(app.speed, Speed::from_tenths(13).expect("1.3x"));

        // mpv が確定値を返したら保持は終わる。
        assert_eq!(
            app.reconcile_speed(
                polled(Speed::from_tenths(13).expect("1.3x")),
                t0 + Duration::from_millis(900),
            ),
            None
        );
        assert_eq!(app.speed, Speed::from_tenths(13).expect("1.3x"));
        assert!(app.speed_sent_at.is_none());
    }

    #[test]
    fn a_speed_changed_in_the_mpv_window_wins_after_the_hold_expires() {
        let t0 = Instant::now();
        let mut app = App::default();
        app.set_speed_sent(Speed::from_tenths(13).expect("1.3x"), t0);

        assert_eq!(
            app.reconcile_speed(
                polled(Speed::from_tenths(20).expect("2.0x")),
                t0 + SPEED_HOLD
            ),
            None
        );
        assert_eq!(app.speed, Speed::from_tenths(20).expect("2.0x"));
        assert!(app.speed_sent_at.is_none());
    }

    #[test]
    fn the_status_line_shows_the_speed_after_the_volume() {
        let mut app = App {
            mode: Mode::Playing,
            speed: Speed::from_tenths(15).expect("1.5x"),
            playback: Playback {
                title: "song".to_string(),
                paused: Some(false),
                volume: Some(70.0),
                ..Playback::default()
            },
            ..App::default()
        };
        let line = app.status_line();
        assert!(line.contains("vol 70  1.5x  ["), "{line}");

        // 等速でも出す。戻ったことが分かるため。
        app.speed = Speed::NORMAL;
        assert!(
            app.status_line().contains("vol 70  1.0x  ["),
            "{}",
            app.status_line()
        );
    }

    #[test]
    fn pause_error_response_is_not_read_as_playing() {
        let mut app = App {
            mode: Mode::Playing,
            ..App::default()
        };
        poll(&mut app, crate::mpv::REQ_PAUSE, Some(json!(true)));
        assert!(app.status_line().starts_with("PAUSED"));
        poll(&mut app, crate::mpv::REQ_PAUSE, None);
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
        // 件数と選択中のタイトルの後ろに続く。
        assert_eq!(
            app.status_line(),
            "1 件  |  title a  |  TUITUBE_FPS_LIMIT=3O を数値として読めません"
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
    fn current_vo_is_applied_from_the_poll() {
        let mut app = App::default();
        poll(
            &mut app,
            crate::mpv::REQ_CURRENT_VO,
            Some(json!("gpu-next")),
        );
        assert_eq!(app.playback.current_vo.as_deref(), Some("gpu-next"));
        // kitty VO では取れないプロパティもあるので、値が消えることも通常の経過。
        poll(&mut app, crate::mpv::REQ_CURRENT_VO, None);
        assert_eq!(app.playback.current_vo, None);
    }

    #[test]
    fn display_label_marks_the_transition_until_mpv_confirms() {
        let mut app = App {
            display: DisplayMode::Window,
            playback: Playback {
                current_vo: Some("kitty".to_string()),
                ..Playback::default()
            },
            ..App::default()
        };
        assert_eq!(app.display_label(), "[別ウィンドウ (切替中)]");

        app.playback.current_vo = Some("gpu-next".to_string());
        assert_eq!(app.display_label(), "[別ウィンドウ]");

        app.display = DisplayMode::Embedded;
        app.playback.current_vo = Some("kitty".to_string());
        assert_eq!(app.display_label(), "[埋め込み 15fps medium]");

        // 制限なしなら fps は出さない。
        app.settings.fps_cap = None;
        assert_eq!(app.display_label(), "[埋め込み medium]");

        // 値が来ていない間は切替中と決めつけない。
        app.playback.current_vo = None;
        assert_eq!(app.display_label(), "[埋め込み medium]");
    }

    #[test]
    fn the_status_line_ends_with_the_display_label_while_playing() {
        let app = App {
            mode: Mode::Playing,
            playback: Playback {
                title: "song".to_string(),
                ..Playback::default()
            },
            ..App::default()
        };
        assert!(
            app.status_line().ends_with(&app.display_label()),
            "{}",
            app.status_line()
        );
    }

    #[test]
    fn default_app_takes_the_display_mode_from_settings() {
        assert_eq!(App::default().display, Settings::default().display.mode);
        assert_eq!(App::default().settings, Settings::default());
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
