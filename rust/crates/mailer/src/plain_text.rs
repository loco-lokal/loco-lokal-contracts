//! Plain-text alternative for HTML emails.
//!
//! A simplified version of `lib/html_to_plain_text.rb`: block elements become line breaks,
//! links become `text ( url )`, and entities are decoded. The full port (headings, lists,
//! and word wrapping at 65 characters) comes with the email templates in Phase 4.

use std::fmt::Write as _;

#[must_use]
pub fn html_to_plain_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    let mut pending_href: Vec<Option<String>> = Vec::new();

    while let Some(start) = rest.find('<') {
        out.push_str(&decode_entities(&rest[..start]));
        let Some(end) = rest[start..].find('>') else {
            rest = "";
            break;
        };
        let tag = &rest[start + 1..start + end];
        rest = &rest[start + end + 1..];

        let closing = tag.starts_with('/');
        let name: String = tag
            .trim_start_matches('/')
            .chars()
            .take_while(char::is_ascii_alphanumeric)
            .collect::<String>()
            .to_ascii_lowercase();

        match (name.as_str(), closing) {
            ("script" | "style", false) => {
                let close = format!("</{name}");
                rest = rest
                    .to_ascii_lowercase()
                    .find(&close)
                    .map_or("", |i| &rest[i..]);
            }
            ("br", _) => out.push('\n'),
            ("p" | "div" | "tr" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "li" | "table", _) => {
                out.push_str("\n\n");
            }
            ("a", false) => pending_href.push(attribute(tag, "href")),
            ("a", true) => {
                if let Some(Some(href)) = pending_href.pop()
                    && href.starts_with("http")
                {
                    let _ = write!(out, " ( {href} )");
                }
            }
            _ => {}
        }
    }
    out.push_str(&decode_entities(rest));

    let mut cleaned = String::new();
    let mut blank_lines = 0;
    for line in out
        .lines()
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
    {
        if line.is_empty() {
            blank_lines += 1;
            if blank_lines > 1 {
                continue;
            }
        } else {
            blank_lines = 0;
        }
        cleaned.push_str(&line);
        cleaned.push('\n');
    }

    cleaned.trim().to_owned()
}

fn attribute(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let start = lower.find(&format!("{name}="))? + name.len() + 1;
    let value = &tag[start..];
    let quote = value.chars().next()?;
    if quote == '"' || quote == '\'' {
        value[1..].split(quote).next().map(decode_entities)
    } else {
        value.split_whitespace().next().map(decode_entities)
    }
}

fn decode_entities(text: &str) -> String {
    text.replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_blocks_links_and_entities() {
        let text = html_to_plain_text(
            "<style>p{}</style><p>Hi&nbsp;Ana &amp; team</p><p><a href=\"https://x.test/s/1\">Sign now</a></p>",
        );
        assert_eq!(text, "Hi Ana & team\n\nSign now ( https://x.test/s/1 )");
    }
}
