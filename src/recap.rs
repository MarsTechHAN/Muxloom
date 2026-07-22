use crate::model::AgentKind;

const MAX_RECAP_CHARS: usize = 180;

pub fn extract_recap(kind: AgentKind, output: &str) -> Option<String> {
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

    let mut last_assistant = None;
    for line in &lines {
        if let Some(content) = assistant_line(kind, line)
            && let Some(content) = clean_recap_text(content)
        {
            last_assistant = Some(content);
        }
    }
    last_assistant.or_else(|| {
        lines
            .iter()
            .rev()
            .filter(|line| !is_terminal_chrome(line))
            .find_map(|line| clean_recap_text(line))
    })
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
        AgentKind::Terminal => true,
    }
}

fn is_terminal_chrome(line: &str) -> bool {
    let line = line.trim();
    if line.is_empty()
        || explicit_recap_content(line).is_some()
        || line.starts_with(['›', '❯', '>', '└', '⎿'])
        || line.chars().all(|character| {
            character.is_whitespace()
                || matches!(
                    character,
                    '─' | '━'
                        | '│'
                        | '┃'
                        | '┌'
                        | '┐'
                        | '└'
                        | '┘'
                        | '╭'
                        | '╮'
                        | '╰'
                        | '╯'
                )
        })
    {
        return true;
    }
    let lowercase = line.to_lowercase();
    [
        "pane is dead",
        "gpt-",
        "tokens left",
        "esc to interrupt",
        "for shortcuts",
        "manual mode on",
        "press enter",
        "tip:",
        "working (",
        "running…",
        "running...",
    ]
    .iter()
    .any(|marker| lowercase.contains(marker))
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
    fn formatting_only_output_has_no_recap() {
        assert_eq!(
            extract_recap(AgentKind::Codex, "\t\n────────\n│\x1b[0m│\n"),
            None
        );
    }
}
