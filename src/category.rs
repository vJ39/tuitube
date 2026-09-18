//! 擬似カテゴリタブ。YouTube 側の本物のカテゴリは廃止済みなので固定キーワードで代える。

use crate::cookies::Target;
use crate::search::SearchResult;

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
    ["音楽", "ゲーム", "ニュース", "アニメ", "スポーツ"]
        .into_iter()
        .map(|label| Category::new(label, label))
        .collect()
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
        }
    }

    pub fn next(&mut self) {
        self.selected = (self.selected + 1) % self.categories.len();
    }

    pub fn prev(&mut self) {
        self.selected = (self.selected + self.categories.len() - 1) % self.categories.len();
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
        }
    }

    #[test]
    fn tabs_always_start_with_the_all_tab() {
        let tabs = Tabs::default();
        assert_eq!(tabs.labels()[0], ALL_LABEL);
        assert_eq!(
            tabs.labels(),
            ["すべて", "音楽", "ゲーム", "ニュース", "アニメ", "スポーツ"]
        );
        assert_eq!(tabs.selected(), 0);
        assert!(tabs.is_all());

        // 設定で差し替えても先頭は「すべて」。
        let custom = Tabs::with_categories(vec![Category::new("将棋", "将棋 対局")]);
        assert_eq!(custom.labels(), [ALL_LABEL, "将棋"]);
    }

    #[test]
    fn next_and_prev_wrap_around() {
        let mut tabs = Tabs::default();
        for expected in [1, 2, 3, 4, 5, 0] {
            tabs.next();
            assert_eq!(tabs.selected(), expected);
        }
        for expected in [5, 4, 3, 2, 1, 0] {
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
    fn select_all_returns_to_the_first_tab() {
        let mut tabs = Tabs::default();
        tabs.next();
        tabs.next();
        tabs.select_all();
        assert!(tabs.is_all());
        assert_eq!(tabs.selected(), 0);
    }

    #[test]
    fn the_default_tab_row_fits_an_80_column_terminal() {
        // ラベルを " │ " で繋いだ幅。80 桁に収まらないと見出しが折り返す。
        let tabs = Tabs::default();
        let labels = tabs.labels();
        let width: usize = labels
            .iter()
            .map(|label| crate::grid::display_width(label))
            .sum::<usize>()
            + 3 * (labels.len() - 1);
        assert!(width <= 80, "{width} 桁");
    }
}
