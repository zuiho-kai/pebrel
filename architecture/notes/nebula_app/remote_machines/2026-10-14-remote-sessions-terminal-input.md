# Remote sessions as typed terminal text

## Status

Accepted on 2026-10-14 in the `remote-sessions` fork branch; not reviewed upstream.

## Context

The first version listed remote agent sessions and showed `conversation.read`
pages read-only. In use, the user needed to operate those sessions and start new
ones from the other machine, and required a remote session to look and behave
like a local one: a local Pebrel shows an agent as its terminal, not as a chat
page. Only the text should cross; font, colors and background stay local.

## Evidence

- A local agent tab is the terminal itself; the desktop has no chat view, so a
  conversation page plus composer was a second interaction model.
- `pane.read` with `screen` returns the cell grid (glyph, width, wire colors,
  style flags, cursor); the bridge turns repeated reads into row deltas against
  an acknowledged `screen_since` baseline (`runtime_api/mobile_screen.rs`).
- `pane.prompt` (text, `submit: false`) and `pane.send_key` (named keys with
  modifiers) are in the bridge input allowlist; Android uses the same split.
- A new Claude session asks whether to trust the folder before it reports a
  session id and writes no transcript before its first message, so any
  conversation-based view cannot reach it at all.
- `agent.start` launches the remote's verified agent command and registers the
  agent; typing `claude` into a new shell depends on the remote PATH.

## Decision

The SSH link runs `mobile-bridge --allow-input`; SSH authentication remains the
only admission rule. `agent.start` joins the bridge input allowlist.

`gpui_shell/remote_terminal.rs` replaces the conversation view. It polls
`pane.read` (`screen`, `screen_viewport`, `screen_since`) every 250 ms and paints
the grid with the local terminal font and palette: default foreground and
indexed colors resolve in the local theme, true-color cells keep their RGB, and
default-background cells stay transparent. The grid keeps the remote pane's
columns and rows; the font size is scaled from the local one so the whole grid
fits the view, growing or shrinking with the local window. Key handling follows the local
terminal's pass-through rules; control keys go out as `pane.send_key`, committed
IME text as one `pane.prompt` line. Inputs are sent one at a time in order and
never retried.

The machine group header has a `+` menu that calls `agent.start` for Claude or
Codex and opens the new pane as soon as the machine lists it.

## Rejected alternatives

- Conversation page with composer, option buttons and an interrupt button: a
  second UI the local app does not have, and blind to pre-session prompts.
- Resizing the remote PTY to the local view: changes the remote user's own pane
  and is not needed to show text.
- Streaming raw PTY bytes into a local `Term`: needs new attach/write/resize
  Runtime methods and a remote-sized terminal model.

## Consequences

- Text size follows the ratio between the local view and the remote grid, not
  the local font setting; a much smaller view gives small text.
- Scrollback exists only when the remote program keeps it in the terminal.
  Claude Code's fullscreen renderer keeps none, so the wheel is forwarded to it
  (`pane.scroll`) and it repaints its own history, as it does locally.
- Selection and copy are local; paste goes as `pane.paste` (bracketed), or as a
  `pane.prompt` line when the remote program has bracketed paste off.
- Adding `agent.start` to the shared policy also grants it to mobile clients.

## Validation

- Unit tests: `gpui_shell::remote_terminal::tests`, `remote_machines::tests`,
  `ssh_session::remote_runtime::tests`.
- Manual, Windows client to macOS host over Tailscale SSH: `+` started Claude,
  the view opened on the trust prompt, ↓/Enter answered it, IME text plus Enter
  sent a message and the reply rendered with the local video background.

## Supersedes

`2026-10-13-ssh-remote-sessions.md`

## Revisit when

Remote scrollback or selection is needed, the Runtime gains a raw attach, or
Windows hosts must be probed.
