# 設定画面の数値直接入力

数値項目(fps_cap / search.limit / thumbnails.max_cached / thumbnails.timeout_secs)は現在←→の1回きざみ増減しか無く、search.limitの上限が1000になったことで操作回数が多すぎる。数字キーで直接入力できるようにする。

## 対象

`SettingsItem`に数値項目かどうかを判定するメソッド(`is_numeric()`)を追加する。対象は`FpsCap`/`SearchLimit`/`ThumbnailsMaxCached`/`ThumbnailsTimeoutSecs`の4つ。

## 状態

`App`に編集バッファを追加する。

```rust
settings_edit: Option<String>,  // Some(バッファ) = 編集中、None = 通常表示
```

編集は`settings.xxx`本体には反映せず、確定(Enter)して初めて書き込む。既存の`settings_backup`(画面全体のEsc巻き戻し)とは別物として扱う。

## キー操作

数値項目を選択中に限り:
- 数字キー(0-9): 編集モードに入り(まだ入っていなければ)、バッファに追記。追記は対象項目の上限値の桁数まで(それ以上はパースできず、Enterが効かないように見えるため)。Ctrl/Alt等の修飾キー付きの数字は従来どおり何もしない
- Backspace: バッファの末尾1文字を削除(空になったら編集モードのまま空文字列を保持)
- Enter: バッファをパースし、対象項目のmin/max(既存の`adjust()`が使っているのと同じ定数)でクランプして`settings`へ反映。バッファを`None`に戻す
- Esc: バッファだけ破棄して`None`に戻す(`settings`本体は変更しない)。設定画面全体のEsc(`close_settings`)とは独立させる。編集中のEscは1回で編集キャンセルのみ行い、設定画面自体は閉じない
- 上記以外のキー(↑↓←→/s/q等)は編集中は無視する(編集モードから抜けるにはEnterかEscのみ)

パースできない/空文字列でのEnterは、バッファを破棄するだけで値を変更しない(既存の`describe`/`notice`パターンは使わず、単に無視でよい)。

## 表示

`draw_settings`で、編集中の行だけ`settings_rows()`の値部分を生入力(バッファの中身)+カーソルに差し替える。カーソル表示は検索欄の`input_cursor`と同種の仕組みを流用する。

ヘルプ行(`settings_hints`)に編集中の操作案内を追加する(通常時と編集中で文言を出し分ける)。編集中は`s`も`Esc`も画面を閉じないので、画面タイトルも同じ条件で出し分ける。

ヘルプ行は幅が足りないと後ろから落ちるため、追加する「0-9:直接入力」は`s:保存`・`Esc`の後ろへ置く(←→で代用できる案内を先に落とす)。

## 対象外

- 数値以外の項目(display.mode/quality、subtitles.enabled、search.layout、thumbnails.enabled)への直接入力は対象外(既存のcycle/toggleのまま)
