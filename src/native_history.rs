//! What an agent CLI records about its own sessions.
//!
//! Codex and Claude Code both keep a transcript of every session they run, and
//! both name the session in it. muxloom reads those files in three places -
//! the resume picker, the backup index, and the session list - so the rules
//! for finding a name in one live here rather than in each reader.

use serde_json::Value;

/// The name Claude Code gives a session today.
///
/// It is written on a line of its own and rewritten as the conversation goes
/// on, so a reader wants the *last* one in the file, not the first.
pub fn claude_ai_title(value: &Value) -> Option<&str> {
    (value.get("type").and_then(Value::as_str) == Some("ai-title"))
        .then(|| value.get("aiTitle").and_then(Value::as_str))
        .flatten()
        .map(str::trim)
        .filter(|title| !title.is_empty())
}

/// The name older Claude Code builds gave a session: a compaction summary, or
/// a title the user typed. Written once, so the first one found is the one.
pub fn claude_legacy_title(value: &Value) -> Option<&str> {
    value
        .get("summary")
        .and_then(Value::as_str)
        .or_else(|| value.get("customTitle").and_then(Value::as_str))
        .map(str::trim)
        .filter(|title| !title.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json(line: &str) -> Value {
        serde_json::from_str(line).expect("valid json")
    }

    #[test]
    fn the_current_title_line_is_recognized_and_the_empty_one_ignored() {
        assert_eq!(
            claude_ai_title(&json(
                r#"{"type":"ai-title","aiTitle":" 优化触摸屏滑动体验 ","sessionId":"x"}"#
            )),
            Some("优化触摸屏滑动体验")
        );
        assert_eq!(
            claude_ai_title(&json(r#"{"type":"ai-title","aiTitle":"","sessionId":"x"}"#)),
            None
        );
        // A line that merely carries the field is not the title line.
        assert_eq!(
            claude_ai_title(&json(r#"{"type":"user","aiTitle":"not this"}"#)),
            None
        );
    }

    #[test]
    fn older_transcripts_still_give_up_their_title() {
        assert_eq!(
            claude_legacy_title(&json(r#"{"type":"summary","summary":"Ship the fix"}"#)),
            Some("Ship the fix")
        );
        assert_eq!(
            claude_legacy_title(&json(r#"{"customTitle":"Named by hand"}"#)),
            Some("Named by hand")
        );
        assert_eq!(claude_legacy_title(&json(r#"{"type":"user"}"#)), None);
    }
}
