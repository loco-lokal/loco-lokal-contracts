//! Plain-text alternative for HTML emails: a port of `lib/html_to_plain_text.rb`.
//!
//! The HTML is tokenized with html5ever into a simple element tree (Rails parses it with
//! `Nokogiri::HTML.fragment`), then rendered as in Ruby: blocks become paragraphs, links
//! `text ( url )`, headings are underlined, list items get `* `, and lines are wrapped
//! at 65 characters.

use std::{cell::RefCell, sync::LazyLock};

use html5ever::tokenizer::{
    BufferQueue, TagKind, Token, TokenSink, TokenSinkResult, Tokenizer, TokenizerOpts,
    states::RawKind,
};
use regex::Regex;

const LINE_LENGTH: usize = 65;

static TEXT_HTML_BLOCK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?s)<!-- start text/html -->.*?<!-- end text/html -->").expect("valid regex")
});
static NBSP: LazyLock<Regex> =
    LazyLock::new(|| Regex::new("[ \t]*\u{00A0}+[ \t]*").expect("valid regex"));
static LEADING_SPACE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\n[ \t]+").expect("valid regex"));
static TRAILING_SPACE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[ \t]+\n").expect("valid regex"));
static BLANK_LINES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\n{3,}").expect("valid regex"));
static WRAP: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!(r"(.{{1,{LINE_LENGTH}}})(\s+|$)")).expect("valid regex"));
static WRAPPED_LINK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\(([ \n])(http[^)]+)([\n ])\)").expect("valid regex"));

const VOID_ELEMENTS: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr",
];

enum Node {
    Text(String),
    Element {
        name: String,
        attrs: Vec<(String, String)>,
        children: Vec<Node>,
    },
}

impl Node {
    fn attr(&self, key: &str) -> Option<&str> {
        match self {
            Node::Element { attrs, .. } => attrs
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.as_str()),
            Node::Text(_) => None,
        }
    }

    /// Nokogiri's `text`: all descendant text.
    fn text(&self) -> String {
        match self {
            Node::Text(text) => text.clone(),
            Node::Element { children, .. } => children.iter().map(Node::text).collect(),
        }
    }
}

/// An open element: name, attributes and the children so far.
type OpenElement = (String, Vec<(String, String)>, Vec<Node>);

/// Builds the element tree from tokens. Unknown end tags are ignored; an end tag closes
/// the innermost open element of that name.
#[derive(Default)]
struct TreeSink {
    /// Open elements: (name, attrs, children). The first entry is the fragment root.
    stack: RefCell<Vec<OpenElement>>,
}

impl TreeSink {
    fn close_top(stack: &mut Vec<OpenElement>) {
        if stack.len() > 1
            && let Some((name, attrs, children)) = stack.pop()
            && let Some(parent) = stack.last_mut()
        {
            parent.2.push(Node::Element {
                name,
                attrs,
                children,
            });
        }
    }
}

impl TokenSink for TreeSink {
    type Handle = ();

    fn process_token(&self, token: Token, _line: u64) -> TokenSinkResult<()> {
        let mut stack = self.stack.borrow_mut();
        match token {
            Token::CharacterTokens(text) => {
                if let Some(top) = stack.last_mut() {
                    match top.2.last_mut() {
                        Some(Node::Text(existing)) => existing.push_str(&text),
                        _ => top.2.push(Node::Text(text.to_string())),
                    }
                }
            }
            Token::TagToken(tag) => {
                let name = tag.name.to_string();
                match tag.kind {
                    TagKind::StartTag => {
                        let attrs = tag
                            .attrs
                            .iter()
                            .map(|a| (a.name.local.to_string(), a.value.to_string()))
                            .collect();
                        if VOID_ELEMENTS.contains(&name.as_str()) || tag.self_closing {
                            if let Some(top) = stack.last_mut() {
                                top.2.push(Node::Element {
                                    name,
                                    attrs,
                                    children: Vec::new(),
                                });
                            }
                            return TokenSinkResult::Continue;
                        }
                        let raw = match name.as_str() {
                            "script" => Some(RawKind::ScriptData),
                            "style" | "xmp" | "iframe" | "noembed" | "noframes" => {
                                Some(RawKind::Rawtext)
                            }
                            "title" | "textarea" => Some(RawKind::Rcdata),
                            _ => None,
                        };
                        stack.push((name, attrs, Vec::new()));
                        if let Some(kind) = raw {
                            return TokenSinkResult::RawData(kind);
                        }
                    }
                    TagKind::EndTag => {
                        if let Some(position) = stack.iter().skip(1).rposition(|(n, ..)| *n == name)
                        {
                            while stack.len() > position + 1 {
                                Self::close_top(&mut stack);
                            }
                        }
                    }
                }
            }
            _ => {}
        }
        TokenSinkResult::Continue
    }
}

fn parse(html: &str) -> Vec<Node> {
    let sink = TreeSink::default();
    sink.stack
        .borrow_mut()
        .push((String::new(), Vec::new(), Vec::new()));
    let tokenizer = Tokenizer::new(sink, TokenizerOpts::default());
    let input = BufferQueue::default();
    input.push_back(html.into());
    let _ = tokenizer.feed(&input);
    tokenizer.end();
    let mut stack = tokenizer.sink.stack.borrow_mut();
    while stack.len() > 1 {
        TreeSink::close_top(&mut stack);
    }
    stack
        .pop()
        .map(|(_, _, children)| children)
        .unwrap_or_default()
}

/// `HtmlToPlainText.call(html)`.
#[must_use]
pub fn html_to_plain_text(html: &str) -> String {
    if html.trim().is_empty() {
        return String::new();
    }
    let cleaned = TEXT_HTML_BLOCK.replace_all(html, "");
    let mut result = process_nodes(&parse(&cleaned));

    result = result.replace("\r\n", "\n").replace('\r', "\n");
    result = NBSP.replace_all(&result, " ").into_owned();
    result = LEADING_SPACE.replace_all(&result, "\n").into_owned();
    result = TRAILING_SPACE.replace_all(&result, "\n").into_owned();
    result = BLANK_LINES.replace_all(&result, "\n\n").into_owned();

    result = word_wrap(&result);

    result = WRAPPED_LINK
        .replace_all(&result, |c: &regex::Captures<'_>| {
            format!(
                "{}( {} ){}",
                if &c[1] == "\n" { "\n" } else { "" },
                &c[2],
                if &c[3] == "\n" { "\n" } else { "" }
            )
        })
        .into_owned();

    result.trim().to_owned()
}

fn process_nodes(nodes: &[Node]) -> String {
    nodes.iter().map(process_node).collect()
}

fn process_node(node: &Node) -> String {
    let Node::Element { name, children, .. } = node else {
        return node.text();
    };
    match name.as_str() {
        "script" => String::new(),
        "br" => "\n".to_owned(),
        "p" | "div" => {
            let inner = process_nodes(children);
            if inner.trim().is_empty() {
                String::new()
            } else {
                format!("{inner}\n\n")
            }
        }
        "img" => node.attr("alt").unwrap_or_default().to_owned(),
        "a" => process_link(node, children),
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => process_heading(name, children),
        "li" => format!("* {}\n", process_nodes(children).trim()),
        _ => process_nodes(children),
    }
}

fn process_link(node: &Node, children: &[Node]) -> String {
    let text = process_nodes(children).trim().to_owned();
    if text.is_empty() {
        return String::new();
    }
    let href = node.attr("href").map(|href| {
        if href.len() >= 7 && href[..7].eq_ignore_ascii_case("mailto:") {
            &href[7..]
        } else {
            href
        }
    });
    match href {
        Some(href) if !text.eq_ignore_ascii_case(href.trim()) => {
            format!("{text} ( {} )", href.trim())
        }
        _ => text,
    }
}

fn process_heading(name: &str, children: &[Node]) -> String {
    let text: String = children
        .iter()
        .map(|child| match child {
            Node::Element { name, .. } if name == "br" => "\n".to_owned(),
            other => other.text(),
        })
        .collect();
    let text = text.trim();
    let hlength = text
        .lines()
        .map(|l| l.trim().chars().count())
        .max()
        .unwrap_or(0)
        .min(LINE_LENGTH);
    let decorated = match name {
        "h1" => format!("{0}\n{text}\n{0}", "*".repeat(hlength)),
        "h2" => format!("{0}\n{text}\n{0}", "-".repeat(hlength)),
        _ => format!("{text}\n{}", "-".repeat(hlength)),
    };
    format!("\n\n{decorated}\n\n")
}

/// `word_wrap`: lines over 65 characters break after the last whitespace that fits.
fn word_wrap(text: &str) -> String {
    text.split('\n')
        .map(|line| {
            if line.chars().count() > LINE_LENGTH {
                WRAP.replace_all(line, "${1}\n").trim().to_owned()
            } else {
                line.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_blocks_links_and_entities() {
        let text = html_to_plain_text(
            "<style>p{}</style><p>Hi&nbsp;Ana &amp; team</p><p><a href=\"https://x.test/s/1\">Sign now</a></p>",
        );
        assert_eq!(text, "p{}Hi Ana & team\n\nSign now ( https://x.test/s/1 )");
    }

    #[test]
    fn links_whose_text_is_the_url_stay_bare() {
        assert_eq!(
            html_to_plain_text("<p>Open <a href=\"https://x.test/s/1\">https://x.test/s/1</a></p>"),
            "Open https://x.test/s/1"
        );
        assert_eq!(
            html_to_plain_text("<a href=\"mailto:a@x.test\">A@x.test</a>"),
            "A@x.test"
        );
    }

    #[test]
    fn renders_headings_lists_and_scripts() {
        assert_eq!(
            html_to_plain_text(
                "<h1>Title</h1><ul><li> one </li><li>two</li></ul><script>x()</script>"
            ),
            "*****\nTitle\n*****\n\n* one\n* two"
        );
        assert_eq!(
            html_to_plain_text("<h3>Sub<br>head</h3>"),
            "Sub\nhead\n----"
        );
    }

    #[test]
    fn wraps_long_lines() {
        let text = html_to_plain_text(&format!("<p>{}</p>", "word ".repeat(20)));
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].len() <= 65);
    }
}
