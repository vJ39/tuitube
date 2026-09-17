use crate::search::SearchResult;
use crossterm::event::KeyEvent;
use serde_json::Value;

pub enum AppEvent {
    Key(KeyEvent),
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

#[derive(Debug, Default, Clone)]
pub struct Playback {
    pub title: String,
    pub paused: Option<bool>,
    pub time_pos: Option<f64>,
    pub duration: Option<f64>,
    pub volume: Option<f64>,
}

pub struct App {
    pub mode: Mode,
    pub query: String,
    pub results: Vec<SearchResult>,
    pub selected: usize,
    pub searching: bool,
    pub error: Option<String>,
    pub playback: Playback,
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
            playback: Playback::default(),
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
            crate::mpv::REQ_TIME_POS => self.playback.time_pos = data.and_then(|v| v.as_f64()),
            crate::mpv::REQ_DURATION => self.playback.duration = data.and_then(|v| v.as_f64()),
            crate::mpv::REQ_PAUSE => self.playback.paused = data.and_then(|v| v.as_bool()),
            crate::mpv::REQ_VOLUME => self.playback.volume = data.and_then(|v| v.as_f64()),
            _ => {}
        }
    }

    pub fn status_line(&self) -> String {
        // 再生中はエラーで再生状況を隠さず、併記する。
        if self.mode == Mode::Playing {
            let line = self.playback_line();
            return match &self.error {
                Some(error) => format!("{line}  |  エラー: {error}"),
                None => line,
            };
        }
        if let Some(error) = &self.error {
            return format!("エラー: {error}");
        }
        if self.searching {
            return "検索中...".to_string();
        }
        match self.mode {
            Mode::Results => format!("{} 件", self.results.len()),
            _ => "検索したい語句を入力して Enter".to_string(),
        }
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
            },
            ..App::default()
        };
        let line = app.status_line();
        assert!(line.starts_with("PAUSED  song  00:30 / 01:00  vol 70"));
        assert!(line.ends_with("エラー: boom"));
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
