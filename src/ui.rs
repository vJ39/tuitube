use crate::app::{App, ChannelView, Mode, format_time};
use crate::comments;
use crate::display::DisplayMode;
use crate::geometry::cell_size;
use crate::grid::{self, LayoutMode};
use crate::query::QueryEditor;
use crate::seekbar::{SeekBar, SeekBarLayout, label_text, label_width};
use crate::video::CellSize;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};

/// 別ウィンドウ再生中に映像領域へ出す案内。
fn window_placeholder(display: DisplayMode) -> String {
    format!("別ウィンドウで再生中  w: {}へ", display.next().label())
}

/// 再生中は [映像, シークバー, ステータス, ヘルプ] の4段。映像に残り全体を渡す。
fn playing_areas(area: Rect) -> [Rect; 4] {
    Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area)
}

/// mpv に渡す `--vo-kitty-*` は描画先と同じ寸法でなければならない。
pub fn video_area(area: Rect) -> Rect {
    playing_areas(area)[0]
}

/// シークバーの行。クリック桁から再生位置を求めるときもこの矩形を使う。
pub fn seek_bar_area(area: Rect) -> Rect {
    playing_areas(area)[1]
}

/// 再生状態を出す行。
pub fn status_area(area: Rect) -> Rect {
    playing_areas(area)[2]
}

/// 操作説明の行。
pub fn help_area(area: Rect) -> Rect {
    playing_areas(area)[3]
}

/// コメント一覧の枠の内側。描画と送り幅が同じ寸法を数える。
pub fn comments_viewport(screen: Rect) -> Rect {
    comments_block().inner(video_area(screen))
}

/// 描画とヒットテストが共有する割り付け。
pub fn seek_bar_layout(app: &App) -> SeekBarLayout {
    layout_for(app.screen, app.playback.duration)
}

fn layout_for(screen: Rect, duration: Option<f64>) -> SeekBarLayout {
    SeekBarLayout::new(seek_bar_area(screen), label_width(duration))
}

/// 分岐は網羅する。モードを増やしたときの描き分け漏れをコンパイラに拾わせる。
pub fn draw(frame: &mut Frame, app: &App) {
    match app.mode {
        Mode::Playing => draw_playing(frame, app),
        Mode::Settings => draw_settings(frame, app),
        // チャンネルも同じ 5 段の画面。中身の参照先だけが app.channel へ移る。
        Mode::Input | Mode::Results | Mode::Channel => draw_search(frame, app),
    }
}

/// 設定画面は [タイトル, 項目, ステータス, ヘルプ] の4段。項目に残り全体を渡す。
pub fn settings_areas(area: Rect) -> [Rect; 4] {
    Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area)
}

const SETTINGS_TITLE: &str = "設定 (s で保存。Esc は編集を捨てて戻る)";
/// 打ち込み中は s が効かず、Esc も打ち込みを捨てるだけで画面は閉じない。
const SETTINGS_TYPING_TITLE: &str = "設定 (数値を打ち込み中)";
/// 選択中の行の目印。カーソルの桁を数えるときも同じ幅を足す。
const SETTINGS_MARKER: &str = "> ";

/// タイトルもヘルプ行と同じ条件で切り替える。片方だけ残すと案内が食い違う。
fn settings_title(app: &App) -> &'static str {
    if app.settings_edit.is_some() {
        SETTINGS_TYPING_TITLE
    } else {
        SETTINGS_TITLE
    }
}

fn draw_settings(frame: &mut Frame, app: &App) {
    let areas = settings_areas(frame.area());
    frame.render_widget(
        Paragraph::new(settings_title(app)).style(Style::default().add_modifier(Modifier::BOLD)),
        areas[0],
    );

    let mut rows = app.settings_rows();
    let selected = app.settings_selected.min(rows.len().saturating_sub(1));
    // 打ち込み中の行は値だけを差し替える。行末に足してある断りはそのまま残す。
    if let Some(raw) = &app.settings_edit
        && let Some(row) = rows.get_mut(selected)
    {
        let item = app.settings_item();
        let note = row
            .strip_prefix(&item.row(&app.settings))
            .unwrap_or_default()
            .to_string();
        *row = format!("{}: {}{note}", item.label(), raw);
    }
    let items: Vec<ListItem> = rows.into_iter().map(ListItem::new).collect();
    let list = List::new(items)
        .highlight_symbol(SETTINGS_MARKER)
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    let mut state = ListState::default();
    state.select(Some(selected));
    frame.render_stateful_widget(list, areas[1], &mut state);

    draw_footer(frame, app, areas[2], areas[3]);

    if let Some(raw) = &app.settings_edit {
        let at = settings_cursor(frame.area(), selected, app.settings_item().label(), raw);
        frame.set_cursor_position(at);
    }
}

/// 打ち込み中の行のカーソル位置 (0 始まり)。
/// 行数が入り切らない端末では一覧が送られて選択行が末尾に来るので、そこへ置く。
pub fn settings_cursor(screen: Rect, index: usize, label: &str, raw: &str) -> (u16, u16) {
    let area = settings_areas(screen)[1];
    let text = format!("{SETTINGS_MARKER}{label}: {raw}");
    let width = Span::raw(text.as_str()).width().min(u16::MAX as usize) as u16;
    let x = area
        .x
        .saturating_add(width)
        .min(area.right().saturating_sub(1));
    let last = area.height.saturating_sub(1) as usize;
    (x, area.y.saturating_add(index.min(last) as u16))
}

/// 検索画面は [入力, タブ, 結果, ステータス, ヘルプ] の5段。結果に残り全体を渡す。
pub fn search_areas(area: Rect) -> [Rect; 5] {
    Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area)
}

/// 結果ブロックの内側。格子の割り付けと画像の貼り付けが同じ矩形を使う。
pub fn results_inner(area: Rect) -> Rect {
    Block::default()
        .borders(Borders::ALL)
        .inner(search_areas(area)[2])
}

/// 描画と画像の貼り付けが共有する割り付け。格子を組めないときは None。
pub fn grid_layout(app: &App, cell: CellSize) -> Option<grid::Layout> {
    grid_layout_in(app, app.screen, cell)
}

/// 指定の画面寸法での割り付け。リサイズは描き直す前の寸法を渡す。
pub fn grid_layout_in(app: &App, screen: Rect, cell: CellSize) -> Option<grid::Layout> {
    if app.settings.search.layout != LayoutMode::Grid {
        return None;
    }
    grid::layout(
        results_inner(screen),
        cell,
        app.view_results().len(),
        app.view_scroll(),
    )
}

/// 入力欄のカーソル位置 (0 始まり)。draw と、画像を貼った後の戻し先が同じ計算を使う。
pub fn input_cursor(screen: Rect, query: &QueryEditor) -> (u16, u16) {
    let area = search_areas(screen)[0];
    let column =
        text_width(query.before_cursor()).saturating_sub(query_scroll(query, input_width(area)));
    (cursor_x(area, column), area.y + 1)
}

/// 入力欄の枠の内側の幅 (桁)。文字が並ぶのも送り幅を決めるのもこの幅の中。
fn input_width(area: Rect) -> usize {
    area.width.saturating_sub(2) as usize
}

/// 1 文字の表示幅。入力欄の桁計算はすべてこれを積む。
/// ratatui は書記素クラスタ単位で桁を進めるため、ZWJ 絵文字のように
/// 複数コードポイントで 1 つの書記素になる文字は対象外 (docs/query-editor-design.md)。
fn char_width(ch: char) -> usize {
    grid::display_width(&ch.to_string())
}

fn text_width(text: &str) -> usize {
    text.chars().map(char_width).sum()
}

/// 横スクロールの送り幅 (桁)。カーソルが枠の内側に入る最小の幅を、
/// 全角を半分に割らないよう文字境界で求める。
fn query_scroll(query: &QueryEditor, width: usize) -> usize {
    let Some(last) = width.checked_sub(1) else {
        return 0;
    };
    let needed = text_width(query.before_cursor()).saturating_sub(last);
    let mut scrolled = 0;
    for ch in query.text().chars() {
        if scrolled >= needed {
            break;
        }
        scrolled += char_width(ch);
    }
    scrolled
}

/// 画面のこの位置にある検索語の文字。検索欄の外では None。
pub fn query_index_at_point(app: &App, column: u16, row: u16) -> Option<usize> {
    let area = search_areas(app.screen)[0];
    if !area.contains(Position::new(column, row)) {
        return None;
    }
    let width = input_width(area);
    // 文字は枠の内側 (x+1) から並ぶ。枠を押したら内側の端を押したものとして扱う。
    let offset =
        (column.saturating_sub(area.x.saturating_add(1)) as usize).min(width.saturating_sub(1));
    Some(query_index_at_column(
        app.query.text(),
        query_scroll(&app.query, width) + offset,
    ))
}

/// 表示幅を積みながら `column` 桁にある文字を探す。末尾より右は文字数を返す。
fn query_index_at_column(text: &str, column: usize) -> usize {
    let mut x = 0;
    for (index, ch) in text.chars().enumerate() {
        x += char_width(ch);
        if column < x {
            return index;
        }
    }
    text.chars().count()
}

/// 選択範囲だけ反転させた入力行。`from` 文字目より前は横スクロールで隠れている。
/// 入力中でなければ反転は出さない。カーソルも出ない欄に選択だけ残ると、
/// どこを編集しているのか分からなくなる。
fn query_line(query: &QueryEditor, from: usize, editing: bool) -> Line<'_> {
    if !editing {
        return Line::from(query.slice_from(from));
    }
    let (head, selected, tail) = query.slices_from(from);
    Line::from(vec![
        Span::raw(head),
        Span::styled(selected, Style::default().add_modifier(Modifier::REVERSED)),
        Span::raw(tail),
    ])
}

fn draw_search(frame: &mut Frame, app: &App) {
    let areas = search_areas(frame.area());

    let scrolled = query_index_at_column(
        app.query.text(),
        query_scroll(&app.query, input_width(areas[0])),
    );
    let input = Paragraph::new(query_line(&app.query, scrolled, app.mode == Mode::Input))
        .block(Block::default().borders(Borders::ALL).title(" 検索 "));
    frame.render_widget(input, areas[0]);
    draw_tabs(frame, app, areas[1]);

    // app.screen は直前の draw の寸法なので、割り付けは今のフレームで組み直す。
    let layout = if app.settings.search.layout == LayoutMode::Grid {
        grid::layout(
            Block::default().borders(Borders::ALL).inner(areas[2]),
            cell_size(),
            app.view_results().len(),
            app.view_scroll(),
        )
    } else {
        None
    };
    match &layout {
        Some(layout) => draw_grid(frame, app, areas[2], layout),
        None => draw_list(frame, app, areas[2]),
    }

    draw_footer(frame, app, areas[3], areas[4]);

    if app.mode == Mode::Input {
        frame.set_cursor_position(input_cursor(frame.area(), &app.query));
    }
}

const TAB_GAP: &str = " │ ";
const TAB_MORE_LEFT: &str = "< ";
const TAB_MORE_RIGHT: &str = " >";

/// タブ行に並べる見出しと選択位置。チャンネル閲覧中はチャンネルのタブへ差し替える。
/// 窓の開始位置はカテゴリタブだけが覚える (チャンネルは 3 つなので常に先頭から数える)。
fn tab_row_source(app: &App) -> (Vec<&str>, usize, usize) {
    match &app.channel {
        Some(channel) => (ChannelView::labels().to_vec(), channel.tab.index(), 0),
        None => (app.tabs.labels(), app.tabs.selected(), app.tabs.window()),
    }
}

fn draw_tabs(frame: &mut Frame, app: &App, area: Rect) {
    let (labels, selected, window) = tab_row_source(app);
    let width = area.width as usize;
    let range = visible_tabs(&labels, selected, width, window);
    if app.channel.is_none() {
        app.tabs.remember_window(range.start);
    }
    let spans = tab_spans(&labels, selected, width, range);
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// `labels[start..end]` を並べたときの行の幅。両端のマーカーぶんも数える。
fn tab_row_width(labels: &[&str], start: usize, end: usize) -> usize {
    let mut width: usize = labels[start..end]
        .iter()
        .map(|label| grid::display_width(label))
        .sum();
    width += grid::display_width(TAB_GAP) * (end - start).saturating_sub(1);
    if start > 0 {
        width += grid::display_width(TAB_MORE_LEFT);
    }
    if end < labels.len() {
        width += grid::display_width(TAB_MORE_RIGHT);
    }
    width
}

/// `start` から右へ詰めたときに入る最後のタブの次。幅が足りなくても 1 つは返す
/// (選択中のタブは切ってでも出す)。
fn tab_window_end(labels: &[&str], start: usize, width: usize) -> usize {
    let mut end = start + 1;
    while end < labels.len() && tab_row_width(labels, start, end + 1) <= width {
        end += 1;
    }
    end
}

/// 幅に入るぶんだけを切り出す窓。前回の窓 `start` から最小限だけ動かすので、
/// 隣のタブへ移っただけでタブ行全体がずれることがない。
fn visible_tabs(
    labels: &[&str],
    selected: usize,
    width: usize,
    start: usize,
) -> std::ops::Range<usize> {
    if labels.is_empty() {
        return 0..0;
    }
    let selected = selected.min(labels.len() - 1);
    // 選択が窓より左にあるなら、そこまで戻す。
    let mut start = start.min(selected);
    // 選択が窓の右から出ているなら、入るまで 1 つずつ送る。
    while tab_window_end(labels, start, width) <= selected {
        start += 1;
    }
    // 端末が広がったぶんは左へ戻す。右端のタブを失わない間だけ動かす。
    while start > 0
        && tab_window_end(labels, start - 1, width) == tab_window_end(labels, start, width)
    {
        start -= 1;
    }
    start..tab_window_end(labels, start, width)
}

/// タブ行に左から並ぶもの。描画とクリックの当たり判定が同じ並びを通るように、
/// 幅の食い方はここだけで決める。
enum TabPiece {
    /// 窓の外にまだタブがあることを示す印。
    Marker(&'static str),
    Gap,
    Label {
        index: usize,
        text: String,
    },
}

impl TabPiece {
    fn text(&self) -> &str {
        match self {
            TabPiece::Marker(text) => text,
            TabPiece::Gap => TAB_GAP,
            TabPiece::Label { text, .. } => text,
        }
    }
}

fn tab_pieces(labels: &[&str], width: usize, range: std::ops::Range<usize>) -> Vec<TabPiece> {
    if range.is_empty() {
        return Vec::new();
    }
    let mut budget = width;
    let mut pieces = Vec::new();
    // マーカーだけで行を埋めない。1 桁も残らないなら出さない。
    if range.start > 0 && grid::display_width(TAB_MORE_LEFT) < budget {
        budget -= grid::display_width(TAB_MORE_LEFT);
        pieces.push(TabPiece::Marker(TAB_MORE_LEFT));
    }
    let tail = range.end < labels.len() && grid::display_width(TAB_MORE_RIGHT) < budget;
    if tail {
        budget -= grid::display_width(TAB_MORE_RIGHT);
    }
    for index in range.clone() {
        if index > range.start {
            if budget <= grid::display_width(TAB_GAP) {
                break;
            }
            budget -= grid::display_width(TAB_GAP);
            pieces.push(TabPiece::Gap);
        }
        // 窓は選択中のタブを必ず残すので、端末より広いラベルが来るのはそれ 1 つのときだけ。
        let text = grid::truncate(labels[index], budget);
        budget -= grid::display_width(&text);
        pieces.push(TabPiece::Label { index, text });
    }
    if tail {
        pieces.push(TabPiece::Marker(TAB_MORE_RIGHT));
    }
    pieces
}

fn tab_spans(
    labels: &[&str],
    selected: usize,
    width: usize,
    range: std::ops::Range<usize>,
) -> Vec<Span<'static>> {
    let dim = Style::default().fg(Color::DarkGray);
    tab_pieces(labels, width, range)
        .into_iter()
        .map(|piece| match piece {
            TabPiece::Marker(text) => Span::styled(text, dim),
            TabPiece::Gap => Span::styled(TAB_GAP, dim),
            TabPiece::Label { index, text } => {
                let style = if index == selected {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
                } else {
                    Style::default().fg(Color::Gray)
                };
                Span::styled(text, style)
            }
        })
        .collect()
}

/// タブ行の `column` 桁にあるタブ。区切り・マーカー・余白の上では None。
fn tab_at_column(
    labels: &[&str],
    width: usize,
    range: std::ops::Range<usize>,
    column: usize,
) -> Option<usize> {
    let mut x = 0;
    for piece in tab_pieces(labels, width, range) {
        let cells = grid::display_width(piece.text());
        if let TabPiece::Label { index, .. } = piece
            && (x..x + cells).contains(&column)
        {
            return Some(index);
        }
        x += cells;
    }
    None
}

/// 画面のこの位置にあるタブ。タブ行の外や、窓の外へ送ったタブの上では None。
pub fn tab_at_point(app: &App, column: u16, row: u16) -> Option<usize> {
    let area = search_areas(app.screen)[1];
    if !area.contains(Position::new(column, row)) {
        return None;
    }
    let (labels, selected, window) = tab_row_source(app);
    let width = area.width as usize;
    // 窓は draw_tabs が覚えたものをそのまま使う。見えている行と判定をずらさない。
    let range = visible_tabs(&labels, selected, width, window);
    tab_at_column(&labels, width, range, (column - area.x) as usize)
}

/// 画面のこの位置にある結果。格子の隙間や、リスト表示では None。
/// 描画と同じ割り付けを通るので、見えているセルと判定がずれない。
pub fn result_at_point(app: &App, cell: CellSize, column: u16, row: u16) -> Option<usize> {
    let layout = grid_layout(app, cell)?;
    let at = Position::new(column, row);
    layout
        .cells
        .iter()
        .position(|cell| {
            cell.image.contains(at) || cell.title.contains(at) || cell.meta.contains(at)
        })
        .map(|i| layout.offset + i)
}

/// 可視範囲と総数。スクロールしても今どこを見ているか分かるようにする。
fn results_title(offset: usize, shown: usize, total: usize) -> String {
    if total == 0 || shown == 0 {
        return " 結果 ".to_string();
    }
    format!(" 結果 {}-{}/{total} ", offset + 1, offset + shown)
}

fn draw_grid(frame: &mut Frame, app: &App, area: Rect, layout: &grid::Layout) {
    let results = app.view_results();
    let title = results_title(layout.offset, layout.cells.len(), results.len());
    frame.render_widget(Block::default().borders(Borders::ALL).title(title), area);

    for (i, cell) in layout.cells.iter().enumerate() {
        let index = layout.offset + i;
        let Some(result) = results.get(index) else {
            break;
        };
        // 画像が来ていないセルは枠だけ。来ていれば空けておき、APC が上に載る。
        if app.thumbs.get(&result.id).is_none() {
            frame.render_widget(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::DarkGray)),
                cell.image,
            );
        }
        let title_style = if index == app.view_selected() {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        };
        frame.render_widget(
            Paragraph::new(grid::truncate(&result.title, usize::from(cell.title.width)))
                .style(title_style),
            cell.title,
        );
        let uploader = result.uploader.as_deref().unwrap_or("-");
        let meta = format!("{}  {uploader}", format_time(result.duration));
        frame.render_widget(
            Paragraph::new(grid::truncate(&meta, usize::from(cell.meta.width)))
                .style(Style::default().fg(Color::DarkGray)),
            cell.meta,
        );
    }
}

/// Kitty graphics protocol 非対応の端末と、格子を組めない狭さのときの従来表示。
fn draw_list(frame: &mut Frame, app: &App, area: Rect) {
    let items: Vec<ListItem> = app
        .view_results()
        .iter()
        .map(|r| {
            let uploader = r.uploader.as_deref().unwrap_or("-");
            ListItem::new(format!(
                "{}  {}  [{}]",
                format_time(r.duration),
                r.title,
                uploader
            ))
        })
        .collect();
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(" 結果 "))
        .highlight_symbol("> ")
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    let mut state = ListState::default();
    if !app.view_results().is_empty() {
        state.select(Some(app.view_selected()));
    }
    frame.render_stateful_widget(list, area, &mut state);
}

/// 分岐は網羅する。モードを増やしたときの描き分け漏れをコンパイラに拾わせる。
fn draw_playing(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let video = video_area(area);
    if app.comments.visible() {
        draw_comments(frame, video, app);
    } else {
        match app.display {
            DisplayMode::Window => draw_window_placeholder(frame, video, app.display),
            DisplayMode::Text => {
                if let Some(sink) = &app.video {
                    sink.render_text(video, frame.buffer_mut());
                }
            }
            // 埋め込みの画像は draw の後にメインループが APC で重ねる。
            DisplayMode::Embedded => {}
        }
    }
    draw_seek_bar(frame, app, area);
    draw_footer(frame, app, status_area(area), help_area(area));
}

fn comments_block() -> Block<'static> {
    Block::default().borders(Borders::ALL).title(" コメント ")
}

/// コメント表示中は映像の代わりに一覧を出す。映像フレームの送出は present_video が止める。
/// 上限 50 件は 1 画面に入らないので、↑↓ の送り幅ぶんだけずらして描く。
fn draw_comments(frame: &mut Frame, area: Rect, app: &App) {
    if area.height == 0 {
        return;
    }
    let block = comments_block();
    let inner = block.inner(area);
    let height = inner.height as usize;
    let lines = comments::display_lines(app.comments.state(), inner.width as usize);
    let offset = app.comments.scroll(lines.len(), height);
    let items: Vec<ListItem> = lines
        .into_iter()
        .skip(offset)
        .take(height)
        .map(ListItem::new)
        .collect();
    frame.render_widget(List::new(items).block(block), area);
}

/// 別ウィンドウ中は映像が来ないので、どこで再生しているかを映像領域に出す。
fn draw_window_placeholder(frame: &mut Frame, area: Rect, display: DisplayMode) {
    if area.height == 0 {
        return;
    }
    let row = Rect {
        y: area.y + area.height / 2,
        height: 1,
        ..area
    };
    frame.render_widget(
        Paragraph::new(window_placeholder(display))
            .style(Style::default().fg(Color::DarkGray))
            .centered(),
        row,
    );
}

/// ポインタが指す列と時刻。duration が無いとシークできないので、印もラベルも出さない。
fn seek_pointer(app: &App, layout: &SeekBarLayout) -> Option<(u16, f64)> {
    let (column, duration) = app.seek_bar.shown_column().zip(app.playback.duration)?;
    Some((column, layout.seconds_at(column, duration)))
}

fn draw_seek_bar(frame: &mut Frame, app: &App, area: Rect) {
    let layout = layout_for(area, app.playback.duration);
    let pointer = seek_pointer(app, &layout);
    let label = label_text(
        pointer
            .map(|(_, seconds)| seconds)
            .or(app.playback.time_pos),
        app.playback.duration,
    );
    let bar = SeekBar {
        layout,
        filled: layout.filled_cells(app.playback.time_pos, app.playback.duration),
        marker: pointer.map(|(column, _)| column),
        label: &label,
        highlighted: pointer.is_some(),
    };
    frame.render_widget(bar, seek_bar_area(area));
}

fn draw_footer(frame: &mut Frame, app: &App, status: Rect, help: Rect) {
    let status_style = if app.error.is_some() {
        Style::default().fg(Color::Red)
    } else {
        Style::default().fg(Color::Cyan)
    };
    frame.render_widget(
        Paragraph::new(status_text(app, status.width as usize)).style(status_style),
        status,
    );
    frame.render_widget(
        Paragraph::new(help_line(app, help.width)).style(Style::default().fg(Color::DarkGray)),
        help,
    );
}

/// 今の画面の案内。設定画面で数値を打ち込んでいる間だけ、その操作へ差し替える。
fn help_line(app: &App, width: u16) -> String {
    if app.mode == Mode::Settings && app.settings_edit.is_some() {
        return fit_hints(&settings_typing_hints(), width as usize);
    }
    help_text(app.mode, app.display, app.comments.visible(), width)
}

/// ステータス行は 1 行で折り返さないので、入らないぶんは "…" にする。
/// 黙って切れると、切れたのか元から短いのかが読み手に分からない。
fn status_text(app: &App, width: usize) -> String {
    grid::truncate(&app.status_line(), width)
}

/// カーソルの桁。`column` は送り幅を引いた後の、枠の内側での位置。
fn cursor_x(input_area: Rect, column: usize) -> u16 {
    let column = column.min(u16::MAX as usize) as u16;
    input_area
        .x
        .saturating_add(1)
        .saturating_add(column)
        .min(input_area.right().saturating_sub(2))
}

fn help_text(mode: Mode, display: DisplayMode, comments_open: bool, width: u16) -> String {
    let hints = match mode {
        Mode::Input => input_hints(),
        Mode::Results => results_hints(),
        Mode::Channel => channel_hints(),
        Mode::Playing => playing_hints(display, comments_open),
        Mode::Settings => settings_hints(),
    };
    fit_hints(&hints, width as usize)
}

/// 検索入力の案内。先頭 5 つで 75 桁ほどになり、80 桁端末にはそこまでが出る。
/// 入力欄では S も検索語なので、設定は Ctrl+S で開く。
/// 後半の編集キーは 80 桁には入らないので、幅のある端末でだけ出る。
fn input_hints() -> Vec<String> {
    vec![
        "Enter:検索".to_string(),
        "Tab:カテゴリ".to_string(),
        ":yt*:ログイン連動の一覧".to_string(),
        "Esc:結果へ/終了".to_string(),
        "Ctrl+S:設定".to_string(),
        "Ctrl+A:全選択".to_string(),
        "Shift+←→:選択".to_string(),
        "Home/End:先頭/末尾".to_string(),
        "クリック:カーソル".to_string(),
    ]
}

/// 結果一覧の案内。ちょうど 80 桁で、80 桁端末に全部入る。
/// h は押さないと気づけないので、矢印と Esc の言葉を削ってでも入れる。
fn results_hints() -> Vec<String> {
    vec![
        "↑↓←→".to_string(),
        "Enter:再生".to_string(),
        "c:チャンネル".to_string(),
        "h:隠す".to_string(),
        "Tab:カテゴリ".to_string(),
        "r:再取得".to_string(),
        "Esc:検索".to_string(),
        "q:終了".to_string(),
        "S:設定".to_string(),
    ]
}

/// チャンネル一覧の案内。全部で 79 桁で 80 桁端末に収まる。
/// タブの案内が長いので、矢印は結果一覧の案内に任せて落としてある。
/// h はこのチャンネルごと隠す。
fn channel_hints() -> Vec<String> {
    vec![
        "Enter:再生".to_string(),
        "Tab:動画/ショート/配信".to_string(),
        "s:登録".to_string(),
        "h:隠す".to_string(),
        "r:再取得".to_string(),
        "Esc:戻る".to_string(),
        "q:終了".to_string(),
        "S:設定".to_string(),
    ]
}

/// 設定画面の案内。全部で 71 桁ほどで 80 桁端末に収まる。
/// 幅が足りないと後ろから落ちるので、←→ でも代用できる直接入力は保存と終了の後ろに置く。
fn settings_hints() -> Vec<String> {
    vec![
        "↑↓:選択".to_string(),
        "←→:値変更".to_string(),
        "Enter/Space:切替".to_string(),
        "s:保存".to_string(),
        "Esc:破棄して戻る".to_string(),
        "0-9:直接入力".to_string(),
    ]
}

/// 数値を打ち込んでいる間の案内。この間は他のキーが効かないので、抜け方を先に出す。
fn settings_typing_hints() -> Vec<String> {
    vec![
        "Enter:確定".to_string(),
        "Esc:取消".to_string(),
        "0-9:入力".to_string(),
        "BS:1字削除".to_string(),
    ]
}

/// 再生中の案内。全部で 130 桁ほどあり 80 桁端末には入らないので、
/// 落ちて困らないものを後ろに置く。先頭 7 つは最も幅を食う w:別ウィンドウとコメント表示中でも
/// 77 桁に収まる。
fn playing_hints(display: DisplayMode, comments_open: bool) -> Vec<String> {
    vec![
        "space:一時停止".to_string(),
        "←→:シーク".to_string(),
        // コメント表示中の ↑↓ は一覧送りに使う。
        if comments_open {
            "↑↓:行送り".to_string()
        } else {
            "↑↓:音量".to_string()
        },
        "c:URLコピー".to_string(),
        format!("w:{}", display.next().label()),
        "Esc:停止".to_string(),
        "q:終了".to_string(),
        "s:字幕".to_string(),
        "o:コメント".to_string(),
        "[ ]:速度±0.1".to_string(),
        "BS:等速".to_string(),
        "クリック:シーク".to_string(),
    ]
}

/// 幅に入るところまでを空白 1 つでつなぐ。help は折り返さないので、
/// 途中で切れた案内を出すより落とす。
fn fit_hints(hints: &[String], width: usize) -> String {
    let mut line = String::new();
    for hint in hints {
        let next = if line.is_empty() {
            hint.clone()
        } else {
            format!("{line} {hint}")
        };
        if grid::display_width(&next) > width {
            break;
        }
        line = next;
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{ChannelView, Playback};
    use crate::category::Tabs;
    use crate::search::SearchResult;
    use crate::seekbar::SeekBarState;

    fn result(index: usize) -> SearchResult {
        SearchResult {
            id: format!("id{index}"),
            title: format!("title {index}"),
            duration: None,
            uploader: None,
            channel_id: None,
        }
    }

    #[test]
    fn a_video_without_duration_gets_no_marker_to_seek_with() {
        let mut app = App {
            mode: Mode::Playing,
            screen: Rect::new(0, 0, 80, 24),
            playback: Playback {
                time_pos: Some(10.0),
                duration: None,
                ..Playback::default()
            },
            seek_bar: SeekBarState {
                hover: Some(10),
                drag: None,
            },
            ..App::default()
        };
        assert_eq!(seek_pointer(&app, &seek_bar_layout(&app)), None);

        // duration が届けば同じホバー列に印と時刻が出る (トラック 65 セルで 1 セル 10 秒)。
        app.playback.duration = Some(650.0);
        assert_eq!(
            seek_pointer(&app, &seek_bar_layout(&app)),
            Some((10, 100.0))
        );
    }

    #[test]
    fn cursor_follows_display_width_not_char_count() {
        let app = input_app("abc");
        assert_eq!(input_cursor(app.screen, &app.query).0, 4);

        // 全角4文字 = 8桁
        let app = input_app("ラーメン");
        assert_eq!(input_cursor(app.screen, &app.query).0, 9);

        let app = input_app("");
        assert_eq!(input_cursor(app.screen, &app.query).0, 1);
    }

    #[test]
    fn cursor_stops_inside_the_border() {
        // 送り幅を入れないと枠に重なる位置でも、枠の内側で止める。
        let area = Rect::new(0, 0, 10, 3);
        assert_eq!(cursor_x(area, 99), 8);
    }

    /// 検索欄を持つ 40x24 の画面。
    fn input_app(text: &str) -> App {
        sized_input_app(text, 40)
    }

    /// 検索欄を持つ幅 `width` の画面。横スクロールの検証に使う。
    fn sized_input_app(text: &str, width: u16) -> App {
        App {
            screen: Rect::new(0, 0, width, 24),
            query: QueryEditor::from(text),
            ..App::default()
        }
    }

    /// 入力欄の行に描かれている文字。
    fn drawn_input_text(app: &App) -> String {
        let width = app.screen.width;
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, 24)).expect("端末");
        terminal.draw(|frame| draw(frame, app)).expect("描ける");
        let buffer = terminal.backend().buffer().clone();
        let mut out = String::new();
        // 両端の枠を除いた内側だけを読む。全角の右半分のセルは空白なので飛ばす。
        let mut skip = false;
        for x in 1..width.saturating_sub(1) {
            if std::mem::take(&mut skip) {
                continue;
            }
            let symbol = buffer[(x, 1)].symbol();
            skip = grid::display_width(symbol) == 2;
            out.push_str(symbol);
        }
        out.trim_end().to_string()
    }

    #[test]
    fn the_cursor_follows_the_editing_position_not_the_end_of_the_text() {
        let mut app = input_app("ラーメン");
        assert_eq!(input_cursor(app.screen, &app.query).0, 9);

        app.query.move_left(false);
        assert_eq!(input_cursor(app.screen, &app.query).0, 7, "全角 3 文字ぶん");

        app.query.move_home(false);
        assert_eq!(input_cursor(app.screen, &app.query).0, 1, "枠の内側の先頭");
    }

    /// 入力欄の行で反転表示されている文字。
    fn reversed_input_text(app: &App) -> String {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 24)).expect("端末");
        terminal.draw(|frame| draw(frame, app)).expect("描ける");
        let buffer = terminal.backend().buffer().clone();
        let mut out = String::new();
        for x in 0..buffer.area.width {
            let cell = &buffer[(x, 1)];
            if cell.modifier.contains(Modifier::REVERSED) {
                out.push_str(cell.symbol());
            }
        }
        out
    }

    #[test]
    fn the_selection_is_drawn_reversed() {
        let mut app = input_app("ラーメン");
        assert_eq!(reversed_input_text(&app), "", "選択が無ければ反転しない");

        app.query.move_left(true);
        app.query.move_left(true);
        assert_eq!(reversed_input_text(&app), "メン");

        app.query.select_all();
        assert_eq!(reversed_input_text(&app), "ラーメン");
    }

    #[test]
    fn the_selection_is_not_drawn_after_the_focus_leaves_the_search_box() {
        let mut app = input_app("ラーメン");
        app.query.select_all();
        assert_eq!(reversed_input_text(&app), "ラーメン");

        // 結果一覧ではカーソルも出ないので、反転だけ残すと編集中に見える。
        app.mode = Mode::Results;
        assert_eq!(reversed_input_text(&app), "");
    }

    #[test]
    fn the_input_scrolls_to_keep_the_cursor_in_the_box() {
        // 枠の内側 8 桁に対して全角 8 文字 (16 桁)。
        let mut app = sized_input_app("ラーメンラーメン", 10);
        assert_eq!(drawn_input_text(&app), "ーメン", "末尾が見えている");
        assert_eq!(input_cursor(app.screen, &app.query).0, 7, "最後の文字の右");

        app.query.move_home(false);
        assert_eq!(drawn_input_text(&app), "ラーメン", "先頭へ戻れば送りも戻る");
        assert_eq!(input_cursor(app.screen, &app.query).0, 1);
    }

    #[test]
    fn a_query_that_fits_is_not_scrolled() {
        let app = input_app("ラーメン");
        assert_eq!(drawn_input_text(&app), "ラーメン");
        assert_eq!(query_index_at_point(&app, 1, 1), Some(0));
    }

    #[test]
    fn clicking_a_scrolled_box_answers_with_the_character_under_it() {
        let app = sized_input_app("ラーメンラーメン", 10);
        // 送り幅は 10 桁。内側の先頭に出ているのは 6 文字目 (index 5)。
        assert_eq!(query_index_at_point(&app, 1, 1), Some(5));
        assert_eq!(query_index_at_point(&app, 3, 1), Some(6));
        assert_eq!(
            query_index_at_point(&app, 7, 1),
            Some(8),
            "末尾より右は文字数"
        );
    }

    #[test]
    fn clicking_the_right_border_stops_at_the_last_visible_character() {
        let mut app = sized_input_app("ラーメンラーメン", 10);
        app.query.move_home(false);
        // 内側 8 桁には 4 文字しか出ていない。右枠を押しても 5 文字目は指さない。
        assert_eq!(query_index_at_point(&app, 9, 1), Some(3));
        assert_eq!(query_index_at_point(&app, 8, 1), Some(3));
    }

    /// 桁の数え方が 1 つなら、カーソルのいる桁を押すと同じ位置が返る。
    #[test]
    fn the_cursor_column_and_the_click_target_count_the_same_way() {
        let text = "aあiうe";
        let mut app = input_app(text);
        for index in 0..text.chars().count() {
            app.query.move_to(index);
            let x = input_cursor(app.screen, &app.query).0;
            assert_eq!(
                query_index_at_point(&app, x, 1),
                Some(index),
                "{index} 文字目"
            );
        }
    }

    #[test]
    fn clicking_the_search_box_answers_with_the_character_under_it() {
        let app = input_app("ラーメン");
        // 文字は枠の内側 (x=1) から並び、全角 1 文字が 2 桁を占める。
        assert_eq!(query_index_at_point(&app, 1, 1), Some(0));
        assert_eq!(query_index_at_point(&app, 2, 1), Some(0));
        assert_eq!(query_index_at_point(&app, 3, 1), Some(1));
        assert_eq!(query_index_at_point(&app, 8, 1), Some(3));
        assert_eq!(query_index_at_point(&app, 9, 1), Some(4), "文字の先は末尾");
        assert_eq!(query_index_at_point(&app, 30, 1), Some(4));
        assert_eq!(
            query_index_at_point(&app, 0, 1),
            Some(0),
            "左の枠は先頭扱い"
        );
    }

    #[test]
    fn clicking_an_empty_search_box_answers_with_the_head() {
        let app = input_app("");
        assert_eq!(query_index_at_point(&app, 10, 1), Some(0));
    }

    #[test]
    fn the_search_box_hit_test_only_answers_where_it_is_drawn() {
        // 全段が入らない高さでは割り付けが潰れる。どこへ潰れても描いた行の中だけで応じる。
        for height in 0..8u16 {
            let app = App {
                screen: Rect::new(0, 0, 40, height),
                ..App::default()
            };
            let area = search_areas(app.screen)[0];
            for row in 0..8u16 {
                let drawn = row >= area.y && row < area.bottom();
                let hit = query_index_at_point(&app, 1, row);
                assert_eq!(
                    hit.is_some(),
                    drawn,
                    "{height} 行 / {row} 行目 (入力欄 {area:?}): {hit:?}"
                );
            }
        }
    }

    /// 80 桁端末のヘルプ。案内が落ちるかどうかはここで決まる。
    fn help_80(mode: Mode, display: DisplayMode) -> String {
        help_text(mode, display, false, 80)
    }

    #[test]
    fn input_help_mentions_the_editing_keys() {
        // 既存の案内で 80 桁が埋まっているので、編集キーは幅のある端末でだけ出る。
        let help = help_text(Mode::Input, DisplayMode::Embedded, false, 140);
        for key in [
            "Ctrl+A:全選択",
            "Shift+←→:選択",
            "Home/End:先頭/末尾",
            "クリック:カーソル",
        ] {
            assert!(help.contains(key), "{key} が無い: {help}");
        }
    }

    #[test]
    fn results_help_mentions_esc() {
        assert!(help_80(Mode::Results, DisplayMode::Embedded).contains("Esc"));
    }

    #[test]
    fn help_text_names_the_next_display_mode() {
        assert!(help_80(Mode::Playing, DisplayMode::Embedded).contains("w:テキスト"));
        assert!(help_80(Mode::Playing, DisplayMode::Text).contains("w:別ウィンドウ"));
        assert!(help_80(Mode::Playing, DisplayMode::Window).contains("w:埋め込み"));
    }

    #[test]
    fn window_placeholder_names_the_next_mode() {
        assert_eq!(
            window_placeholder(DisplayMode::Window),
            "別ウィンドウで再生中  w: 埋め込みへ"
        );
    }

    #[test]
    fn video_area_layout_is_unchanged_by_the_text_mode() {
        // 文字ブロックは映像と同じ矩形に描くので、割り付けはモードで変わらない。
        let area = Rect::new(0, 0, 80, 24);
        assert_eq!(video_area(area), Rect::new(0, 0, 80, 21));
        assert_eq!(seek_bar_area(area), Rect::new(0, 21, 80, 1));
    }

    #[test]
    fn playing_help_mentions_the_speed_keys() {
        // 80 桁では入らないので、広い端末での案内で見る。
        let help = help_text(Mode::Playing, DisplayMode::Embedded, false, 200);
        assert!(help.contains("[ ]"), "{help}");
        assert!(help.contains("BS"), "{help}");
        assert!(help.contains("速度"), "{help}");
    }

    #[test]
    fn video_area_layout_is_unchanged_by_the_display_mode() {
        // プレースホルダは映像と同じ矩形に描くので、割り付けはモードで変わらない。
        let area = Rect::new(0, 0, 80, 24);
        assert_eq!(video_area(area), Rect::new(0, 0, 80, 21));
        assert_eq!(seek_bar_area(area), Rect::new(0, 21, 80, 1));
    }

    #[test]
    fn input_help_mentions_feed_keywords() {
        // 4 つ並べると 83 桁で 80 桁端末に入らないため、":yt*" に畳んである。
        let help = help_80(Mode::Input, DisplayMode::Embedded);
        assert!(help.contains(":yt*"), "{help}");
        assert!(help.contains("Enter:検索"), "{help}");
        assert!(help.contains("Tab:カテゴリ"), "{help}");
        assert!(grid::display_width(&help) <= 80, "{help}");
    }

    #[test]
    fn results_help_mentions_the_grid_and_tab_keys() {
        let help = help_80(Mode::Results, DisplayMode::Embedded);
        for key in ["↑↓←→", "Tab:カテゴリ", "r:再取得", "Enter:再生"] {
            assert!(help.contains(key), "{key} がない: {help}");
        }
        assert!(grid::display_width(&help) <= 80, "{help}");
    }

    #[test]
    fn the_status_row_marks_where_it_was_cut() {
        let app = App {
            error: Some("あ".repeat(100)),
            ..App::default()
        };
        let line = status_text(&app, 80);
        // 全角は 2 桁なので、端に 1 桁余ることがある。
        assert!((79..=80).contains(&grid::display_width(&line)), "{line}");
        assert!(line.ends_with('…'), "{line}");
    }

    #[test]
    fn the_cookie_refusal_is_shown_whole_on_an_80_column_terminal() {
        // cookie 未設定でフィードのタブを選ぶと出る行。切れると設定先が読めない。
        for feed in crate::cookies::Feed::ALL {
            let app = App {
                error: Some(crate::cookies::CookieState::Off.refusal(feed)),
                ..App::default()
            };
            let line = status_text(&app, 80);
            assert!(line.contains(feed.label()), "{line}");
            // browser / file どちらの設定先も切れずに出る長さにする。
            assert!(line.contains("[cookies] browser"), "{line}");
            assert!(line.contains("file"), "{line}");
            assert!(!line.contains('…'), "{line}");
        }
    }

    #[test]
    fn search_areas_do_not_overlap_and_cover_the_screen() {
        let area = Rect::new(0, 0, 80, 24);
        let areas = search_areas(area);
        assert_eq!(areas[0].y, area.y);
        for pair in areas.windows(2) {
            assert_eq!(pair[0].bottom(), pair[1].y, "{pair:?}");
            assert_eq!(pair[0].width, area.width);
        }
        assert_eq!(areas[4].bottom(), area.bottom());
    }

    /// spans をつないだ行。幅と中身をまとめて見る。窓は先頭から開いた状態で始める。
    fn tab_row(labels: &[&str], selected: usize, width: usize) -> String {
        tab_row_from(labels, selected, width, 0).0
    }

    /// 前回の窓を渡す版。行と、次に持ち越す窓の開始位置を返す。
    fn tab_row_from(
        labels: &[&str],
        selected: usize,
        width: usize,
        start: usize,
    ) -> (String, usize) {
        let range = visible_tabs(labels, selected, width, start);
        let next = range.start;
        let row = tab_spans(labels, selected, width, range)
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        (row, next)
    }

    #[test]
    fn a_wide_terminal_shows_every_tab_without_markers() {
        let tabs = Tabs::default();
        let labels = tabs.labels();
        let row = tab_row(&labels, 0, 200);
        for label in &labels {
            assert!(row.contains(label), "{label} が落ちた: {row}");
        }
        assert!(!row.contains('<') && !row.contains('>'), "{row}");
    }

    #[test]
    fn the_tab_row_keeps_the_selected_tab_visible_inside_80_columns() {
        let tabs = Tabs::default();
        let labels = tabs.labels();
        for selected in 0..labels.len() {
            let row = tab_row(&labels, selected, 80);
            assert!(grid::display_width(&row) <= 80, "{selected}: {row}");
            assert!(
                row.contains(labels[selected]),
                "選択中の {} が出ていない: {row}",
                labels[selected]
            );
        }
    }

    #[test]
    fn the_tab_row_marks_the_side_that_is_scrolled_out() {
        let tabs = Tabs::default();
        let labels = tabs.labels();
        // 先頭を選んでいるので右側だけが隠れる。
        let head = tab_row(&labels, 0, 80);
        assert!(head.ends_with('>'), "{head}");
        assert!(!head.starts_with('<'), "{head}");

        // 末尾を選ぶと窓が送られ、左側が隠れる。
        let tail = tab_row(&labels, labels.len() - 1, 80);
        assert!(tail.starts_with('<'), "{tail}");
        assert!(!tail.ends_with('>'), "{tail}");
        assert!(!tail.contains("音楽"), "窓の外は出さない: {tail}");
    }

    #[test]
    fn moving_to_the_next_tab_keeps_the_row_still_while_it_fits() {
        let tabs = Tabs::default();
        let labels = tabs.labels();
        // 80 桁では 5 番目 (アニメ) と 6 番目 (スポーツ) が同じ窓に入る。
        let (row, start) = tab_row_from(&labels, 4, 80, 0);
        let (next, _) = tab_row_from(&labels, 5, 80, start);
        assert_eq!(row, next, "1 つ隣に移っただけでタブ行が動いている");
    }

    #[test]
    fn the_tab_row_scrolls_only_when_the_selection_leaves_the_window() {
        let tabs = Tabs::default();
        let labels = tabs.labels();
        let last = labels.len() - 1;
        let mut range = visible_tabs(&labels, 0, 80, 0);
        // Tab で一周し、BackTab で戻る。
        for selected in (1..=last).chain((0..last).rev()) {
            let next = visible_tabs(&labels, selected, 80, range.start);
            assert!(next.contains(&selected), "{selected} が窓の外: {next:?}");
            if next.start != range.start {
                assert!(
                    !range.contains(&selected),
                    "窓の中にいるのに動かした: {range:?} -> {next:?}"
                );
            }
            range = next;
        }
    }

    #[test]
    fn widening_the_terminal_brings_the_left_side_back() {
        let tabs = Tabs::default();
        let labels = tabs.labels();
        let last = labels.len() - 1;
        let (_, start) = tab_row_from(&labels, last, 80, 0);
        assert!(start > 0, "80 桁では左が隠れる");

        let (row, start) = tab_row_from(&labels, last, 200, start);
        assert_eq!(start, 0, "広げたら窓も戻す");
        assert!(row.starts_with("すべて"), "{row}");
    }

    #[test]
    fn the_tab_row_never_overflows_even_on_a_tiny_terminal() {
        let tabs = Tabs::default();
        let labels = tabs.labels();
        for width in 0..60 {
            for selected in 0..labels.len() {
                let row = tab_row(&labels, selected, width);
                assert!(
                    grid::display_width(&row) <= width,
                    "{width} 桁 / {selected}: {row}"
                );
            }
        }
    }

    #[test]
    fn a_single_tab_needs_no_window() {
        assert_eq!(tab_row(&["すべて"], 0, 80), "すべて");
        assert_eq!(tab_row(&[], 0, 80), "");
    }

    /// 窓を先頭から開いた状態での当たり判定。
    fn tab_hit(labels: &[&str], selected: usize, width: usize, column: usize) -> Option<usize> {
        let range = visible_tabs(labels, selected, width, 0);
        tab_at_column(labels, width, range, column)
    }

    #[test]
    fn clicking_a_tab_label_selects_that_tab() {
        let tabs = Tabs::default();
        let labels = tabs.labels();
        // 「すべて」は 6 桁、区切りが 3 桁、「音楽」が 4 桁と並ぶ。
        assert_eq!(tab_hit(&labels, 0, 200, 0), Some(0));
        assert_eq!(tab_hit(&labels, 0, 200, 5), Some(0));
        assert_eq!(tab_hit(&labels, 0, 200, 9), Some(1));
        assert_eq!(tab_hit(&labels, 0, 200, 12), Some(1));
        assert_eq!(tab_hit(&labels, 0, 200, 16), Some(2));
    }

    #[test]
    fn clicking_a_separator_or_the_empty_tail_selects_nothing() {
        let tabs = Tabs::default();
        let labels = tabs.labels();
        for column in 6..9 {
            assert_eq!(tab_hit(&labels, 0, 200, column), None, "{column} は区切り");
        }
        let row = tab_row(&labels, 0, 200);
        let end = grid::display_width(&row);
        assert_eq!(tab_hit(&labels, 0, 200, end), None, "行の右の余白");
        assert_eq!(tab_hit(&labels, 0, 200, 199), None);
    }

    #[test]
    fn clicking_a_scroll_marker_does_not_select_a_tab() {
        let tabs = Tabs::default();
        let labels = tabs.labels();
        let last = labels.len() - 1;

        // 先頭を選んでいるので行末に " >" が出る。
        let head = tab_row(&labels, 0, 80);
        assert!(head.ends_with('>'), "{head}");
        let end = grid::display_width(&head);
        assert_eq!(tab_hit(&labels, 0, 80, end - 1), None);
        assert_eq!(tab_hit(&labels, 0, 80, end - 2), None);

        // 末尾を選ぶと窓が送られ、行頭に "< " が出る。
        let range = visible_tabs(&labels, last, 80, 0);
        assert!(range.start > 0, "80 桁では左が隠れる");
        assert_eq!(tab_at_column(&labels, 80, range.clone(), 0), None);
        assert_eq!(tab_at_column(&labels, 80, range, 1), None);
    }

    #[test]
    fn the_hit_test_agrees_with_the_drawn_row() {
        let tabs = Tabs::default();
        let labels = tabs.labels();
        let dim = Style::default().fg(Color::DarkGray);
        for width in [0usize, 1, 3, 5, 20, 40, 80, 120, 200] {
            for selected in 0..labels.len() {
                let range = visible_tabs(&labels, selected, width, 0);
                // 描いた行を左から辿り、各桁がどのタブの上かを並べる。
                // 区切りとマーカーだけが dim なので、そこでラベルと見分けられる。
                let mut columns: Vec<Option<usize>> = Vec::new();
                let mut index = range.start;
                for span in tab_spans(&labels, selected, width, range.clone()) {
                    let label = span.style != dim;
                    let cells = grid::display_width(span.content.as_ref());
                    columns.extend(std::iter::repeat_n(label.then_some(index), cells));
                    if label {
                        index += 1;
                    }
                }
                for column in 0..width + 2 {
                    assert_eq!(
                        tab_at_column(&labels, width, range.clone(), column),
                        columns.get(column).copied().flatten(),
                        "{width} 桁 / 選択 {selected} / {column} 桁目"
                    );
                }
            }
        }
    }

    #[test]
    fn only_the_tab_row_answers_the_hit_test() {
        let app = App {
            screen: Rect::new(0, 0, 80, 24),
            ..App::default()
        };
        assert_eq!(
            tab_at_point(&app, 0, 3),
            Some(0),
            "タブ行の先頭は「すべて」"
        );
        for row in [0u16, 1, 2, 4, 12, 23] {
            assert_eq!(tab_at_point(&app, 0, row), None, "{row} 行目はタブ行でない");
        }
    }

    #[test]
    fn the_hit_test_follows_the_window_that_was_drawn() {
        let tabs = Tabs::default();
        let labels = tabs.labels();
        let last = labels.len() - 1;
        let range = visible_tabs(&labels, last, 80, 0);
        assert!(range.start > 0, "80 桁では左が隠れる");

        let mut app = App {
            screen: Rect::new(0, 0, 80, 24),
            ..App::default()
        };
        app.tabs.select(last);
        app.tabs.remember_window(range.start);

        // 窓の外のタブは行に出ていないので、どの桁を押しても選べない。
        for column in 0..80u16 {
            let hit = tab_at_point(&app, column, 3);
            assert!(
                hit.is_none_or(|index| range.contains(&index)),
                "{column} 桁目で窓の外の {hit:?} を返した"
            );
        }
        // 行頭は "< " なので、窓の先頭のタブはその次から。
        assert_eq!(tab_at_point(&app, 2, 3), Some(range.start));
    }

    #[test]
    fn a_terminal_too_short_for_the_tab_row_answers_only_where_it_is_drawn() {
        // 全段が入らない高さでは割り付けが潰れ、タブ行が y=3 に来るとは限らない。
        // どこへ潰れても、当たり判定は描いた行の中だけで応じる。
        for height in 0..8u16 {
            let app = App {
                screen: Rect::new(0, 0, 80, height),
                ..App::default()
            };
            let row_area = search_areas(app.screen)[1];
            for row in 0..8u16 {
                let drawn = row >= row_area.y && row < row_area.bottom();
                let hit = tab_at_point(&app, 0, row);
                assert_eq!(
                    hit.is_some(),
                    drawn,
                    "{height} 行 / {row} 行目 (タブ行 {row_area:?}): {hit:?}"
                );
            }
        }
    }

    /// 割り付けを端末の申告で揺らさないための寸法。
    const CELL: CellSize = CellSize {
        width_px: 8,
        height_px: 16,
    };

    /// 80x24 の検索画面。CELL なら格子は 4 列 2 行になる。
    fn grid_app(count: usize) -> App {
        App {
            mode: Mode::Results,
            screen: Rect::new(0, 0, 80, 24),
            results: (0..count).map(result).collect(),
            ..App::default()
        }
    }

    /// 80x24 のチャンネル画面。現在タブに `count` 件持たせる。
    fn channel_app(count: usize) -> App {
        let mut view = ChannelView::new("UCabc".to_string(), "Some Channel".to_string());
        view.state_mut().results = (0..count).map(result).collect();
        view.state_mut().loaded = true;
        App {
            mode: Mode::Channel,
            screen: Rect::new(0, 0, 80, 24),
            // 検索結果は残したまま、画面はチャンネルを見ている。
            results: vec![result(99)],
            channel: Some(view),
            ..App::default()
        }
    }

    /// 画面の `row` 行目に描かれている文字。全角の右半分のセルは空白なので飛ばす。
    fn drawn_row(app: &App, row: u16) -> String {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).expect("端末");
        terminal.draw(|frame| draw(frame, app)).expect("描ける");
        let buffer = terminal.backend().buffer().clone();
        let mut out = String::new();
        let mut skip = false;
        for x in 0..buffer.area.width {
            if std::mem::take(&mut skip) {
                continue;
            }
            let symbol = buffer[(x, row)].symbol();
            skip = grid::display_width(symbol) == 2;
            out.push_str(symbol);
        }
        out.trim_end().to_string()
    }

    #[test]
    fn the_channel_screen_shows_the_channel_tabs() {
        let app = channel_app(2);
        let row = drawn_row(&app, 3);
        for label in ChannelView::labels() {
            assert!(row.contains(label), "{label} が無い: {row}");
        }
        assert!(!row.contains("すべて"), "カテゴリタブは出さない: {row}");
    }

    #[test]
    fn the_channel_screen_draws_the_channel_results_not_the_search_results() {
        let app = channel_app(2);
        let layout = grid_layout(&app, CELL).expect("格子を組める");
        assert_eq!(layout.cells.len(), 2, "チャンネルの件数で組む");

        let title = drawn_row(&app, layout.cells[0].title.y);
        assert!(title.contains("title 0"), "{title}");
        assert!(!title.contains("title 99"), "{title}");
    }

    #[test]
    fn the_channel_grid_hit_test_uses_the_channel_results() {
        let app = channel_app(10);
        let layout = grid_layout(&app, CELL).expect("格子を組める");
        assert_eq!(layout.cells.len(), 8, "4 列 2 行");
        let first = layout.cells[0].image;
        assert_eq!(result_at_point(&app, CELL, first.x, first.y), Some(0));

        // 検索結果は 1 件しかないので、参照先を取り違えるとここが None になる。
        let last = layout.cells[7].image;
        assert_eq!(result_at_point(&app, CELL, last.x, last.y), Some(7));
    }

    #[test]
    fn a_click_on_the_channel_tab_row_answers_with_the_channel_tab() {
        let app = channel_app(2);
        let area = search_areas(app.screen)[1];
        assert_eq!(tab_at_point(&app, area.x, area.y), Some(0));
        // 「動画」(4 桁) と区切りの後ろは「ショート」。
        assert_eq!(tab_at_point(&app, area.x + 8, area.y), Some(1));
    }

    #[test]
    fn the_channel_help_names_the_way_back() {
        let help = help_text(Mode::Channel, DisplayMode::Embedded, false, 80);
        for key in ["Enter:再生", "Tab:", "Esc", "q:終了"] {
            assert!(help.contains(key), "{key} が無い: {help}");
        }
    }

    #[test]
    fn the_channel_help_names_the_settings_and_the_reload() {
        // 80 桁端末で出ないと、設定も取り直しも使えることに気づけない。
        let help = help_80(Mode::Channel, DisplayMode::Embedded);
        for key in ["r:再取得", "S:設定"] {
            assert!(help.contains(key), "{key} が無い: {help}");
        }
    }

    #[test]
    fn the_results_help_names_the_hide_key() {
        // 非表示リストを見る画面が無いので、案内に出ないと h に気づけない。
        // h を足すぶん言葉を削ってあるので、他の案内が落ちていないことまで見る。
        let help = help_80(Mode::Results, DisplayMode::Embedded);
        for key in ["h:隠す", "Esc:検索", "q:終了", "S:設定"] {
            assert!(help.contains(key), "{key} が無い: {help}");
        }
        assert!(grid::display_width(&help) <= 80, "{help}");
    }

    #[test]
    fn the_channel_help_names_the_hide_and_subscribe_keys() {
        let help = help_80(Mode::Channel, DisplayMode::Embedded);
        for key in ["s:登録", "h:隠す"] {
            assert!(help.contains(key), "{key} が無い: {help}");
        }
        assert!(grid::display_width(&help) <= 80, "{help}");
    }

    #[test]
    fn the_results_help_names_the_channel_key() {
        // 80 桁端末で出ないと、チャンネルへ移れること自体に気づけない。
        assert!(help_80(Mode::Results, DisplayMode::Embedded).contains("c:チャンネル"));
    }

    /// 矩形の左上と右下。両端が同じセルを指すことを確かめるための 2 点。
    fn corners(rect: Rect) -> [(u16, u16); 2] {
        [(rect.x, rect.y), (rect.right() - 1, rect.bottom() - 1)]
    }

    #[test]
    fn clicking_a_cell_answers_with_the_result_behind_it() {
        let app = grid_app(10);
        let layout = grid_layout(&app, CELL).expect("格子を組める");
        assert_eq!(layout.cells.len(), 8, "4 列 2 行");

        for (i, cell) in layout.cells.iter().enumerate() {
            // 画像・タイトル・時間の行はどれも同じ結果を指す。
            for rect in [cell.image, cell.title, cell.meta] {
                for (column, row) in corners(rect) {
                    assert_eq!(
                        result_at_point(&app, CELL, column, row),
                        Some(layout.offset + i),
                        "{rect:?} の ({column},{row})"
                    );
                }
            }
        }
    }

    #[test]
    fn the_hit_test_agrees_with_the_drawn_cells() {
        let app = grid_app(10);
        let layout = grid_layout(&app, CELL).expect("格子を組める");
        for row in 0..app.screen.height {
            for column in 0..app.screen.width {
                let at = Position::new(column, row);
                let drawn = layout.cells.iter().position(|cell| {
                    cell.image.contains(at) || cell.title.contains(at) || cell.meta.contains(at)
                });
                assert_eq!(
                    result_at_point(&app, CELL, column, row),
                    drawn.map(|i| layout.offset + i),
                    "({column},{row})"
                );
            }
        }
    }

    #[test]
    fn the_margins_around_a_cell_answer_nothing() {
        let app = grid_app(10);
        let layout = grid_layout(&app, CELL).expect("格子を組める");
        let first = layout.cells[0];
        let inner = results_inner(app.screen);

        // セルの右に空けた余白。
        assert_eq!(
            result_at_point(&app, CELL, first.image.right(), first.image.y),
            None
        );
        // 時間の行の下に積んだ下余白。
        assert_eq!(
            result_at_point(&app, CELL, first.meta.x, first.meta.bottom()),
            None
        );
        // 結果ブロックの枠。
        assert_eq!(result_at_point(&app, CELL, inner.x - 1, inner.y), None);
        assert_eq!(result_at_point(&app, CELL, inner.x, inner.y - 1), None);
    }

    #[test]
    fn the_list_view_has_no_clickable_cells() {
        let mut app = grid_app(10);
        let first = grid_layout(&app, CELL).expect("格子を組める").cells[0].image;
        assert_eq!(result_at_point(&app, CELL, first.x, first.y), Some(0));

        app.settings.search.layout = LayoutMode::List;
        for row in 0..app.screen.height {
            for column in 0..app.screen.width {
                assert_eq!(
                    result_at_point(&app, CELL, column, row),
                    None,
                    "({column},{row})"
                );
            }
        }
    }

    #[test]
    fn a_terminal_too_small_for_a_grid_has_no_clickable_cells() {
        let mut app = grid_app(10);
        app.screen = Rect::new(0, 0, 80, 10);
        assert!(grid_layout(&app, CELL).is_none(), "格子を組めない");

        for row in 0..app.screen.height {
            for column in 0..app.screen.width {
                assert_eq!(
                    result_at_point(&app, CELL, column, row),
                    None,
                    "({column},{row})"
                );
            }
        }
    }

    #[test]
    fn an_empty_result_list_has_no_clickable_cells() {
        let app = grid_app(0);
        for row in 0..app.screen.height {
            for column in 0..app.screen.width {
                assert_eq!(
                    result_at_point(&app, CELL, column, row),
                    None,
                    "({column},{row})"
                );
            }
        }
    }

    #[test]
    fn a_scrolled_grid_answers_with_the_scrolled_index() {
        let mut app = grid_app(20);
        app.scroll = 4;
        let layout = grid_layout(&app, CELL).expect("格子を組める");
        assert_eq!(layout.offset, 4);

        let first = layout.cells[0].image;
        assert_eq!(result_at_point(&app, CELL, first.x, first.y), Some(4));
    }

    #[test]
    fn the_last_page_has_nothing_past_the_last_result() {
        let mut app = grid_app(10);
        app.scroll = 8;
        let layout = grid_layout(&app, CELL).expect("格子を組める");
        assert_eq!(layout.cells.len(), 2, "残りは 2 件");

        let image = layout.cells[0].image;
        assert_eq!(result_at_point(&app, CELL, image.x, image.y), Some(8));
        // 3 つめが来ていたはずの桁 (セル 1 つぶん右) には何も無い。
        let pitch = layout.cells[1].image.x - image.x;
        assert_eq!(
            result_at_point(&app, CELL, image.x + 2 * pitch, image.y),
            None
        );
    }

    #[test]
    fn a_cell_without_a_thumbnail_is_still_clickable() {
        let app = grid_app(10);
        assert!(app.thumbs.get("id0").is_none(), "画像はまだ届いていない");

        let first = grid_layout(&app, CELL).expect("格子を組める").cells[0];
        assert_eq!(
            result_at_point(&app, CELL, first.image.x, first.image.y),
            Some(0)
        );
    }

    #[test]
    fn tab_row_sits_between_the_input_box_and_the_results() {
        let areas = search_areas(Rect::new(0, 0, 80, 24));
        assert_eq!(areas[0], Rect::new(0, 0, 80, 3), "入力ボックスは 3 行");
        assert_eq!(areas[1], Rect::new(0, 3, 80, 1), "タブは 1 行");
        assert_eq!(areas[2].y, 4, "結果はタブの下");
    }

    #[test]
    fn results_area_shrinks_by_one_row_compared_to_the_current_layout() {
        // タブ行が1行増えたぶんだけ結果が狭くなる。ステータス・ヘルプは動かさない。
        let areas = search_areas(Rect::new(0, 0, 80, 24));
        assert_eq!(areas[2], Rect::new(0, 4, 80, 18));
        assert_eq!(areas[3], Rect::new(0, 22, 80, 1));
        assert_eq!(areas[4], Rect::new(0, 23, 80, 1));
        // 結果ブロックの内側は枠のぶんさらに狭い。
        assert_eq!(
            results_inner(Rect::new(0, 0, 80, 24)),
            Rect::new(1, 5, 78, 16)
        );
    }

    #[test]
    fn search_areas_survive_a_terminal_too_short_for_every_row() {
        for height in 0..8 {
            let area = Rect::new(0, 0, 40, height);
            let areas = search_areas(area);
            for rect in areas {
                assert!(rect.bottom() <= area.bottom(), "{rect:?} / {area:?}");
            }
            // 内側を取っても矩形として成立する。
            let inner = results_inner(area);
            assert!(inner.bottom() <= area.bottom(), "{inner:?} / {area:?}");
        }
    }

    #[test]
    fn grid_layout_follows_the_configured_mode() {
        let mut app = App {
            screen: Rect::new(0, 0, 80, 24),
            results: (0..10).map(result).collect(),
            ..App::default()
        };
        let cell = CellSize {
            width_px: 8,
            height_px: 16,
        };
        let layout = grid_layout(&app, cell).expect("格子を組める");
        assert_eq!((layout.columns, layout.rows), (4, 2));
        assert_eq!(layout.image_px, (144, 80));

        app.settings.search.layout = LayoutMode::List;
        assert!(grid_layout(&app, cell).is_none(), "list ではリスト表示");

        // 狭い端末では設定が grid でもリスト表示へ落ちる。
        app.settings.search.layout = LayoutMode::Grid;
        app.screen = Rect::new(0, 0, 80, 10);
        assert!(grid_layout(&app, cell).is_none());
    }

    #[test]
    fn the_results_title_shows_the_visible_range() {
        assert_eq!(results_title(0, 8, 10), " 結果 1-8/10 ");
        assert_eq!(results_title(8, 2, 10), " 結果 9-10/10 ");
        assert_eq!(results_title(0, 0, 0), " 結果 ");
    }

    #[test]
    fn playing_rows_do_not_overlap_the_video() {
        let area = Rect::new(0, 0, 80, 24);
        assert_eq!(video_area(area), Rect::new(0, 0, 80, 21));
        assert_eq!(seek_bar_area(area), Rect::new(0, 21, 80, 1));
        assert_eq!(status_area(area), Rect::new(0, 22, 80, 1));
        assert_eq!(help_area(area), Rect::new(0, 23, 80, 1));
    }

    #[test]
    fn playing_help_keeps_the_main_keys_inside_80_columns() {
        for display in [
            DisplayMode::Embedded,
            DisplayMode::Text,
            DisplayMode::Window,
        ] {
            let help = help_80(Mode::Playing, display);
            let width = grid::display_width(&help);
            assert!(width <= 80, "{width} 桁: {help}");
            // 切れた案内を出さない代わりに、押せないと困るキーは必ず入れる。
            for key in [
                "space:一時停止",
                "c:URLコピー",
                &format!("w:{}", display.next().label()),
                "Esc:停止",
                "q:終了",
            ] {
                assert!(help.contains(key), "{key} が落ちた: {help}");
            }
        }
    }

    #[test]
    fn playing_help_drops_whole_hints_when_the_terminal_is_narrow() {
        let wide = help_text(Mode::Playing, DisplayMode::Embedded, false, 200);
        assert!(wide.contains("クリック:シーク"), "{wide}");

        let narrow = help_text(Mode::Playing, DisplayMode::Embedded, false, 30);
        assert_eq!(narrow, "space:一時停止 ←→:シーク");
        assert_eq!(
            help_text(Mode::Playing, DisplayMode::Embedded, false, 0),
            ""
        );
    }

    #[test]
    fn playing_help_mentions_the_subtitle_key() {
        // 80 桁では主要キーが先で入らないので、広い端末での案内で見る。
        let wide = help_text(Mode::Playing, DisplayMode::Embedded, false, 200);
        assert!(wide.contains("s:字幕"), "{wide}");
        // 幅に入らないぶんは丸ごと落ちる。途中で切れた案内は出さない。
        let narrow = help_80(Mode::Playing, DisplayMode::Text);
        assert!(grid::display_width(&narrow) <= 80, "{narrow}");
        assert!(!narrow.contains("s:字"), "{narrow}");
    }

    /// TestBackend に 1 フレーム描いて、画面の文字だけを行ごとに取り出す。
    fn rendered(app: &App, width: u16, height: u16) -> String {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height))
                .expect("端末");
        terminal.draw(|frame| draw(frame, app)).expect("描ける");
        let buffer = terminal.backend().buffer().clone();
        (0..buffer.area.height)
            .map(|y| {
                let mut line = String::new();
                // 全角文字は 2 セルを占め、後ろのセルは埋め草なので読み飛ばす。
                let mut skip = 0;
                for x in 0..buffer.area.width {
                    if skip > 0 {
                        skip -= 1;
                        continue;
                    }
                    let symbol = buffer[(x, y)].symbol();
                    skip = grid::display_width(symbol).saturating_sub(1);
                    line.push_str(symbol);
                }
                line
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn settings_rows_do_not_overlap_and_cover_the_screen() {
        let area = Rect::new(0, 0, 80, 24);
        let areas = settings_areas(area);
        assert_eq!(areas[0], Rect::new(0, 0, 80, 1), "タイトルは 1 行");
        assert_eq!(areas[1], Rect::new(0, 1, 80, 21), "項目に残り全部");
        assert_eq!(areas[2], Rect::new(0, 22, 80, 1));
        assert_eq!(areas[3], Rect::new(0, 23, 80, 1));
        for pair in areas.windows(2) {
            assert_eq!(pair[0].bottom(), pair[1].y, "{pair:?}");
            assert_eq!(pair[0].width, area.width);
        }
    }

    #[test]
    fn settings_areas_survive_a_terminal_too_short_for_every_row() {
        for height in 0..6 {
            let area = Rect::new(0, 0, 40, height);
            for rect in settings_areas(area) {
                assert!(rect.bottom() <= area.bottom(), "{rect:?} / {area:?}");
            }
        }
    }

    #[test]
    fn settings_help_lists_the_keys_inside_80_columns() {
        let help = help_80(Mode::Settings, DisplayMode::Embedded);
        for key in [
            "↑↓:選択",
            "←→:値変更",
            "Enter/Space:切替",
            "s:保存",
            // 閉じるだけでなく編集を捨てることが分かる文言にする。
            "Esc:破棄して戻る",
        ] {
            assert!(help.contains(key), "{key} が落ちた: {help}");
        }
        assert!(grid::display_width(&help) <= 80, "{help}");
        // 狭い端末では途中で切らず丸ごと落とす。
        assert_eq!(
            help_text(Mode::Settings, DisplayMode::Embedded, false, 0),
            ""
        );
    }

    #[test]
    fn a_narrow_settings_help_drops_the_direct_input_before_the_way_out() {
        // 直接入力を足す前の 5 つは 58 桁に収まる。足りないぶんは末尾から落とす。
        let narrow = help_text(Mode::Settings, DisplayMode::Embedded, false, 58);
        for key in ["↑↓:選択", "←→:値変更", "Enter/Space:切替", "s:保存"] {
            assert!(narrow.contains(key), "{key} が落ちた: {narrow}");
        }
        assert!(narrow.contains("Esc:破棄して戻る"), "{narrow}");
        assert!(!narrow.contains("0-9"), "{narrow}");
        assert!(grid::display_width(&narrow) <= 58, "{narrow}");
    }

    #[test]
    fn the_settings_screen_draws_every_row_and_marks_the_selection() {
        let mut app = App {
            mode: Mode::Settings,
            ..App::default()
        };
        app.settings_selected = 2;
        let screen = rendered(&app, 80, 24);

        for row in app.settings_rows() {
            assert!(screen.contains(&row), "{row} がない:\n{screen}");
        }
        assert!(
            screen.contains("> fps_cap"),
            "選択中の行に印が出る:\n{screen}"
        );
        assert!(screen.contains("設定"), "タイトルが出る:\n{screen}");
        assert!(screen.contains("s:保存"), "ヘルプが出る:\n{screen}");
    }

    /// search.limit の行を打ち込み中にした設定画面。
    fn typing_app(raw: &str) -> App {
        let mut app = App {
            mode: Mode::Settings,
            ..App::default()
        };
        app.settings_selected = 5;
        app.settings_edit = Some(raw.to_string());
        app
    }

    #[test]
    fn the_row_being_typed_shows_the_digits_instead_of_the_stored_value() {
        let app = typing_app("12");
        let screen = rendered(&app, 80, 24);

        assert!(screen.contains("search.limit: 12"), "{screen}");
        assert!(!screen.contains("search.limit: 10"), "{screen}");
        // 打ち込んでいない行はそのまま。
        assert!(screen.contains("fps_cap: 15"), "{screen}");
        assert!(screen.contains("Enter:確定"), "案内も切り替わる:\n{screen}");
    }

    #[test]
    fn the_row_being_typed_keeps_the_note_about_the_environment() {
        // 環境変数が効いている断りは、値を打ち込んでいる間も消さない。
        let mut app = App {
            mode: Mode::Settings,
            env_overridden: crate::settings::EnvOverridden {
                fps_cap: Some(crate::display::FpsCap::new(30)),
                ..crate::settings::EnvOverridden::default()
            },
            ..App::default()
        };
        app.settings_selected = 2;
        app.settings_edit = Some("24".to_string());
        let screen = rendered(&app, 120, 24);

        assert!(screen.contains("fps_cap: 24"), "{screen}");
        assert!(screen.contains(crate::settings::FPS_LIMIT_VAR), "{screen}");
        assert!(screen.contains("保存しません"), "{screen}");
    }

    #[test]
    fn the_title_stops_promising_the_save_and_close_keys_while_typing() {
        // 打ち込み中は s も Esc も画面を閉じないので、タイトルにも出さない。
        let app = App {
            mode: Mode::Settings,
            ..App::default()
        };
        assert!(rendered(&app, 80, 24).contains("Esc は編集を捨てて戻る"));

        let typing = rendered(&typing_app("12"), 80, 24);
        assert!(!typing.contains("Esc は編集を捨てて戻る"), "{typing}");
        assert!(typing.contains("打ち込み中"), "{typing}");
    }

    #[test]
    fn an_emptied_row_shows_no_value_while_it_is_being_typed() {
        let app = typing_app("");
        let screen = rendered(&app, 80, 24);

        assert!(screen.contains("search.limit:"), "{screen}");
        assert!(!screen.contains("search.limit: 10"), "{screen}");
    }

    #[test]
    fn the_cursor_follows_the_digits_on_the_row_being_typed() {
        // "> search.limit: 12" の後ろ。行は項目欄の 6 行目。
        assert_eq!(
            settings_cursor(Rect::new(0, 0, 80, 24), 5, "search.limit", "12"),
            (18, 6)
        );
        // 幅も高さも足りない端末でも画面の中に収める。
        let screen = Rect::new(0, 0, 20, 5);
        let (x, y) = settings_cursor(screen, 8, "thumbnails.max_cached", "123");
        assert!(x < screen.width, "{x}");
        assert!(y < screen.height, "{y}");
    }

    #[test]
    fn the_settings_help_changes_while_a_number_is_typed() {
        let app = App {
            mode: Mode::Settings,
            ..App::default()
        };
        let normal = help_line(&app, 80);
        assert!(normal.contains("0-9:直接入力"), "{normal}");
        assert!(normal.contains("s:保存"), "{normal}");
        assert!(grid::display_width(&normal) <= 80, "{normal}");

        let typing = help_line(&typing_app("12"), 80);
        for hint in ["0-9:入力", "BS:1字削除", "Enter:確定", "Esc:取消"] {
            assert!(typing.contains(hint), "{hint} がない: {typing}");
        }
        assert!(!typing.contains("s:保存"), "保存は効かない: {typing}");
        assert!(grid::display_width(&typing) <= 80, "{typing}");
    }

    #[test]
    fn the_settings_screen_does_not_draw_the_search_boxes() {
        // 検索画面の上に重ねず、設定だけの画面にする。
        let app = App {
            mode: Mode::Settings,
            query: QueryEditor::from("ラーメン"),
            ..App::default()
        };
        let screen = rendered(&app, 80, 24);
        assert!(!screen.contains("ラーメン"), "{screen}");
        assert!(!screen.contains("結果"), "{screen}");
    }

    /// コメントを取り終えた再生画面。
    fn commented_app(list: Vec<crate::comments::Comment>) -> App {
        let mut app = App {
            mode: Mode::Playing,
            display: DisplayMode::Embedded,
            screen: Rect::new(0, 0, 80, 24),
            ..App::default()
        };
        app.comments.begin("abc".to_string());
        app.comments.apply("abc", Ok(list));
        app
    }

    fn comment(author: &str, text: &str) -> crate::comments::Comment {
        crate::comments::Comment {
            author: author.to_string(),
            text: text.to_string(),
            like_count: Some(3),
        }
    }

    #[test]
    fn the_comment_list_takes_over_the_video_area() {
        let mut app = commented_app(vec![comment("alice", "おもしろい")]);
        app.comments.toggle();
        let screen = rendered(&app, 80, 24);

        assert!(screen.contains("alice (+3)"), "{screen}");
        assert!(screen.contains("おもしろい"), "{screen}");
        // 映像領域を置き換えるだけで、下 3 段はそのまま。
        assert!(screen.contains("space:一時停止"), "{screen}");
    }

    #[test]
    fn a_video_with_comments_disabled_says_so_instead_of_looking_broken() {
        let mut app = commented_app(Vec::new());
        app.comments.toggle();
        assert!(
            rendered(&app, 80, 24).contains(crate::comments::NO_COMMENTS),
            "コメント無効の動画はエラーにしない"
        );
    }

    #[test]
    fn the_video_comes_back_when_the_comment_list_is_closed() {
        let mut app = commented_app(vec![comment("alice", "おもしろい")]);
        app.display = DisplayMode::Window;
        app.comments.toggle();
        assert!(rendered(&app, 80, 24).contains("alice"));

        app.comments.toggle();
        let screen = rendered(&app, 80, 24);
        assert!(!screen.contains("alice"), "{screen}");
        assert!(screen.contains("別ウィンドウで再生中"), "{screen}");
    }

    #[test]
    fn playing_help_mentions_the_comment_key() {
        // 80 桁では主要キーが先で入らないので、広い端末での案内で見る。
        let wide = help_text(Mode::Playing, DisplayMode::Embedded, false, 200);
        assert!(wide.contains("o:コメント"), "{wide}");
    }

    #[test]
    fn the_arrow_hint_switches_to_the_comment_list_while_it_is_open() {
        let open = help_text(Mode::Playing, DisplayMode::Embedded, true, 200);
        assert!(open.contains("↑↓:行送り"), "{open}");
        assert!(!open.contains("↑↓:音量"), "{open}");

        let closed = help_text(Mode::Playing, DisplayMode::Embedded, false, 200);
        assert!(closed.contains("↑↓:音量"), "{closed}");
    }

    /// 1 件 2 行なので、80x24 の枠 (内側 19 行) には 9 件と少ししか入らない。
    fn many_comments(count: usize) -> App {
        commented_app(
            (0..count)
                .map(|i| comment(&format!("author{i}"), &format!("本文{i}")))
                .collect(),
        )
    }

    #[test]
    fn the_comment_list_scrolls_to_the_entries_that_do_not_fit() {
        let mut app = many_comments(comments::COMMENT_LIMIT);
        app.comments.toggle();
        let top = rendered(&app, 80, 24);
        assert!(top.contains("author0"), "{top}");
        assert!(!top.contains("author20"), "{top}");

        let view = comments_viewport(app.screen);
        let lines = comments::display_lines(app.comments.state(), view.width as usize).len();
        app.comments.scroll_by(40, lines, view.height as usize);
        let scrolled = rendered(&app, 80, 24);
        assert!(scrolled.contains("author20"), "{scrolled}");
        assert!(!scrolled.contains("author0 "), "{scrolled}");

        // 最後の 1 件までは送れる。
        app.comments
            .scroll_by(lines as isize, lines, view.height as usize);
        let bottom = rendered(&app, 80, 24);
        assert!(
            bottom.contains(&format!("author{}", comments::COMMENT_LIMIT - 1)),
            "{bottom}"
        );
    }

    #[test]
    fn the_comment_viewport_is_the_inside_of_the_frame() {
        let screen = Rect::new(0, 0, 80, 24);
        let view = comments_viewport(screen);
        assert_eq!(view, Rect::new(1, 1, 78, 19));
    }

    #[test]
    fn playing_help_mentions_the_mouse() {
        // マウスの案内は幅が余ったときだけ出す。
        assert!(help_text(Mode::Playing, DisplayMode::Embedded, false, 200).contains("クリック"));
        assert!(help_80(Mode::Playing, DisplayMode::Embedded).contains("シーク"));
    }
}
