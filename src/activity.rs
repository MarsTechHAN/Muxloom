//! What a session is doing, read off the terminal rather than off the words
//! on its screen.
//!
//! A coding agent tells its terminal what it is up to whether or not anybody
//! is reading the transcript: it rewrites the window title with a spinner
//! while a turn runs, blinks that title when it is stuck on a question, paints
//! its own spinner and stream a dozen times a second while it works, goes
//! silent the moment it stops, hides the terminal cursor while a dialog has
//! the keyboard and shows it again when the prompt box does, and rings the
//! bell or sends a desktop notification when it wants somebody. None of that
//! is prose, none of it scrolls away, and none of it can be quoted back by an
//! agent summarising what it did yesterday — which is what every reading of
//! the words on the screen tripped over.
//!
//! Measured off live sessions, one per runtime this crate drives:
//!
//! - Codex rewrites `OSC 0` ten times a second with a braille spinner at its
//!   head while a turn runs (`⠹ project`), holds `[ . ] Action Required` and
//!   `[ ! ] Action Required` in alternation once a second while it waits on
//!   an approval, and leaves the bare project name when it is idle. It only
//!   writes a title at all when asked to; `runtime::launch_arguments` asks.
//! - Claude Code cycles `◐` and `◑` at the head of its title about once a
//!   second for the whole of a turn and rests on `✳` when the turn ends. Its
//!   permission and question dialogs hide the cursor; its prompt box shows it.
//! - OpenCode paints twenty to forty synchronised frames a second while a
//!   turn runs and nothing while it sits at its box. Its permission dialog
//!   hides the cursor.
//! - pi paints fifteen frames a second while a turn runs, nothing while it
//!   waits, never shows the terminal cursor (it draws its own) and rings the
//!   bell when a turn ends. It has no dialogs to wait on.
//!
//! The tracker is fed every byte a session writes, in order, with the time it
//! arrived, and answers with an [`Activity`] a classifier turns into a
//! [`Status`]. Nothing in here reads the grid.

use std::{
    collections::VecDeque,
    time::{SystemTime, UNIX_EPOCH},
};

/// Now, as epoch milliseconds: the clock every reading here is timed against.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

/// Output inside this window counts as the session painting a turn.
pub const PAINT_WINDOW_MS: u64 = 2_000;
/// A running turn's spinner alone lands this many writes in the window; the
/// blink a dialog keeps up (Claude Code redraws one glyph twice a second)
/// lands a third of it.
pub const PAINT_MIN_WRITES: usize = 6;
/// Or this many bytes: a streaming answer can arrive in a few large writes.
pub const PAINT_MIN_BYTES: usize = 1_200;
/// A title rewritten this often with no spinner at its head is being blinked
/// at whoever is looking, which is what Codex does while it waits.
pub const BLINK_WINDOW_MS: u64 = 3_000;
pub const BLINK_MIN_CHANGES: usize = 2;
/// A spinner held in the title while nothing at all comes off the PTY says
/// a turn is running — a turn that shells out to a build says nothing for
/// minutes — until it has said nothing for this long, at which point it is a
/// frame frozen by a wedged runtime.
pub const SPINNER_HELD_QUIET_MS: u64 = 10 * 60_000;
/// A shell's child is asking something when it has been quiet this long with
/// the cursor parked after the text it printed.
pub const TERMINAL_ASK_QUIET_MS: u64 = 2_000;
/// A cursor hidden this long is a dialog holding it, rather than a frame
/// being painted: every CLI hides the cursor for the few milliseconds it
/// takes to draw a screen, and a startup that hides it for one frame must not
/// read as a question.
pub const CURSOR_HIDDEN_SETTLE_MS: u64 = 1_000;

/// What a session is doing, as its terminal reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Sitting at its prompt with nothing running.
    Idle,
    /// A turn is running.
    Working,
    /// Stopped on something only a person or a parent agent can answer.
    Waiting,
}

/// The signals the tracker has read, as of one moment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Activity {
    /// Enough came off the PTY inside [`PAINT_WINDOW_MS`] to be a turn
    /// painting itself.
    pub painting: bool,
    /// The title carries a spinner glyph at its head, heard live from this
    /// session, and the session has not gone quiet for [`SPINNER_HELD_QUIET_MS`].
    pub spinner: bool,
    /// The title is being rewritten every second or so without a spinner at
    /// its head.
    pub blinking: bool,
    /// The cursor has been hidden for [`CURSOR_HIDDEN_SETTLE_MS`].
    pub cursor_hidden: bool,
    /// The cursor has been shown at least once, so hiding it means something.
    pub cursor_shown_once: bool,
    /// The title as last written, if one ever was.
    pub title: Option<String>,
    /// Milliseconds since the last byte; `u64::MAX` when nothing was heard.
    pub quiet_ms: u64,
    /// The last desktop notification the session sent (OSC 9, OSC 99 or
    /// OSC 777), with when it was sent.
    pub notice: Option<Notice>,
    /// When the bell last rang, if it ever did.
    pub bell_at: Option<u64>,
}

/// A desktop notification a session sent through its terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub at: u64,
    pub text: String,
}

/// Reads the signals out of one session's output stream.
#[derive(Debug, Default)]
pub struct ActivityTracker {
    scan: Scan,
    /// Parameter bytes of the CSI, or payload of the OSC, being scanned.
    params: Vec<u8>,
    title: Option<String>,
    /// When the title last changed, live: `0` until a title has been heard.
    title_changed_at: u64,
    /// When the title changed and whether it changed to a spinner, newest
    /// last, bounded to what blink detection needs. A spinner's own frames
    /// are not blinks, and neither is the one change that puts the title to
    /// rest after them.
    title_changes: VecDeque<(u64, bool)>,
    /// Writes heard, newest last, as (when, bytes), bounded to the paint window.
    writes: VecDeque<(u64, usize)>,
    last_output: u64,
    cursor_hidden: bool,
    /// When the cursor was last hidden after being shown; `0` when it never
    /// was (a runtime that never shows it hides it once, at startup).
    cursor_hidden_since: u64,
    cursor_shown_once: bool,
    notice: Option<Notice>,
    bell_at: Option<u64>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum Scan {
    #[default]
    Ground,
    Escape,
    Csi,
    Osc,
    /// An ESC inside an OSC payload, which is either the start of an ST or
    /// the start of something else.
    OscEscape,
    /// A DCS, APC, PM or SOS string, which runs until ST and is nobody's
    /// business here.
    Skip,
    SkipEscape,
}

impl ActivityTracker {
    const OSC_LIMIT: usize = 4_096;
    const WRITES_LIMIT: usize = 1_024;

    /// Feed `bytes` that arrived at `now` (epoch milliseconds). Bytes replayed
    /// out of a record rather than heard from the session should be fed with
    /// `now == 0`: their title and cursor state stand, but they are not
    /// evidence of anything happening now.
    pub fn process(&mut self, bytes: &[u8], now: u64) {
        if bytes.is_empty() {
            return;
        }
        if now != 0 {
            self.last_output = now;
            self.writes.push_back((now, bytes.len()));
            while self
                .writes
                .front()
                .is_some_and(|(at, _)| now.saturating_sub(*at) > PAINT_WINDOW_MS)
                || self.writes.len() > Self::WRITES_LIMIT
            {
                self.writes.pop_front();
            }
        }
        for &byte in bytes {
            self.step(byte, now);
        }
    }

    fn step(&mut self, byte: u8, now: u64) {
        match self.scan {
            Scan::Ground => match byte {
                0x1b => self.scan = Scan::Escape,
                0x07 => self.bell_at = Some(now),
                _ => {}
            },
            Scan::Escape => {
                self.params.clear();
                self.scan = match byte {
                    b'[' => Scan::Csi,
                    b']' => Scan::Osc,
                    b'P' | b'_' | b'^' | b'X' => Scan::Skip,
                    0x1b => Scan::Escape,
                    _ => Scan::Ground,
                };
            }
            Scan::Csi => match byte {
                0x20..=0x3f => self.params.push(byte),
                0x40..=0x7e => {
                    self.finish_csi(byte, now);
                    self.scan = Scan::Ground;
                }
                0x1b => self.scan = Scan::Escape,
                // A C0 control inside a CSI is executed and the sequence goes
                // on; a BEL there is still a bell.
                0x07 => self.bell_at = Some(now),
                _ => {}
            },
            Scan::Osc => match byte {
                0x07 => {
                    self.finish_osc(now);
                    self.scan = Scan::Ground;
                }
                0x1b => self.scan = Scan::OscEscape,
                _ => {
                    if self.params.len() < Self::OSC_LIMIT {
                        self.params.push(byte);
                    }
                }
            },
            Scan::OscEscape => {
                self.finish_osc(now);
                if byte == b'\\' {
                    self.scan = Scan::Ground;
                } else {
                    self.scan = Scan::Escape;
                    self.step(byte, now);
                }
            }
            Scan::Skip => {
                if byte == 0x1b {
                    self.scan = Scan::SkipEscape;
                }
            }
            Scan::SkipEscape => {
                self.scan = match byte {
                    b'\\' => Scan::Ground,
                    0x1b => Scan::SkipEscape,
                    _ => Scan::Skip,
                };
            }
        }
    }

    fn finish_csi(&mut self, last: u8, now: u64) {
        if !matches!(last, b'h' | b'l') {
            return;
        }
        let Some(modes) = self.params.strip_prefix(b"?") else {
            return;
        };
        for mode in modes.split(|byte| *byte == b';') {
            if mode == b"25" {
                let hidden = last == b'l';
                if hidden && !self.cursor_hidden {
                    self.cursor_hidden_since = now;
                }
                self.cursor_hidden = hidden;
                if !hidden {
                    self.cursor_shown_once = true;
                }
            }
        }
    }

    fn finish_osc(&mut self, now: u64) {
        let payload = std::mem::take(&mut self.params);
        let payload = String::from_utf8_lossy(&payload);
        let (code, rest) = payload.split_once(';').unwrap_or((&payload, ""));
        match code {
            "0" | "2" => {
                if self.title.as_deref() != Some(rest) {
                    self.title = Some(rest.to_string());
                    self.title_changed_at = now;
                    self.title_changes.push_back((now, title_has_spinner(rest)));
                    while self.title_changes.len() > 32 {
                        self.title_changes.pop_front();
                    }
                }
            }
            // ConEmu-style progress (`9;4;state;value`) shares the number with
            // iTerm2's notification and is not one.
            "9" if !rest.starts_with("4;") => self.notify(now, rest.to_string()),
            "99" => self.notify_kitty(now, rest),
            "777" => {
                if let Some(rest) = rest.strip_prefix("notify;") {
                    let text = match rest.split_once(';') {
                        Some((title, body)) if !body.trim().is_empty() => {
                            format!("{}: {}", title.trim(), body.trim())
                        }
                        Some((title, _)) => title.trim().to_string(),
                        None => rest.trim().to_string(),
                    };
                    self.notify(now, text);
                }
            }
            _ => {}
        }
    }

    /// A kitty notification arrives in chunks — `i=6941:d=0:p=title;Claude
    /// Code`, then `i=6941:p=body;Claude needs your permission`, then a bare
    /// `i=6941:d=1` to show it — and the capability query (`p=?`) and the
    /// housekeeping payloads carry no words for anybody.
    fn notify_kitty(&mut self, now: u64, rest: &str) {
        let (metadata, text) = rest.split_once(';').unwrap_or((rest, ""));
        let mut payload = "title";
        for item in metadata.split(':') {
            if let Some(value) = item.strip_prefix("p=") {
                payload = value;
            }
        }
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        match payload {
            "title" => self.notify(now, text.to_string()),
            "body" => match self.notice.as_mut() {
                Some(notice) if now.saturating_sub(notice.at) <= 2_000 => {
                    notice.text = format!("{}: {text}", notice.text);
                }
                _ => self.notify(now, text.to_string()),
            },
            _ => {}
        }
    }

    fn notify(&mut self, now: u64, text: String) {
        let text = text.trim().to_string();
        if text.is_empty() {
            return;
        }
        self.notice = Some(Notice { at: now, text });
    }

    /// The signals as they stand at `now`.
    pub fn report(&self, now: u64) -> Activity {
        let (writes, bytes) = self
            .writes
            .iter()
            .filter(|(at, _)| *at != 0 && now.saturating_sub(*at) <= PAINT_WINDOW_MS)
            .fold((0usize, 0usize), |(writes, bytes), (_, len)| {
                (writes + 1, bytes + len)
            });
        let painting = writes >= PAINT_MIN_WRITES || bytes >= PAINT_MIN_BYTES;
        let quiet_ms = match self.last_output {
            0 => u64::MAX,
            heard => now.saturating_sub(heard),
        };
        let spinner_head = self.title.as_deref().is_some_and(title_has_spinner);
        let spinner =
            spinner_head && self.title_changed_at != 0 && quiet_ms < SPINNER_HELD_QUIET_MS;
        let recent_changes = self
            .title_changes
            .iter()
            .filter(|(at, to_spinner)| {
                !to_spinner && *at != 0 && now.saturating_sub(*at) <= BLINK_WINDOW_MS
            })
            .count();
        let blinking = !spinner_head && recent_changes >= BLINK_MIN_CHANGES;
        Activity {
            painting,
            spinner,
            blinking,
            cursor_hidden: self.cursor_hidden
                && now.saturating_sub(self.cursor_hidden_since) >= CURSOR_HIDDEN_SETTLE_MS,
            cursor_shown_once: self.cursor_shown_once,
            title: self.title.clone(),
            quiet_ms,
            notice: self.notice.clone(),
            bell_at: self.bell_at,
        }
    }

    /// The title as last written.
    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }
}

/// Whether a title starts with one of the glyphs a CLI animates at its head to
/// say a turn is running: the braille dots Codex cycles, the half-circles
/// Claude Code alternates, and the ASCII bar a terminal without either falls
/// back to. Claude Code's resting `✳` is not among them, deliberately.
pub fn title_has_spinner(title: &str) -> bool {
    title.trim_start().chars().next().is_some_and(|glyph| {
        matches!(glyph,
            '\u{2800}'..='\u{28ff}'
            | '\u{25d0}'..='\u{25d3}'
            | '\u{25f4}'..='\u{25f7}'
            | '\u{25dc}'..='\u{25df}'
            | '|' | '/' | '-' | '\\')
    })
}

/// What an agent session is doing.
///
/// A spinner in the title or a turn painting itself is a turn running,
/// whatever else is on screen: a dialog drawn during a turn belongs to it. A
/// blinked title is the runtime asking for somebody. A hidden cursor, on a
/// runtime that shows one at its prompt box, is a dialog holding the keyboard.
/// Otherwise the session is at its box with nothing to do.
pub fn agent_status(activity: &Activity) -> Status {
    if activity.spinner || activity.painting {
        Status::Working
    } else if activity.blinking || (activity.cursor_hidden && activity.cursor_shown_once) {
        Status::Waiting
    } else {
        Status::Idle
    }
}

/// Where a plain terminal's cursor is, as far as a shell's child can be read
/// off it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CursorRow {
    /// The program has the alternate screen: a pager, an editor, a picker.
    pub alternate_screen: bool,
    /// The text on the cursor's row to the left of the cursor.
    pub before_cursor: String,
}

/// What a plain terminal is doing: nothing without a child to wait on, and
/// with one, working unless the child has gone quiet with its cursor parked
/// right after the last thing it printed — `Overwrite? [y/N] `,
/// `Password:`, `--More--` — or has taken the whole screen and stopped
/// painting it, which is a full-screen program waiting on a key. The row the
/// cursor sits on is the question, in the program's own words.
pub fn terminal_status(
    activity: &Activity,
    has_child: bool,
    cursor: &CursorRow,
) -> (Status, Option<String>) {
    if !has_child {
        return (Status::Idle, None);
    }
    if activity.painting || activity.quiet_ms < TERMINAL_ASK_QUIET_MS {
        return (Status::Working, None);
    }
    if cursor.alternate_screen {
        return (
            Status::Waiting,
            Some("a full-screen program is waiting for a key".into()),
        );
    }
    let asked = cursor.before_cursor.trim_end();
    let trailing = cursor.before_cursor.len() - asked.len();
    if !asked.is_empty() && trailing <= 1 {
        return (Status::Waiting, Some(asked.to_string()));
    }
    (Status::Working, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feed one write per `step` milliseconds, `count` times, from `start`.
    fn paint(
        tracker: &mut ActivityTracker,
        bytes: &[u8],
        start: u64,
        step: u64,
        count: u64,
    ) -> u64 {
        let mut at = start;
        for _ in 0..count {
            tracker.process(bytes, at);
            at += step;
        }
        at
    }

    #[test]
    fn codex_spins_its_title_while_working_and_rests_it_when_done() {
        let mut tracker = ActivityTracker::default();
        let frames: Vec<String> = "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏"
            .chars()
            .map(|glyph| format!("\x1b]0;{glyph} codex\x07"))
            .collect();
        let mut at = 1_000;
        for frame in frames.iter().cycle().take(30) {
            tracker.process(frame.as_bytes(), at);
            at += 100;
        }
        let activity = tracker.report(at);
        assert!(activity.spinner, "{activity:?}");
        assert_eq!(agent_status(&activity), Status::Working);

        // Quiet under a spinner is a turn that shells out, for a long time.
        let last = at - 100;
        let activity = tracker.report(last + SPINNER_HELD_QUIET_MS - 1);
        assert!(activity.spinner && !activity.painting, "{activity:?}");
        assert_eq!(agent_status(&activity), Status::Working);
        // But not forever.
        assert_eq!(
            agent_status(&tracker.report(last + SPINNER_HELD_QUIET_MS + 1)),
            Status::Idle
        );

        // Rested, and silent: an idle Codex writes nothing at all.
        tracker.process(b"\x1b]0;codex\x07\x1b[?25h", at);
        assert_eq!(
            agent_status(&tracker.report(at + PAINT_WINDOW_MS + 1)),
            Status::Idle
        );
    }

    #[test]
    fn codex_blinks_its_title_while_blocked_on_an_approval() {
        let mut tracker = ActivityTracker::default();
        tracker.process(b"\x1b[?25h\x1b]0;codex\x07", 1_000);
        let mut at = 2_000;
        for blink in [
            "[ . ] Action Required | codex",
            "[ ! ] Action Required | codex",
        ]
        .iter()
        .cycle()
        .take(6)
        {
            tracker.process(format!("\x1b]0;{blink}\x07").as_bytes(), at);
            at += 1_000;
        }
        let activity = tracker.report(at);
        assert!(
            activity.blinking && !activity.spinner && !activity.painting,
            "{activity:?}"
        );
        assert_eq!(agent_status(&activity), Status::Waiting);

        // The notification Codex sends with it names the question.
        tracker.process(
            b"\x1b]9;Approval requested: /bin/zsh -lc \"rm -rf build\"\x07",
            at,
        );
        assert_eq!(
            tracker.report(at).notice,
            Some(Notice {
                at,
                text: "Approval requested: /bin/zsh -lc \"rm -rf build\"".into()
            })
        );
    }

    #[test]
    fn claude_alternates_its_title_while_working_and_hides_the_cursor_on_a_dialog() {
        let mut tracker = ActivityTracker::default();
        tracker.process(b"\x1b[?25h\x1b]0;\xe2\x9c\xb3 Claude Code\x07", 1_000);
        assert_eq!(agent_status(&tracker.report(1_500)), Status::Idle);

        let mut at = 2_000;
        for glyph in ["◐", "◑"].iter().cycle().take(6) {
            tracker.process(format!("\x1b]0;{glyph} Claude Code\x07").as_bytes(), at);
            at += 900;
        }
        assert_eq!(agent_status(&tracker.report(at)), Status::Working);

        // The turn stops on a permission dialog: resting title, hidden cursor,
        // one glyph blinked twice a second.
        tracker.process(b"\x1b]0;\xe2\x9c\xb3 curl headers\x07\x1b[?25l", at);
        let at = paint(
            &mut tracker,
            b"\x1b[H\r\x1b[10B \x1b[24;2H",
            at + 100,
            600,
            8,
        );
        let activity = tracker.report(at);
        assert!(
            !activity.painting && !activity.spinner && !activity.blinking,
            "{activity:?}"
        );
        assert!(activity.cursor_hidden && activity.cursor_shown_once);
        assert_eq!(agent_status(&activity), Status::Waiting);

        // Answered: the box is back and the cursor with it.
        tracker.process(b"\x1b[?25h", at);
        assert_eq!(
            agent_status(&tracker.report(at + PAINT_WINDOW_MS + 1)),
            Status::Idle
        );
    }

    #[test]
    fn a_kitty_notification_is_read_in_its_chunks() {
        let mut tracker = ActivityTracker::default();
        // The capability query OpenCode sends at startup is not a notice.
        tracker.process(b"\x1b]99;i=opentui-notifications:p=?;\x1b\\", 1_000);
        assert_eq!(tracker.report(1_000).notice, None);
        tracker.process(
            concat!(
                "\x1b]99;i=6941:d=0:p=title;Claude Code\x1b\\",
                "\x1b]99;i=6941:p=body;Claude needs your permission\x1b\\",
                "\x1b]99;i=6941:d=1:a=focus;\x1b\\"
            )
            .as_bytes(),
            2_000,
        );
        assert_eq!(
            tracker.report(2_000).notice.map(|notice| notice.text),
            Some("Claude Code: Claude needs your permission".into())
        );
        tracker.process(b"\x1b]777;notify;pi;turn finished\x1b\\", 3_000);
        assert_eq!(
            tracker.report(3_000).notice.map(|notice| notice.text),
            Some("pi: turn finished".into())
        );
    }

    #[test]
    fn a_runtime_that_never_shows_the_cursor_is_never_read_as_waiting() {
        let mut tracker = ActivityTracker::default();
        tracker.process(b"\x1b[?25l\x1b]0;\xcf\x80 - pi\x07", 1_000);
        assert_eq!(agent_status(&tracker.report(1_500)), Status::Idle);
        let at = paint(&mut tracker, &[b'x'; 200], 2_000, 70, 30);
        assert_eq!(agent_status(&tracker.report(at)), Status::Working);
        tracker.process(b"\x07", at);
        let activity = tracker.report(at + PAINT_WINDOW_MS + 1);
        assert_eq!(activity.bell_at, Some(at));
        assert_eq!(agent_status(&activity), Status::Idle);
    }

    #[test]
    fn painting_is_measured_over_the_window_and_a_dialog_blink_is_under_it() {
        let mut tracker = ActivityTracker::default();
        // OpenCode: thirty small frames a second.
        let at = paint(&mut tracker, &[b'.'; 150], 1_000, 33, 60);
        assert!(tracker.report(at).painting);
        assert!(!tracker.report(at + PAINT_WINDOW_MS + 1).painting);
        // A single large write is a streamed answer.
        tracker.process(&[b'.'; 1_500], at + 5_000);
        assert!(tracker.report(at + 5_000).painting);
        // Two writes a second of a hundred bytes is a glyph being blinked.
        let mut tracker = ActivityTracker::default();
        let at = paint(&mut tracker, &[b'.'; 100], 1_000, 500, 10);
        assert!(!tracker.report(at).painting);
    }

    #[test]
    fn an_osc_split_across_writes_and_a_replayed_record_are_read_right() {
        let mut tracker = ActivityTracker::default();
        tracker.process(b"\x1b]0;\xe2\xa0", 1_000);
        tracker.process(b"\x8b project\x07", 1_100);
        assert_eq!(tracker.title(), Some("⠋ project"));
        assert!(tracker.report(1_200).spinner);
        // A title ended by ST rather than BEL, with a CSI right behind it.
        tracker.process(b"\x1b]0;project\x1b\\\x1b[?25h", 1_300);
        assert_eq!(tracker.title(), Some("project"));
        assert!(tracker.report(1_400).cursor_shown_once);

        // Replayed out of a record: the title and cursor stand, but nothing
        // about it is happening now.
        let mut adopted = ActivityTracker::default();
        adopted.process(b"\x1b[?25h\x1b]0;\xe2\xa0\x8b project\x07", 0);
        let activity = adopted.report(50_000);
        assert_eq!(activity.title.as_deref(), Some("⠋ project"));
        assert!(
            !activity.spinner && !activity.painting && !activity.blinking,
            "{activity:?}"
        );
        assert_eq!(activity.quiet_ms, u64::MAX);
        assert_eq!(agent_status(&activity), Status::Idle);
        // Heard once, the same title turns live.
        adopted.process(b"\x1b]0;\xe2\xa0\x99 project\x07", 50_000);
        assert_eq!(agent_status(&adopted.report(50_100)), Status::Working);
    }

    #[test]
    fn a_terminal_waits_when_its_child_parks_the_cursor_after_a_question() {
        let mut tracker = ActivityTracker::default();
        tracker.process(b"cp: overwrite '/tmp/a'? [y/N] ", 1_000);
        let asked = CursorRow {
            alternate_screen: false,
            before_cursor: "cp: overwrite '/tmp/a'? [y/N] ".into(),
        };
        // Nothing to wait on: no child.
        assert_eq!(
            terminal_status(&tracker.report(5_000), false, &asked),
            (Status::Idle, None)
        );
        // Just printed: still working.
        assert_eq!(
            terminal_status(&tracker.report(1_500), true, &asked).0,
            Status::Working
        );
        assert_eq!(
            terminal_status(&tracker.report(5_000), true, &asked),
            (
                Status::Waiting,
                Some("cp: overwrite '/tmp/a'? [y/N]".into())
            )
        );
        // A build that ended its line is not asking.
        let building = CursorRow {
            alternate_screen: false,
            before_cursor: String::new(),
        };
        assert_eq!(
            terminal_status(&tracker.report(5_000), true, &building).0,
            Status::Working
        );
        let pager = CursorRow {
            alternate_screen: true,
            before_cursor: String::new(),
        };
        assert_eq!(
            terminal_status(&tracker.report(5_000), true, &pager).0,
            Status::Waiting
        );
    }
}
