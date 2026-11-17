//! Helpers for Telegram's HTML parse mode and its length limit.
//!
//! Telegram counts message length in UTF-16 code units, so a string of emoji hits the limit at
//! half the characters a `str::len` or `chars().count()` check would allow.

/// The marker appended to truncated text. U+2026 is one UTF-16 unit.
const ELLIPSIS: char = '…';

/// Escapes text for HTML parse mode. Telegram requires only `&`, `<` and `>` to be escaped
/// outside attribute values.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            c => out.push(c),
        }
    }
    out
}

/// Length as Telegram counts it.
pub fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

/// Cuts `text` to at most `max` UTF-16 units, ending in "…" when anything was cut. Cuts fall on
/// character boundaries, so a surrogate pair is never split. Meant for plain text; for HTML use
/// [`truncate_html`], which does not cut through tags and entities.
pub fn truncate_for_message(text: &str, max: usize) -> String {
    if utf16_len(text) <= max {
        return text.to_owned();
    }
    let Some(budget) = max.checked_sub(ELLIPSIS.len_utf16()) else {
        return String::new();
    };
    let mut used = 0;
    let mut end = 0;
    for (i, c) in text.char_indices() {
        if used + c.len_utf16() > budget {
            break;
        }
        used += c.len_utf16();
        end = i + c.len_utf8();
    }
    let mut out = String::with_capacity(end + ELLIPSIS.len_utf8());
    out.push_str(&text[..end]);
    out.push(ELLIPSIS);
    out
}

/// Cuts HTML-formatted `text` to at most `max` UTF-16 units, ending in "…". Unlike
/// [`truncate_for_message`] it never cuts inside a tag or an `&entity;`, and it closes the tags
/// still open at the cut, because Telegram rejects the whole message on malformed HTML.
pub fn truncate_html(text: &str, max: usize) -> String {
    if utf16_len(text) <= max {
        return text.to_owned();
    }
    let mut open: Vec<&str> = Vec::new();
    // UTF-16 length of the closing tags `open` would need.
    let mut closers_len = 0;
    let mut used = 0;
    let mut end = 0;
    for token in tokens(text) {
        let token_len = utf16_len(token.text);
        let closers_after = match token.kind {
            TokenKind::Open(name) => closers_len + closer_len(name),
            TokenKind::Close(name) if open.contains(&name) => closers_len - closer_len(name),
            _ => closers_len,
        };
        if used + token_len + ELLIPSIS.len_utf16() + closers_after > max {
            break;
        }
        match token.kind {
            TokenKind::Open(name) => open.push(name),
            TokenKind::Close(name) => {
                if let Some(pos) = open.iter().rposition(|n| *n == name) {
                    open.remove(pos);
                }
            }
            TokenKind::Text => {}
        }
        closers_len = closers_after;
        used += token_len;
        end = token.end;
    }
    if used + ELLIPSIS.len_utf16() + closers_len > max {
        // Only reachable when `max` is too small for even the ellipsis.
        return String::new();
    }
    let mut out = String::with_capacity(end + 16);
    out.push_str(&text[..end]);
    out.push(ELLIPSIS);
    for name in open.iter().rev() {
        out.push_str("</");
        out.push_str(name);
        out.push('>');
    }
    out
}

fn closer_len(name: &str) -> usize {
    // `</` + name + `>`; tag names are ASCII, so bytes equal UTF-16 units.
    name.len() + 3
}

enum TokenKind<'a> {
    Text,
    Open(&'a str),
    Close(&'a str),
}

struct Token<'a> {
    kind: TokenKind<'a>,
    text: &'a str,
    end: usize,
}

/// Splits HTML into characters, whole tags and whole entities: the units a cut must not split.
fn tokens(text: &str) -> impl Iterator<Item = Token<'_>> {
    let mut pos = 0;
    std::iter::from_fn(move || {
        let rest = &text[pos..];
        let c = rest.chars().next()?;
        let len = match c {
            '<' => rest.find('>').map_or(1, |i| i + 1),
            '&' => rest
                .find(';')
                .filter(|&i| {
                    i > 1
                        && rest[1..i]
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || c == '#')
                })
                .map_or(1, |i| i + 1),
            c => c.len_utf8(),
        };
        let token_text = &rest[..len];
        pos += len;
        Some(Token {
            kind: tag_kind(token_text),
            text: token_text,
            end: pos,
        })
    })
}

fn tag_kind(token: &str) -> TokenKind<'_> {
    let Some(inner) = token.strip_prefix('<').and_then(|t| t.strip_suffix('>')) else {
        return TokenKind::Text;
    };
    let (closing, inner) = match inner.strip_prefix('/') {
        Some(rest) => (true, rest),
        None => (false, inner),
    };
    let name_end = inner
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
        .unwrap_or(inner.len());
    let name = &inner[..name_end];
    if name.is_empty() || !name.starts_with(|c: char| c.is_ascii_alphabetic()) {
        return TokenKind::Text;
    }
    if closing {
        TokenKind::Close(name)
    } else {
        TokenKind::Open(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_the_three_special_characters() {
        assert_eq!(escape("a < b && c > d"), "a &lt; b &amp;&amp; c &gt; d");
        assert_eq!(escape("\"quotes\" stay"), "\"quotes\" stay");
    }

    #[test]
    fn short_text_is_untouched() {
        assert_eq!(truncate_for_message("hello", 5), "hello");
        assert_eq!(truncate_for_message("", 0), "");
    }

    #[test]
    fn truncates_ascii_with_ellipsis() {
        let out = truncate_for_message("hello world", 6);
        assert_eq!(out, "hello…");
        assert_eq!(utf16_len(&out), 6);
    }

    #[test]
    fn counts_utf16_units_not_chars_or_bytes() {
        // Each emoji is one char, four UTF-8 bytes and two UTF-16 units.
        let text = "😀😀😀😀";
        assert_eq!(utf16_len(text), 8);
        let out = truncate_for_message(text, 6);
        // Two emoji (4 units) + ellipsis (1) fit; a third emoji would need 7.
        assert_eq!(out, "😀😀…");
        assert!(utf16_len(&out) <= 6);
        // An odd budget never splits a surrogate pair.
        assert_eq!(truncate_for_message(text, 4), "😀…");
    }

    #[test]
    fn cyrillic_counts_one_unit_per_letter() {
        let text = "привет мир";
        assert_eq!(truncate_for_message(text, 7), "привет…");
    }

    #[test]
    fn respects_telegram_limit_on_large_input() {
        let text = "ы😀".repeat(3000);
        let out = truncate_for_message(&text, crate::MAX_MESSAGE_LEN);
        assert!(utf16_len(&out) <= crate::MAX_MESSAGE_LEN);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn tiny_limits_do_not_panic() {
        assert_eq!(truncate_for_message("abc", 0), "");
        assert_eq!(truncate_for_message("abc", 1), "…");
        assert_eq!(truncate_html("<b>abc</b>", 0), "");
    }

    #[test]
    fn html_cut_closes_open_tags() {
        let out = truncate_html("<pre>0123456789</pre>", 16);
        assert_eq!(out, "<pre>0123…</pre>");
        assert_eq!(utf16_len(&out), 16);
    }

    #[test]
    fn html_cut_never_splits_entities_or_tags() {
        let out = truncate_html("ab &amp; cd", 6);
        assert_eq!(out, "ab …");
        let out = truncate_html("ab <a href=\"https://example.com\">link</a>", 10);
        assert_eq!(out, "ab …");
    }

    #[test]
    fn html_nested_tags_close_in_reverse_order() {
        let out = truncate_html("<b><i>bold italic text</i></b>", 22);
        assert_eq!(out, "<b><i>bold it…</i></b>");
        assert!(utf16_len(&out) <= 22);
    }

    #[test]
    fn html_stray_angle_brackets_are_text() {
        let out = truncate_html("1 < 2 & 3", 4);
        assert_eq!(out, "1 <…");
    }
}
