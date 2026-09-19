//! 検索欄の編集状態。文字列・カーソル・選択範囲をここだけで持つ。

use std::ops::Range;

/// 検索欄の編集状態。位置は文字インデックスで持つ。
/// 全角文字のバイト境界を呼び出し側が気にしなくて済む。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QueryEditor {
    text: String,
    cursor: usize,
    /// Some なら選択中。cursor との間が選択範囲。
    anchor: Option<usize>,
}

impl QueryEditor {
    pub fn text(&self) -> &str {
        &self.text
    }

    #[allow(dead_code, reason = "カーソル位置の読み出し口。今はテストだけが見る")]
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// 全置換。カーソルは末尾へ、選択は解除する。
    pub fn set(&mut self, text: &str) {
        self.text = text.to_string();
        self.cursor = self.len();
        self.anchor = None;
    }

    /// 選択があれば消してから、カーソル位置へ 1 文字入れる。
    pub fn insert(&mut self, c: char) {
        self.delete_selection();
        let at = self.byte_of(self.cursor);
        self.text.insert(at, c);
        self.cursor += 1;
    }

    /// 選択があればその範囲を、無ければカーソル直前の 1 文字を消す。
    pub fn backspace(&mut self) {
        if self.delete_selection() || self.cursor == 0 {
            return;
        }
        let start = self.byte_of(self.cursor - 1);
        let end = self.byte_of(self.cursor);
        self.text.replace_range(start..end, "");
        self.cursor -= 1;
    }

    pub fn move_left(&mut self, extend: bool) {
        match (extend, self.selection()) {
            (true, _) => self.extend_to(self.cursor.saturating_sub(1)),
            // 選択を解くときは端へ寄せるだけ。もう 1 文字ぶん動かさない。
            (false, Some(range)) => self.move_to(range.start),
            (false, None) => self.move_to(self.cursor.saturating_sub(1)),
        }
    }

    pub fn move_right(&mut self, extend: bool) {
        match (extend, self.selection()) {
            (true, _) => self.extend_to(self.cursor + 1),
            (false, Some(range)) => self.move_to(range.end),
            (false, None) => self.move_to(self.cursor + 1),
        }
    }

    pub fn move_home(&mut self, extend: bool) {
        if extend {
            self.extend_to(0);
        } else {
            self.move_to(0);
        }
    }

    pub fn move_end(&mut self, extend: bool) {
        let end = self.len();
        if extend {
            self.extend_to(end);
        } else {
            self.move_to(end);
        }
    }

    pub fn select_all(&mut self) {
        self.anchor = Some(0);
        self.cursor = self.len();
    }

    /// クリックで指した位置へ移す。範囲外は端へ丸め、選択は解除する。
    pub fn move_to(&mut self, index: usize) {
        self.cursor = index.min(self.len());
        self.anchor = None;
    }

    /// 選択範囲 (文字インデックス)。幅ゼロの選択は無選択として返す。
    pub fn selection(&self) -> Option<Range<usize>> {
        let anchor = self.anchor?;
        let start = anchor.min(self.cursor);
        let end = anchor.max(self.cursor);
        (start < end).then_some(start..end)
    }

    /// `from` 文字目から後ろ。横スクロールで隠れる先頭を落として描画に渡す。
    pub fn slice_from(&self, from: usize) -> &str {
        &self.text[self.byte_of(from)..]
    }

    /// `slice_from` を選択の前・選択・選択の後に割る。描画が反転表示に使う。
    /// 選択が無ければ 2 つめ以降は空。
    pub fn slices_from(&self, from: usize) -> (&str, &str, &str) {
        let visible = self.slice_from(from);
        let Some(range) = self.selection() else {
            return (visible, "", "");
        };
        let head = self.byte_of(from);
        let start = self.byte_of(range.start.max(from)) - head;
        let end = self.byte_of(range.end.max(from)) - head;
        (&visible[..start], &visible[start..end], &visible[end..])
    }

    /// カーソルまでの部分文字列。描画がカーソル桁の計算に使う。
    pub fn before_cursor(&self) -> &str {
        &self.text[..self.byte_of(self.cursor)]
    }

    fn len(&self) -> usize {
        self.text.chars().count()
    }

    /// 文字インデックスのバイト位置。末尾より後ろは文字列長へ丸める。
    fn byte_of(&self, index: usize) -> usize {
        self.text
            .char_indices()
            .nth(index)
            .map_or(self.text.len(), |(at, _)| at)
    }

    /// 選択範囲を消す。消したら true。
    fn delete_selection(&mut self) -> bool {
        let Some(range) = self.selection() else {
            self.anchor = None;
            return false;
        };
        let start = self.byte_of(range.start);
        let end = self.byte_of(range.end);
        self.text.replace_range(start..end, "");
        self.cursor = range.start;
        self.anchor = None;
        true
    }

    /// 選択を伸縮させる。起点が無ければ今のカーソルを起点にする。
    fn extend_to(&mut self, index: usize) {
        let end = self.len();
        self.anchor = Some(self.anchor.unwrap_or(self.cursor).min(end));
        self.cursor = index.min(end);
    }
}

impl From<&str> for QueryEditor {
    fn from(text: &str) -> Self {
        let mut editor = Self::default();
        editor.set(text);
        editor
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_replaces_everything_and_parks_the_cursor_at_the_end() {
        let mut query = QueryEditor::from("ラーメン");
        query.move_home(false);
        query.select_all();

        query.set("うどん");

        assert_eq!(query.text(), "うどん");
        assert_eq!(query.cursor(), 3, "カーソルは末尾");
        assert_eq!(query.selection(), None, "選択は持ち越さない");
    }

    #[test]
    fn typing_appends_at_the_cursor() {
        let mut query = QueryEditor::default();
        for c in "abc".chars() {
            query.insert(c);
        }

        assert_eq!(query.text(), "abc");
        assert_eq!(query.cursor(), 3);
    }

    #[test]
    fn typing_in_the_middle_inserts_there() {
        let mut query = QueryEditor::from("ac");
        query.move_left(false);
        query.insert('b');

        assert_eq!(query.text(), "abc");
        assert_eq!(query.cursor(), 2, "入れた文字の右");
    }

    /// IME 経由でも 1 文字ずつ Char として届く。文字単位で扱えていれば途中挿入も崩れない。
    #[test]
    fn multibyte_characters_can_be_inserted_one_by_one_in_the_middle() {
        let mut query = QueryEditor::from("ラン");
        query.move_left(false);
        for c in "ーメ".chars() {
            query.insert(c);
        }

        assert_eq!(query.text(), "ラーメン");
        assert_eq!(query.cursor(), 3);
    }

    #[test]
    fn backspace_removes_one_character_not_one_byte() {
        let mut query = QueryEditor::from("ラー");
        query.backspace();

        assert_eq!(query.text(), "ラ");
        assert_eq!(query.cursor(), 1);
    }

    #[test]
    fn backspace_removes_the_character_before_the_cursor() {
        let mut query = QueryEditor::from("ラーメン");
        query.move_left(false);
        query.backspace();

        assert_eq!(query.text(), "ラーン");
        assert_eq!(query.cursor(), 2);
    }

    #[test]
    fn backspace_at_the_head_does_nothing() {
        let mut query = QueryEditor::from("ラ");
        query.move_home(false);
        query.backspace();

        assert_eq!(query.text(), "ラ");
        assert_eq!(query.cursor(), 0);
    }

    #[test]
    fn backspace_on_an_empty_query_does_nothing() {
        let mut query = QueryEditor::default();
        query.backspace();

        assert_eq!(query.text(), "");
        assert_eq!(query.cursor(), 0);
    }

    #[test]
    fn the_cursor_stays_inside_the_text() {
        let mut query = QueryEditor::from("ab");
        query.move_home(false);
        query.move_left(false);
        assert_eq!(query.cursor(), 0);

        query.move_end(false);
        query.move_right(false);
        assert_eq!(query.cursor(), 2);
    }

    #[test]
    fn home_and_end_work_on_an_empty_query() {
        let mut query = QueryEditor::default();
        query.move_home(false);
        assert_eq!(query.cursor(), 0);
        query.move_end(false);
        assert_eq!(query.cursor(), 0);
        assert_eq!(query.selection(), None);
    }

    #[test]
    fn shift_arrows_grow_the_selection_from_where_it_started() {
        let mut query = QueryEditor::from("ラーメン");
        query.move_left(true);
        assert_eq!(query.selection(), Some(3..4));
        query.move_left(true);
        assert_eq!(query.selection(), Some(2..4), "同じ起点から伸びる");
        query.move_right(true);
        assert_eq!(query.selection(), Some(3..4), "戻せば縮む");
    }

    #[test]
    fn a_selection_that_shrinks_to_nothing_is_no_selection() {
        let mut query = QueryEditor::from("ab");
        query.move_left(true);
        query.move_right(true);

        assert_eq!(query.selection(), None);
        assert_eq!(query.cursor(), 2);
    }

    #[test]
    fn the_selection_reads_the_same_whichever_way_it_was_made() {
        let mut backwards = QueryEditor::from("ラーメン");
        backwards.move_left(true);
        backwards.move_left(true);

        let mut forwards = QueryEditor::from("ラーメン");
        forwards.move_home(false);
        forwards.move_right(true);
        forwards.move_right(true);

        assert_eq!(backwards.selection(), Some(2..4));
        assert_eq!(forwards.selection(), Some(0..2));
    }

    #[test]
    fn an_arrow_without_shift_collapses_the_selection_to_its_edge() {
        let mut query = QueryEditor::from("ラーメン");
        query.move_left(true);
        query.move_left(true);
        query.move_left(false);
        assert_eq!(query.cursor(), 2, "左端へ寄る (1 文字ぶん余計に動かさない)");
        assert_eq!(query.selection(), None);

        let mut query = QueryEditor::from("ラーメン");
        query.move_home(false);
        query.move_right(true);
        query.move_right(true);
        query.move_right(false);
        assert_eq!(query.cursor(), 2, "右端へ寄る");
        assert_eq!(query.selection(), None);
    }

    #[test]
    fn home_and_end_drop_the_selection() {
        let mut query = QueryEditor::from("ラーメン");
        query.select_all();
        query.move_home(false);
        assert_eq!(query.cursor(), 0);
        assert_eq!(query.selection(), None);

        query.select_all();
        query.move_end(false);
        assert_eq!(query.cursor(), 4);
        assert_eq!(query.selection(), None);
    }

    #[test]
    fn shift_home_and_shift_end_select_to_the_edges() {
        let mut query = QueryEditor::from("ラーメン");
        query.move_left(false);
        query.move_home(true);
        assert_eq!(query.selection(), Some(0..3));

        let mut query = QueryEditor::from("ラーメン");
        query.move_home(false);
        query.move_right(false);
        query.move_end(true);
        assert_eq!(query.selection(), Some(1..4));
    }

    #[test]
    fn select_all_covers_the_whole_text() {
        let mut query = QueryEditor::from("ラーメン");
        query.move_home(false);
        query.select_all();

        assert_eq!(query.selection(), Some(0..4));
        assert_eq!(query.cursor(), 4);
    }

    #[test]
    fn select_all_on_an_empty_query_selects_nothing() {
        let mut query = QueryEditor::default();
        query.select_all();

        assert_eq!(query.selection(), None);
        assert_eq!(query.cursor(), 0);
    }

    #[test]
    fn typing_over_a_selection_replaces_it() {
        let mut query = QueryEditor::from("ラーメン");
        query.select_all();
        query.insert('丼');

        assert_eq!(query.text(), "丼");
        assert_eq!(query.cursor(), 1);
        assert_eq!(query.selection(), None);
    }

    #[test]
    fn typing_over_a_partial_selection_keeps_the_rest() {
        let mut query = QueryEditor::from("ラーメン");
        query.move_left(true);
        query.move_left(true);
        query.insert('丼');

        assert_eq!(query.text(), "ラー丼");
        assert_eq!(query.cursor(), 3);
    }

    #[test]
    fn backspace_deletes_the_selection_instead_of_one_character() {
        let mut query = QueryEditor::from("ラーメン");
        query.move_home(false);
        query.move_right(true);
        query.move_right(true);
        query.backspace();

        assert_eq!(query.text(), "メン");
        assert_eq!(query.cursor(), 0);
        assert_eq!(query.selection(), None);
    }

    #[test]
    fn move_to_places_the_cursor_and_drops_the_selection() {
        let mut query = QueryEditor::from("ラーメン");
        query.select_all();
        query.move_to(2);

        assert_eq!(query.cursor(), 2);
        assert_eq!(query.selection(), None);
    }

    #[test]
    fn move_to_clamps_a_position_past_the_end() {
        let mut query = QueryEditor::from("ラー");
        query.move_to(99);

        assert_eq!(query.cursor(), 2);
    }

    #[test]
    fn before_cursor_splits_on_a_character_boundary() {
        let mut query = QueryEditor::from("ラーメン");
        assert_eq!(query.before_cursor(), "ラーメン");
        query.move_left(false);
        assert_eq!(query.before_cursor(), "ラーメ");
        query.move_home(false);
        assert_eq!(query.before_cursor(), "");
    }

    #[test]
    fn slices_split_the_text_around_the_selection() {
        let mut query = QueryEditor::from("ラーメン");
        query.move_left(false);
        query.move_left(true);
        query.move_left(true);

        assert_eq!(query.slices_from(0), ("ラ", "ーメ", "ン"));
    }

    #[test]
    fn slices_without_a_selection_are_all_head() {
        let query = QueryEditor::from("ラーメン");
        assert_eq!(query.slices_from(0), ("ラーメン", "", ""));
    }

    #[test]
    fn slices_drop_the_characters_scrolled_off_the_left() {
        let mut query = QueryEditor::from("ラーメン");
        assert_eq!(query.slice_from(2), "メン");
        assert_eq!(query.slices_from(2), ("メン", "", ""));

        // 選択の途中から始めたら、見えているぶんだけが選択範囲として残る。
        query.select_all();
        assert_eq!(query.slices_from(2), ("", "メン", ""));

        query.move_home(false);
        query.move_right(true);
        query.move_right(true);
        query.move_right(true);
        assert_eq!(
            query.slices_from(2),
            ("", "メ", "ン"),
            "0..3 の選択の後ろ半分"
        );
    }

    #[test]
    fn slices_past_the_end_are_empty() {
        let query = QueryEditor::from("ラー");
        assert_eq!(query.slice_from(9), "");
        assert_eq!(query.slices_from(9), ("", "", ""));
    }
}
