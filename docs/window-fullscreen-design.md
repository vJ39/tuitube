# 別ウィンドウ再生の全画面設定

`[window]`セクションに`fullscreen`(bool)を追加する。既存の`ontop`と全く同じ扱いにする。

## 設定

```toml
[window]
# fullscreen = false
```

既定`false`。既存の`ontop`と同じく起動時の`WindowOptions::args()`にのみ反映し、再生中`w`キーでの別ウィンドウ切替(`switch_commands`)には反映しない(既存の`vo`以外のwindowオプション全て(`ontop`/`geometry`/`autofit`/`focus_on`/`title`)と同じ制約)。設定ファイルで`window`モードとして起動した場合のみ全画面になる。

## 実装箇所

- `src/settings.rs`: `RawWindow`に`fullscreen: bool`(`#[serde(default)]`)、`validate_window`でそのままコピー、`render`で`ontop`と同じif/else形式で出力
- `src/display.rs`: `WindowOptions`に`fullscreen: bool`追加、`args()`で`ontop`と同じ単体フラグとして`--fullscreen`を追加(値なし)

## 対象外(v1)

- `w`キーでの別ウィンドウ切替時への反映(既存の`ontop`等と同じ制約を継承)
- 設定画面(#31)への項目追加(`window.*`は既存どおり対象外。#42のスコープ)
