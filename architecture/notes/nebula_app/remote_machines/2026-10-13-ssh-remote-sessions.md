# Remote Pebrel sessions over an SSH pane

## Status

Proposed on 2026-10-13 in the `remote-sessions` fork branch; not reviewed upstream.

## Context

A user works on one desktop and runs agents in Pebrel on another machine. They
want the other machine's agent sessions in their own sidebar, without a second
login, pairing, approval prompt or LAN requirement, and only to read messages.
The machines are often on different networks.

## Evidence

- `runtime_api/mobile_bridge.rs` already serves an allowlisted, view-only
  Runtime channel over stdin/stdout ("one authenticated SSH exec channel"), and
  the Android client already invokes `pebrel mobile-bridge`; the desktop CLI
  never exposed that subcommand.
- `conversation.read` returns structured, revision-checked messages from the
  agent's own session file. A pane's `pane.read` screen grid depends on the
  remote window geometry, which a reader on another display cannot control.
- `ssh_session/transcript.rs` already lends a pane's authenticated connection to
  short exec channels and revokes the capability when the pane ends.
- Non-interactive SSH on macOS has `PATH=/usr/bin:/bin:/usr/sbin:/sbin`; the app
  binary is not on it. Older builds without the subcommand exit non-zero for
  `pebrel help mobile-bridge`.

## Decision

SSH authentication is the only admission rule. After a native SSH pane reaches
`Ready`, `ssh_session/remote_runtime.rs` opens a separate exec channel on that
same connection. A POSIX probe selects the first installed Pebrel that has the
subcommand and runs `mobile-bridge` without `--allow-input`. Nothing is listed
unless the bridge answers `mobile.ready`.

`remote_machines.rs` is a transport-neutral registry keyed by the pane's SSH
destination. One link per destination owns the request queue; other panes to
the same destination hold a standby task and take over when the owner closes.
A dropped link keeps its last projection, dimmed, until a pane reconnects.
Group names are stored in `remote_machines.json` beside other app data.

The GPUI shell renders one sidebar group per machine below TABS and shows a
selected conversation in the content area, outside `tabs`, so it never enters
session persistence or tab ordering. The view polls `conversation.read` every
two seconds with the last revision.

## Rejected alternatives

- Mirroring the remote terminal grid: needs push subscription, full keyboard
  input and has no control over the remote pane size.
- mDNS discovery plus first-use approval: limited to one LAN, and an extra
  approval adds nothing when the user already authenticated over SSH.
- A local TCP forward to the remote Runtime port: exposes the desktop token
  model to another host and bypasses the bridge allowlist.
- A separate agent-host daemon that owns PTYs apart from the GUI: changes PTY
  ownership and is far outside this read-only feature.

## Consequences

- The remote must run a Pebrel build with `mobile-bridge`, and its GUI must be
  running; remote sessions still end when that Pebrel quits.
- Remote shells must accept `sh -c`; Windows OpenSSH with `cmd.exe` is not
  probed yet and simply shows no group.
- Destinations are not canonicalized: two spellings of one host make two groups.
- Groups appear only in sidebar tab mode.

## Validation

- Unit tests: `remote_machines::tests`, `ssh_session::remote_runtime::tests`,
  `gpui_shell::remote_conversation::tests`.
- Manual on macOS 27 with an isolated `PEBREL_CONFIG_DIR` instance restoring an
  SSH tab to `localhost`: the probe skipped an older `/Applications` build and
  started the new CLI; the group listed three Claude sessions; selecting one
  rendered its latest page; a write request was refused `input_not_authorized`.

## Supersedes

None.

## Revisit when

Remote input is wanted, Windows hosts need probing, or the Runtime gains a
pushed conversation subscription that can replace polling.
