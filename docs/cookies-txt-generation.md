# cookies.txtの作り方

`[cookies] browser = "chrome:Default"`は、Chromeの拡張機能(例: `glic`)が持つ独自のCookieストア(`Default/Storage/ext/<拡張機能ID>/Cookies`)を、本来のログインCookie(`Default/Cookies`)と間違えて読むことがある。yt-dlpはプロファイルディレクトリ配下を再帰的に探し、名前が`Cookies`のファイルの中で最終更新時刻が一番新しいものを選ぶため、拡張機能側のファイルが僅差で新しいと誤って選ばれる。これが起きると実質未ログイン状態になり、ログイン必須のフィード(後で見る等)が`The playlist does not exist`で失敗する。

対策は、本来のCookiesファイルだけを一時ディレクトリにコピーしてyt-dlpに渡すこと。コピー元はChromeが使用中でも読み取れる。

## 生成手順

```sh
TMPPROFILE="$(mktemp -d)/Default"
mkdir -p "$TMPPROFILE"
cp "$HOME/Library/Application Support/Google/Chrome/Default/Cookies" "$TMPPROFILE/Cookies"

yt-dlp --cookies-from-browser "chrome:${TMPPROFILE}" \
  --cookies "$HOME/.config/tuitube/cookies.txt" \
  --flat-playlist --dump-json --playlist-end 1 ":ytwatchlater"
```

- `-v`を付けて`Extracted N cookies from chrome`を見る。500件前後ならログインCookieを含む本来のファイルを読めている。30件程度しか出なければ拡張機能側を掴んでいるので、コピー元のパスを確認する。
- Chromeのプロファイルが`Default`以外の場合は、パスの`Default`をそのプロファイル名に置き換える。

## config.tomlの設定

```toml
[cookies]
# browser = "chrome:Default"
file = "~/.config/tuitube/cookies.txt"
```

`file`が指定されていれば`browser`より優先されるが、両方書くとnoticeが出るので`browser`側はコメントアウトする。

## 再生成が必要になったら

YouTubeのログインCookieには有効期限がある。tuitube終了時にyt-dlpがこのファイルへcookieを書き戻すので多少延命されるが、期限切れで再びログイン必須のフィードが失敗するようになったら、上の生成手順をもう一度実行する。
