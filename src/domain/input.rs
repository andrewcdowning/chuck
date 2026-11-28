//! Rules for what a client may send, independent of how it arrived.

use super::schedule::REPEATS;

/// Longest card text, in bytes.
pub const MAX_BODY: usize = 20_000;

/// Most attachments one card can hold.
pub const MAX_ATTACHMENTS_PER_NOTE: i64 = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputError {
    EmptyBody,
    BodyTooLong,
    BadRepeat,
}

/// Trims the text and checks it. `allow_empty`: a card may have no text when it holds media.
pub fn checked_body(body: &str, repeat: Option<&str>, allow_empty: bool) -> Result<String, InputError> {
    let body = body.trim().to_string();
    if body.is_empty() && !allow_empty {
        return Err(InputError::EmptyBody);
    }
    if body.len() > MAX_BODY {
        return Err(InputError::BodyTooLong);
    }
    if let Some(r) = repeat {
        if !r.is_empty() && !REPEATS.contains(&r) {
            return Err(InputError::BadRepeat);
        }
    }
    Ok(body)
}

/// File names are display-only: keep the last path component, drop control characters, cap the length.
pub fn clean_name(n: Option<String>) -> Option<String> {
    let n = n?;
    let base = n.rsplit(['/', '\\']).next().unwrap_or("");
    let s: String = base.chars().filter(|c| !c.is_control()).take(200).collect();
    let s = s.trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// Whether a card holding `count` attachments has room for one more.
pub fn has_room_for_attachment(count: i64) -> bool {
    count < MAX_ATTACHMENTS_PER_NOTE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_rules() {
        assert_eq!(checked_body("  hi \n", None, false).unwrap(), "hi");
        assert_eq!(checked_body("  ", None, false), Err(InputError::EmptyBody));
        assert_eq!(checked_body("  ", None, true).unwrap(), "", "media cards may have no text");
        assert_eq!(checked_body(&"x".repeat(MAX_BODY + 1), None, false), Err(InputError::BodyTooLong));
        assert!(checked_body(&"x".repeat(MAX_BODY), None, false).is_ok());
        assert_eq!(checked_body("a", Some("hourly"), false), Err(InputError::BadRepeat));
        assert!(checked_body("a", Some("weekdays"), false).is_ok() && checked_body("a", Some(""), false).is_ok());
    }

    #[test]
    fn file_names() {
        assert_eq!(clean_name(Some("C:\\x\\a.jpg".into())).as_deref(), Some("a.jpg"));
        assert_eq!(clean_name(Some("../../etc/passwd".into())).as_deref(), Some("passwd"));
        assert_eq!(clean_name(Some("a\u{7}b.png".into())).as_deref(), Some("ab.png"));
        assert_eq!(clean_name(Some("dir/".into())), None);
        assert_eq!(clean_name(None), None);
        assert_eq!(clean_name(Some("x".repeat(500))).unwrap().len(), 200);
    }

    #[test]
    fn attachment_room() {
        assert!(has_room_for_attachment(MAX_ATTACHMENTS_PER_NOTE - 1));
        assert!(!has_room_for_attachment(MAX_ATTACHMENTS_PER_NOTE));
    }
}
