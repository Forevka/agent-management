//! Secret redaction, shared by both sides: the node scrubs agent output before it leaves the
//! machine, and the orchestrator scrubs agent-written text before passing it on.

/// Shorter values are not redacted: they would match ordinary text everywhere.
pub const MIN_REDACT_LEN: usize = 6;

/// Replaces every secret value in `text` with `[secret]`, both raw and as it appears inside a
/// JSON string (escaped quotes, backslashes, newlines). Values shorter than [`MIN_REDACT_LEN`]
/// are left alone, and longer values go first, so one that contains another is not left half
/// visible.
pub fn redact_secrets(text: &mut String, values: &[String]) {
    let mut values: Vec<&String> = values
        .iter()
        .filter(|v| v.len() >= MIN_REDACT_LEN)
        .collect();
    values.sort_by_key(|v| std::cmp::Reverse(v.len()));
    for v in values {
        if text.contains(v.as_str()) {
            *text = text.replace(v.as_str(), "[secret]");
        }
        let escaped = serde_json::to_string(v).unwrap_or_default();
        let escaped = escaped.trim_matches('"');
        if escaped != v && text.contains(escaped) {
            *text = text.replace(escaped, "[secret]");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_are_redacted_raw_and_json_escaped() {
        let values = vec!["hunter22".to_owned(), "line1\nline2\"q".to_owned()];
        let mut a = "token is hunter22!".to_owned();
        redact_secrets(&mut a, &values);
        assert_eq!(a, "token is [secret]!");
        let mut b = serde_json::json!({"out": "key: line1\nline2\"q"}).to_string();
        redact_secrets(&mut b, &values);
        assert!(!b.contains("line1"), "{b}");
        // Short values are ignored; a value containing another is redacted whole.
        let mut c = "pin 1234, key abcdef-long, also abcdef".to_owned();
        redact_secrets(
            &mut c,
            &["1234".into(), "abcdef".into(), "abcdef-long".into()],
        );
        assert_eq!(c, "pin 1234, key [secret], also [secret]");
    }
}
