//! Content safety: UTF-8 safe truncation + sticky-note PII redaction.
//! Red line per CLAUDE.md: sticky-note content must never leak to plain logs.

/// Truncate to at most `max_bytes`, snapping down to a UTF-8 char boundary.
/// Returns the input unchanged when it already fits.
pub fn safe_text_prefix(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Redact common PII patterns for log output.
/// Current scope: sticky-note (便签) markers — expand as new red lines appear.
pub fn redact_for_log(s: &str) -> String {
    const STICKY_PREFIXES: &[&str] = &["[便签]", "[sticky]", "[STICKY]"];
    if STICKY_PREFIXES.iter().any(|p| s.starts_with(p)) {
        let prefix_end = s.char_indices().nth(1).map(|(i, _)| i).unwrap_or(0);
        return format!("{}<redacted {} bytes>", &s[..prefix_end], s.len());
    }
    s.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_no_truncation_needed() {
        assert_eq!(safe_text_prefix("abc", 10), "abc");
    }

    #[test]
    fn prefix_cjk_boundary() {
        // "你" = 3 bytes (0xE4 0xBD 0xA0); max_bytes=4 snaps down to 3
        assert_eq!(safe_text_prefix("你好世界", 4), "你");
    }

    #[test]
    fn prefix_emoji_snaps_to_zero() {
        // "🎉" = 4 bytes; max_bytes=2 cannot fit any char → empty
        assert_eq!(safe_text_prefix("🎉ok", 2), "");
    }

    #[test]
    fn prefix_long_ascii_within_limit() {
        let long = "ascii".repeat(1000);
        assert!(safe_text_prefix(&long, 100).len() <= 100);
    }

    #[test]
    fn redact_sticky_note_marker() {
        let result = redact_for_log("[便签] secret card 12345");
        assert!(
            result.contains("<redacted"),
            "expected <redacted in: {result}"
        );
        assert!(
            !result.contains("secret card 12345"),
            "PII must not appear in: {result}"
        );
    }

    #[test]
    fn redact_normal_line_unchanged() {
        assert_eq!(redact_for_log("normal log line"), "normal log line");
    }
}
