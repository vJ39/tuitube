//! 擬似カテゴリタブ。YouTube 側の本物のカテゴリは廃止済みなので固定キーワードで代える。

use crate::cookies::{Feed, Target};
use crate::search::SearchResult;
use std::cell::Cell;

/// 先頭に必ず入るタブ。検索ボックスの入力を使う。
pub const ALL_LABEL: &str = "すべて";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Category {
    pub label: String,
    pub query: String,
}

impl Category {
    pub fn new(label: &str, query: &str) -> Self {
        Self {
            label: label.to_string(),
            query: query.to_string(),
        }
    }
}

/// 設定ファイルで丸ごと差し替えられる既定の一覧。「すべて」は含めない。
pub fn default_categories() -> Vec<Category> {
    let keywords = ["音楽", "ゲーム", "ニュース", "アニメ", "スポーツ"]
        .into_iter()
        .map(|label| Category::new(label, label));
    // cookie 連携のフィードは `:ytrec` 等を覚えていないと打てないのでタブにも出す。
    // query はそのまま Target::for_query() を通り、cookie が無ければ既存の断り文になる。
    let feeds = Feed::ALL
        .into_iter()
        .map(|feed| Category::new(feed.label(), feed.keyword()));
    keywords.chain(feeds).collect()
}

/// タブごとに持ち越す画面の状態。戻ったときに再検索を待たせないために保つ。
#[derive(Debug, Default, Clone, PartialEq)]
pub struct TabState {
    pub results: Vec<SearchResult>,
    pub selected: usize,
    pub scroll: usize,
    pub loaded: bool,
}

#[derive(Debug)]
pub struct Tabs {
    categories: Vec<Category>,
    selected: usize,
    states: Vec<TabState>,
    /// タブ行に出している窓の開始位置。端末の幅は描画時にしか分からないので、
    /// 描く側 (ui::draw_tabs) が調整した結果をここへ戻す。
    window: Cell<usize>,
}

impl Default for Tabs {
    fn default() -> Self {
        Self::with_categories(default_categories())
    }
}

impl Tabs {
    /// 先頭へ「すべて」を差し込む。
    pub fn with_categories(categories: Vec<Category>) -> Self {
        let mut all = vec![Category::new(ALL_LABEL, "")];
        all.extend(categories);
        let states = (0..all.len()).map(|_| TabState::default()).collect();
        Self {
            categories: all,
            selected: 0,
            states,
            window: Cell::new(0),
        }
    }

    /// タブ行に出している窓の開始位置。
    pub fn window(&self) -> usize {
        self.window.get()
    }

    /// 描いた窓を覚える。次に描くときはここから最小限だけ動かす。
    pub fn remember_window(&self, start: usize) {
        self.window.set(start);
    }

    pub fn next(&mut self) {
        self.selected = (self.selected + 1) % self.categories.len();
    }

    pub fn prev(&mut self) {
        self.selected = (self.selected + self.categories.len() - 1) % self.categories.len();
    }

    /// 位置を指して移る。クリックの当たり判定から来る index は窓の外を指しうるので、
    /// 範囲外は黙って無視し、選べたかどうかを返す。範囲の判定はここだけに置く。
    pub fn select(&mut self, index: usize) -> bool {
        if index >= self.categories.len() {
            return false;
        }
        self.selected = index;
        true
    }

    pub fn labels(&self) -> Vec<&str> {
        self.categories
            .iter()
            .map(|category| category.label.as_str())
            .collect()
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    pub fn is_all(&self) -> bool {
        self.selected == 0
    }

    /// 検索ボックスとタブの対応を 1 対 1 に保つため、Enter は必ず「すべて」へ戻す。
    pub fn select_all(&mut self) {
        self.selected = 0;
    }

    /// 「すべて」なら検索ボックスの文字列、それ以外はタブのクエリ。
    pub fn target(&self, query: &str) -> Option<Target> {
        let raw = if self.is_all() {
            query
        } else {
            self.categories[self.selected].query.as_str()
        };
        let raw = raw.trim();
        (!raw.is_empty()).then(|| Target::for_query(raw))
    }

    pub fn state(&self) -> &TabState {
        &self.states[self.selected]
    }

    pub fn state_mut(&mut self) -> &mut TabState {
        &mut self.states[self.selected]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cookies::Feed;

    fn result(id: &str) -> SearchResult {
        SearchResult {
            id: id.to_string(),
            title: format!("title {id}"),
            duration: None,
            uploader: None,
            channel_id: None,
        }
    }

    #[test]
    fn tabs_always_start_with_the_all_tab() {
        let tabs = Tabs::default();
        assert_eq!(tabs.labels()[0], ALL_LABEL);
        assert_eq!(
            tabs.labels(),
            [
                "すべて",
                "音楽",
                "ゲーム",
                "ニュース",
                "アニメ",
                "スポーツ",
                "おすすめ",
                "履歴",
                "登録チャンネル",
                "後で見る"
            ]
        );
        assert_eq!(tabs.selected(), 0);
        assert!(tabs.is_all());

        // 設定で差し替えても先頭は「すべて」。
        let custom = Tabs::with_categories(vec![Category::new("将棋", "将棋 対局")]);
        assert_eq!(custom.labels(), [ALL_LABEL, "将棋"]);
    }

    #[test]
    fn the_cookie_feeds_are_reachable_as_tabs() {
        // キーワードを覚えていなくてもタブで選べる。
        for feed in Feed::ALL {
            let mut tabs = Tabs::default();
            let index = tabs
                .labels()
                .iter()
                .position(|label| *label == feed.label())
                .unwrap_or_else(|| panic!("{} のタブがない", feed.label()));
            while tabs.selected() != index {
                tabs.next();
            }
            // 検索ボックスに何が入っていてもフィードとして扱う。
            assert_eq!(tabs.target("ラーメン"), Some(Target::Feed(feed)));
        }
    }

    #[test]
    fn the_cookie_feed_tabs_come_after_the_keyword_tabs() {
        let labels = Tabs::default().labels().join(" ");
        let music = labels.find("音楽").expect("音楽");
        let recommended = labels.find("おすすめ").expect("おすすめ");
        assert!(music < recommended, "{labels}");
    }

    #[test]
    fn next_and_prev_wrap_around() {
        let mut tabs = Tabs::default();
        let last = tabs.labels().len() - 1;
        for expected in (1..=last).chain([0]) {
            tabs.next();
            assert_eq!(tabs.selected(), expected);
        }
        for expected in (1..=last).rev().chain([0]) {
            tabs.prev();
            assert_eq!(tabs.selected(), expected);
        }
    }

    #[test]
    fn target_of_a_category_tab_is_a_keyword_search() {
        let mut tabs = Tabs::default();
        tabs.next();
        assert_eq!(
            tabs.target("ラーメン"),
            Some(Target::Search("音楽".to_string())),
            "タブを選んでいる間は検索ボックスの語を使わない"
        );
    }

    #[test]
    fn target_of_the_all_tab_uses_the_query_box() {
        let tabs = Tabs::default();
        assert_eq!(
            tabs.target("ラーメン"),
            Some(Target::Search("ラーメン".to_string()))
        );
    }

    #[test]
    fn target_of_the_all_tab_is_none_when_the_box_is_blank() {
        let tabs = Tabs::default();
        assert_eq!(tabs.target(""), None);
        assert_eq!(tabs.target("   "), None);
    }

    #[test]
    fn a_tab_whose_query_is_a_feed_keyword_becomes_a_feed_target() {
        let mut tabs = Tabs::with_categories(vec![Category::new("おすすめ", ":ytrec")]);
        tabs.next();
        assert_eq!(tabs.target(""), Some(Target::Feed(Feed::Recommended)));
    }

    #[test]
    fn switching_tabs_keeps_each_tabs_results_and_selection() {
        let mut tabs = Tabs::default();
        tabs.state_mut().results = vec![result("a"), result("b")];
        tabs.state_mut().selected = 1;
        tabs.state_mut().scroll = 4;
        tabs.state_mut().loaded = true;

        tabs.next();
        assert_eq!(tabs.state(), &TabState::default());
        tabs.state_mut().results = vec![result("c")];
        tabs.state_mut().loaded = true;

        tabs.prev();
        assert_eq!(tabs.state().results.len(), 2);
        assert_eq!(tabs.state().selected, 1);
        assert_eq!(tabs.state().scroll, 4);
    }

    #[test]
    fn switching_back_does_not_mark_the_tab_as_unloaded() {
        let mut tabs = Tabs::default();
        tabs.next();
        tabs.state_mut().loaded = true;
        tabs.prev();
        tabs.next();
        assert!(tabs.state().loaded, "戻ったタブでは再検索しない");
    }

    #[test]
    fn select_jumps_straight_to_a_tab() {
        let mut tabs = Tabs::default();
        assert!(tabs.select(3));
        assert_eq!(tabs.selected(), 3);
        assert!(tabs.select(0));
        assert!(tabs.is_all());
        // 押し直しも選べたものとして扱う。取り直すかどうかは呼ぶ側が決める。
        assert!(tabs.select(0));
    }

    #[test]
    fn select_ignores_an_index_that_does_not_exist() {
        let mut tabs = Tabs::default();
        let last = tabs.labels().len() - 1;
        assert!(tabs.select(last));
        assert!(!tabs.select(last + 1));
        assert!(!tabs.select(usize::MAX));
        assert_eq!(tabs.selected(), last, "範囲外では選択を動かさない");
    }

    #[test]
    fn select_all_returns_to_the_first_tab() {
        let mut tabs = Tabs::default();
        tabs.next();
        tabs.next();
        tabs.select_all();
        assert!(tabs.is_all());
        assert_eq!(tabs.selected(), 0);
    }

    #[test]
    fn every_default_tab_label_fits_a_narrow_terminal() {
        // 全部を一度に並べる幅はもう無いので、タブ行は窓で切り出す (ui::tab_spans)。
        // 選択中のタブが切れずに出るために、ラベル 1 つぶんは狭い端末にも入る必要がある。
        for label in Tabs::default().labels() {
            let width = crate::grid::display_width(label);
            assert!(width <= 16, "{label} は {width} 桁");
        }
    }
}
