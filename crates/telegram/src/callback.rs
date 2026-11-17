//! Compact callback data for inline keyboard buttons.
//!
//! Telegram limits `callback_data` to 64 bytes, so a button carries only `kind|id|choice`: what
//! the button belongs to (an approval, a question), the id of that thing, and the option picked.
//! Anything bigger stays on the orchestrator's side, looked up by `id`.

use crate::error::TgError;

/// Telegram's limit on `callback_data`, in bytes.
pub const MAX_CALLBACK_DATA: usize = 64;

const SEP: char = '|';

/// A decoded `kind|id|choice` triple, borrowing from the callback data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CallbackData<'a> {
    pub kind: &'a str,
    pub id: &'a str,
    pub choice: &'a str,
}

/// Encodes `kind|id|choice`. `kind` must be non-empty, `kind` and `id` must not contain `|`
/// (`choice`, being last, may), and the result must fit in 64 bytes.
pub fn encode(kind: &str, id: &str, choice: &str) -> Result<String, TgError> {
    if kind.is_empty() {
        return Err(TgError::CallbackData("kind is empty".into()));
    }
    if kind.contains(SEP) || id.contains(SEP) {
        return Err(TgError::CallbackData(format!(
            "kind and id must not contain '{SEP}'"
        )));
    }
    let data = format!("{kind}{SEP}{id}{SEP}{choice}");
    if data.len() > MAX_CALLBACK_DATA {
        return Err(TgError::CallbackData(format!(
            "{} bytes, over Telegram's {MAX_CALLBACK_DATA}-byte limit",
            data.len()
        )));
    }
    Ok(data)
}

/// Decodes data produced by [`encode`]. Callback data comes from the network and any client can
/// forge it, so every malformed shape is an error rather than a panic.
pub fn decode(data: &str) -> Result<CallbackData<'_>, TgError> {
    if data.len() > MAX_CALLBACK_DATA {
        return Err(TgError::CallbackData(format!(
            "{} bytes, over the limit",
            data.len()
        )));
    }
    let mut parts = data.splitn(3, SEP);
    match (parts.next(), parts.next(), parts.next()) {
        (Some(kind), Some(id), Some(choice)) if !kind.is_empty() => {
            Ok(CallbackData { kind, id, choice })
        }
        _ => Err(TgError::CallbackData(format!(
            "not of the form kind{SEP}id{SEP}choice: {data:?}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let data = encode("perm", "a1b2", "allow").expect("fits");
        assert_eq!(data, "perm|a1b2|allow");
        assert_eq!(
            decode(&data).expect("decodes"),
            CallbackData {
                kind: "perm",
                id: "a1b2",
                choice: "allow"
            }
        );
    }

    #[test]
    fn empty_id_and_choice_are_allowed() {
        let data = encode("menu", "", "").expect("fits");
        assert_eq!(
            decode(&data).expect("decodes"),
            CallbackData {
                kind: "menu",
                id: "",
                choice: ""
            }
        );
    }

    #[test]
    fn choice_may_contain_the_separator() {
        let data = encode("q", "7", "a|b").expect("fits");
        assert_eq!(decode(&data).expect("decodes").choice, "a|b");
    }

    #[test]
    fn exactly_64_bytes_is_accepted_65_is_not() {
        // "k|" + id + "|" + choice: 3 bytes of framing plus 61 bytes of payload.
        let id = "x".repeat(30);
        assert_eq!(encode("k", &id, &"y".repeat(31)).expect("fits").len(), 64);
        assert!(encode("k", &id, &"y".repeat(32)).is_err());
    }

    #[test]
    fn limit_is_in_bytes_not_chars() {
        // 31 Cyrillic letters are 62 bytes, so with the framing the data is 65 bytes long
        // although it is only 34 characters.
        assert!(encode("k", &"я".repeat(30), "").is_ok());
        assert!(encode("k", &"я".repeat(31), "").is_err());
    }

    #[test]
    fn rejects_bad_parts() {
        assert!(encode("", "id", "c").is_err());
        assert!(encode("a|b", "id", "c").is_err());
        assert!(encode("k", "i|d", "c").is_err());
    }

    #[test]
    fn decode_rejects_malformed_data() {
        assert!(decode("").is_err());
        assert!(decode("nokind").is_err());
        assert!(decode("k|only-id").is_err());
        assert!(decode("|id|choice").is_err());
        assert!(decode(&"k|".repeat(40)).is_err());
    }
}
