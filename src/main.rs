use std::{
    backtrace::Backtrace,
    env, fs,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, bail};
use crossterm::{
    cursor::{Hide, Show},
    event::{
        self, DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
        EnableFocusChange, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    style::{Attribute, ResetColor, SetAttribute},
    terminal::{
        EnableLineWrap, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
        enable_raw_mode,
    },
};
use muxloom::{
    app::{Action, App},
    config::{
        Config, EXAMPLE_CONFIG, State, default_config_path, default_debug_log_path,
        default_state_path, legacy_config_path, legacy_state_path, migrate_legacy_file,
    },
    debug,
    model::Target,
    runtime::Runtime,
    ssh_config, ui,
    worker::Worker,
};
use ratatui::{Terminal, backend::CrosstermBackend};

type Tui = Terminal<CrosstermBackend<io::Stdout>>;
static KEYBOARD_ENHANCEMENT_ACTIVE: AtomicBool = AtomicBool::new(false);

fn main() -> Result<()> {
    install_panic_hook();
    install_job_control_guard();
    let result = real_main();
    if let Err(error) = &result {
        debug::log("fatal", format!("{error:#}; {}", debug::tty_state()));
        restore_terminal_best_effort();
    }
    result
}

fn real_main() -> Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();
    let options = parse_args(&args)?;
    match options.command {
        Command::Help => {
            print_help();
            Ok(())
        }
        Command::Version => {
            // The first line stays exactly "muxloom <version>": the bootstrap
            // script reads a companion's version off the last word of it. The
            // build a nightly came from goes on a line of its own.
            println!("muxloom {}", env!("CARGO_PKG_VERSION"));
            if let Some(commit) = muxloom::update::build_commit() {
                let stream = muxloom::update::build_channel().unwrap_or("untagged");
                match muxloom::update::build_height() {
                    Some(height) => println!("build {commit} ({stream}, commit {height} on main)"),
                    None => println!("build {commit} ({stream})"),
                }
            }
            Ok(())
        }
        Command::Update => {
            let config = Config::load(&options.config_path)?;
            let environment = config.environment_for(muxloom::model::LOCAL_TARGET_ID)?;
            let channel = options
                .update_channel
                .unwrap_or_else(|| muxloom::update::Channel::from_config(&config.update_channel));
            muxloom::update::run_cli(channel, &environment)
        }
        Command::Init => {
            if !options.config_explicit
                && migrate_legacy_file(&legacy_config_path(), &options.config_path)?
            {
                println!(
                    "Migrated legacy configuration to {}",
                    options.config_path.display()
                );
                Ok(())
            } else {
                init_config(&options.config_path)
            }
        }
        Command::Mcp => {
            // The MCP transport owns stdout: no terminal setup, no update
            // banner, nothing printed that is not a JSON-RPC message.
            if let Some(log_path) = &options.debug_log {
                debug::init(log_path)?;
            }
            let config = Config::load(&options.config_path)?;
            let surface = muxloom::control::ControllerControl::new(config)?;
            muxloom::mcp::serve(
                &surface,
                "muxloom",
                io::BufReader::new(io::stdin()),
                io::stdout(),
            )
        }
        Command::BoardClear => {
            let config = Config::load(&options.config_path)?;
            let target = match options.machine.as_deref() {
                None | Some(muxloom::model::LOCAL_TARGET_ID) => Target::local(),
                Some(alias) => Target::ssh(alias),
            };
            let runtime = Runtime::new(&config);
            let dropped = runtime
                .bridge_pool()
                .talk_clear(&target)
                .with_context(|| format!("clearing the talk board on {}", target.id))?;
            // The board is replicated rather than shared, so this says which
            // copy went: a machine that still holds its own is not wrong, it
            // just has not been asked.
            println!(
                "Cleared {dropped} records from the talk board on {}.",
                target.id
            );
            Ok(())
        }
        Command::Run => {
            if !options.config_explicit {
                migrate_legacy_file(&legacy_config_path(), &options.config_path)?;
            }
            if let Some(log_path) = &options.debug_log {
                debug::init(log_path)?;
            }
            run(options.config_path)
        }
    }
}

/// Check GitHub for a newer build in the background, on whichever channel the
/// configuration follows. With `ask` (the default) the finding opens a prompt
/// in the UI; with `auto`, a real install is staged silently the way pre-0.5
/// releases did. Failures (offline, rate-limited, etc.) stay silent.
fn spawn_update_check(
    slot: muxloom::app::UpdateSlot,
    channel: muxloom::update::Channel,
    environment: Vec<(String, String)>,
    ask: bool,
) {
    std::thread::spawn(move || {
        let Ok(result) = muxloom::update::check_and_maybe_apply(channel, !ask, &environment) else {
            return;
        };
        let Some(release) = result.release else {
            return;
        };
        let note = if result.applied {
            muxloom::app::UpdateNote {
                message: Some(format!(
                    "muxloom {} downloaded — restart to apply",
                    release.label
                )),
                staged_version: Some(release.label),
                available_version: None,
                prompt: None,
            }
        } else if ask {
            muxloom::app::UpdateNote {
                message: None,
                staged_version: None,
                available_version: Some(release.label.clone()),
                prompt: Some(muxloom::app::UpdatePrompt {
                    latest: release.label,
                    current: result.current,
                    tag: release.tag,
                    version: release.version,
                    can_self_update: cfg!(unix) && muxloom::update::is_installed_bundle(),
                }),
            }
        } else {
            // Nothing was downloaded on this path, so the header must not
            // offer a restart: the user still has to run the update.
            muxloom::app::UpdateNote {
                message: Some(format!(
                    "muxloom {} available — run `muxloom update`",
                    release.label
                )),
                staged_version: None,
                available_version: Some(release.label),
                prompt: None,
            }
        };
        if let Ok(mut guard) = slot.lock() {
            *guard = Some(note);
        }
    });
}

fn run(config_path: PathBuf) -> Result<()> {
    let config = Config::load(&config_path)?;
    let state_path = default_state_path();
    migrate_legacy_file(&legacy_state_path(), &state_path)?;
    let state = State::load(&state_path)?;
    let mut targets = vec![Target::local()];
    targets.extend(
        ssh_config::load_hosts(&config.ssh_config_path())?
            .into_iter()
            .filter(|alias| alias != "local")
            .map(Target::ssh),
    );

    let auto_update = config.auto_update && config.update_prompt != "never";
    let ask_before_updating = config.update_prompt != "auto";
    let update_channel = muxloom::update::Channel::from_config(&config.update_channel);
    let update_environment = config
        .environment_for(muxloom::model::LOCAL_TARGET_ID)
        .unwrap_or_default();

    let runtime = Runtime::new(&config);
    let worker = Worker::start(runtime.clone());
    let mut app = App::new(config, config_path, state, state_path, targets, worker);
    if let Some(path) = debug::path() {
        app.status_message = format!("Debug log: {}", path.display());
    }
    if auto_update {
        spawn_update_check(
            app.update_slot(),
            update_channel,
            update_environment,
            ask_before_updating,
        );
    }
    app.start();

    let shutdown = install_shutdown_handlers()?;
    spawn_terminal_hangup_watchdog(Arc::clone(&shutdown));
    let mut restore_guard = TerminalRestoreGuard::new();
    let mut terminal = enter_terminal()?;
    debug::log("app", format!("terminal entered; {}", debug::tty_state()));
    let result = run_loop(&mut terminal, &mut app, &shutdown);
    let restore_result = leave_terminal(&mut terminal);
    if restore_result.is_ok() {
        restore_guard.disarm();
    }
    result.and(restore_result)
}

fn run_loop(terminal: &mut Tui, app: &mut App, shutdown: &AtomicBool) -> Result<()> {
    loop {
        if shutdown.load(Ordering::Relaxed) {
            debug::log("app", "termination signal received");
            return Ok(());
        }
        app.on_tick();
        terminal.draw(|frame| ui::draw(frame, app))?;
        for notification in app.take_notifications() {
            emit_terminal_notification(terminal, &notification)?;
        }
        if let Some(text) = app.take_clipboard_request() {
            match emit_clipboard_copy(terminal, &text) {
                Ok(confirmed) => app.note_clipboard_delivery(confirmed),
                Err(error) => app.status_message = format!("Clipboard copy failed: {error}"),
            }
        }
        if app.take_clipboard_paste_request() {
            app.deliver_clipboard_paste(read_native_clipboard());
        }
        if !event::poll(Duration::from_millis(33))? {
            continue;
        }
        let action = match event::read()? {
            Event::FocusGained => {
                // Terminal regained OS focus – the visible screen may have been
                // cleared or corrupted while we were in the background.  Force
                // a full repaint on the next frame so the whole UI reappears.
                terminal.clear()?;
                Action::Continue
            }
            Event::Resize(_, _) => {
                // Ratatui re-queries the backend size on the next draw, but
                // explicitly clearing avoids a one-frame flash of stale content.
                terminal.clear()?;
                Action::Continue
            }
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                // Manual full-redraw (fallback for terminals without focus events)
                if key.code == KeyCode::F(5) {
                    let _ = terminal.clear();
                    Action::Continue
                } else {
                    log_navigation_key(key);
                    app.handle_key(key)
                }
            }
            Event::Mouse(mouse) => app.handle_mouse(mouse),
            Event::Paste(text) => {
                if debug::enabled() {
                    debug::log(
                        "input",
                        format!(
                            "committed text chars={} bytes={} interactive={}",
                            text.chars().count(),
                            text.len(),
                            app.interactive
                        ),
                    );
                }
                app.handle_paste(text);
                Action::Continue
            }
            _ => Action::Continue,
        };
        if matches!(action, Action::Quit) {
            return Ok(());
        }
    }
}

/// Watch the controlling terminal and force the dashboard to exit if it hangs
/// up, then never return.
///
/// crossterm 0.28's `event::poll`/`event::read` spin forever on `read()==EOF`
/// once the terminal closes (their inner loop only breaks on `WouldBlock` or a
/// parsed event, never on end-of-file). That pins a CPU core *and* strands the
/// main thread in native code where it can no longer observe the shutdown flag,
/// so SIGTERM/SIGHUP are silently ignored. An independent watchdog is the only
/// reliable escape: it blocks in its own `poll` — regardless of where the main
/// thread is stuck — and terminates the process the moment the tty reports
/// hangup.
///
/// Only polling here (never reading) keeps input bytes intact for crossterm.
#[cfg(unix)]
fn spawn_terminal_hangup_watchdog(shutdown: Arc<AtomicBool>) {
    // Only meaningful when stdin is the tty crossterm actually reads from.
    if unsafe { libc::isatty(libc::STDIN_FILENO) } != 1 {
        return;
    }
    std::thread::spawn(move || {
        loop {
            if shutdown.load(Ordering::Relaxed) {
                return;
            }
            let mut poll_fd = libc::pollfd {
                // macOS only reports POLLHUP when POLLIN is also requested.
                fd: libc::STDIN_FILENO,
                events: libc::POLLIN,
                revents: 0,
            };
            // Wake at least once a second to re-check the shutdown flag; a real
            // hangup wakes the poll immediately with POLLHUP latched.
            let ready = unsafe { libc::poll(&mut poll_fd, 1, 1000) };
            if ready <= 0 {
                // Timeout (idle terminal) or EINTR — nothing to do, loop again.
                continue;
            }
            if poll_fd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
                debug::log(
                    "app",
                    "watchdog: controlling terminal hung up; exiting dashboard",
                );
                restore_terminal_best_effort();
                std::process::exit(0);
            }
            // Readable with pending input (no hangup): the main thread is
            // draining it. Back off briefly so we don't spin while it does.
            std::thread::sleep(Duration::from_millis(100));
        }
    });
}

#[cfg(not(unix))]
fn spawn_terminal_hangup_watchdog(_shutdown: Arc<AtomicBool>) {}

fn log_navigation_key(key: KeyEvent) {
    if !debug::enabled() {
        return;
    }
    if matches!(
        key.code,
        KeyCode::Left | KeyCode::Right | KeyCode::Up | KeyCode::Down | KeyCode::Enter
    ) {
        debug::log(
            "input",
            format!(
                "key={:?} modifiers={:?} kind={:?}",
                key.code, key.modifiers, key.kind
            ),
        );
    } else if cfg!(target_os = "macos")
        && key.modifiers.contains(crossterm::event::KeyModifiers::ALT)
        && matches!(key.code, KeyCode::Char('b' | 'f'))
    {
        debug::log(
            "input",
            format!(
                "key=OptionHorizontalAlias modifiers={:?} kind={:?}",
                key.modifiers, key.kind
            ),
        );
    } else if matches!(key.code, KeyCode::Char(character) if !character.is_ascii()) {
        debug::log(
            "input",
            format!(
                "key=Char(non-ascii) modifiers={:?} kind={:?}",
                key.modifiers, key.kind
            ),
        );
    }
}

fn emit_terminal_notification(terminal: &mut Tui, message: &str) -> Result<()> {
    let message: String = message
        .chars()
        .filter(|character| !character.is_control())
        .take(240)
        .collect();
    // Bell works everywhere; OSC 9 lets supporting terminals surface the
    // same event as a desktop notification without invoking platform tools.
    write!(terminal.backend_mut(), "\x07\x1b]9;Muxloom: {message}\x07")?;
    terminal.backend_mut().flush()?;
    Ok(())
}

/// Put `text` on the clipboard, reporting whether anything confirmed it landed.
///
/// A clipboard tool that exits cleanly has taken the text. OSC 52 is a request
/// the terminal is free to ignore — many refuse it by default — so it goes out
/// as well but cannot be counted as an answer.
fn emit_clipboard_copy(terminal: &mut Tui, text: &str) -> Result<bool> {
    let native = copy_native_clipboard(text);
    let encoded = base64_encode(text.as_bytes());
    write!(terminal.backend_mut(), "\x1b]52;c;{encoded}\x07")?;
    terminal.backend_mut().flush()?;
    debug::log(
        "clipboard",
        format!(
            "copied characters={} native={native} osc52=true",
            text.chars().count()
        ),
    );
    Ok(native)
}

#[cfg(target_os = "macos")]
fn copy_native_clipboard(text: &str) -> bool {
    write_clipboard_command("pbcopy", &[], text)
}

#[cfg(target_os = "windows")]
fn copy_native_clipboard(text: &str) -> bool {
    write_clipboard_command("clip.exe", &[], text)
}

/// Wayland and X11 both hand the clipboard to a helper program. Try the one
/// that matches the session first, then the other two, and let the OSC 52
/// fallback carry the copy on a machine that has none of them.
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn copy_native_clipboard(text: &str) -> bool {
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
    let mut tools: Vec<(&str, &[&str])> = vec![
        ("xclip", &["-selection", "clipboard"]),
        ("xsel", &["--clipboard", "--input"]),
    ];
    if wayland {
        tools.insert(0, ("wl-copy", &[]));
    } else {
        tools.push(("wl-copy", &[]));
    }
    tools
        .into_iter()
        .any(|(program, arguments)| write_clipboard_command(program, arguments, text))
}

/// Read the clipboard back, for the right-click that pastes.
///
/// Only a native tool can answer this. OSC 52 has a read form, but terminals
/// refuse it almost universally — letting a remote program read the clipboard
/// is a hole nobody wants — so a machine without a clipboard tool simply has
/// no clipboard to offer, and the user pastes with their own terminal instead.
#[cfg(target_os = "macos")]
fn read_native_clipboard() -> Option<String> {
    read_clipboard_command("pbpaste", &[])
}

/// `Get-Clipboard` prints the clipboard as lines, so it hands back a trailing
/// newline that was never copied. Left on, it would submit whatever it pasted.
#[cfg(target_os = "windows")]
fn read_native_clipboard() -> Option<String> {
    let text = read_clipboard_command("powershell", &["-NoProfile", "-Command", "Get-Clipboard"])?;
    Some(text.trim_end_matches(['\r', '\n']).to_string())
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn read_native_clipboard() -> Option<String> {
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
    let mut tools: Vec<(&str, &[&str])> = vec![
        ("xclip", &["-selection", "clipboard", "-o"]),
        ("xsel", &["--clipboard", "--output"]),
    ];
    if wayland {
        tools.insert(0, ("wl-paste", &["--no-newline"]));
    } else {
        tools.push(("wl-paste", &["--no-newline"]));
    }
    tools
        .into_iter()
        .find_map(|(program, arguments)| read_clipboard_command(program, arguments))
}

fn read_clipboard_command(program: &str, arguments: &[&str]) -> Option<String> {
    use std::process::{Command, Stdio};

    let output = Command::new(program)
        .args(arguments)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

fn write_clipboard_command(program: &str, arguments: &[&str], text: &str) -> bool {
    use std::process::{Command, Stdio};

    let Ok(mut child) = Command::new(program)
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let wrote = child
        .stdin
        .take()
        .is_some_and(|mut stdin| stdin.write_all(text.as_bytes()).is_ok());
    wrote && child.wait().is_ok_and(|status| status.success())
}

fn base64_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        output.push(ALPHABET[usize::from(first >> 2)] as char);
        output.push(ALPHABET[usize::from((first & 0x03) << 4 | second >> 4)] as char);
        output.push(if chunk.len() > 1 {
            ALPHABET[usize::from((second & 0x0f) << 2 | third >> 6)] as char
        } else {
            '='
        });
        output.push(if chunk.len() > 2 {
            ALPHABET[usize::from(third & 0x3f)] as char
        } else {
            '='
        });
    }
    output
}

fn enter_terminal() -> Result<Tui> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste,
        EnableFocusChange,
        PushKeyboardEnhancementFlags(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS,
        ),
        Hide
    )?;
    KEYBOARD_ENHANCEMENT_ACTIVE.store(true, Ordering::Relaxed);
    Terminal::new(CrosstermBackend::new(stdout)).context("failed to initialize terminal")
}

fn leave_terminal(terminal: &mut Tui) -> Result<()> {
    let raw_result = disable_raw_mode();
    if KEYBOARD_ENHANCEMENT_ACTIVE.swap(false, Ordering::Relaxed) {
        execute!(terminal.backend_mut(), PopKeyboardEnhancementFlags)?;
    }
    execute!(
        terminal.backend_mut(),
        DisableFocusChange,
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen,
        EnableLineWrap,
        SetAttribute(Attribute::Reset),
        ResetColor,
        Show
    )?;
    terminal.show_cursor()?;
    raw_result.context("failed to disable terminal raw mode")
}

fn restore_terminal_best_effort() {
    let _ = disable_raw_mode();
    let mut stdout = io::stdout();
    if KEYBOARD_ENHANCEMENT_ACTIVE.swap(false, Ordering::Relaxed) {
        let _ = execute!(stdout, PopKeyboardEnhancementFlags);
    }
    let _ = execute!(
        stdout,
        DisableFocusChange,
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen,
        EnableLineWrap,
        SetAttribute(Attribute::Reset),
        ResetColor,
        Show
    );
}

struct TerminalRestoreGuard {
    active: bool,
}

impl TerminalRestoreGuard {
    fn new() -> Self {
        Self { active: true }
    }

    fn disarm(&mut self) {
        self.active = false;
    }
}

impl Drop for TerminalRestoreGuard {
    fn drop(&mut self) {
        if self.active {
            debug::log("terminal", "restore guard activated");
            restore_terminal_best_effort();
        }
    }
}

fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        debug::log(
            "panic",
            format!("{panic_info}; backtrace={}", Backtrace::force_capture()),
        );
        restore_terminal_best_effort();
        default_hook(panic_info);
    }));
}

#[cfg(unix)]
fn install_job_control_guard() {
    // A background read must fail and unwind through the restore guard instead
    // of stopping the whole dashboard with `suspended (tty input)`.
    unsafe {
        libc::signal(libc::SIGTTIN, libc::SIG_IGN);
    }
}

#[cfg(not(unix))]
fn install_job_control_guard() {}

fn install_shutdown_handlers() -> Result<Arc<AtomicBool>> {
    let shutdown = Arc::new(AtomicBool::new(false));
    #[cfg(unix)]
    for signal in [
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGHUP,
        signal_hook::consts::SIGQUIT,
    ] {
        signal_hook::flag::register(signal, Arc::clone(&shutdown))?;
    }
    Ok(shutdown)
}

fn init_config(path: &Path) -> Result<()> {
    if path.exists() {
        bail!("config already exists: {}", path.display());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::write(path, EXAMPLE_CONFIG)
        .with_context(|| format!("failed to write {}", path.display()))?;
    println!("Created {}", path.display());
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Command {
    Run,
    Init,
    Update,
    Mcp,
    /// `board clear`: empty one machine's talk board.
    BoardClear,
    Help,
    Version,
}

struct CliOptions {
    command: Command,
    config_path: PathBuf,
    debug_log: Option<PathBuf>,
    config_explicit: bool,
    /// `--nightly` / `--stable`: which stream this one `update` should take,
    /// overriding the configuration. Nothing is written back — the build that
    /// gets installed is stamped with its own stream, so the next check
    /// follows it on its own.
    update_channel: Option<muxloom::update::Channel>,
    /// `--machine`: which machine a command that names one is aimed at.
    machine: Option<String>,
}

fn parse_args(args: &[String]) -> Result<CliOptions> {
    let mut command = Command::Run;
    let mut config_path = default_config_path();
    let mut debug_log = None;
    let mut config_explicit = false;
    let mut update_channel = None;
    let mut machine = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "init" => command = Command::Init,
            "update" => command = Command::Update,
            "mcp" => command = Command::Mcp,
            "board" => {
                index += 1;
                match args.get(index).map(String::as_str) {
                    Some("clear") => command = Command::BoardClear,
                    Some(other) => bail!("unknown board command: {other}"),
                    None => bail!("`board` needs a command: `muxloom board clear`"),
                }
            }
            "--machine" => {
                index += 1;
                let Some(name) = args.get(index) else {
                    bail!("--machine requires a machine id");
                };
                machine = Some(name.clone());
            }
            "--help" | "-h" => command = Command::Help,
            "--version" | "-V" => command = Command::Version,
            "--nightly" => update_channel = Some(muxloom::update::Channel::Nightly),
            "--stable" => update_channel = Some(muxloom::update::Channel::Stable),
            "--debug" => debug_log = Some(default_debug_log_path()),
            "--debug-log" => {
                index += 1;
                let Some(path) = args.get(index) else {
                    bail!("--debug-log requires a path");
                };
                debug_log = Some(PathBuf::from(path));
            }
            "--config" => {
                index += 1;
                let Some(path) = args.get(index) else {
                    bail!("--config requires a path");
                };
                config_path = PathBuf::from(path);
                config_explicit = true;
            }
            other => bail!("unknown argument: {other}"),
        }
        index += 1;
    }
    if update_channel.is_some() && command != Command::Update {
        bail!("--nightly and --stable only apply to `muxloom update`");
    }
    if machine.is_some() && command != Command::BoardClear {
        bail!("--machine only applies to `muxloom board clear`");
    }
    Ok(CliOptions {
        command,
        config_path,
        debug_log,
        config_explicit,
        update_channel,
        machine,
    })
}

fn print_help() {
    println!(
        "muxloom {}\n\nUSAGE:\n    muxloom [--config PATH] [--debug | --debug-log PATH]\n    muxloom init [--config PATH]\n    muxloom update [--nightly | --stable]\n    muxloom mcp [--config PATH]\n    muxloom board clear [--machine ID]\n\nOPTIONS:\n    -h, --help           Show this help\n    -V, --version        Show version\n        --config PATH    Use a custom TOML config\n        --nightly        Update from the rolling nightly build, and stay on it\n        --stable         Update from the tagged releases, and stay on them\n        --machine ID     Which machine `board clear` empties (default: this one)\n        --debug          Write detailed diagnostics to the state directory\n        --debug-log PATH Write diagnostics to a custom file\n\nCOMMANDS:\n    init                 Write an example config file\n    update               Download and install the newest build, if newer\n    mcp                  Serve muxloom's control surface over MCP stdio\n    board clear          Empty one machine's talk board, keeping replication marks\n",
        env!("CARGO_PKG_VERSION")
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(words: &[&str]) -> Result<CliOptions> {
        parse_args(
            &words
                .iter()
                .map(|word| word.to_string())
                .collect::<Vec<_>>(),
        )
    }

    #[test]
    fn clearing_a_board_names_the_machine_it_empties() {
        // The board is replicated rather than shared, so emptying one is
        // always about one machine, and the default is the one you are on.
        let here = parse(&["board", "clear"]).unwrap();
        assert_eq!(here.command, Command::BoardClear);
        assert_eq!(here.machine, None);
        assert_eq!(
            parse(&["board", "clear", "--machine", "soil"])
                .unwrap()
                .machine
                .as_deref(),
            Some("soil")
        );
        // A word that is not a command must not be read as one, and a machine
        // named for anything else is a typo rather than an instruction.
        assert!(parse(&["board"]).is_err());
        assert!(parse(&["board", "burn"]).is_err());
        assert!(parse(&["--machine", "soil"]).is_err());
        assert!(parse(&["board", "clear", "--machine"]).is_err());
    }
}
