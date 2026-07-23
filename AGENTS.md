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

## Non-disruptive daemon upgrades

- Never kill or replace a daemon process that owns a live PTY. Controller exit,
  binary deployment, and daemon upgrades must not stop running agents.
- Deploy a new binary atomically, then drain the old daemon. While it owns live
  sessions or another controller client, the new bridge must continue using the
  old generation through the compatible protocol.
- Handover is allowed only after the old daemon reports no live sessions and no
  other clients. The bridge may then request and observe its voluntary exit,
  remove stale socket state, start the newly installed generation, and
  reconnect.
- Current daemons must enter draining atomically with client registration and
  agent launch. Once draining starts, reject new work, acknowledge handover, and
  exit voluntarily; do not rely on a check-then-kill race.
- Preserve session metadata and append-only history across handover. A future
  incompatible upgrade must use side-by-side routing or explicit state
  transfer; it must not sacrifice active sessions for a simpler restart.
- Upgrade regressions must cover both an active session, where handover is
  deferred, and an idle daemon, where the new generation starts without tmux.

## Terminal correctness

- Bound every embedded terminal parser and resize operation to the actual pane
  dimensions. Preserve wide-cell invariants across shrink, reflow, and erase.
- Mouse reporting, direct text selection, bracketed paste, modifier keys, and
  IME input must keep working in attached sessions.
- Normal exit, errors, panics, and handled signals must restore raw mode, the
  alternate screen, mouse capture, cursor visibility, and the outer terminal's
  attributes.
- Responsive layout follows rendered geometry. Non-compact portrait keeps the
  terminal above navigation; compact mode may show only the focused pane; only
  landscape places navigation beside the terminal. Persist portrait and
  landscape split ratios independently.

## Status, files, and media

- Animate only Codex or Claude sessions whose current visible terminal state is
  classified as working. Idle, waiting, archived, and plain terminal sessions
  must not animate. Keep machine, folder, and agent-row indicators consistent.
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
- A release version must match in `Cargo.toml`, `Cargo.lock`, and the Git tag.
- Pushes, tags, releases, and other externally visible mutations require
  explicit user authorization.
