# 再生位置の記憶

動画/音声の再生位置をローカルに記憶し、次に同じ動画を選んだとき視聴状況に応じて再開位置を変える。

## 保存先

`resume.toml`(`hidden.toml`と同じ置き場、`settings::app_config_dir`)。

```toml
[[videos]]
id = "..."
position_secs = 123.4
duration_secs = 600.0
```

`hidden.toml`と違い、この内容は完全に内部状態で手編集する想定がない(手書きの保護は不要)。そのため書き込みは追記ではなく、配列を丸ごと読み直して該当IDを差し替え(無ければ末尾へ追加)→丸ごと書き直す。並び順が「古い順」を表し、末尾が最新(下記「上限」参照)。

## 記憶する情報・完了判定

- `position_secs`: 記録時点の再生位置
- `duration_secs`: そのときの動画の長さ(以後の判定は記録時点の値を使う。動画の長さが変わることは無いため)

完了(最後まで見た)とみなす条件、いずれか:
- `position_secs >= duration_secs * 0.95`(95%以上)
- 動画の長さが60秒未満(短い動画は再開のズレが気になりやすく、途中からの意味が薄い)

完了と判定したら、そのIDのエントリを書かない(既にあれば消す)。次に選んだときは最初から再生する。

再開を書く条件: 完了でない、かつ `position_secs >= 10.0`(始まってすぐの位置は最初からと変わらないので書かない)。

## 保存タイミング

`app.playback`(再生中の位置・長さを持つ)が入れ替わる/消える直前に、そのときの値を保存する。呼び出し箇所:

- `end_playback`(自然終了・エラー): `app.playback`を`Playback::default()`へ戻す前
- `enter_playback`(次の動画を選んで新しく始める): `app.playback`を新しい動画のもので上書きする前。バックグラウンド再生中に別の動画を選んだときもここを通る
- アプリを終了する経路(Ctrl+C、終了確認`y`/`Enter`): `stop_playback`を呼ぶ直前

途中を1秒ごとにディスクへ書くと待たせるので、再生中の定期保存はしない。上記の「入れ替わる/消える直前」だけで十分カバーできる(バックグラウンド中に停電等で異常終了した場合の直近数十秒分だけが記憶されないが、対象外(v1)とする)。

## 上限

`RESUME_CAPACITY`(固定値、例: 500件)を超えたら先頭(最も古く更新された)から捨てる。タイムスタンプは持たず、配列の並び順(古い→新しい)だけで管理する。更新時は既存のエントリを一度取り除いてから末尾へ追加し直すことで、常に「更新した分が最新」を保つ。

## 再開位置の指定

`LaunchPlan`に`resume_at: Option<f64>`を追加する(既存の`speed`/`subtitles`と同じ並びの起動時パラメータ)。`args()`で`Some(secs)`なら`--start={secs}`をURLの前に足す(mpvの起動引数。既存の`speed.launch_arg()`と同じ`Option<String>`を`extend`する形)。

`playback_plan`(`start_playback`が呼ぶ)で、選んだ動画のIDを`app.resume`から引き、完了していないエントリがあれば`plan.resume_at = Some(position_secs)`にする。無ければ`None`(最初から)。

## 状態管理

- `App`に`resume: Resume`を追加(`hidden`と同じ並び)
- `Playback`に`id: String`を追加(動画IDそのもの。`Playback.url`から都度パースする代わりに`start_playback`が持っている`result.id`をそのまま渡す)
- `enter_playback`の引数に`id: String`を追加

## 実装箇所

- `src/resume.rs`(新規): `Resume`構造体(`entries: Vec<Entry>`、`path: Option<PathBuf>`)、`Entry { id, position_secs, duration_secs }`、`resume_path`、`load`/`load_from`、`is_complete`、`lookup(&self, id) -> Option<f64>`(完了していない場合だけ位置を返す)、`remember(&mut self, id, position_secs, duration_secs) -> Result<(), String>`(完了なら削除、未完了なら上限を見て upsert して丸ごと書き直す)
- `src/app.rs`: `App::resume`、`Playback::id`
- `src/actions.rs`: `enter_playback`/`start_playback`の`id`引き渡し、`playback_plan`での`resume_at`セット、`end_playback`/`enter_playback`/Ctrl+C・終了確認の各所での`resume.remember(...)`呼び出し
- `src/display.rs`: `LaunchPlan::resume_at`、`args()`への反映
- `src/main.rs`: 起動時の`resume::load()`読み込み(`hidden::load()`と同じ並び)

## 対象外(v1)

- 再生中の定期保存(入れ替わり時の保存だけで足りるとする)
- 異常終了(kill -9等、SIGINT以外)時の直近位置の保存
- 再開位置の一覧表示・手動クリアUI(手で`resume.toml`を編集すれば消せる)
- 上限件数の設定項目化(固定値`RESUME_CAPACITY`)
