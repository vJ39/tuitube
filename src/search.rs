use serde_json::Value;
use std::io::ErrorKind;
use tokio::process::Command;

#[derive(Debug, Clone, PartialEq)]
pub struct SearchResult {
    pub id: String,
    pub title: String,
    pub duration: Option<f64>,
    pub uploader: Option<String>,
}

impl SearchResult {
    pub fn url(&self) -> String {
        format!("https://www.youtube.com/watch?v={}", self.id)
    }
}

pub fn parse_lines(output: &str) -> Vec<SearchResult> {
    output.lines().filter_map(parse_line).collect()
}

fn parse_line(line: &str) -> Option<SearchResult> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let value: Value = serde_json::from_str(line).ok()?;
    let id = value.get("id")?.as_str()?.to_string();
    let title = value
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("(title unknown)")
        .to_string();
    let duration = value.get("duration").and_then(Value::as_f64);
    let uploader = value
        .get("uploader")
        .and_then(Value::as_str)
        .map(str::to_string);
    Some(SearchResult {
        id,
        title,
        duration,
        uploader,
    })
}

pub async fn search(query: &str) -> Result<Vec<SearchResult>, String> {
    let output = Command::new("yt-dlp")
        .arg(format!("ytsearch10:{query}"))
        .arg("--flat-playlist")
        .arg("--dump-json")
        // 検索中にアプリを終了しても yt-dlp を孤児にしない。
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|e| {
            if e.kind() == ErrorKind::NotFound {
                "yt-dlp が見つかりません (PATH を確認してください)".to_string()
            } else {
                format!("yt-dlp の起動に失敗しました: {e}")
            }
        })?;

    let results = parse_lines(&String::from_utf8_lossy(&output.stdout));
    if results.is_empty() && !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr.lines().last().unwrap_or("").trim();
        return Err(format!("yt-dlp が失敗しました: {detail}"));
    }
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINE_FULL: &str =
        r#"{"id":"abc123","title":"Rust TUI tutorial","duration":612.0,"uploader":"someone"}"#;

    #[test]
    fn parses_full_lines() {
        let out = format!("{LINE_FULL}\n{LINE_FULL}\n");
        let results = parse_lines(&out);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].id, "abc123");
        assert_eq!(results[0].title, "Rust TUI tutorial");
        assert_eq!(results[0].duration, Some(612.0));
        assert_eq!(results[0].uploader.as_deref(), Some("someone"));
    }

    #[test]
    fn missing_optional_fields_become_none() {
        let results = parse_lines(r#"{"id":"xyz","title":"no meta"}"#);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].duration, None);
        assert_eq!(results[0].uploader, None);
    }

    #[test]
    fn null_duration_and_missing_title() {
        let results = parse_lines(r#"{"id":"xyz","duration":null,"uploader":null}"#);
        assert_eq!(results[0].title, "(title unknown)");
        assert_eq!(results[0].duration, None);
        assert_eq!(results[0].uploader, None);
    }

    #[test]
    fn skips_blank_and_broken_lines() {
        let out = format!("\n  \n{LINE_FULL}\nnot json\n{{\"title\":\"no id\"}}\n\n");
        let results = parse_lines(&out);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "abc123");
    }

    #[test]
    fn empty_output_yields_no_results() {
        assert!(parse_lines("").is_empty());
    }

    #[test]
    fn ignores_unrelated_fields_of_real_output() {
        let line = r#"{"_type":"url","ie_key":"Youtube","id":"awX7DUp-r14","url":"https://www.youtube.com/watch?v=awX7DUp-r14","title":"Rust TUI Tutorial: Ratatui, Multithreading, and Responsiveness","duration":2404.0,"channel_id":"UCxxx","uploader":"Green Tea Coding","uploader_id":"@greenteacoding","view_count":16224,"thumbnails":[{"url":"https://i.ytimg.com/vi/awX7DUp-r14/hq720.jpg","height":404}],"playlist":"rust tui","epoch":1758100000}"#;
        let results = parse_lines(line);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "awX7DUp-r14");
        assert_eq!(results[0].duration, Some(2404.0));
        assert_eq!(results[0].uploader.as_deref(), Some("Green Tea Coding"));
    }

    #[test]
    fn builds_watch_url() {
        let results = parse_lines(LINE_FULL);
        assert_eq!(results[0].url(), "https://www.youtube.com/watch?v=abc123");
    }
}
