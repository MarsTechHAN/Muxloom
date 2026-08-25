use crate::model::AgentKind;

const MAX_RECAP_CHARS: usize = 180;

/// The last thing an agent said, read off a *rendered* screen.
///
/// Give this the terminal's contents, not the bytes that produced them: an
/// agent draws its window with cursor moves rather than newlines, so the raw
/// stream has no lines to read.
///
/// A shell has no turns to summarise. Whatever scrolled past last is a
/// command, a prompt, or half a build log, and dressing that up as a recap
/// only puts a misleading sentence under the session. Say nothing instead -
/// `read_screen` is how a terminal is read.
pub fn extract_recap(kind: AgentKind, output: &str) -> Option<String> {
    if kind == AgentKind::Terminal {
        return None;
    }
    let plain = strip_terminal_controls(output);
    let lines: Vec<_> = plain.lines().collect();

    for (index, line) in lines.iter().enumerate().rev() {
        if let Some(after_marker) = explicit_recap_content(line) {
            if let Some(recap) = clean_recap_text(after_marker) {
                return Some(recap);
            }
            if let Some(recap) = lines[index + 1..]
                .iter()
                .find_map(|next| clean_recap_text(next))
            {
                return Some(recap);
            }
        }
    }

    // Only a line the agent marked as its own. Anything else on the screen is
    // the frame around the conversation - a spinner, a token counter, a model
    // name, half a prompt the person is still typing - and none of that is
    // what the session is about. Saying nothing is the honest answer when the
    // last answer is not in view.
    let mut last_assistant = None;
    for line in &lines {
        if let Some(content) = assistant_line(kind, line)
            && let Some(content) = clean_recap_text(content)
        {
            last_assistant = Some(content);
        }
    }
    last_assistant
}

fn explicit_recap_content(line: &str) -> Option<&str> {
    let lowercase = line.to_lowercase();
    for marker in ["※ recap:", "※ recap："] {
        if let Some(index) = lowercase.find(marker) {
            return line.get(index + marker.len()..);
        }
    }
    None
}

fn assistant_line(kind: AgentKind, line: &str) -> Option<&str> {
    let line = line.trim_start_matches(|character: char| {
        character.is_whitespace() || matches!(character, '│' | '┃')
    });
    let content = match kind {
        AgentKind::Codex => line.strip_prefix('•').or_else(|| line.strip_prefix('●'))?,
        AgentKind::Claude => line.strip_prefix('⏺').or_else(|| line.strip_prefix('●'))?,
        // OpenCode and Pi mark a turn with a bullet too, so take any of the
        // three rather than guess which; a wrong guess only costs a recap.
        AgentKind::OpenCode | AgentKind::Pi => line
            .strip_prefix('•')
            .or_else(|| line.strip_prefix('●'))
            .or_else(|| line.strip_prefix('⏺'))?,
        AgentKind::Terminal => return None,
    }
    .trim_start();
    (!is_tool_or_status(kind, content)).then_some(content)
}

fn is_tool_or_status(kind: AgentKind, content: &str) -> bool {
    let lowercase = content.to_lowercase();
    let common = [
        "working (",
        "running…",
        "running...",
        "cooked for ",
        "esc to interrupt",
    ];
    if common.iter().any(|prefix| lowercase.starts_with(prefix)) {
        return true;
    }
    match kind {
        AgentKind::Codex => [
            "ran ",
            "explored ",
            "searched ",
            "read ",
            "edited ",
            "wrote ",
            "called ",
        ]
        .iter()
        .any(|prefix| lowercase.starts_with(prefix)),
        AgentKind::Claude => {
            let first = content.split_whitespace().next().unwrap_or_default();
            let tool_call = first
                .split_once('(')
                .is_some_and(|(name, _)| name.chars().all(|character| character.is_alphanumeric()));
            tool_call
                || [
                    "bash(",
                    "read(",
                    "edit(",
                    "write(",
                    "grep(",
                    "glob(",
                    "task(",
                    "webfetch(",
                    "websearch(",
                ]
                .iter()
                .any(|prefix| lowercase.starts_with(prefix))
        }
        // The shared prefixes above already cover the status lines these two
        // print; a tool call reads as `name(argument)` in both.
        AgentKind::OpenCode | AgentKind::Pi => {
            let first = content.split_whitespace().next().unwrap_or_default();
            first
                .split_once('(')
                .is_some_and(|(name, _)| name.chars().all(|character| character.is_alphanumeric()))
        }
        AgentKind::Terminal => true,
    }
}

fn clean_recap_text(value: &str) -> Option<String> {
    let mut result = String::new();
    let mut pending_space = false;
    for character in value.chars() {
        if character.is_control() || character.is_whitespace() {
            pending_space = !result.is_empty();
            continue;
        }
        if pending_space {
            result.push(' ');
            pending_space = false;
        }
        result.push(character);
        if result.chars().count() >= MAX_RECAP_CHARS {
            break;
        }
    }
    let result = result
        .trim_matches(|character: char| matches!(character, '│' | '┃'))
        .trim()
        .to_string();
    (result.chars().any(char::is_alphanumeric)).then_some(result)
}

fn strip_terminal_controls(output: &str) -> String {
    let mut plain = String::with_capacity(output.len());
    let mut characters = output.chars().peekable();
    while let Some(character) = characters.next() {
        if character != '\x1b' {
            if character == '\n' || !character.is_control() {
                plain.push(character);
            }
            continue;
        }
        match characters.peek().copied() {
            Some('[') => {
                characters.next();
                for next in characters.by_ref() {
                    if ('@'..='~').contains(&next) {
                        break;
                    }
                }
            }
            Some(']') => {
                characters.next();
                while let Some(next) = characters.next() {
                    if next == '\x07' {
                        break;
                    }
                    if next == '\x1b' && characters.peek() == Some(&'\\') {
                        characters.next();
                        break;
                    }
                }
            }
            Some(_) => {
                characters.next();
            }
            None => {}
        }
    }
    plain
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_recap_wins_and_is_plain_single_line_text() {
        let output = concat!(
            "\x1b[31m• older response\x1b[0m\n",
            "※ recap:\tYou're understanding the renderer\n",
            "• newer but less authoritative response\n"
        );
        let recap = extract_recap(AgentKind::Codex, output).unwrap();
        assert_eq!(recap, "You're understanding the renderer");
        assert!(!recap.contains('\t'));
        assert!(!recap.chars().any(char::is_control));
    }

    #[test]
    fn codex_falls_back_to_last_model_reply_not_tool_or_prompt() {
        let output = concat!(
            "› Run sleep 8 and explain the result\n",
            "• I’ll verify the behavior first.\n",
            "• Ran sleep 8\n  └ (no output)\n",
            "• The renderer now preserves the selected width across restarts.\n",
            "────────────────────\n",
            "› Write tests for @filename\n",
            "gpt-5.6-sol xhigh · /work\n"
        );
        assert_eq!(
            extract_recap(AgentKind::Codex, output).as_deref(),
            Some("The renderer now preserves the selected width across restarts.")
        );
    }

    #[test]
    fn claude_falls_back_to_last_model_reply_not_tool_call() {
        let output = concat!(
            "❯ Inspect the project\n",
            "⏺ I’ll inspect the relevant files.\n",
            "⏺ Read(src/app.rs)\n  ⎿  120 lines\n",
            "⏺ The issue comes from reusing stale preview state.\n",
            "✻ Cooked for 4s\n",
            "❯ \nmanual mode on · ? for shortcuts\n"
        );
        assert_eq!(
            extract_recap(AgentKind::Claude, output).as_deref(),
            Some("The issue comes from reusing stale preview state.")
        );
    }

    #[test]
    fn a_shell_never_gets_a_recap() {
        let output = concat!(
            "<Trial 368492514 worker_0> tiger $ cargo build\n",
            "   Compiling muxloom v0.5.4\n",
            "<Trial 368492514 worker_0> tiger $ \n"
        );
        assert_eq!(extract_recap(AgentKind::Terminal, output), None);
    }

    /// Every line here was once shown as a recap under a live session: a
    /// spinner, a prompt caught mid-keystroke, and the status bar. None of
    /// them is anything the agent said.
    #[test]
    fn the_frame_around_a_conversation_is_never_the_recap() {
        let screen = concat!(
            "✻ Whirlpooling… (21s · ↓ 25 tokens · thought for 17s)\n",
            "❯ 你看看Arxiv AI Reader\n",
            "⏸ manual mode on · ? for shortcuts ◉ xhigh · /effort\n"
        );
        assert_eq!(extract_recap(AgentKind::Claude, screen), None);

        let status =
            "↑72M ↓191k 34.2%/1.0M (auto) (sglang-soil) /dev/shm/models/Flash-0731 • high\n";
        assert_eq!(extract_recap(AgentKind::Pi, status), None);
    }

    #[test]
    fn formatting_only_output_has_no_recap() {
        assert_eq!(
            extract_recap(AgentKind::Codex, "\t\n────────\n│\x1b[0m│\n"),
            None
        );
    }
}
