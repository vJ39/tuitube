# 表示モード切替の自動保存 + ontopの設定画面対応

## 表示モードの自動保存

再生中`w`キーでの表示モード切替(`cycle_display_mode`)は、現在`app.display`(実行中の状態)のみを変更し`app.settings.display.mode`(設定ファイル由来の値)には反映されない。切替が成功したら、設定ファイルへも自動的に保存する。

- `cycle_display_mode`がmpvへのコマンド送信に成功した後、設定ファイルの`[display]`の`mode`行だけを差し替える(`settings::save_display_mode_to`。パス解決は既存の`config_path_from_env`、書き込みは既存のatomic write)
  - 設定画面の`s`が使う`save_settings_to`→`settings::render`は全文を作り直すため、利用者が書いたコメント・tuitubeがモデル化していないTOMLキー・起動時に丸められた値が消える。`w`は保存を意図した操作ではないので、この経路には載せない
  - `mode`行が無ければ`[display]`の直後へ足し、`[display]`ごと無ければ末尾へ足す。ファイル自体が無ければ起動時と同じテンプレートを作る
  - 差し替えた全文が読み直せて`display.mode`が意図した値になることを確かめてから置き換える。確かめられない場合は保存失敗として扱い、ファイルは書き換えない
- 保存できたときだけ`app.settings.display.mode`と`app.settings_backup.display.mode`を更新する
  - `settings_backup`も更新するのは、設定画面を開いてEscで戻した時に直前の自動保存を巻き戻さないため。`display.mode`以外の項目は`settings_backup`の値をそのまま残す
  - 保存に失敗したときに`app.settings`だけ進めると、その後の設定画面の`s`がファイルに書けなかった値を焼き付ける
- 保存に失敗しても再生は継続する。失敗時は`set_temporary_error`で知らせる(表示モードの切替自体は成功しているため、この失敗で再生を止めない)
- 成功時は何も出さない。`w`は保存を意図した操作ではなく、再生行に長い保存先パスを繰り返し出すとその間の再生状況が見えなくなる

## ontopの設定画面対応

設定画面(`SETTINGS_ITEMS`)に`window.ontop`(bool)を追加する。既存の`subtitles.enabled`/`thumbnails.enabled`と同じtoggle項目として実装する。

## 対象外(v1)

- `focus_on`の設定画面対応。現在の値は`Option<FocusOn>`(未指定=mpvの既定に任せる)で、既存の設定画面のcycle項目はいずれも「未指定」状態を持たない具体的な値の列挙(`DisplayMode`/`Quality`/`LayoutMode`)。`focus_on`を追加するには「未指定」を含む新しい選択パターンが要り、今回のスコープには含めない
- `vo`/`autofit`/`geometry`/`title`(自由文字列)の設定画面対応。テキスト入力ウィジェットが無いため対象外
