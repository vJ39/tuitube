//! サムネイルのダウンロード。yt-dlp / mpv と同じく外部プロセスを呼ぶ形にして、
//! テストでは偽物へ差し替える。

use std::future::Future;
use std::path::PathBuf;
use std::process::Output;
use std::time::Duration;
use tokio::process::Command;

/// 同時接続数。同じホストなので接続は使い回される。
pub const PARALLEL_MAX: u8 = 6;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Download {
    pub id: String,
    pub url: String,
    pub path: PathBuf,
}

/// curl 1 プロセスぶんの引数。空の一覧では起動しないよう空を返す。
pub fn curl_args(items: &[Download], timeout: Duration, parallel_max: u8) -> Vec<String> {
    if items.is_empty() {
        return Vec::new();
    }
    let mut args = vec![
        "-sS".to_string(),
        "--fail".to_string(),
        "--max-time".to_string(),
        timeout.as_secs().max(1).to_string(),
        "--parallel".to_string(),
        "--parallel-max".to_string(),
        parallel_max.max(1).to_string(),
    ];
    for item in items {
        args.push("-o".to_string());
        args.push(item.path.to_string_lossy().into_owned());
        args.push(item.url.clone());
    }
    args
}

/// 本番は tokio Command、テストは台本どおりの Output を返す偽物。
pub trait Fetcher {
    fn run(&self, args: Vec<String>) -> impl Future<Output = std::io::Result<Output>> + Send;
}

pub struct RealCurl;

impl Fetcher for RealCurl {
    fn run(&self, args: Vec<String>) -> impl Future<Output = std::io::Result<Output>> + Send {
        // 取得中にアプリを終了しても curl を孤児にしない。
        Command::new("curl").args(args).kill_on_drop(true).output()
    }
}

#[cfg(test)]
pub(crate) mod fixtures {
    use super::*;
    use std::os::unix::process::ExitStatusExt;
    use std::sync::{Arc, Mutex};

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum CurlResult {
        /// -o で指定された各パスへ body を書き、終了コード 0 を返す。
        Wrote,
        /// 何も書かずに終了コードだけ返す (404 やタイムアウト)。
        Failed,
        /// curl そのものが PATH に無い。
        Missing,
    }

    /// 起動せず台本どおりに振る舞う curl。呼ばれた引数を溜める。
    pub struct FakeCurl {
        calls: Arc<Mutex<Vec<Vec<String>>>>,
        result: CurlResult,
        body: Vec<u8>,
    }

    impl FakeCurl {
        pub fn new(result: CurlResult, body: &[u8]) -> Self {
            Self {
                calls: Arc::new(Mutex::new(Vec::new())),
                result,
                body: body.to_vec(),
            }
        }

        pub fn calls(&self) -> Vec<Vec<String>> {
            self.calls.lock().expect("溜め込み先").clone()
        }

        /// 取りに行った URL だけを並べる。
        pub fn urls(&self) -> Vec<String> {
            self.calls()
                .iter()
                .flat_map(|args| {
                    args.iter()
                        .filter(|a| a.starts_with("https://"))
                        .cloned()
                        .collect::<Vec<_>>()
                })
                .collect()
        }
    }

    impl Fetcher for FakeCurl {
        fn run(&self, args: Vec<String>) -> impl Future<Output = std::io::Result<Output>> + Send {
            self.calls.lock().expect("溜め込み先").push(args.clone());
            if self.result == CurlResult::Wrote {
                let mut next_is_path = false;
                for arg in &args {
                    if next_is_path {
                        let _ = std::fs::write(arg, &self.body);
                    }
                    next_is_path = arg == "-o";
                }
            }
            let result = self.result;
            async move {
                match result {
                    CurlResult::Missing => Err(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        "no such file",
                    )),
                    CurlResult::Wrote => Ok(Output {
                        status: ExitStatusExt::from_raw(0),
                        stdout: Vec::new(),
                        stderr: Vec::new(),
                    }),
                    CurlResult::Failed => Ok(Output {
                        status: ExitStatusExt::from_raw(22 << 8),
                        stdout: Vec::new(),
                        stderr: b"curl: (22) The requested URL returned error: 404\n".to_vec(),
                    }),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn download(id: &str) -> Download {
        Download {
            id: id.to_string(),
            url: format!("https://i.ytimg.com/vi/{id}/mqdefault.jpg"),
            path: PathBuf::from(format!("/cache/{id}.jpg")),
        }
    }

    fn args(count: usize) -> Vec<String> {
        let items: Vec<Download> = (0..count).map(|i| download(&format!("id{i}"))).collect();
        curl_args(&items, Duration::from_secs(10), PARALLEL_MAX)
    }

    #[test]
    fn curl_args_pair_each_output_path_with_its_url() {
        let args = args(2);
        let pairs: Vec<&[String]> = args
            .windows(3)
            .filter(|w| w[0] == "-o")
            .map(|w| &w[..3])
            .collect();
        assert_eq!(
            pairs,
            [
                [
                    "-o",
                    "/cache/id0.jpg",
                    "https://i.ytimg.com/vi/id0/mqdefault.jpg"
                ],
                [
                    "-o",
                    "/cache/id1.jpg",
                    "https://i.ytimg.com/vi/id1/mqdefault.jpg"
                ],
            ]
        );
    }

    #[test]
    fn curl_args_include_fail_silent_and_timeout() {
        let args = args(1);
        assert_eq!(args[0], "-sS");
        assert!(args.contains(&"--fail".to_string()), "{args:?}");
        let at = args.iter().position(|a| a == "--max-time").expect("上限");
        assert_eq!(args[at + 1], "10");
    }

    #[test]
    fn curl_args_enable_parallel_transfers() {
        let args = args(3);
        assert!(args.contains(&"--parallel".to_string()), "{args:?}");
        let at = args
            .iter()
            .position(|a| a == "--parallel-max")
            .expect("同時数");
        assert_eq!(args[at + 1], "6");
    }

    #[test]
    fn curl_args_of_a_single_item_have_the_same_shape() {
        let one = args(1);
        let two = args(2);
        assert_eq!(one[..7], two[..7]);
        assert_eq!(one.len(), 7 + 3);
        assert_eq!(two.len(), 7 + 6);
    }

    #[test]
    fn curl_args_are_empty_for_an_empty_list() {
        assert!(curl_args(&[], Duration::from_secs(10), PARALLEL_MAX).is_empty());
    }

    #[test]
    fn curl_args_never_pass_a_zero_timeout_or_zero_parallelism() {
        let items = [download("a")];
        let args = curl_args(&items, Duration::from_millis(1), 0);
        let timeout = args.iter().position(|a| a == "--max-time").expect("上限");
        assert_eq!(args[timeout + 1], "1");
        let parallel = args
            .iter()
            .position(|a| a == "--parallel-max")
            .expect("同時数");
        assert_eq!(args[parallel + 1], "1");
    }
}
