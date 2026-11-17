//! Minimal Slack mrkdwn to plain text.
//!
//! Slack escapes `&`, `<` and `>` in message text, so every literal `<...>` in the raw text is
//! markup: a mention, a channel, a special command or a link. Those are rewritten to something a
//! person (or a model) reads naturally, and the rest is unescaped. Formatting such as `*bold*` is
//! left alone, because it reads fine as is and stripping it could change meaning.

/// Converts mrkdwn to plain text. `user` and `channel` look up names by id; a mention whose name
/// is unknown keeps its id, so nothing is ever dropped.
pub fn to_plain(
    text: &str,
    user: impl Fn(&str) -> Option<String>,
    channel: impl Fn(&str) -> Option<String>,
) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('<') {
        out.push_str(&unescape(&rest[..start]));
        let after = &rest[start + 1..];
        match after.find('>') {
            Some(end) => {
                out.push_str(&token(&after[..end], &user, &channel));
                rest = &after[end + 1..];
            }
            None => {
                // An unclosed bracket is not markup; keep it verbatim.
                out.push_str(&unescape(&rest[start..]));
                rest = "";
            }
        }
    }
    out.push_str(&unescape(rest));
    out
}

/// The user ids mentioned as `<@U…>`, so their names can be looked up before converting.
pub fn mentioned_users(text: &str) -> Vec<String> {
    let mut ids = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("<@") {
        let after = &rest[start + 2..];
        let Some(end) = after.find('>') else { break };
        let inner = &after[..end];
        let (id, label) = split_label(inner);
        if label.is_none() && !id.is_empty() && !ids.iter().any(|i| i == id) {
            ids.push(id.to_owned());
        }
        rest = &after[end + 1..];
    }
    ids
}

fn token(
    inner: &str,
    user: &impl Fn(&str) -> Option<String>,
    channel: &impl Fn(&str) -> Option<String>,
) -> String {
    let (target, label) = split_label(inner);
    let label = label.map(unescape);
    if let Some(id) = target.strip_prefix('@') {
        let name = label.or_else(|| user(id)).unwrap_or_else(|| id.to_owned());
        return format!("@{}", name.trim_start_matches('@'));
    }
    if let Some(id) = target.strip_prefix('#') {
        let name = label
            .or_else(|| channel(id))
            .unwrap_or_else(|| id.to_owned());
        return format!("#{}", name.trim_start_matches('#'));
    }
    if let Some(cmd) = target.strip_prefix('!') {
        // <!here>, <!channel>, <!everyone>, <!subteam^S123|@team>, <!date^…|fallback>.
        if let Some(label) = label {
            return label;
        }
        let word = cmd.split('^').next().unwrap_or(cmd);
        return format!("@{word}");
    }
    let url = unescape(target);
    match label {
        Some(label) if !label.is_empty() && label != url => format!("{label} ({url})"),
        _ => url,
    }
}

fn split_label(inner: &str) -> (&str, Option<&str>) {
    match inner.split_once('|') {
        Some((target, label)) => (target, Some(label)),
        None => (inner, None),
    }
}

/// Undoes Slack's three escapes. `&amp;` goes last so that an escaped `&lt;` stays literal.
pub fn unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_owned();
    }
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conv(s: &str) -> String {
        to_plain(
            s,
            |id| (id == "U1").then(|| "alice".to_owned()),
            |id| (id == "C9").then(|| "ops".to_owned()),
        )
    }

    #[test]
    fn mentions_channels_and_links() {
        assert_eq!(conv("hi <@U1> and <@U2>"), "hi @alice and @U2");
        assert_eq!(
            conv("<@U3|bob> see <#C1|general> <#C9>"),
            "@bob see #general #ops"
        );
        assert_eq!(
            conv("<https://x.io/a?b=1&amp;c=2|the docs>"),
            "the docs (https://x.io/a?b=1&c=2)"
        );
        assert_eq!(conv("<https://x.io>"), "https://x.io");
        assert_eq!(conv("<https://x.io|https://x.io>"), "https://x.io");
        assert_eq!(
            conv("<!here> <!subteam^S1|@devs> <!channel>"),
            "@here @devs @channel"
        );
    }

    #[test]
    fn unescapes_and_survives_odd_input() {
        assert_eq!(conv("a &lt;b&gt; &amp;amp; &amp;lt;"), "a <b> &amp; &lt;");
        assert_eq!(conv("dangling < bracket"), "dangling < bracket");
        assert_eq!(conv("<>"), "");
        assert_eq!(conv("émoji 🚀 <@U1>"), "émoji 🚀 @alice");
    }

    #[test]
    fn finds_mentions_needing_lookup() {
        assert_eq!(
            mentioned_users("<@U1> <@U2|x> <@U1> <@U3"),
            vec!["U1".to_owned()]
        );
    }
}
