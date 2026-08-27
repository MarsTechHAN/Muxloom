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

/// Rows of rendered scrollback the embedded emulator retains. Scrolling reads
/// from this buffer so back-scroll shows the emulator's actual lines instead of
/// linearizing the raw output log, which mangles live-redrawing TUIs. vt100
/// grows the buffer lazily, so this cap only bounds a long-lived session.
const SCROLLBACK_LINES: usize = 20_000;

/// Rows of rendered scrollback to ask the daemon for when attaching.
///
/// The raw output the daemon retains repaints the screen but is a poor source
/// of history: a redraw-heavy agent can spend the whole 2 MiB ring on frames
/// that commit only a handful of transcript lines, which left a fresh attach
/// with less than a screenful to page through. The daemon renders these rows
/// from the session's full log instead, so scrolling starts out deep.
pub(crate) const SCROLLBACK_SEED_ROWS: usize = 2_000;

/// Rows a seed may carry at most, whatever a client asks for. Bounds both the
/// daemon's transient emulator and the bytes an attach puts on the wire.
#[cfg(test)]
pub(crate) const SCROLLBACK_SEED_ROWS_LIMIT: usize = 5_000;

pub struct TerminalSession {
    parser: vt100::Parser,
    inline: InlineScrollback,
    codex_activity: CodexActivity,
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
            parser: vt100::Parser::new(height, width, SCROLLBACK_LINES),
            inline: InlineScrollback::default(),
            codex_activity: CodexActivity::default(),
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
        let stream = bridges.open_pty(
            target,
            session_id.into(),
            width,
            height,
            SCROLLBACK_SEED_ROWS,
        )?;
        Ok(Self {
            parser: vt100::Parser::new(height, width, SCROLLBACK_LINES),
            inline: InlineScrollback::default(),
            codex_activity: CodexActivity::default(),
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
                self.codex_activity.process(&bytes);
                self.inline.process(&mut self.parser, &bytes);
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
                        self.codex_activity.process(&bytes);
                        self.inline.process(&mut self.parser, &bytes);
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

    pub fn codex_working_hint(&self) -> Option<bool> {
        self.codex_activity.working()
    }

    /// A session with no process behind it, for tests that only exercise the
    /// emulator side.
    #[cfg(test)]
    pub(crate) fn detached(width: u16, height: u16) -> Self {
        Self {
            parser: vt100::Parser::new(height, width, SCROLLBACK_LINES),
            inline: InlineScrollback::default(),
            codex_activity: CodexActivity::default(),
            master: None,
            writer: None,
            child: None,
            events: None,
            daemon: None,
            closed: false,
            width,
            height,
        }
    }

    #[cfg(test)]
    pub(crate) fn process_output_for_test(&mut self, bytes: &[u8]) {
        self.inline.process(&mut self.parser, bytes);
    }

    /// The text on the live screen, whatever the view is scrolled to. Reading
    /// [`Self::screen`] instead answers with the rows the user is looking at,
    /// which is the past — and past a screenful of scrollback, vt100 0.15 reads
    /// them through a subtraction that underflows and returns more rows than
    /// the screen has.
    pub fn live_contents(&mut self) -> String {
        let offset = self.parser.screen().scrollback();
        if offset == 0 {
            return self.parser.screen().contents();
        }
        self.parser.set_scrollback(0);
        let contents = self.parser.screen().contents();
        self.parser.set_scrollback(offset);
        contents
    }

    /// Move the visible window `rows` lines up into rendered scrollback (0 is the
    /// live bottom). vt100 clamps to what the buffer actually holds; read the
    /// applied position back with [`Self::scrollback`].
    pub fn set_scrollback(&mut self, rows: usize) {
        self.parser.set_scrollback(rows);
    }

    /// The current scrollback offset in rows from the live bottom.
    pub fn scrollback(&self) -> usize {
        self.parser.screen().scrollback()
    }

    /// The deepest scrollback offset the emulator currently retains. It is 0
    /// when the screen has no buffered history — an agent painting a
    /// self-contained view on the alternate screen, say — rather than flowing
    /// finished lines off the top. The live view is restored before returning,
    /// so this reads as a side-effect-free query.
    pub fn max_scrollback(&mut self) -> usize {
        let current = self.parser.screen().scrollback();
        self.parser.set_scrollback(usize::MAX);
        let max = self.parser.screen().scrollback();
        self.parser.set_scrollback(current);
        max
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
            // Fire and forget: the controller resizes from the render loop, so
            // waiting for the daemon's acknowledgement would stall every pane
            // for a full round trip whenever the layout changes.
            daemon.bridges.resize_detached(
                &daemon.target,
                daemon.session_id.clone(),
                width,
                height,
            )?;
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
        self.inline.reset();
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
        let screen = self.parser.screen();
        let Some(bytes) = mouse_report(
            screen.mouse_protocol_mode(),
            screen.mouse_protocol_encoding(),
            event,
            column,
            row,
        ) else {
            return Ok(false);
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

/// Tracks Codex's terminal-title spinner independently from its screen redraw.
/// The title is a stable activity signal even while the visible `Working` text
/// is being erased and repainted one character at a time.
#[derive(Debug, Default)]
pub(crate) struct CodexActivity {
    tail: Vec<u8>,
    working: Option<bool>,
}

impl CodexActivity {
    const TAIL_LIMIT: usize = 2_048;

    pub(crate) fn process(&mut self, bytes: &[u8]) {
        self.tail.extend_from_slice(bytes);
        let mut cursor = 0;
        let mut latest = None;
        while let Some(relative) = self.tail[cursor..]
            .windows(4)
            .position(|window| window == b"\x1b]0;")
        {
            let start = cursor + relative + 4;
            let Some((end, terminator)) = osc_terminator(&self.tail[start..]) else {
                break;
            };
            let title = String::from_utf8_lossy(&self.tail[start..start + end]);
            latest = Some(
                title
                    .trim_start()
                    .chars()
                    .next()
                    .is_some_and(|character| ('\u{2800}'..='\u{28ff}').contains(&character)),
            );
            cursor = start + end + terminator;
        }
        if latest.is_some() {
            self.working = latest;
        }
        if self.tail.len() > Self::TAIL_LIMIT {
            self.tail.drain(..self.tail.len() - Self::TAIL_LIMIT);
        }
    }

    pub(crate) fn working(&self) -> Option<bool> {
        self.working
    }
}

fn osc_terminator(bytes: &[u8]) -> Option<(usize, usize)> {
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == 0x07 {
            return Some((index, 1));
        }
        if *byte == 0x1b && bytes.get(index + 1) == Some(&b'\\') {
            return Some((index, 2));
        }
    }
    None
}

/// Keeps the scrollback of agents that pin a footer with a scroll region.
///
/// Codex prints its transcript by setting a top-anchored region (`ESC[1;Nr`),
/// parking the cursor on that region's last row and emitting a newline, so the
/// finished line leaves the top of the screen while the composer below stays
/// put. Terminals put that line in their scrollback because the region starts
/// at the first row; vt100 only fills scrollback when the region spans the
/// whole screen, so for these agents nothing was ever buffered and paging had
/// nothing to show. Rewrite each such scroll as a whole-screen scroll — which
/// vt100 does record — followed by an insert that puts the footer back exactly
/// where the agent painted it.
///
/// Only newline-driven scrolls are rewritten. `ESC[S` and a line wrapping past
/// the last column also scroll a region, but no agent muxloom drives prints its
/// transcript that way, and both stay on vt100's own path.
#[derive(Debug, Default)]
pub(crate) struct InlineScrollback {
    scan: Scan,
    params: Vec<u8>,
    /// Set while the agent holds a region that starts at the first row and ends
    /// above the last one, holding that region's 0-based last row.
    footer_top: Option<u16>,
    /// The region the agent last installed, as 0-based inclusive rows. Kept
    /// whole — not just the footer form — so a seed can hand the region to
    /// another emulator that never saw the `ESC[...r` that set it.
    region: Option<(u16, u16)>,
    private: bool,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum Scan {
    #[default]
    Ground,
    Escape,
    Csi,
    /// An OSC/DCS/APC string, which runs until BEL or ST and may hold bytes
    /// that would otherwise read as newlines.
    Str,
    StrEscape,
}

impl InlineScrollback {
    /// Forget the tracked region. Resizing clamps vt100's own region in ways an
    /// agent is about to overwrite anyway, and guessing wrong would move rows
    /// the agent never asked to move, so start over and rewrite nothing until
    /// the next `ESC[...r`.
    fn reset(&mut self) {
        self.scan = Scan::Ground;
        self.params.clear();
        self.private = false;
        self.footer_top = None;
        self.region = None;
    }

    pub(crate) fn process(&mut self, parser: &mut vt100::Parser, bytes: &[u8]) {
        let mut flushed = 0;
        for index in 0..bytes.len() {
            if !self.step(parser, bytes[index]) {
                continue;
            }
            parser.process(&bytes[flushed..index]);
            flushed = index;
            if let Some(sequence) = self.scroll_rewrite(parser) {
                parser.process(&sequence);
                // The rewrite already scrolled; letting the newline through
                // would scroll a second time.
                flushed = index + 1;
            }
        }
        parser.process(&bytes[flushed..]);
    }

    /// Advance the scanner by one byte, reporting whether it is a newline that
    /// may need rewriting.
    fn step(&mut self, parser: &vt100::Parser, byte: u8) -> bool {
        match self.scan {
            Scan::Ground => match byte {
                0x1b => {
                    self.scan = Scan::Escape;
                    false
                }
                // LF, VT and FF all index the cursor down one row.
                b'\n' | 0x0b | 0x0c => true,
                _ => false,
            },
            Scan::Escape => {
                self.scan = match byte {
                    b'[' => {
                        self.params.clear();
                        self.private = false;
                        Scan::Csi
                    }
                    b']' | b'P' | b'X' | b'^' | b'_' => Scan::Str,
                    _ => Scan::Ground,
                };
                false
            }
            Scan::Csi => match byte {
                // Private markers and intermediates: anything carrying them is
                // not the plain DECSTBM we act on.
                0x3c..=0x3f | 0x20..=0x2f => {
                    self.private = true;
                    false
                }
                0x30..=0x3b => {
                    self.params.push(byte);
                    false
                }
                0x40..=0x7e => {
                    self.scan = Scan::Ground;
                    if byte == b'r' && !self.private {
                        self.track_region(parser);
                    }
                    false
                }
                _ => false,
            },
            Scan::Str => {
                self.scan = match byte {
                    0x07 => Scan::Ground,
                    0x1b => Scan::StrEscape,
                    _ => Scan::Str,
                };
                false
            }
            Scan::StrEscape => {
                self.scan = if byte == b'\\' {
                    Scan::Ground
                } else {
                    Scan::Str
                };
                false
            }
        }
    }

    /// Record the region a `DECSTBM` just installed, canonicalised the way
    /// vt100 canonicalises it so the two never disagree about what is active.
    fn track_region(&mut self, parser: &vt100::Parser) {
        let (rows, _) = parser.screen().size();
        let mut params = self
            .params
            .split(|byte| *byte == b';')
            .map(|part| std::str::from_utf8(part).ok()?.parse::<u16>().ok());
        let top = params.next().flatten().unwrap_or(0).max(1);
        let bottom = match params.next().flatten().unwrap_or(0) {
            0 => rows,
            bottom => bottom,
        };
        let top = top - 1;
        let bottom = bottom.saturating_sub(1).min(rows.saturating_sub(1));
        self.region = (top < bottom).then_some((top, bottom));
        self.footer_top = (top == 0 && top < bottom && bottom + 1 < rows).then_some(bottom);
    }

    /// The `DECSTBM` that reinstalls the tracked region, for handing an
    /// emulator the region without replaying the stream that set it.
    #[cfg(any(unix, test))]
    pub(crate) fn region_sequence(&self) -> Option<String> {
        let (top, bottom) = self.region?;
        Some(format!("\x1b[{};{}r", top + 1, bottom + 1))
    }

    /// The sequence that performs the pending scroll while keeping the line
    /// that leaves the top of the screen, or `None` when vt100 already keeps it.
    fn scroll_rewrite(&self, parser: &vt100::Parser) -> Option<Vec<u8>> {
        let bottom = self.footer_top?;
        let screen = parser.screen();
        if screen.alternate_screen() {
            // The alternate screen has no scrollback anywhere.
            return None;
        }
        let (rows, _) = screen.size();
        if bottom + 1 >= rows {
            return None;
        }
        let (row, column) = screen.cursor_position();
        if row != bottom {
            // Not at the region's last row, so this newline only moves the
            // cursor and no line leaves the screen.
            return None;
        }
        // Scroll the whole screen so the top row is buffered, then insert the
        // row the region scroll would have left blank, which pushes the footer
        // back down and drops the blank row the whole-screen scroll added at
        // the bottom. Both region changes home the cursor, so restore it last.
        Some(
            format!(
                "\x1b[r\x1b[{rows};1H\n\
                 \x1b[{footer};{rows}r\x1b[{footer};1H\x1b[L\
                 \x1b[1;{footer}r\x1b[{};{}H",
                row + 1,
                column + 1,
                footer = bottom + 1,
            )
            .into_bytes(),
        )
    }
}

/// Render `stream` — a session's older raw output — into bytes that refill an
/// attaching emulator's scrollback with the lines that scrolled off it.
///
/// The caller replays the newest raw output itself to repaint the screen, so
/// `stream` must stop where that replay begins: everything still on screen at
/// that point is the replay's to redraw, and everything above it comes back as
/// the rendered rows returned here. Feeding those rows to a fresh emulator ahead
/// of the replay leaves it with the same history a terminal that had watched the
/// whole session would hold.
///
/// Kept under `#[cfg(test)]`: an attach no longer seeds scrollback (it sends
/// the live snapshot and pages history on demand), so nothing in the product
/// calls this any more — its tests are the only reason it stays.
#[cfg(test)]
pub(crate) fn render_scrollback_seed(
    stream: impl Read,
    columns: u16,
    rows: u16,
    keep: usize,
) -> Result<Vec<u8>> {
    let columns = columns.max(20);
    let rows = rows.max(5);
    let keep = keep.min(SCROLLBACK_SEED_ROWS_LIMIT);
    if keep == 0 {
        return Ok(Vec::new());
    }
    let (mut parser, inline) = replay_history(stream, columns, rows, keep)?;
    if parser.screen().alternate_screen() {
        // vt100 only reaches the scrollback of the grid it is showing, and a
        // full-screen app is drawn on one that has none. Step off it so the
        // history underneath is what gets seeded; the client's own replay of
        // the newest raw output repaints the app over it.
        parser.process(b"\x1b[?1049l");
    }
    let (cursor_row, cursor_column) = parser.screen().cursor_position();
    let input_modes = parser.screen().input_mode_formatted();
    // Growing the screen below resizes its rows, and vt100 drops a row's wrap
    // flag whenever it is resized, so read them off while they are still set.
    let screen_wraps: Vec<bool> = (0..rows)
        .map(|row| parser.screen().row_wrapped(row))
        .collect();
    parser.set_scrollback(usize::MAX);
    let depth = parser.screen().scrollback();
    if depth == 0 {
        // Nothing has scrolled off yet, so the raw output the client is about
        // to replay is the whole session and this would only repeat it.
        return Ok(Vec::new());
    }

    // vt100 0.15 reads rows past the first screenful of scrollback through a
    // subtraction that underflows, so grow the screen to span the buffer and
    // the screen at once before asking for them. Taking both together is also
    // what keeps a line that wrapped across their boundary in one piece. This
    // emulator is discarded here, and rows that already scrolled off are never
    // reflowed by a resize.
    let tall = u16::try_from(depth)
        .unwrap_or(u16::MAX)
        .saturating_add(rows);
    parser.set_size(tall, columns);
    parser.set_scrollback(depth);
    let deep = parser.screen();
    let mut seed = b"\x1b[r\x1b[m\x1b[2J\x1b[H".to_vec();
    let mut wrapped = false;
    for (index, row) in deep
        .rows_formatted(0, columns)
        .take(depth + usize::from(rows))
        .enumerate()
    {
        // A line the agent ran off the right edge has to run off the client's
        // edge too. Its continuation is rendered assuming the cursor arrived
        // there by wrapping, so breaking the line here would misplace that
        // text and split one line into two.
        if index > 0 && !wrapped {
            seed.extend_from_slice(b"\r\n");
        }
        wrapped = match index.checked_sub(depth) {
            Some(screen_row) => screen_wraps.get(screen_row).copied().unwrap_or(false),
            None => u16::try_from(index).is_ok_and(|row| deep.row_wrapped(row)),
        };
        seed.extend_from_slice(&row);
        // Every row is rendered as if the terminal started it with default
        // attributes, so leave it that way for the next one.
        seed.extend_from_slice(b"\x1b[m");
    }
    // The last screenful written is the screen the session left off on, and
    // everything above it scrolled into history on the way past. Agents redraw
    // only what changed, so handing over that screen rather than a blank one
    // is what keeps the replay from papering history with its gaps.
    seed.extend_from_slice(&input_modes);
    if let Some(region) = inline.region_sequence() {
        // Installing a region homes the cursor, so put it back afterwards.
        seed.extend_from_slice(region.as_bytes());
    }
    seed.extend_from_slice(format!("\x1b[{};{}H", cursor_row + 1, cursor_column + 1).as_bytes());
    Ok(seed)
}

/// Replay `stream` into a throwaway emulator that keeps `keep` rows of
/// scrollback, so its rendered rows can be read back afterwards.
#[cfg(any(unix, test))]
fn replay_history(
    mut stream: impl Read,
    columns: u16,
    rows: u16,
    keep: usize,
) -> Result<(vt100::Parser, InlineScrollback)> {
    let mut parser = vt100::Parser::new(rows, columns, keep);
    let mut inline = InlineScrollback::default();
    let mut buffer = vec![0; 64 * 1024];
    loop {
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => inline.process(&mut parser, &buffer[..read]),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error).context("failed to read session history"),
        }
    }
    Ok((parser, inline))
}

/// Read back every row a replayed emulator holds — its scrollback and its
/// screen — as the rows a terminal would have shown.
#[cfg(any(unix, test))]
fn buffered_rows(parser: &mut vt100::Parser, columns: u16, rows: u16) -> Vec<Vec<u8>> {
    parser.set_scrollback(usize::MAX);
    let depth = parser.screen().scrollback();
    // vt100 0.15 reads rows past the first screenful of scrollback through a
    // subtraction that underflows, so grow the screen to span the buffer and
    // the screen at once before asking for them. The emulator is discarded by
    // the caller, and rows that already scrolled off are never reflowed by a
    // resize.
    let tall = u16::try_from(depth)
        .unwrap_or(u16::MAX)
        .saturating_add(rows);
    parser.set_size(tall, columns);
    parser.set_scrollback(depth);
    parser.screen().rows_formatted(0, columns).collect()
}

/// Render `stream` — the tail of a session's raw output — into the rows a
/// terminal would have shown, and return the `wanted` of them that end
/// `offset_from_bottom` rows above the newest.
///
/// A raw log line is a redraw instruction, not a line of the terminal: agents
/// repaint whole screens, so slicing the log by lines shows fragments of paint
/// rather than what was on screen. Reading history back in rows also keeps it
/// in the unit an attached emulator scrolls in, so a client can page out of its
/// own buffer and into these without the view jumping somewhere unrelated.
///
/// Returns the rows, how many the render reached in all (its scrollback plus
/// the screen), and the offset it could honour — the caller only gets as far
/// back as the window it handed over reaches.
#[cfg(any(unix, test))]
pub(crate) fn render_history_rows(
    stream: impl Read,
    columns: u16,
    rows: u16,
    offset_from_bottom: usize,
    wanted: usize,
) -> Result<(Vec<u8>, usize, usize)> {
    let columns = columns.max(20);
    let rows = rows.max(5);
    let wanted = wanted.max(1);
    let (mut parser, _) = replay_history(
        stream,
        columns,
        rows,
        offset_from_bottom.saturating_add(wanted),
    )?;
    // An agent that draws on the alternate screen — Claude Code does, and its
    // whole session lives there — leaves the primary grid holding only what ran
    // before it opened. Its screen *is* the newest history, so read it off
    // first and step off afterwards to reach what scrolled by underneath;
    // rendering the primary alone hands back a screenful of blanks.
    let rendered = if parser.screen().alternate_screen() {
        let application: Vec<Vec<u8>> = parser.screen().rows_formatted(0, columns).collect();
        parser.process(b"\x1b[?1049l");
        let mut history = buffered_rows(&mut parser, columns, rows);
        // The primary screen was left mid-page when the app opened, so drop the
        // blanks below it rather than pushing the app down by a screenful.
        while history.last().is_some_and(|row| row.is_empty()) {
            history.pop();
        }
        history.extend(application);
        history
    } else {
        buffered_rows(&mut parser, columns, rows)
    };
    let total = rendered.len();
    let actual_offset = offset_from_bottom.min(total.saturating_sub(usize::from(rows)));
    let end = total - actual_offset;
    let start = end.saturating_sub(wanted);
    let mut page = Vec::new();
    for (index, row) in rendered.iter().take(end).enumerate().skip(start) {
        if index > start {
            page.push(b'\n');
        }
        page.extend_from_slice(row);
        // Every row is rendered as if the terminal started it with default
        // attributes, so leave it that way for the next one.
        page.extend_from_slice(b"\x1b[m");
    }
    Ok((page, total, actual_offset))
}

pub(crate) fn resize_parser(parser: &mut vt100::Parser, height: u16, width: u16) {
    let (previous_height, previous_width) = parser.screen().size();
    if height < previous_height && !parser.screen().alternate_screen() {
        // `set_size` truncates the tail when the height shrinks, which would
        // drop the newest lines. A real terminal keeps the bottom of the
        // viewport, so delete the top rows that are about to be discarded,
        // sliding the surviving rows up; `set_size` then drops the blanks left
        // at the bottom. The scroll region is reset so the deletion spans the
        // whole grid, which is also the region a resized screen should have.
        // In alt-screen mode the TUI draws from the top, so the default
        // top-anchored truncation is already correct.
        use std::fmt::Write as _;
        let mut sequence = String::new();
        let _ = write!(sequence, "\x1b[r\x1b[H\x1b[{}M", previous_height - height);
        parser.process(sequence.as_bytes());
    }
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

/// The bytes an application expects for `event`, or None when it did not ask to
/// hear about that kind of event.
fn mouse_report(
    mode: vt100::MouseProtocolMode,
    encoding: vt100::MouseProtocolEncoding,
    event: MouseEvent,
    column: u16,
    row: u16,
) -> Option<Vec<u8>> {
    use vt100::{MouseProtocolEncoding, MouseProtocolMode};

    if mode == MouseProtocolMode::None {
        return None;
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
        // Wheel events are reported as buttons 64-67 and are never released.
        // Without them a pager or editor inside the pane cannot scroll at all,
        // because the wheel would only move Muxloom's own scrollback over it.
        MouseEventKind::ScrollUp => (64, false),
        MouseEventKind::ScrollDown => (65, false),
        MouseEventKind::ScrollLeft => (66, false),
        MouseEventKind::ScrollRight => (67, false),
        _ => return None,
    };
    let mut code = button + mouse_modifier(event.modifiers);
    if release && encoding != MouseProtocolEncoding::Sgr {
        code = 3 + mouse_modifier(event.modifiers);
    }
    let x = column.saturating_add(1);
    let y = row.saturating_add(1);
    Some(match encoding {
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
    })
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

    /// Claude Code opens the alternate screen on its first byte and never
    /// leaves it, so a render that steps off to reach the history underneath
    /// hands back the empty grid the agent never drew on.
    #[test]
    fn history_renders_the_screen_an_agent_drew_on_the_alternate_screen() {
        let mut log = b"$ claude\r\n".to_vec();
        log.extend_from_slice(b"\x1b[?1049h\x1b[H");
        log.extend_from_slice(b"Do you want to create hello.txt?\r\n");
        log.extend_from_slice(b" 1. Yes\r\n 2. No");

        let (page, total, offset) = render_history_rows(&log[..], 40, 6, 0, 6).unwrap();
        let newest = String::from_utf8_lossy(&page).into_owned();
        assert!(
            newest.contains("Do you want to create hello.txt?"),
            "{newest}"
        );
        assert!(newest.contains("2. No"), "{newest}");
        assert_eq!(offset, 0);
        // The screen the agent drew, plus the shell line it opened over.
        assert_eq!(total, 7);

        // That line stays reachable above the agent's screen rather than being
        // buried under the screenful of blanks the alternate screen replaced.
        let (page, _, offset) = render_history_rows(&log[..], 40, 6, 1, 6).unwrap();
        let older = String::from_utf8_lossy(&page).into_owned();
        assert_eq!(offset, 1);
        assert!(older.contains("$ claude"), "{older}");
    }

    /// An agent that closes the alternate screen leaves the shell's own
    /// scrollback showing, which is what has to come back then.
    #[test]
    fn history_renders_what_is_left_after_an_agent_closes_the_alternate_screen() {
        let mut log = b"$ claude\r\n".to_vec();
        log.extend_from_slice(b"\x1b[?1049h\x1b[Hagent screen\x1b[?1049l");
        log.extend_from_slice(b"$ echo done\r\ndone\r\n");

        let (page, _, _) = render_history_rows(&log[..], 40, 6, 0, 6).unwrap();
        let text = String::from_utf8_lossy(&page).into_owned();
        assert!(text.contains("$ echo done"), "{text}");
        assert!(text.contains("done"), "{text}");
        assert!(!text.contains("agent screen"), "{text}");
    }

    #[test]
    fn codex_title_spinner_tracks_activity_across_split_osc_sequences() {
        let mut activity = CodexActivity::default();

        activity.process(b"\x1b]0;\xe2\xa0");
        assert_eq!(activity.working(), None);
        activity.process(b"\x8b project\x07");
        assert_eq!(activity.working(), Some(true));

        // Screen redraws do not erase the last title-derived state.
        activity.process(b"\x1b[2J\x1b[Hpartial redraw");
        assert_eq!(activity.working(), Some(true));

        activity.process(b"\x1b]0;project\x1b\\");
        assert_eq!(activity.working(), Some(false));
    }

    #[test]
    fn a_wheel_event_is_reported_to_an_application_that_asked_for_the_mouse() {
        use vt100::{MouseProtocolEncoding, MouseProtocolMode};

        let wheel = |kind| MouseEvent {
            kind,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        };

        // Buttons 64 and 65 in SGR, at the one-based cell the wheel is over.
        assert_eq!(
            mouse_report(
                MouseProtocolMode::PressRelease,
                MouseProtocolEncoding::Sgr,
                wheel(MouseEventKind::ScrollUp),
                4,
                9,
            ),
            Some(b"\x1b[<64;5;10M".to_vec())
        );
        assert_eq!(
            mouse_report(
                MouseProtocolMode::PressRelease,
                MouseProtocolEncoding::Sgr,
                wheel(MouseEventKind::ScrollDown),
                4,
                9,
            ),
            Some(b"\x1b[<65;5;10M".to_vec())
        );
        // The default encoding offsets every field by 32.
        assert_eq!(
            mouse_report(
                MouseProtocolMode::Press,
                MouseProtocolEncoding::Default,
                wheel(MouseEventKind::ScrollUp),
                0,
                0,
            ),
            Some(vec![0x1b, b'[', b'M', 96, 33, 33])
        );
        // An application that never asked for the mouse leaves the wheel to
        // Muxloom's own scrollback.
        assert_eq!(
            mouse_report(
                MouseProtocolMode::None,
                MouseProtocolEncoding::Sgr,
                wheel(MouseEventKind::ScrollUp),
                0,
                0,
            ),
            None
        );
    }

    #[test]
    fn scrollback_buffer_retains_lines_and_clamps_the_offset() {
        // Guards the emulator-scrollback behaviour the terminal scroll relies on.
        let mut parser = vt100::Parser::new(2, 10, SCROLLBACK_LINES);
        for index in 0..10 {
            parser.process(format!("line{index}\r\n").as_bytes());
        }
        assert_eq!(parser.screen().scrollback(), 0, "starts at the live bottom");
        parser.set_scrollback(3);
        assert_eq!(parser.screen().scrollback(), 3);
        parser.set_scrollback(usize::MAX);
        let deepest = parser.screen().scrollback();
        assert!(deepest >= 3, "output scrolled into scrollback: {deepest}");
        parser.set_scrollback(0);
        assert_eq!(
            parser.screen().scrollback(),
            0,
            "returns to the live bottom"
        );
    }

    /// Feeds `stream` twice: straight into one parser, and through the
    /// harvester into another in ragged chunks so the scanner has to survive
    /// splits mid-sequence. Returns (raw, harvested).
    fn feed_both(rows: u16, columns: u16, stream: &str) -> (vt100::Parser, vt100::Parser) {
        let mut raw = vt100::Parser::new(rows, columns, SCROLLBACK_LINES);
        raw.process(stream.as_bytes());
        let mut kept = vt100::Parser::new(rows, columns, SCROLLBACK_LINES);
        let mut inline = InlineScrollback::default();
        for chunk in stream.as_bytes().chunks(7) {
            inline.process(&mut kept, chunk);
        }
        (raw, kept)
    }

    fn deepest_scrollback(parser: &mut vt100::Parser) -> usize {
        parser.set_scrollback(usize::MAX);
        let deepest = parser.screen().scrollback();
        parser.set_scrollback(0);
        deepest
    }

    #[test]
    fn scrolling_inside_a_pinned_footer_region_fills_the_scrollback() {
        // The shape Codex prints its transcript with: a region anchored to the
        // first row, the cursor parked on that region's last row, one newline
        // per finished line, and a composer painted below the region.
        let mut stream = String::from("\x1b[5;1Hprompt>\x1b[6;1Hstatus");
        for line in 1..=8 {
            stream.push_str(&format!(
                "\x1b[1;4r\x1b[4;1H\r\n\x1b[Kline{line}\x1b[r\x1b[5;8H"
            ));
        }
        let (mut raw, mut kept) = feed_both(6, 20, &stream);

        assert_eq!(
            kept.screen().contents(),
            raw.screen().contents(),
            "the visible screen must match what the agent painted"
        );
        assert_eq!(
            kept.screen().contents_formatted(),
            raw.screen().contents_formatted(),
            "styling must survive the rewrite"
        );
        assert!(
            kept.screen().contents().ends_with("prompt>\nstatus"),
            "the pinned footer must stay below the region: {:?}",
            kept.screen().contents()
        );
        assert_eq!(
            deepest_scrollback(&mut raw),
            0,
            "vt100 alone keeps nothing from a region scroll"
        );
        assert_eq!(deepest_scrollback(&mut kept), 8);

        kept.set_scrollback(4);
        assert_eq!(
            kept.screen().contents(),
            "line1\nline2\nline3\nline4\nline5\nline6",
            "paging up must reveal the finished lines"
        );
    }

    #[test]
    fn regions_that_do_not_start_at_the_top_are_left_to_vt100() {
        // Only a region anchored to the first row pushes its top line out of
        // the screen; anything else scrolls in place and keeps no history.
        let (raw, mut kept) = feed_both(6, 20, "\x1b[2;4r\x1b[4;1H\r\nmiddle");

        assert_eq!(kept.screen().contents(), raw.screen().contents());
        assert_eq!(deepest_scrollback(&mut kept), 0);
    }

    #[test]
    fn newlines_the_agent_writes_elsewhere_are_untouched() {
        // A newline above the region's last row only moves the cursor, and a
        // newline inside an OSC payload is not a newline at all.
        let (raw, mut kept) = feed_both(6, 20, "\x1b[1;4r\x1b]0;ti\ntle\x07\x1b[2;1H\nplain");

        assert_eq!(kept.screen().contents(), raw.screen().contents());
        assert_eq!(deepest_scrollback(&mut kept), 0);
    }

    /// The stream a Codex-shaped agent writes for `lines`: a region anchored
    /// to the top, one newline per finished line, and a footer below it.
    fn transcript(lines: std::ops::RangeInclusive<u32>) -> String {
        let mut stream = String::from("\x1b[5;1Hprompt>\x1b[6;1Hstatus");
        for line in lines {
            stream.push_str(&format!(
                "\x1b[1;4r\x1b[4;1H\r\n\x1b[Kline{line}\x1b[r\x1b[5;8H"
            ));
        }
        stream
    }

    /// A screen tall enough that these tests can read every row they seed:
    /// vt100 0.15 computes a screenful past the scrollback offset, which
    /// underflows once the offset passes the screen height. Release builds
    /// wrap and still render the right rows, but a test build aborts.
    const SEED_ROWS: u16 = 40;
    const SEED_COLUMNS: u16 = 20;

    /// Replays `stream` into a client the way an attach does — rendered
    /// history up to `split`, then the raw output the daemon still holds —
    /// and returns it beside a parser that watched the whole session go by.
    fn seed_and_continue(
        stream: &str,
        split: usize,
        keep: usize,
    ) -> (vt100::Parser, vt100::Parser) {
        let (seeded, rest) = stream.as_bytes().split_at(split);
        let seed = render_scrollback_seed(seeded, SEED_COLUMNS, SEED_ROWS, keep).expect("seed");
        let mut client = vt100::Parser::new(SEED_ROWS, SEED_COLUMNS, SCROLLBACK_LINES);
        let mut inline = InlineScrollback::default();
        inline.process(&mut client, &seed);
        for chunk in rest.chunks(7) {
            inline.process(&mut client, chunk);
        }
        (client, feed_both(SEED_ROWS, SEED_COLUMNS, stream).1)
    }

    fn assert_same_history(client: &mut vt100::Parser, whole: &mut vt100::Parser) {
        assert_eq!(
            client.screen().contents(),
            whole.screen().contents(),
            "the screen a client lands on"
        );
        let depth = deepest_scrollback(whole);
        assert_eq!(deepest_scrollback(client), depth, "rows to page through");
        for offset in 1..=depth {
            client.set_scrollback(offset);
            whole.set_scrollback(offset);
            assert_eq!(
                client.screen().contents(),
                whole.screen().contents(),
                "scrolled back {offset} rows"
            );
        }
        client.set_scrollback(0);
        whole.set_scrollback(0);
    }

    #[test]
    fn a_seed_hands_a_client_history_the_raw_output_never_held() {
        // The whole session is rendered into the seed, so the client can page
        // back through it without replaying a byte of the original stream.
        let stream = transcript(1..=30);
        let (mut client, mut whole) = seed_and_continue(&stream, stream.len(), 100);

        assert_eq!(deepest_scrollback(&mut whole), 30);
        assert_same_history(&mut client, &mut whole);
    }

    #[test]
    fn a_seed_leaves_no_seam_where_the_replayed_output_takes_over() {
        // What an attach actually does: rendered history up to the point the
        // daemon still has raw output for, then that output. The agent only
        // repaints what changed, so the seam shows up as blank rows if the
        // seed stops short of handing over the screen it left off on.
        let stream = transcript(1..=30);
        // Split where the agent finished a line, the way a session's log is
        // whole up to the point the daemon's retained output picks it up.
        let split = transcript(1..=15).len();
        let (mut client, mut whole) = seed_and_continue(&stream, split, 100);

        assert_same_history(&mut client, &mut whole);
    }

    #[test]
    fn a_seed_keeps_a_line_that_ran_off_the_right_edge_whole() {
        // An overlong line is two rows that read as one. The client has to
        // learn that from the seed, since nothing else says they are joined.
        let mut stream = String::new();
        for line in 1..=30 {
            stream.push_str(&format!("line{line:02} {}\r\n", "x".repeat(28)));
        }
        let (mut client, mut whole) = seed_and_continue(&stream, stream.len(), 100);

        let depth = deepest_scrollback(&mut whole);
        assert!(
            (1..=depth).any(|offset| {
                whole.set_scrollback(offset);
                whole
                    .screen()
                    .contents()
                    .lines()
                    .next()
                    .unwrap()
                    .chars()
                    .count()
                    > usize::from(SEED_COLUMNS)
            }),
            "the fixture must produce a wrapped row"
        );
        whole.set_scrollback(0);
        assert_same_history(&mut client, &mut whole);
    }

    #[test]
    fn a_seed_keeps_only_the_rows_it_was_asked_for() {
        let stream = transcript(1..=30);
        let seed =
            render_scrollback_seed(stream.as_bytes(), SEED_COLUMNS, SEED_ROWS, 10).expect("seed");
        let mut client = vt100::Parser::new(SEED_ROWS, SEED_COLUMNS, SCROLLBACK_LINES);
        InlineScrollback::default().process(&mut client, &seed);

        assert_eq!(deepest_scrollback(&mut client), 10);
        client.set_scrollback(10);
        assert!(
            client.screen().contents().starts_with("line17"),
            "the newest rows are the ones worth keeping: {:?}",
            client.screen().contents()
        );
    }

    #[test]
    fn nothing_to_seed_costs_nothing() {
        assert!(
            render_scrollback_seed(b"".as_slice(), SEED_COLUMNS, SEED_ROWS, 100)
                .expect("seed")
                .is_empty(),
            "a session with no output"
        );
        assert!(
            render_scrollback_seed(transcript(1..=30).as_bytes(), SEED_COLUMNS, SEED_ROWS, 0)
                .expect("seed")
                .is_empty(),
            "a client that asked for no history"
        );
        assert!(
            render_scrollback_seed(b"hello".as_slice(), SEED_COLUMNS, SEED_ROWS, 100)
                .expect("seed")
                .is_empty(),
            "a session still on its first screen, which the replay repaints"
        );
    }

    #[test]
    fn a_seed_leaves_an_alternate_screen_to_the_replay() {
        // Restoring a full-screen app's display onto the primary screen would
        // paste it into the history, so the seed stops at the rows below it.
        let stream = format!("{}\x1b[?1049hfull screen", transcript(1..=30));
        let seed =
            render_scrollback_seed(stream.as_bytes(), SEED_COLUMNS, SEED_ROWS, 100).expect("seed");
        let mut client = vt100::Parser::new(SEED_ROWS, SEED_COLUMNS, SCROLLBACK_LINES);
        InlineScrollback::default().process(&mut client, &seed);

        assert!(!client.screen().alternate_screen());
        assert!(!client.screen().contents().contains("full screen"));
        assert_eq!(deepest_scrollback(&mut client), 30);
    }

    /// The text of a rendered history page, with the attributes taken back off.
    fn page_text(page: &[u8]) -> String {
        let text = String::from_utf8(page.to_vec()).expect("utf-8 page");
        let mut plain = String::with_capacity(text.len());
        let mut characters = text.chars();
        while let Some(character) = characters.next() {
            if character != '\x1b' {
                plain.push(character);
                continue;
            }
            // Step over the sequence's introducer before looking for the final
            // byte, which shares its range.
            characters.next();
            for escape in characters.by_ref() {
                if ('@'..='~').contains(&escape) {
                    break;
                }
            }
        }
        plain
            .lines()
            .map(str::trim_end)
            .collect::<Vec<_>>()
            .join("\n")
            .trim_end()
            .to_string()
    }

    #[test]
    fn a_history_page_reads_back_the_rows_an_emulator_would_scroll_to() {
        // What makes a page a continuation of an attached emulator rather than
        // a jump: at the same offset it holds the same rows, because both count
        // rows a terminal showed instead of lines an agent wrote.
        let stream = transcript(1..=30);
        let mut whole = feed_both(SEED_ROWS, SEED_COLUMNS, &stream).1;
        let depth = deepest_scrollback(&mut whole);
        assert_eq!(depth, 30);

        for offset in [0, 7, depth] {
            let (page, total, actual) = render_history_rows(
                stream.as_bytes(),
                SEED_COLUMNS,
                SEED_ROWS,
                offset,
                usize::from(SEED_ROWS),
            )
            .expect("page");

            assert_eq!(actual, offset, "the offset the page was read at");
            assert_eq!(total, depth + usize::from(SEED_ROWS), "rows in all");
            whole.set_scrollback(offset);
            assert_eq!(
                page_text(&page),
                whole.screen().contents().trim_end(),
                "the screen {offset} rows up"
            );
        }
        whole.set_scrollback(0);
    }

    #[test]
    fn a_history_page_stops_at_the_oldest_row_the_window_reaches() {
        // The daemon hands over a window of the log, and rows older than it
        // simply are not in there. Saying so is what lets the caller widen the
        // window instead of believing the history ended.
        let stream = transcript(1..=30);
        let (page, total, actual) = render_history_rows(
            stream.as_bytes(),
            SEED_COLUMNS,
            SEED_ROWS,
            500,
            usize::from(SEED_ROWS),
        )
        .expect("page");

        assert_eq!(actual, 30, "as far back as the rows go");
        assert_eq!(total, 30 + usize::from(SEED_ROWS));
        assert!(
            page_text(&page).trim_start().starts_with("line1\n"),
            "the oldest rows: {:?}",
            page_text(&page)
        );
    }

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
    fn scrolling_up_does_not_change_what_the_agent_is_doing_now() {
        let mut session = TerminalSession::detached(20, 4);
        for line in 1..=12 {
            session.parser.process(format!("line{line}\r\n").as_bytes());
        }
        session.set_scrollback(3);

        assert!(
            session.screen().contents().contains("line9"),
            "the user is looking at rows that scrolled off"
        );
        let live = session.live_contents();
        assert!(
            live.contains("line12") && !live.contains("line9"),
            "the live screen is the bottom of the session, not the view: {live:?}"
        );
        assert_eq!(
            session.scrollback(),
            3,
            "reading it leaves the user where they were"
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
