use std::{
    io::{Read, Write},
    sync::mpsc,
    thread,
};

use anyhow::{Context, Result, bail};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::{
    bridge::{BridgePool, BridgeStream},
    debug,
    model::{Target, Transport},
    runtime::{
        SSH_CONNECTION_ATTEMPTS_OPTION, SSH_CONTROL_PERSIST_OPTION, SSH_SERVER_ALIVE_COUNT_OPTION,
        SSH_SERVER_ALIVE_INTERVAL_OPTION, is_managed_session_id, ssh_control_path,
    },
};

enum TerminalEvent {
    Output(Vec<u8>),
    Closed,
}

pub struct TerminalSession {
    parser: vt100::Parser,
    master: Option<Box<dyn MasterPty + Send>>,
    writer: Option<Box<dyn Write + Send>>,
    child: Option<Box<dyn Child + Send + Sync>>,
    events: Option<mpsc::Receiver<TerminalEvent>>,
    daemon: Option<DaemonTerminal>,
    closed: bool,
    width: u16,
    height: u16,
}

struct DaemonTerminal {
    stream: BridgeStream,
    bridges: BridgePool,
    target: Target,
    session_id: String,
}

impl TerminalSession {
    pub fn attach(target: &Target, session_id: &str, width: u16, height: u16) -> Result<Self> {
        if !is_managed_session_id(session_id) {
            bail!("refusing invalid Muxloom session id");
        }
        let width = width.max(20);
        let height = height.max(5);
        debug::log(
            "pty",
            format!(
                "attach start target={} session={session_id} size={width}x{height}; {}",
                target.id,
                debug::tty_state()
            ),
        );
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: height,
                cols: width,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("failed to open embedded PTY")?;

        let mut command = match &target.transport {
            Transport::Local => {
                let mut command = CommandBuilder::new("tmux");
                command.args([
                    "set-option",
                    "-t",
                    session_id,
                    "mouse",
                    "on",
                    ";",
                    "attach-session",
                    "-t",
                    session_id,
                ]);
                command
            }
            Transport::Ssh { alias } => {
                let mut command = CommandBuilder::new("ssh");
                let control_option = format!("ControlPath={}", ssh_control_path());
                let remote = format!(
                    "tmux set-option -t {session_id} mouse on \\; attach-session -t {session_id}"
                );
                command.args([
                    "-tt",
                    "-o",
                    "BatchMode=yes",
                    "-o",
                    "ControlMaster=auto",
                    "-o",
                    SSH_CONTROL_PERSIST_OPTION,
                    "-o",
                    &control_option,
                    "-o",
                    SSH_SERVER_ALIVE_INTERVAL_OPTION,
                    "-o",
                    SSH_SERVER_ALIVE_COUNT_OPTION,
                    "-o",
                    SSH_CONNECTION_ATTEMPTS_OPTION,
                    alias,
                    &remote,
                ]);
                command
            }
        };
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        command.env("TERM_PROGRAM", "muxloom");

        let child = pair
            .slave
            .spawn_command(command)
            .context("failed to start embedded tmux client")?;
        debug::log(
            "pty",
            format!(
                "attach child spawned target={} session={session_id} child_pid={:?}; {}",
                target.id,
                child.process_id(),
                debug::tty_state()
            ),
        );
        drop(pair.slave);
        let mut reader = pair
            .master
            .try_clone_reader()
            .context("failed to clone PTY reader")?;
        let writer = pair
            .master
            .take_writer()
            .context("failed to open PTY writer")?;
        let (event_tx, event_rx) = mpsc::channel();
        thread::spawn(move || {
            let mut buffer = vec![0; 16 * 1024];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => {
                        debug::log("pty", "reader reached EOF");
                        let _ = event_tx.send(TerminalEvent::Closed);
                        break;
                    }
                    Err(error) => {
                        debug::log("pty", format!("reader failed: {error}"));
                        let _ = event_tx.send(TerminalEvent::Closed);
                        break;
                    }
                    Ok(read) => {
                        if event_tx
                            .send(TerminalEvent::Output(buffer[..read].to_vec()))
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }
        });

        Ok(Self {
            parser: vt100::Parser::new(height, width, 0),
            master: Some(pair.master),
            writer: Some(writer),
            child: Some(child),
            events: Some(event_rx),
            daemon: None,
            closed: false,
            width,
            height,
        })
    }

    pub fn attach_daemon(
        bridges: BridgePool,
        target: &Target,
        session_id: &str,
        width: u16,
        height: u16,
    ) -> Result<Self> {
        if !crate::runtime::is_daemon_session_id(session_id) {
            bail!("refusing invalid muxloomd session id");
        }
        let width = width.max(20);
        let height = height.max(5);
        let stream = bridges.open_pty(target, session_id.into(), width, height)?;
        Ok(Self {
            parser: vt100::Parser::new(height, width, 0),
            master: None,
            writer: None,
            child: None,
            events: None,
            daemon: Some(DaemonTerminal {
                stream,
                bridges,
                target: target.clone(),
                session_id: session_id.into(),
            }),
            closed: false,
            width,
            height,
        })
    }

    pub fn drain(&mut self) -> bool {
        let mut changed = false;
        if let Some(daemon) = &mut self.daemon {
            while let Some(bytes) = daemon.stream.try_read() {
                self.parser.process(&bytes);
                changed = true;
            }
            if daemon.stream.is_closed() && !self.closed {
                self.closed = true;
                changed = true;
            }
        } else if let Some(events) = &self.events {
            while let Ok(event) = events.try_recv() {
                match event {
                    TerminalEvent::Output(bytes) => {
                        self.parser.process(&bytes);
                        changed = true;
                    }
                    TerminalEvent::Closed => {
                        self.closed = true;
                        changed = true;
                    }
                }
            }
        }
        changed
    }

    pub fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }

    pub fn is_closed(&self) -> bool {
        self.closed
    }

    pub fn resize(&mut self, width: u16, height: u16) -> Result<()> {
        let width = width.max(20);
        let height = height.max(5);
        if self.width == width && self.height == height {
            return Ok(());
        }
        if let Some(daemon) = &self.daemon {
            daemon
                .bridges
                .resize(&daemon.target, daemon.session_id.clone(), width, height)?;
        } else if let Some(master) = &self.master {
            master
                .resize(PtySize {
                    rows: height,
                    cols: width,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .context("failed to resize embedded PTY")?;
        }
        debug::log("pty", format!("resized to {width}x{height}"));
        resize_parser(&mut self.parser, height, width);
        self.width = width;
        self.height = height;
        Ok(())
    }

    pub fn write_key(&mut self, key: KeyEvent) -> Result<()> {
        if let Some(bytes) = encode_key(key, self.parser.screen().application_cursor()) {
            self.write(&bytes)?;
        }
        Ok(())
    }

    pub fn write_paste(&mut self, text: &str) -> Result<()> {
        let bracketed = self.parser.screen().bracketed_paste();
        let mut bytes = Vec::with_capacity(text.len() + usize::from(bracketed) * 12);
        if bracketed {
            bytes.extend_from_slice(b"\x1b[200~");
        }
        bytes.extend_from_slice(text.as_bytes());
        if bracketed {
            bytes.extend_from_slice(b"\x1b[201~");
        }
        self.write(&bytes)
    }

    pub fn write_mouse(&mut self, event: MouseEvent, column: u16, row: u16) -> Result<bool> {
        use vt100::{MouseProtocolEncoding, MouseProtocolMode};

        let screen = self.parser.screen();
        let mode = screen.mouse_protocol_mode();
        if mode == MouseProtocolMode::None {
            return Ok(false);
        }
        let (button, release) = match event.kind {
            MouseEventKind::Down(button) => (mouse_button(button), false),
            MouseEventKind::Up(button) if mode != MouseProtocolMode::Press => {
                (mouse_button(button), true)
            }
            MouseEventKind::Drag(button)
                if matches!(
                    mode,
                    MouseProtocolMode::ButtonMotion | MouseProtocolMode::AnyMotion
                ) =>
            {
                (mouse_button(button) + 32, false)
            }
            MouseEventKind::Moved if mode == MouseProtocolMode::AnyMotion => (35, false),
            _ => return Ok(false),
        };
        let mut code = button + mouse_modifier(event.modifiers);
        if release && screen.mouse_protocol_encoding() != MouseProtocolEncoding::Sgr {
            code = 3 + mouse_modifier(event.modifiers);
        }
        let x = column.saturating_add(1);
        let y = row.saturating_add(1);
        let bytes = match screen.mouse_protocol_encoding() {
            MouseProtocolEncoding::Sgr => {
                format!("\x1b[<{};{x};{y}{}", code, if release { 'm' } else { 'M' }).into_bytes()
            }
            MouseProtocolEncoding::Default => vec![
                0x1b,
                b'[',
                b'M',
                code.saturating_add(32),
                x.min(223) as u8 + 32,
                y.min(223) as u8 + 32,
            ],
            MouseProtocolEncoding::Utf8 => {
                let mut bytes = b"\x1b[M".to_vec();
                push_utf8_codepoint(&mut bytes, u32::from(code) + 32);
                push_utf8_codepoint(&mut bytes, u32::from(x) + 32);
                push_utf8_codepoint(&mut bytes, u32::from(y) + 32);
                bytes
            }
        };
        self.write(&bytes)?;
        Ok(true)
    }

    fn write(&mut self, bytes: &[u8]) -> Result<()> {
        if let Some(daemon) = &self.daemon {
            daemon.stream.write(bytes)
        } else {
            let writer = self
                .writer
                .as_mut()
                .context("embedded terminal has no writer")?;
            writer
                .write_all(bytes)
                .context("failed to write to embedded terminal")?;
            writer.flush().context("failed to flush embedded terminal")
        }
    }
}

fn resize_parser(parser: &mut vt100::Parser, height: u16, width: u16) {
    let (previous_height, previous_width) = parser.screen().size();
    if width < previous_width {
        // vt100 0.15 can leave the first half of a wide glyph in the new last
        // column when shrinking a row. A later erase then indexes one cell
        // past that row. Erase that boundary in both grids while it still has
        // a valid continuation cell. Keeping the parser also preserves mouse,
        // bracketed-paste, cursor, and other input modes.
        let alternate = parser.screen().alternate_screen();
        scrub_shrink_boundary(parser, previous_height, width);
        parser.process(if alternate {
            b"\x1b[?47l"
        } else {
            b"\x1b[?47h"
        });
        scrub_shrink_boundary(parser, previous_height, width);
        parser.process(if alternate {
            b"\x1b[?47h"
        } else {
            b"\x1b[?47l"
        });
    }
    parser.set_size(height, width);
}

fn scrub_shrink_boundary(parser: &mut vt100::Parser, rows: u16, new_width: u16) {
    use std::fmt::Write as _;

    if new_width == 0 {
        return;
    }
    let (cursor_row, cursor_col) = parser.screen().cursor_position();
    let mut sequence = String::with_capacity(usize::from(rows) * 12 + 16);
    for row in 1..=rows {
        let _ = write!(sequence, "\x1b[{row};{new_width}H\x1b[X");
    }
    let _ = write!(
        sequence,
        "\x1b[{};{}H",
        cursor_row.saturating_add(1),
        cursor_col
            .min(new_width.saturating_sub(1))
            .saturating_add(1)
    );
    parser.process(sequence.as_bytes());
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let Some(child) = self.child.as_mut() else {
            return;
        };
        debug::log(
            "pty",
            format!(
                "dropping attached client child_pid={:?}",
                child.process_id()
            ),
        );
        let _ = child.kill();
        let _ = child.wait();
        debug::log("pty", "attached client stopped");
    }
}

fn encode_key(key: KeyEvent, application_cursor: bool) -> Option<Vec<u8>> {
    if let KeyCode::Char(character) = key.code {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            let lower = character.to_ascii_lowercase();
            let byte = match lower {
                '@' | ' ' => 0,
                'a'..='z' => lower as u8 - b'a' + 1,
                '[' => 27,
                '\\' => 28,
                ']' => 29,
                '^' => 30,
                '_' => 31,
                '?' => 127,
                _ => return None,
            };
            return Some(vec![byte]);
        }
        let mut bytes = Vec::new();
        if key.modifiers.contains(KeyModifiers::ALT) {
            bytes.push(0x1b);
        }
        let mut encoded = [0; 4];
        bytes.extend_from_slice(character.encode_utf8(&mut encoded).as_bytes());
        return Some(bytes);
    }

    let modifiers = xterm_modifier(key.modifiers);
    let sequence = match key.code {
        KeyCode::Enter
            if key
                .modifiers
                .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) =>
        {
            // Ctrl-J is the portable terminal newline used by Codex, Claude,
            // and shells without triggering their normal Enter submission.
            "\n".into()
        }
        KeyCode::Enter => "\r".into(),
        KeyCode::Esc => "\x1b".into(),
        KeyCode::Backspace => "\x7f".into(),
        KeyCode::Tab => "\t".into(),
        KeyCode::BackTab => "\x1b[Z".into(),
        KeyCode::Up => cursor_sequence('A', modifiers, application_cursor),
        KeyCode::Down => cursor_sequence('B', modifiers, application_cursor),
        KeyCode::Right => cursor_sequence('C', modifiers, application_cursor),
        KeyCode::Left => cursor_sequence('D', modifiers, application_cursor),
        KeyCode::Home => cursor_sequence('H', modifiers, application_cursor),
        KeyCode::End => cursor_sequence('F', modifiers, application_cursor),
        KeyCode::Insert => tilde_sequence(2, modifiers),
        KeyCode::Delete => tilde_sequence(3, modifiers),
        KeyCode::PageUp => tilde_sequence(5, modifiers),
        KeyCode::PageDown => tilde_sequence(6, modifiers),
        KeyCode::F(number) => function_sequence(number, modifiers)?,
        _ => return None,
    };
    Some(sequence.into_bytes())
}

fn xterm_modifier(modifiers: KeyModifiers) -> u8 {
    1 + u8::from(modifiers.contains(KeyModifiers::SHIFT))
        + 2 * u8::from(modifiers.contains(KeyModifiers::ALT))
        + 4 * u8::from(modifiers.contains(KeyModifiers::CONTROL))
}

fn cursor_sequence(final_byte: char, modifier: u8, application_cursor: bool) -> String {
    if modifier == 1 {
        format!(
            "\x1b{}{final_byte}",
            if application_cursor { 'O' } else { '[' }
        )
    } else {
        format!("\x1b[1;{modifier}{final_byte}")
    }
}

fn mouse_button(button: MouseButton) -> u8 {
    match button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    }
}

fn mouse_modifier(modifiers: KeyModifiers) -> u8 {
    4 * u8::from(modifiers.contains(KeyModifiers::SHIFT))
        + 8 * u8::from(modifiers.contains(KeyModifiers::ALT))
        + 16 * u8::from(modifiers.contains(KeyModifiers::CONTROL))
}

fn push_utf8_codepoint(output: &mut Vec<u8>, value: u32) {
    if let Some(character) = char::from_u32(value) {
        let mut encoded = [0; 4];
        output.extend_from_slice(character.encode_utf8(&mut encoded).as_bytes());
    }
}

fn tilde_sequence(code: u8, modifier: u8) -> String {
    if modifier == 1 {
        format!("\x1b[{code}~")
    } else {
        format!("\x1b[{code};{modifier}~")
    }
}

fn function_sequence(number: u8, modifier: u8) -> Option<String> {
    let final_byte = match number {
        1 => Some('P'),
        2 => Some('Q'),
        3 => Some('R'),
        4 => Some('S'),
        _ => None,
    };
    if let Some(final_byte) = final_byte {
        return Some(if modifier == 1 {
            format!("\x1bO{final_byte}")
        } else {
            format!("\x1b[1;{modifier}{final_byte}")
        });
    }
    let code = match number {
        5 => 15,
        6 => 17,
        7 => 18,
        8 => 19,
        9 => 20,
        10 => 21,
        11 => 23,
        12 => 24,
        _ => return None,
    };
    Some(tilde_sequence(code, modifier))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shrinking_after_a_wide_glyph_at_the_boundary_stays_valid() {
        let mut parser = vt100::Parser::new(3, 141, 0);
        parser.process(b"\x1b[?1000h\x1b[?2004h\x1b[?1049h");
        parser.process(b"\x1b[1;139H");
        parser.process("界".as_bytes());

        resize_parser(&mut parser, 3, 139);
        parser.process(b"\x1b[1;139H\x1b[K");

        assert_eq!(parser.screen().size(), (3, 139));
        assert!(parser.screen().alternate_screen());
        assert!(parser.screen().bracketed_paste());
        assert_ne!(
            parser.screen().mouse_protocol_mode(),
            vt100::MouseProtocolMode::None
        );
    }

    #[test]
    fn encodes_control_and_modified_navigation() {
        assert_eq!(
            encode_key(
                KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
                false
            ),
            Some(vec![4])
        );
        assert_eq!(
            encode_key(KeyEvent::new(KeyCode::Up, KeyModifiers::CONTROL), false),
            Some(b"\x1b[1;5A".to_vec())
        );
    }

    #[test]
    fn modified_enter_inserts_a_newline() {
        assert_eq!(
            encode_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT), false),
            Some(b"\n".to_vec())
        );
        assert_eq!(
            encode_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT), false),
            Some(b"\n".to_vec())
        );
        assert_eq!(
            encode_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), false),
            Some(b"\r".to_vec())
        );
    }

    #[test]
    fn non_ascii_input_is_forwarded_as_utf8() {
        assert_eq!(
            encode_key(
                KeyEvent::new(KeyCode::Char('中'), KeyModifiers::NONE),
                false
            ),
            Some("中".as_bytes().to_vec())
        );
    }
}
