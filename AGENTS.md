# Muxloom Engineering Guide

These rules define the product invariants that changes in this repository must
preserve.

## Runtime boundaries

- `muxloom` is the controller. `muxloomd` owns remote PTYs, session metadata,
  append-only history, and file operations. Closing the controller or losing an
  SSH connection must not terminate a running child process.
- The normal daemon data plane must not depend on target-side `tmux`, `file`,
  `ffmpeg`, or similar utilities. Transfer encoded media to the controller and
  decode it there; never stream remote RGB frames.
- Initial bootstrap may require SSH and a POSIX shell. When a compatible
  companion is installed, normal operation must use the Rust implementation.
- The explicit tmux path is a compatibility fallback. It must remain usable for
  older sessions, but it must never be selected silently.
- Terminal sessions are ephemeral: removing one deletes it. Supported Codex and
  Claude sessions can be archived, searched, and resumed.
- A temporary session is a scratch pad and gets a scratch folder: the daemon
  makes one it owns, runs the session there whatever directory the client named,
  and removes it when the session ends, is deleted, or is found stale. It must
  never inherit a project directory, and it must never aim the machine's next
  ordinary launch.

## Transport and compatibility

- Keep one persistent SSH bridge as the normal data plane for each target.
  Requests, file streams, status, and reverse-tunnel traffic should multiplex
  through it instead of opening a connection per operation.
- Bootstrap, legacy runtime staging, and compatibility fallbacks may currently
  reuse an SSH ControlMaster and `scp`. Treat these as compatibility paths to be
  converged, not as proof that every target has exactly one SSH process today.
- A new controller must preserve each session kind's supported discovery,
  attach, archive, search, and identification semantics for sessions created by
  older `muxloomd` generations and by the explicit tmux fallback.
- Prefer additive protocol and capability changes. Do not bump the wire
  protocol merely to add a file type, metadata field, or optional feature that
  the controller can normalize safely.
- A compatibility fallback must be visible in the TUI, debug log, and terminal
  notifications. Include the reason and affected machine.
- Compare companion build fingerprints, not only the wire protocol. Fingerprint
  calculation must live in the Rust binaries and must not depend on target
  utilities such as `sha256sum` or `shasum`.
- Provisioning a runtime or a companion onto a remote target must offer the
  target its own download first and fall back to pushing bytes over the
  existing connection. The controller resolves only release metadata; the
  target verifies the payload against that digest before installing it, and a
  companion pull is offered only when the published digest equals the asset we
  would otherwise send, so a pull can never install different bytes than a push.
- A publisher's manifest says what to download, not where from: a payload URL
  that leaves the host the manifest was itself read from must be refused rather
  than followed. Which digest a publisher stands behind is its call, not ours,
  so the algorithm travels with the release and the controller and the target
  must check the same one.
- Every target-side fetch must be bounded — connect timeout, total timeout, and
  a stall guard — so a machine with no route to the release fails in seconds
  instead of hanging the install. When every built-in path fails, the reported
  error must name each attempt rather than only the last one.

## Session keepers and non-disruptive daemon upgrades

- Every managed session is owned by its keeper process: PTY, child process,
  and raw history append, nothing more. The keeper's socket protocol is
  version 1 forever — running keepers outlive arbitrarily many daemon
  generations, so every future daemon must keep speaking it. Do not extend the
  keeper's responsibilities; new behavior belongs in the daemon.
- Controller exit, binary deployment, daemon upgrades, and daemon crashes must
  not stop running agents. Typed input from the daemon goes through the keeper;
  the daemon never owns a session PTY directly.
- Deploy a new binary atomically, then drain the old daemon. Handover requires
  a sole client but not idle sessions: live sessions ride their keepers across,
  and the next generation adopts every keeper socket it finds, rebuilding
  screen and activity state from the history tail.
- Current daemons must enter draining atomically with client registration and
  agent launch. Once draining starts, reject new work, acknowledge handover,
  exit voluntarily, and treat the keeper hanging up as the transfer it is —
  never as a session death.
- An upgrade must land without anyone pressing anything. A handover is asked
  for by the arriving build and answered by the one being replaced, so every
  improvement to the answer reaches only daemons newer than the ones that need
  it: the arriving build must keep its own patience and, after waiting longer
  than a daemon able to retire itself would ever need, stop the running one and
  serve in its place. Only a newer version may insist — two builds of one
  version take turns by design — and only because the keepers make it free.
- A keeper that outlives its record must be dismissed, and a record that
  outlives its keeper must be retired into the archive with its history; no
  session may end up unaccounted for on either side.
- Preserve session metadata and append-only history across handover. A future
  incompatible upgrade must use side-by-side routing or explicit state
  transfer; it must not sacrifice active sessions for a simpler restart.
- Upgrade regressions must cover a live session crossing a handover with its
  process intact, a daemon crash followed by adoption, and a dead keeper
  followed by archive recovery.

## Control surfaces

- External adapters (MCP today, hardware bridges tomorrow) reach muxloom only
  through the `ControlSurface` trait in `src/control.rs`. New capabilities are
  added to the surface, not to an individual transport.
- Typed input from an adapter must use the `SendInput` request. Opening a PTY
  stream resizes the session under an attached terminal and must never be used
  just to write bytes.
- An adapter client of the local daemon must use short-lived connections per
  call: a resident client holds the client count up and defers generation
  handover indefinitely.
- A machine the user has not enabled must be unreachable from every adapter,
  by name or otherwise.
- MCP transports own stdout. Nothing but protocol messages may be printed
  while a surface is being served; diagnostics go to the debug log.
- A starting daemon registers an MCP surface in the agent configuration of the
  user it runs as. It owns exactly the `muxloom` entry, rewrites it only when
  missing or stale, leaves a file it cannot parse untouched, and honours an
  explicit opt-out from the environment. The skill it installs follows the same
  rule under its own revision stamp: no stamp means the file is the user's.
- That entry belongs to the machine, not to a process: one per machine, pointing
  at the daemon on every machine including the controller's own, and claimed
  only by the daemon serving the machine's own state directory. A daemon handed
  someone else's state directory stays out unless it is told otherwise, so no
  test, scratch instance, or second daemon can quietly repoint the user's agents
  at a fleet that isn't theirs.
- Which surface a session gets is decided when it calls, not by a second entry.
  The daemon serves its own flavor to every agent, and hands the session over to
  the controller beside it when the caller's working directory is a moderator's.
  An ordinary agent drives its own machine and talks to the rest; the fleet-wide
  flavor is the moderator's.
- A launch on the daemon flavor lands in the caller's own folder or inside it,
  and defaults to it, so starting a subagent needs no argument at all. Not a
  sandbox — the same agent has a shell on that machine — but a statement of what
  the surface is for: the sessions it starts are the caller's own. A refusal must
  name both folders and point at the better move, which is to ask the agent that
  already lives there.
- Cross-machine capability belongs to the controller flavor. A daemon-flavor
  surface reaches other machines only by relaying through an attached
  controller, and only for the relay whitelist — never a shell, a machine
  enablement, or an SSH edit. With no controller attached those calls fail
  immediately rather than waiting. The line is look, speak, and — only after a
  person approves it — a narrow set of writes. Tools that only report, plus
  the ones that say something, may be relayed; starting a session on a far
  machine and typing into one may be relayed too, but the controller holds
  each such call behind the person's approval (one-shot, or remembered for
  the asking session) and asks over the bound chat; deny ends it, and the most
  destructive writes are never remembered across a cart approval. Everything
  else that alters the far machine belongs to the agents living there.
  Nothing that waits is relayed either — a relayed job runs on the controller's
  own round, and a tool that sits there holds up every other machine's errands.
- A daemon learns the fleet only from a controller coming round to ask for
  work, and never goes looking: no SSH from a daemon, no discovery. Two machines
  with no route between them are neighbours for exactly as long as a controller
  that sees both is running, and machines reached that way are marked `remote`
  with the name of the way there. `local` is what a daemon calls the machine it
  runs on, so a controller names its own machine to the fleet by its hostname
  and reads that name back as itself.
- The talk store is append-only per origin, idempotent by message id, and
  replicated by version vector. Only the controller drives a sync round, since
  it is the only side that can open a connection. Compaction raises the low
  water mark rather than rewriting history.
- A direct message is delivered inside an envelope naming its sender, machine,
  and how to answer, rendered in exactly one place. It must never be typed into
  a session in a form that reads as the user's own input.
- Screens handed to an adapter are plain text: styling and cursor control are
  stripped while column positions survive as spaces, so a rendered menu stays
  readable as one.
- A moderator is an ordinary local session in a folder muxloom owns, and the
  scope it was given is a briefing, not a sandbox: the surface answers for
  everything the user enabled, whoever is asking. The briefing has to say so,
  and so does the form that collects it.

## Terminal correctness

- Bound every embedded terminal parser and resize operation to the actual pane
  dimensions. Preserve wide-cell invariants across shrink, reflow, and erase.
- Mouse reporting, direct text selection, bracketed paste, modifier keys, and
  IME input must keep working in attached sessions.
- The clipboard gets a button of its own: right-click over the terminal copies
  the selection and pastes when there is none, and `Alt` is the way through to
  an application that wants the button. A selection is never taken on
  button-up — a release the user did not aim must not overwrite the clipboard.
- Reading a pointer as a finger takes evidence, and the terminal's own identity
  outranks how the pointer moved: a desktop emulator never latches touch mode,
  and one hover — which no touch screen can produce — takes back a guess for
  the rest of the run. Losing text selection to a misread mouse is worse than
  any gesture it buys.
- Normal exit, errors, panics, and handled signals must restore raw mode, the
  alternate screen, mouse capture, cursor visibility, and the outer terminal's
  attributes.
- Responsive layout follows rendered geometry. Non-compact portrait keeps the
  terminal above navigation; compact mode may show only the focused pane; only
  landscape places navigation beside the terminal. Persist portrait and
  landscape split ratios independently.

## Status, files, and media

- Animate only Codex or Claude sessions whose current visible terminal state
  is classified as working, and only on the agent row itself. Folder rows
  carry aggregated state as steady colour (attention outranks working);
  machine rows show static capability icons. Idle, waiting, archived, and
  plain terminal sessions must not animate.
- Working requires recent PTY output plus a visible sign that the turn is
  running: the CLI's interrupt marker, or the live status line a phase that
  offers no interrupt still draws. A stale screen over a quiet PTY must read as
  idle. Attention outranks working, and controller-configured attention
  patterns are sunk into the daemon so both classifiers agree.
- The grouped session view must separate one folder from the next visually, not
  only by indentation, and temporal sessions sort above every folder so the
  scratch chat just opened is the first thing on screen.
- File-browser input is modal: while it is focused, application shortcuts must
  not leak into the machine or agent views.
- Tag asynchronous directory and preview results with their request identity.
  Ignore stale replies, keep cached navigation responsive, and clear preview
  state while an empty or new selection loads.
- Keep large histories and files off the hot render path. Page or stream them,
  cache bounded neighboring data, and compress large transfers when useful.
- Detect text by content as well as extension. Structured JSON, JSONL, CSV, and
  Markdown parsing errors must be explicit; preserve readable source text when
  practical.

## Documentation

- `README.md` describes the current product. Keep the complete English guide
  first and the corresponding Chinese guide second.
- Keep migration history, incident narratives, fixed-bug lists, release-operator
  commands, and development logs out of the main README. Put release history in
  a changelog or GitHub Releases when needed.
- User-visible behavior, shortcuts, configuration fields, architecture claims,
  platform support, and limitations must match the code and CI workflows.
- Prefer stable section anchors and verify internal links after restructuring.

## Change discipline

- Keep each major feature or independent fix in its own commit. Commit messages
  must name the behavior changed rather than use a generic update description.
- Add focused regression coverage for changed behavior. Before handoff, run:
  `cargo fmt --all -- --check`,
  `cargo clippy --locked --all-targets -- -D warnings`, and
  `cargo test --locked --all-targets -- --test-threads=1`.
- Remote integration tests are opt-in. Use an explicit target, leave no test
  sessions behind, and never include private infrastructure paths in fixtures.
- No test may reach the machine's own state directory. A suite that needs a
  daemon, a keeper, or a companion stands up a scratch root and hands it to the
  child processes it starts; `MUXLOOMD_STATE_DIR` unset means the developer's
  live fleet, and a test build refuses to open a local companion without one.
- A release version must match in `Cargo.toml`, `Cargo.lock`, and the Git tag.
- Every commit on `main` that passes the full regression suite is republished
  as the rolling `nightly` prerelease. It must stay a prerelease, because
  `releases/latest` — which the stable check and the remote companion pull both
  read — has to keep naming the newest tagged release. A rolling tag cannot say
  which build it holds and every nightly between two releases shares one package
  version, so the build must carry the stream, commit, and commit count it was
  made at, and a build that carries no count is never offered a same-version
  update.
- An install follows the stream its own build came from unless it is told
  otherwise: a release stays on releases, a nightly stays on nightlies. Nobody
  is moved onto a cadence they did not ask for, and crossing over must not
  require editing configuration — the build that gets installed is what decides
  the next check.
- Pushes, tags, releases, and other externally visible mutations require
  explicit user authorization.
