use std::fmt::Write as _;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use serde_json::json;
use tracing::{Instrument, info, info_span};

use crate::web_security::{ssrf_check, validate_url_scheme, wrap_external_content};

const DEFAULT_MAX_CHARS: usize = 50_000;

pub(crate) struct FetchConfig {
    pub max_chars: usize,
    pub timeout: Duration,
    pub max_redirects: usize,
    pub user_agent: String,
}

impl Default for FetchConfig {
    fn default() -> Self {
        Self {
            max_chars: DEFAULT_MAX_CHARS,
            timeout: Duration::from_secs(30),
            max_redirects: 3,
            user_agent: "Mozilla/5.0 (compatible; Coop/1.0)".to_owned(),
        }
    }
}

#[allow(clippy::too_many_lines)]
pub(crate) async fn fetch_url(
    client: &reqwest::Client,
    url: &str,
    extract_mode: &str,
    max_chars: Option<usize>,
    config: &FetchConfig,
) -> Result<serde_json::Value> {
    let span = info_span!("web_fetch", url = %url);

    async {
        let start = Instant::now();

        validate_url_scheme(url)?;
        ssrf_check(url).await?;

        let max = max_chars.unwrap_or(config.max_chars);
        let mut current_url = url.to_owned();
        let mut redirect_count = 0;

        let response = loop {
            let resp = client
                .get(&current_url)
                .header("User-Agent", &config.user_agent)
                .timeout(config.timeout)
                .send()
                .await?;

            let status = resp.status();
            if status.is_redirection() {
                if redirect_count >= config.max_redirects {
                    bail!("Too many redirects (max {})", config.max_redirects);
                }
                let location = resp
                    .headers()
                    .get("location")
                    .and_then(|v| v.to_str().ok())
                    .ok_or_else(|| anyhow::anyhow!("Redirect without Location header"))?;

                let next_url = reqwest::Url::parse(&current_url)?
                    .join(location)?
                    .to_string();

                validate_url_scheme(&next_url)?;
                ssrf_check(&next_url).await?;

                current_url = next_url;
                redirect_count += 1;
                continue;
            }

            break resp;
        };

        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("text/plain")
            .to_owned();

        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            let truncated = &body[..body.len().min(500)];
            bail!("HTTP {status}: {truncated}");
        }

        let body = response.text().await?;

        #[allow(clippy::cast_possible_truncation)]
        let took_ms = start.elapsed().as_millis() as u64;

        let (text, title) = if content_type.contains("text/html") || content_type.contains("xhtml")
        {
            let title = extract_title(&body);
            let converted = if extract_mode == "text" {
                html_to_text(&body)
            } else {
                html_to_markdown(&body)
            };
            (converted, title)
        } else if content_type.contains("json") {
            let pretty = serde_json::from_str::<serde_json::Value>(&body)
                .map(|v| serde_json::to_string_pretty(&v).unwrap_or_else(|_| body.clone()))
                .unwrap_or(body);
            (pretty, None)
        } else {
            (body, None)
        };

        let truncated = text.len() > max;
        let text = if truncated {
            text[..text.floor_char_boundary(max)].to_owned()
        } else {
            text
        };

        let wrapped_text = wrap_external_content(&text);
        let wrapped_title = title.as_deref().map(wrap_external_content);

        info!(
            url,
            status,
            content_type = %content_type,
            length = text.len(),
            truncated,
            took_ms,
            "fetch complete"
        );

        Ok(json!({
            "url": url,
            "final_url": current_url,
            "status": status,
            "content_type": content_type,
            "title": wrapped_title,
            "extract_mode": extract_mode,
            "truncated": truncated,
            "length": text.len(),
            "took_ms": took_ms,
            "text": wrapped_text,
        }))
    }
    .instrument(span)
    .await
}

// ---------------------------------------------------------------------------
// HTML extraction
// ---------------------------------------------------------------------------

fn extract_title(html: &str) -> Option<String> {
    let lower = html.to_lowercase();
    let start = lower.find("<title")?;
    let after_tag = lower[start..].find('>')?;
    let content_start = start + after_tag + 1;
    let end = lower[content_start..].find("</title>")?;
    let title = html[content_start..content_start + end].trim();
    if title.is_empty() {
        None
    } else {
        Some(decode_entities(title))
    }
}

fn html_to_markdown(html: &str) -> String {
    let mut s = html.to_owned();

    s = remove_tag_blocks(&s, "script");
    s = remove_tag_blocks(&s, "style");
    s = remove_tag_blocks(&s, "noscript");

    s = convert_links(&s);

    for level in (1..=6).rev() {
        let prefix = "#".repeat(level);
        let open = format!("<h{level}");
        let close = format!("</h{level}>");
        s = convert_block_tag(&s, &open, &close, &format!("{prefix} "), "\n\n");
    }

    s = convert_block_tag(&s, "<li", "</li>", "- ", "\n");

    s = replace_self_closing(&s, "br");
    s = replace_self_closing(&s, "hr");

    for tag in &[
        "p",
        "div",
        "section",
        "article",
        "header",
        "footer",
        "main",
        "nav",
        "aside",
        "blockquote",
        "pre",
        "table",
        "tr",
        "ul",
        "ol",
    ] {
        s = s.replace(&format!("</{tag}>"), "\n\n");
    }

    s = strip_tags(&s);
    s = decode_entities(&s);
    normalize_whitespace(&s)
}

fn html_to_text(html: &str) -> String {
    let md = html_to_markdown(html);
    let mut result = String::new();
    let mut chars = md.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '[' {
            let mut link_text = String::new();
            let mut found_close = false;
            for inner in chars.by_ref() {
                if inner == ']' {
                    found_close = true;
                    break;
                }
                link_text.push(inner);
            }
            if found_close && chars.peek() == Some(&'(') {
                chars.next();
                let mut depth = 1;
                for c in chars.by_ref() {
                    if c == '(' {
                        depth += 1;
                    } else if c == ')' {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                }
                result.push_str(&link_text);
            } else {
                result.push('[');
                result.push_str(&link_text);
                if found_close {
                    result.push(']');
                }
            }
        } else {
            result.push(ch);
        }
    }
    result
}

fn remove_tag_blocks(html: &str, tag: &str) -> String {
    let mut result = String::with_capacity(html.len());
    let lower = html.to_lowercase();
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut pos = 0;

    while pos < html.len() {
        if let Some(start) = lower[pos..].find(&open) {
            let abs_start = pos + start;
            result.push_str(&html[pos..abs_start]);
            if let Some(end) = lower[abs_start..].find(&close) {
                pos = abs_start + end + close.len();
            } else {
                pos = html.len();
            }
        } else {
            result.push_str(&html[pos..]);
            break;
        }
    }

    result
}

fn convert_links(html: &str) -> String {
    let mut result = String::with_capacity(html.len());
    let lower = html.to_lowercase();
    let mut pos = 0;

    while pos < html.len() {
        if let Some(start) = lower[pos..].find("<a ") {
            let abs_start = pos + start;
            result.push_str(&html[pos..abs_start]);

            if let Some(tag_end) = html[abs_start..].find('>') {
                let tag = &html[abs_start..abs_start + tag_end];
                let href = extract_attr(tag, "href");

                let content_start = abs_start + tag_end + 1;
                if let Some(close) = lower[content_start..].find("</a>") {
                    let text = &html[content_start..content_start + close];
                    let clean_text = strip_tags(text).trim().to_owned();
                    if let Some(href) = href {
                        let _ = write!(result, "[{clean_text}]({href})");
                    } else {
                        result.push_str(&clean_text);
                    }
                    pos = content_start + close + 4;
                } else {
                    result.push_str(&html[abs_start..content_start]);
                    pos = content_start;
                }
            } else {
                result.push_str(&html[abs_start..=abs_start]);
                pos = abs_start + 1;
            }
        } else {
            result.push_str(&html[pos..]);
            break;
        }
    }

    result
}

fn extract_attr<'a>(tag: &'a str, attr: &str) -> Option<&'a str> {
    let lower = tag.to_lowercase();
    let pattern = format!("{attr}=\"");
    if let Some(start) = lower.find(&pattern) {
        let value_start = start + pattern.len();
        if let Some(end) = tag[value_start..].find('"') {
            return Some(&tag[value_start..value_start + end]);
        }
    }
    let pattern_single = format!("{attr}='");
    if let Some(start) = lower.find(&pattern_single) {
        let value_start = start + pattern_single.len();
        if let Some(end) = tag[value_start..].find('\'') {
            return Some(&tag[value_start..value_start + end]);
        }
    }
    None
}

fn convert_block_tag(html: &str, open: &str, close: &str, prefix: &str, suffix: &str) -> String {
    let mut result = String::with_capacity(html.len());
    let lower = html.to_lowercase();
    let mut pos = 0;

    while pos < html.len() {
        if let Some(start) = lower[pos..].find(open) {
            let abs_start = pos + start;
            result.push_str(&html[pos..abs_start]);

            if let Some(tag_end) = html[abs_start..].find('>') {
                let content_start = abs_start + tag_end + 1;
                if let Some(close_pos) = lower[content_start..].find(close) {
                    let content = &html[content_start..content_start + close_pos];
                    result.push_str(prefix);
                    result.push_str(content.trim());
                    result.push_str(suffix);
                    pos = content_start + close_pos + close.len();
                } else {
                    result.push_str(&html[abs_start..content_start]);
                    pos = content_start;
                }
            } else {
                result.push_str(&html[abs_start..=abs_start]);
                pos = abs_start + 1;
            }
        } else {
            result.push_str(&html[pos..]);
            break;
        }
    }

    result
}

fn replace_self_closing(html: &str, tag: &str) -> String {
    let mut result = html.to_owned();

    for pattern in &[
        format!("<{tag}/>"),
        format!("<{tag} />"),
        format!("<{tag}>"),
    ] {
        let mut new_result = String::with_capacity(result.len());
        let lower = result.to_lowercase();
        let mut pos = 0;

        while pos < result.len() {
            if let Some(found) = lower[pos..].find(pattern.as_str()) {
                let abs = pos + found;
                new_result.push_str(&result[pos..abs]);
                new_result.push('\n');
                pos = abs + pattern.len();
            } else {
                new_result.push_str(&result[pos..]);
                break;
            }
        }

        result = new_result;
    }

    result
}

fn strip_tags(html: &str) -> String {
    let mut result = String::with_capacity(html.len());
    let mut in_tag = false;

    for ch in html.chars() {
        if ch == '<' {
            in_tag = true;
        } else if ch == '>' {
            in_tag = false;
        } else if !in_tag {
            result.push(ch);
        }
    }

    result
}

fn decode_entities(text: &str) -> String {
    let mut result = text.to_owned();
    result = result.replace("&amp;", "&");
    result = result.replace("&lt;", "<");
    result = result.replace("&gt;", ">");
    result = result.replace("&quot;", "\"");
    result = result.replace("&#39;", "'");
    result = result.replace("&apos;", "'");
    result = result.replace("&nbsp;", " ");

    let mut decoded = String::with_capacity(result.len());
    let mut chars = result.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '&' && chars.peek() == Some(&'#') {
            chars.next();
            let mut num_str = String::new();
            let is_hex = chars.peek() == Some(&'x') || chars.peek() == Some(&'X');
            if is_hex {
                chars.next();
            }
            for c in chars.by_ref() {
                if c == ';' {
                    break;
                }
                num_str.push(c);
            }
            let code = if is_hex {
                u32::from_str_radix(&num_str, 16).ok()
            } else {
                num_str.parse::<u32>().ok()
            };
            if let Some(code) = code
                && let Some(c) = char::from_u32(code)
            {
                decoded.push(c);
                continue;
            }
            decoded.push('&');
            decoded.push('#');
            if is_hex {
                decoded.push('x');
            }
            decoded.push_str(&num_str);
            decoded.push(';');
        } else {
            decoded.push(ch);
        }
    }

    decoded
}

fn normalize_whitespace(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut prev_newline_count = 0;

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            prev_newline_count += 1;
            if prev_newline_count <= 2 {
                result.push('\n');
            }
        } else {
            if !result.is_empty() && prev_newline_count == 0 {
                result.push('\n');
            }
            prev_newline_count = 0;
            result.push_str(trimmed);
        }
    }

    result.trim().to_owned()
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_title_basic() {
        let html = "<html><head><title>Test Page</title></head></html>";
        assert_eq!(extract_title(html), Some("Test Page".to_owned()));
    }

    #[test]
    fn extract_title_empty() {
        let html = "<html><head><title></title></head></html>";
        assert_eq!(extract_title(html), None);
    }

    #[test]
    fn extract_title_with_entities() {
        let html = "<title>A &amp; B</title>";
        assert_eq!(extract_title(html), Some("A & B".to_owned()));
    }

    #[test]
    fn html_to_markdown_headings() {
        let html = "<h1>Title</h1><h2>Subtitle</h2>";
        let md = html_to_markdown(html);
        assert!(md.contains("# Title"));
        assert!(md.contains("## Subtitle"));
    }

    #[test]
    fn html_to_markdown_links() {
        let html = r#"<a href="https://example.com">Example</a>"#;
        let md = html_to_markdown(html);
        assert!(md.contains("[Example](https://example.com)"));
    }

    #[test]
    fn html_to_markdown_lists() {
        let html = "<ul><li>First</li><li>Second</li></ul>";
        let md = html_to_markdown(html);
        assert!(md.contains("- First"));
        assert!(md.contains("- Second"));
    }

    #[test]
    fn html_to_markdown_strips_scripts() {
        let html = "<p>Hello</p><script>alert('xss')</script><p>World</p>";
        let md = html_to_markdown(html);
        assert!(!md.contains("alert"));
        assert!(md.contains("Hello"));
        assert!(md.contains("World"));
    }

    #[test]
    fn entity_decoding() {
        assert_eq!(decode_entities("&amp;&lt;&gt;"), "&<>");
        assert_eq!(decode_entities("&#65;"), "A");
        assert_eq!(decode_entities("&#x41;"), "A");
        assert_eq!(decode_entities("&#39;"), "'");
    }

    #[test]
    fn whitespace_normalization() {
        let input = "Hello\n\n\n\n\nWorld\n\nFoo";
        let result = normalize_whitespace(input);
        assert!(!result.contains("\n\n\n"));
        assert!(result.contains("Hello"));
        assert!(result.contains("World"));
    }

    #[test]
    fn html_to_text_strips_link_syntax() {
        let html = r#"Visit <a href="https://example.com">Example</a> now"#;
        let text = html_to_text(html);
        assert!(text.contains("Example"));
        assert!(!text.contains("[Example]"));
        assert!(!text.contains("https://example.com"));
    }
}
