# 一覧/グリッドでの動画/ショート/ライブの見分け(#70)

検索結果・グリッド表示上で、動画がライブ配信か、いま見ているタブがショートのタブかを一目で分かるようにする。

## 前提: 判定できる範囲(実機で確認済み)

- **ライブ**: yt-dlp の `--flat-playlist`(既存のtuitubeの検索方式)でも `is_live` フィールドが正しく返る。通常検索・フィード・プレイリスト・チャンネルの全タブで行ごとに判定できる。
- **ショート**: 既存の検索方式(`ytsearchN:<query>`)では、ショート動画でも `url` が通常の `watch?v=` のままで、行ごとに「これはショートか」を判定する情報が無い。チャンネルの Shorts タブ(`https://www.youtube.com/@channel/shorts`)経由でだけ `url` が `https://www.youtube.com/shorts/<id>` になり、判定できる。
  - 通常検索でもショート判定を可能にする手段(`https://www.youtube.com/results?search_query=` へ検索方式を変える)は実機で確認できたが、既存の検索の核心部分(ページネーション・キャッシュ・チャンネル行の混在)への影響が大きいため、今回は見送る(v1対象外)。
- 結論: **ライブは行ごとに判定(全画面共通)、ショートはタブ単位で判定(チャンネルのShortsタブのみ)**。

## データモデル

`SearchResult`(`src/search.rs`)に追加:

```rust
pub struct SearchResult {
    // ...既存フィールド
    pub is_live: bool, // JSON の is_live をそのまま読む。無ければ false。
}
```

ショートは `SearchResult` にフィールドを持たせない。「今どのタブを見ているか」という画面全体の状態(`app.channel.as_ref().map(|c| c.tab) == Some(ChannelTab::Shorts)`)で決まるため。

## 見た目

既存の #66(`badge.rs`)と同じ2系統(grid=Kitty画像の重ね描き、list=テキスト接頭辞)にもう1種類ずつ追加する。

- ライブ: 赤系、記号は `●LIVE`(既存の `Badge::symbol()` と同じ短い文字)
- ショート(タブ単位): 記号は `S`

`badge.rs` の `Badge` enum に `Live` を追加(色は赤、`♥`(いいね)とは別の見た目にするため形は変える)。ショートは行ごとの `Badge` ではなく、Shorts タブを見ている間だけ描画側で無条件に付ける別処理にする(`badges_for` の対象外)。

- グリッド: 各セルの角に印を送る(#66 と同じ `badge::encode` の仕組み)。ショートタブを見ている間は、印の位置がいいね/登録と被らないよう反対側の角(左上 vs 右上等)に置く。
- リスト: `draw_list` のタイトル接頭辞に、いいね/登録の記号と並べて追加する。

## 実装箇所

- `src/search.rs`: `SearchResult.is_live` 追加、パース関数に `is_live` の読み取りを追加
- `src/badge.rs`: `Badge::Live` 追加(色・記号)。`badges_for` に `result.is_live` の判定を追加
- `src/ui.rs`: `draw_grid`/`draw_list` で、Shorts タブを見ている間は行ごとに関係なくショートの印を追加で出す(既存の `badges_for` の結果に混ぜず、呼び出し側で別に足す)
- `src/main.rs`: `present_thumbs` でショートの印をサムネイルの反対側の角へ送る。ライブ・ショートは OAuth の設定(`engagement.enabled`)では消さない

## 対象外(v1)

- 通常検索・フィード・プレイリスト内動画のショート判定(検索方式の変更が必要なため見送り)
- ライブの視聴者数・経過時間などの追加情報表示(判定できるかどうかのみ)
- 配信予定(`is_upcoming`)の表示
