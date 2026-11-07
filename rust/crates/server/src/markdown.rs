//! `MarkdownToHtml`: the small Markdown dialect of email bodies (bold, italics, strike,
//! underline, inline code, links and auto-linked URLs), sanitized to a few tags.

use std::{collections::HashSet, fmt::Write as _, sync::LazyLock};

use regex::Regex;

static INLINE_TOKENIZER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(\[)|(\]\(([^)]+?)\))|(?:`([^`].*?)`)|(\*\*\*|\*\*|\*|~~)").expect("valid regex")
});
static URL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?:https?://|www\.)[^\s<>\x{00A0}"]+"#).expect("valid regex"));
static LINK_OPENING: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\]\(\s*\z").expect("valid regex"));
static UNDERLINE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\+\+([^+]+)\+\+").expect("valid regex"));
static PARAGRAPHS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\n{2,}").expect("valid regex"));
static LINE_BREAK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r" *\n").expect("valid regex"));

/// `MarkdownToHtml.call`.
#[must_use]
pub fn to_html(markdown: &str) -> String {
    if markdown.trim().is_empty() {
        return String::new();
    }
    let html = render_markdown(&auto_link_urls(markdown));
    sanitize(&html)
}

/// `sanitize(html, tags: %w[p br strong b em i u a], attributes: %w[href])`.
fn sanitize(html: &str) -> String {
    let tags: HashSet<&str> = ["p", "br", "strong", "b", "em", "i", "u", "a"].into();
    ammonia::Builder::default()
        .tags(tags)
        .generic_attributes(HashSet::new())
        .tag_attributes([("a", ["href"].into())].into())
        .link_rel(None)
        // Rails' sanitizer drops the tags of `<script>` and `<style>` but keeps the text.
        .clean_content_tags(HashSet::new())
        .clean(html)
        .to_string()
}

/// Is `c` matched by `[^\p{Word}/\-=;]`?
fn is_trailing_punctuation(c: char) -> bool {
    !(c.is_alphanumeric() || c == '_' || matches!(c, '/' | '-' | '=' | ';'))
}

fn opening_bracket(c: char) -> Option<char> {
    match c {
        ')' => Some('('),
        ']' => Some('['),
        '}' => Some('{'),
        _ => None,
    }
}

/// `auto_link_urls`: bare URLs become `[url](url)`, without trailing punctuation.
fn auto_link_urls(text: &str) -> String {
    // `split` with a capture group: text and URLs alternate.
    let mut parts: Vec<&str> = Vec::new();
    let mut last = 0;
    for m in URL.find_iter(text) {
        parts.push(&text[last..m.start()]);
        parts.push(m.as_str());
        last = m.end();
    }
    parts.push(&text[last..]);

    let mut out = String::new();
    for (index, part) in parts.iter().enumerate() {
        let is_url =
            part.starts_with("http://") || part.starts_with("https://") || part.starts_with("www.");
        if !is_url || (index > 0 && LINK_OPENING.is_match(parts[index - 1])) {
            out.push_str(part);
            continue;
        }
        let mut url_part: String = (*part).to_owned();
        let mut punctuation: Vec<char> = Vec::new();
        while let Some(c) = url_part
            .chars()
            .last()
            .filter(|c| is_trailing_punctuation(*c))
        {
            url_part.pop();
            punctuation.push(c);
            if let Some(opening) = opening_bracket(c)
                && url_part.matches(opening).count() > url_part.matches(c).count()
            {
                url_part.push(c);
                punctuation.pop();
                break;
            }
        }
        let trail: String = punctuation.iter().rev().collect();
        let url = if url_part.starts_with("www.") {
            format!("https://{url_part}")
        } else {
            url_part.clone()
        };
        let _ = write!(out, "[{url_part}]({url}){trail}");
    }
    out
}

fn render_markdown(text: &str) -> String {
    let text = UNDERLINE.replace_all(text, "<u>$1</u>");
    let html: String = PARAGRAPHS
        .split(&text)
        .filter_map(|para| {
            let content = para.trim();
            if content.is_empty() {
                return None;
            }
            if content == "&nbsp;" || content == "&amp;nbsp;" {
                return Some("<p><br></p>".to_owned());
            }
            let content = LINE_BREAK.replace_all(content, "<br>");
            Some(format!("<p>{}</p>", parse_inline(&content)))
        })
        .collect();
    if html.is_empty() {
        "<p></p>".to_owned()
    } else {
        html
    }
}

fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// `TAGS[t[1] || '']`.
fn tag_pair(token: &str) -> Option<(&'static str, &'static str)> {
    match token.chars().nth(1) {
        None => Some(("<em>", "</em>")),
        Some('*') => Some(("<strong>", "</strong>")),
        Some('~') => Some(("<s>", "</s>")),
        _ => None,
    }
}

fn tag(context: &mut Vec<String>, token: &str) -> String {
    let Some((open, close)) = tag_pair(token) else {
        return token.to_owned();
    };
    if context.last().map(String::as_str) == Some(token) {
        context.pop();
        close.to_owned()
    } else {
        context.push(token.to_owned());
        open.to_owned()
    }
}

fn flush(context: &mut Vec<String>) -> String {
    let mut out = String::new();
    while let Some(last) = context.last().cloned() {
        out.push_str(&tag(context, &last));
    }
    out
}

/// `parse_inline`.
fn parse_inline(text: &str) -> String {
    let mut context: Vec<String> = Vec::new();
    let mut out = String::new();
    let mut last = 0;
    while last <= text.len() {
        let Some(m) = INLINE_TOKENIZER.captures_at(text, last) else {
            break;
        };
        let whole = m.get(0).expect("match");
        let prev = &text[last..whole.start()];
        last = whole.end();
        let chunk = if let Some(code) = m.get(4) {
            format!("<code>{}</code>", html_escape(code.as_str()))
        } else if m.get(2).is_some() {
            let url = m.get(3).map_or("", |u| u.as_str());
            if let Some(at) = out.rfind("<a>") {
                out.replace_range(at..at + 3, &format!("<a href=\"{}\">", html_escape(url)));
            }
            out = out.replace("<a>", "[");
            format!("{}</a>", flush(&mut context))
        } else if m.get(1).is_some() {
            "<a>".to_owned()
        } else if let Some(token) = m.get(5) {
            if token.as_str() == "***" {
                if context.iter().any(|c| c == "*") && context.iter().any(|c| c == "**") {
                    tag(&mut context, "*") + &tag(&mut context, "**")
                } else {
                    tag(&mut context, "**") + &tag(&mut context, "*")
                }
            } else {
                tag(&mut context, token.as_str())
            }
        } else {
            whole.as_str().to_owned()
        };
        out.push_str(prev);
        out.push_str(&chunk);
        if whole.is_empty() {
            break;
        }
    }
    out.push_str(&text[last.min(text.len())..]);
    out.push_str(&flush(&mut context));
    out.replace("<a>", "[")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_email_markdown() {
        assert_eq!(
            to_html("Hello **there**"),
            "<p>Hello <strong>there</strong></p>"
        );
        assert_eq!(to_html("a\nb\n\nc"), "<p>a<br>b</p><p>c</p>");
        assert_eq!(
            to_html("Go to https://example.com/x."),
            "<p>Go to <a href=\"https://example.com/x\">https://example.com/x</a>.</p>"
        );
        assert_eq!(
            to_html("[Sign](https://e.com/s/abc) now"),
            "<p><a href=\"https://e.com/s/abc\">Sign</a> now</p>"
        );
        assert_eq!(to_html("++u++ ~~s~~ *i*"), "<p><u>u</u> s <em>i</em></p>");
        assert_eq!(to_html("<script>x</script>hi"), "<p>xhi</p>");
        assert_eq!(
            to_html("Tom & Jerry <b>x</b> \"q\" 's'"),
            "<p>Tom &amp; Jerry <b>x</b> \"q\" 's'</p>"
        );
        assert_eq!(
            to_html("Grüße, ***both*** `co<de>`"),
            "<p>Grüße, <strong><em>both</em></strong> co&lt;de&gt;</p>"
        );
        assert_eq!(
            to_html("(see www.example.com/a_(b))"),
            "<p>(see <a href=\"https://www.example.com/a_(b\">www.example.com/a_(b)</a>))</p>"
        );
        assert_eq!(to_html("&nbsp;"), "<p><br></p>");
        assert_eq!(to_html(" "), "");
    }
}
