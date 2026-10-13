//! One AI session running in a remote Pebrel, shown as its terminal text.
//!
//! Only the text grid crosses the link: `pane.read` with `screen_history`, so
//! every row carries its absolute position, plus row deltas against the
//! acknowledged `screen_since` baseline. Without a start the read is the live
//! tail; the wheel moves an absolute start into the scrollback, or is forwarded
//! as `pane.scroll` when the remote program owns the wheel.
//!
//! Font, colors and background are this machine's; a default-background cell
//! stays transparent like a local terminal cell, and rows wrap at the local
//! width instead of being clipped. Selection is local and anchored to absolute
//! rows. Typing goes back in order as `pane.prompt` (text, never submitted),
//! `pane.send_key` (named keys) and `pane.paste` (clipboard blocks). The view
//! uses the local terminal's key context, so copy/paste bindings are shared.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::ops::Range;
use std::rc::Rc;
use std::time::Duration;

use gpui::prelude::FluentBuilder as _;
use gpui::{
    App, Bounds, ClipboardItem, Context, ElementInputHandler, EntityInputHandler, FocusHandle,
    FontStyle, FontWeight, HighlightStyle, InteractiveElement as _, IntoElement, KeyDownEvent,
    MouseButton, MouseDownEvent, MouseMoveEvent, ParentElement as _, Pixels, Point, Render, Rgba,
    ScrollWheelEvent, SharedString, StrikethroughStyle, Styled as _, StyledText, Task, TextLayout,
    UTF16Selection, UnderlineStyle, Window, canvas, div, px,
};
use serde_json::{Value, json};

use crate::gpui_shell::config::Settings;
use crate::gpui_shell::prelude::*;
use crate::gpui_shell::terminal::colors::Palette;
use crate::gpui_shell::terminal::element;
use crate::gpui_shell::terminal::{KEY_CONTEXT, TerminalBackTab, TerminalTab};
use crate::gpui_shell::workspace::{CopySelection, PasteClipboard};
use crate::i18n::Message;
use crate::remote_machines::{self, RemoteAgent};

const POLL: Duration = Duration::from_millis(250);
/// Wire color codes for the default foreground and background (`-(index + 1)`).
const DEFAULT_FG: i32 = -257;
const DEFAULT_BG: i32 = -258;
/// Rows asked for before the remote said how tall its viewport is.
const FIRST_ROWS: usize = 200;
/// Line height relative to the font size, shared by layout and hit testing.
const LINE_SCALE: f32 = 1.3;
/// The bridge refuses requests above 40 KiB; keep room for the envelope.
const MAX_INPUT_BYTES: usize = 38 * 1024;

pub struct RemoteTerminalView {
    pub focus_handle: FocusHandle,
    machine: String,
    agent: RemoteAgent,
    connected: bool,
    screen: Screen,
    /// Absolute top row while reading the scrollback; `None` follows the tail.
    anchor: Option<u64>,
    /// Wheel movement not yet worth a whole row.
    wheel_rest: f32,
    error: Option<String>,
    /// Inputs not yet delivered, sent one at a time so their order holds.
    outbox: VecDeque<Input>,
    sending: bool,
    marked_text: Option<String>,
    selection: Option<(TextPos, TextPos)>,
    selecting: bool,
    /// The rows as last rendered: their text, layout, and paint bounds.
    shown: Vec<String>,
    layouts: Vec<TextLayout>,
    painted: Rc<RefCell<Vec<Option<Bounds<Pixels>>>>>,
    /// Displayed row and byte of the remote cursor, for the IME window.
    cursor_at: Option<(usize, usize)>,
    _poll: Task<()>,
}

/// An absolute remote row and a byte offset in its displayed text.
type TextPos = (u64, usize);

struct Input {
    method: &'static str,
    params: Value,
    /// Sent instead when the remote refuses the first form (a paste into a
    /// program that has not enabled bracketed paste).
    fallback: Option<(&'static str, Value)>,
}

#[derive(Default)]
struct Screen {
    rows: Vec<Vec<Cell>>,
    /// Rows the remote soft-wrapped into the next one; copy joins them.
    wrapped: Vec<bool>,
    /// `(row, column)` within `rows` when the remote cursor is visible there.
    cursor: Option<(usize, usize)>,
    /// Absolute row of `rows[0]` and the scrollback bounds around it.
    first: u64,
    oldest: u64,
    live_start: u64,
    live_rows: usize,
    application_scroll: bool,
    /// Bridge baseline the next read acknowledges; rows arrive as deltas on it.
    seq: Option<u64>,
}

#[derive(Clone, Debug, PartialEq)]
struct Cell {
    text: String,
    fg: i32,
    bg: i32,
    flags: u8,
}

impl Screen {
    /// Apply one `pane.read` result: a full `screen` or a `screen_delta` on the
    /// baseline we acknowledged. A delta on any other base is dropped and the
    /// next read, acknowledging nothing usable, gets a full snapshot.
    fn apply(&mut self, result: &Value) {
        let seq = result["screen_seq"].as_u64();
        if let Some(screen) = result.get("screen").filter(|screen| screen.is_object()) {
            self.rows = screen["rows"].as_array().into_iter().flatten().map(row).collect();
            self.apply_meta(screen);
            self.seq = seq;
        } else if let Some(delta) = result.get("screen_delta").filter(|delta| delta.is_object()) {
            if delta["base"].as_u64() != self.seq {
                self.seq = None;
                return;
            }
            for change in delta["rows"].as_array().into_iter().flatten() {
                if let Some(index) = change[0].as_u64().map(|index| index as usize)
                    && let Some(slot) = self.rows.get_mut(index)
                {
                    *slot = row(&change[1]);
                }
            }
            self.apply_meta(delta);
            self.seq = seq;
        }
    }

    fn apply_meta(&mut self, screen: &Value) {
        if let Some(wrapped) = screen["wrapped"].as_array() {
            self.wrapped = wrapped.iter().map(|flag| flag == true).collect();
        }
        // The wire cursor is `[column, row, visible]`.
        let cursor = &screen["cursor"];
        self.cursor = (cursor[2] != 0)
            .then(|| Some((cursor[1].as_u64()? as usize, cursor[0].as_u64()? as usize)))
            .flatten();
        let history = &screen["history"];
        if let (Some(first), Some(oldest), Some(live_start)) =
            (history["first"].as_u64(), history["oldest"].as_u64(), history["live_start"].as_u64())
        {
            self.first = first;
            self.oldest = oldest;
            self.live_start = live_start;
            self.live_rows = history["live_rows"].as_u64().unwrap_or(0) as usize;
            self.application_scroll = history["application_scroll"] == true;
        }
    }
}

fn row(cells: &Value) -> Vec<Cell> {
    cells
        .as_array()
        .into_iter()
        .flatten()
        .map(|cell| Cell {
            text: cell[0].as_str().unwrap_or_default().to_owned(),
            fg: cell[2].as_i64().unwrap_or(DEFAULT_FG as i64) as i32,
            bg: cell[3].as_i64().unwrap_or(DEFAULT_BG as i64) as i32,
            flags: cell[4].as_u64().unwrap_or(0) as u8,
        })
        .collect()
}

/// A wire color in this machine's palette. Indexed and named colors use the
/// local theme; only true-color cells keep the remote's exact RGB. `None` is
/// the default background, which stays transparent.
fn local_color(code: i32, palette: &Palette) -> Option<Rgba> {
    match code {
        DEFAULT_BG => None,
        DEFAULT_FG => Some(palette.foreground),
        code if code >= 0 => Some(rgba(code as u32)),
        code => {
            let rgb = palette.query_reply(
                (-code - 1) as usize,
                &nebula_terminal::term::color::Colors::default(),
            );
            Some(rgba((u32::from(rgb.r) << 16) | (u32::from(rgb.g) << 8) | u32::from(rgb.b)))
        },
    }
}

fn rgba(packed: u32) -> Rgba {
    let channel = |shift: u32| ((packed >> shift) & 0xff) as f32 / 255.0;
    Rgba { r: channel(16), g: channel(8), b: channel(0), a: 1.0 }
}

fn cell_style(cell: &Cell, palette: &Palette) -> HighlightStyle {
    let mut style = HighlightStyle {
        color: local_color(cell.fg, palette).map(Into::into),
        background_color: local_color(cell.bg, palette).map(Into::into),
        ..Default::default()
    };
    if cell.flags & 1 != 0 {
        style.font_weight = Some(FontWeight::BOLD);
    }
    if cell.flags & 2 != 0 {
        style.font_style = Some(FontStyle::Italic);
    }
    if cell.flags & 4 != 0 {
        style.underline = Some(UnderlineStyle { thickness: px(1.0), ..Default::default() });
    }
    if cell.flags & 8 != 0 {
        style.strikethrough = Some(StrikethroughStyle { thickness: px(1.0), ..Default::default() });
    }
    if cell.flags & 16 != 0 {
        style.fade_out = Some(0.4);
    }
    style
}

/// GPUI key names that `pane.send_key` accepts under its own name.
fn runtime_key(key: &str) -> Option<&'static str> {
    Some(match key {
        "escape" => "escape",
        "enter" => "enter",
        "tab" => "tab",
        "backspace" => "backspace",
        "up" => "up",
        "down" => "down",
        "left" => "left",
        "right" => "right",
        "home" => "home",
        "end" => "end",
        "insert" => "insert",
        "delete" => "delete",
        "pageup" => "page_up",
        "pagedown" => "page_down",
        "f1" => "f1",
        "f2" => "f2",
        "f3" => "f3",
        "f4" => "f4",
        "f5" => "f5",
        "f6" => "f6",
        "f7" => "f7",
        "f8" => "f8",
        "f9" => "f9",
        "f10" => "f10",
        "f11" => "f11",
        "f12" => "f12",
        _ => return None,
    })
}

const LETTERS: [&str; 26] = [
    "a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l", "m", "n", "o", "p", "q", "r", "s",
    "t", "u", "v", "w", "x", "y", "z",
];

/// One plain-text line for `pane.prompt`, which refuses control characters.
fn prompt_text(text: &str) -> Option<String> {
    let line: String = text
        .chars()
        .map(|ch| if ch == '\n' || ch == '\r' || ch == '\t' { ' ' } else { ch })
        .filter(|ch| !ch.is_control())
        .collect();
    (!line.is_empty()).then_some(line)
}

fn ordered((a, b): (TextPos, TextPos)) -> (TextPos, TextPos) {
    if a <= b { (a, b) } else { (b, a) }
}

/// The scrollback anchor after the wheel moved `rows` (positive = older).
/// `None` means back at the live tail.
fn scrolled(screen: &Screen, anchor: Option<u64>, rows: i64) -> Option<u64> {
    let top = anchor.unwrap_or(screen.first);
    let next = top.saturating_add_signed(-rows).max(screen.oldest);
    (next < screen.live_start).then_some(next)
}

impl RemoteTerminalView {
    pub fn new(machine: String, agent: RemoteAgent, cx: &mut Context<Self>) -> Self {
        let poll = cx.spawn(async move |this, cx| {
            loop {
                let Ok((machine, anchor, params)) = this.update(cx, |view, _| view.read_request())
                else {
                    return;
                };
                let reply = remote_machines::request(&machine, "pane.read", params).await;
                if this.update(cx, |view, cx| view.apply(anchor, reply, cx)).is_err() {
                    return;
                }
                cx.background_executor().timer(POLL).await;
            }
        });
        Self {
            focus_handle: cx.focus_handle(),
            machine,
            agent,
            connected: true,
            screen: Screen::default(),
            anchor: None,
            wheel_rest: 0.0,
            error: None,
            outbox: VecDeque::new(),
            sending: false,
            marked_text: None,
            selection: None,
            selecting: false,
            shown: Vec::new(),
            layouts: Vec::new(),
            painted: Rc::default(),
            cursor_at: None,
            _poll: poll,
        }
    }

    pub fn machine(&self) -> &str {
        &self.machine
    }

    pub fn target(&self) -> (u64, u64) {
        (self.agent.window_id, self.agent.pane_id)
    }

    /// Follow the machine's live projection of this pane.
    pub fn sync(&mut self, agent: Option<&RemoteAgent>, connected: bool, cx: &mut Context<Self>) {
        let mut changed = self.connected != connected;
        self.connected = connected;
        if let Some(agent) = agent
            && *agent != self.agent
        {
            self.agent = agent.clone();
            changed = true;
        }
        if changed {
            cx.notify();
        }
    }

    /// The next read, tagged with the anchor it was asked for.
    fn read_request(&self) -> (String, Option<u64>, Value) {
        let rows = if self.screen.live_rows == 0 { FIRST_ROWS } else { self.screen.live_rows };
        let mut history = json!({ "rows": rows.clamp(1, 200) });
        if let Some(anchor) = self.anchor {
            history["start"] = anchor.into();
        }
        let params = json!({
            "window_id": self.agent.window_id,
            "pane_id": self.agent.pane_id,
            "screen": true,
            "screen_history": history,
            "screen_since": self.screen.seq.unwrap_or(0),
        });
        (self.machine.clone(), self.anchor, params)
    }

    fn apply(&mut self, anchor: Option<u64>, reply: Result<Value, String>, cx: &mut Context<Self>) {
        // A read asked before the wheel or typing moved the anchor is stale.
        if anchor != self.anchor {
            return;
        }
        match reply {
            Ok(result) => {
                self.error = None;
                self.screen.apply(&result);
                if self.screen.application_scroll {
                    self.anchor = None;
                }
            },
            Err(error) => self.error = Some(error),
        }
        cx.notify();
    }

    fn read_now(&self, cx: &mut Context<Self>) {
        let (machine, anchor, params) = self.read_request();
        cx.spawn(async move |this, cx| {
            let reply = remote_machines::request(&machine, "pane.read", params).await;
            let _ = this.update(cx, |view, cx| view.apply(anchor, reply, cx));
        })
        .detach();
    }

    fn on_scroll_wheel(
        &mut self,
        event: &ScrollWheelEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let size = cx.try_global::<Settings>().map_or(15.0, |settings| settings.font_size_px);
        let line = px(size * LINE_SCALE);
        let delta = event.delta.pixel_delta(line).y / line + self.wheel_rest;
        let rows = delta.trunc();
        self.wheel_rest = delta - rows;
        let rows = rows as i64;
        cx.stop_propagation();
        if rows == 0 {
            return;
        }
        if self.screen.application_scroll {
            // The remote program reads the wheel itself (alt screen, mouse mode).
            let params = json!({ "lines": rows.clamp(-32, 32), "column": 0, "row": 0 });
            self.push(Input::new("pane.scroll", params), cx);
            return;
        }
        let next = scrolled(&self.screen, self.anchor, rows);
        if next != self.anchor {
            self.anchor = next;
            self.read_now(cx);
            cx.notify();
        }
    }

    /// Back to the live tail, as typing does in a terminal.
    fn follow_live(&mut self, cx: &mut Context<Self>) {
        if self.anchor.take().is_some() {
            self.read_now(cx);
            cx.notify();
        }
    }

    fn push(&mut self, mut input: Input, cx: &mut Context<Self>) {
        if !self.connected {
            return;
        }
        let (window_id, pane_id) = (self.agent.window_id, self.agent.pane_id);
        let target = |params: &mut Value| {
            params["window_id"] = window_id.into();
            params["pane_id"] = pane_id.into();
        };
        target(&mut input.params);
        if let Some((_, params)) = input.fallback.as_mut() {
            target(params);
        }
        self.outbox.push_back(input);
        self.flush(cx);
    }

    /// Deliver the next input. A failed one is reported, not retried: its
    /// outcome on the remote is unknown, and replaying keys could repeat them.
    /// Only an explicit refusal of the first form sends its fallback.
    fn flush(&mut self, cx: &mut Context<Self>) {
        if self.sending {
            return;
        }
        let Some(input) = self.outbox.pop_front() else { return };
        self.sending = true;
        let machine = self.machine.clone();
        cx.spawn(async move |this, cx| {
            let reply = remote_machines::request(&machine, input.method, input.params).await;
            let _ = this.update(cx, |view, cx| {
                view.sending = false;
                match (reply, input.fallback) {
                    (Ok(_), _) => {},
                    (Err(error), Some((method, params))) if refused_paste(&error) => {
                        view.outbox.push_front(Input { method, params, fallback: None });
                    },
                    (Err(error), _) => {
                        view.outbox.clear();
                        view.error = Some(error);
                    },
                }
                view.read_now(cx);
                view.flush(cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn send_text(&mut self, text: &str, cx: &mut Context<Self>) {
        if let Some(text) = prompt_text(text) {
            self.follow_live(cx);
            self.push(Input::new("pane.prompt", json!({ "text": text, "submit": false })), cx);
        }
    }

    /// A clipboard block goes as one bracketed paste, line breaks included.
    fn paste(&mut self, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else { return };
        if text.trim().is_empty() {
            return;
        }
        if serde_json::to_vec(&json!(text)).map_or(usize::MAX, |bytes| bytes.len())
            > MAX_INPUT_BYTES
        {
            let language = super::config::ui_language(cx);
            self.error = Some(language.text(Message::RemoteSessionsPasteTooLarge).to_owned());
            cx.notify();
            return;
        }
        self.follow_live(cx);
        let mut input = Input::new("pane.paste", json!({ "text": text, "submit": false }));
        input.fallback = prompt_text(&text)
            .map(|line| ("pane.prompt", json!({ "text": line, "submit": false })));
        self.push(input, cx);
    }

    fn selected_text(&self) -> Option<String> {
        let ((start_row, start_byte), (end_row, end_byte)) = ordered(self.selection?);
        if (start_row, start_byte) == (end_row, end_byte) {
            return None;
        }
        let first = self.screen.first;
        let mut out = String::new();
        for absolute in start_row.max(first)..=end_row {
            let index = (absolute - first) as usize;
            let Some(text) = self.shown.get(index) else { break };
            let from = if absolute == start_row { start_byte.min(text.len()) } else { 0 };
            let to = if absolute == end_row { end_byte.min(text.len()) } else { text.len() };
            out.push_str(text.get(from..to).unwrap_or_default().trim_end());
            if absolute != end_row && !self.screen.wrapped.get(index).copied().unwrap_or(false) {
                out.push('\n');
            }
        }
        (!out.is_empty()).then_some(out)
    }

    /// The absolute row and byte under a window position, among painted rows.
    fn text_pos(&self, position: Point<Pixels>) -> Option<TextPos> {
        let painted = self.painted.borrow();
        let rows: Vec<_> = painted
            .iter()
            .enumerate()
            .filter_map(|(row, bounds)| bounds.map(|bounds| (row, bounds)))
            .collect();
        let (row, _) = rows
            .iter()
            .find(|(_, bounds)| position.y < bounds.bottom())
            .or(rows.last())
            .copied()?;
        let index = match self.layouts.get(row)?.index_for_position(position) {
            Ok(index) | Err(index) => index,
        };
        Some((self.screen.first + row as u64, index))
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus_handle, cx);
        self.selection = self.text_pos(event.position).map(|pos| (pos, pos));
        self.selecting = self.selection.is_some();
        cx.notify();
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selecting || event.pressed_button != Some(MouseButton::Left) {
            self.selecting = false;
            return;
        }
        if let (Some((anchor, _)), Some(head)) = (self.selection, self.text_pos(event.position)) {
            self.selection = Some((anchor, head));
            cx.notify();
        }
    }

    /// Copy is conditional, as in a local terminal: without a selection the
    /// key goes on and Ctrl+C reaches the remote program.
    fn on_copy(&mut self, _: &CopySelection, _: &mut Window, cx: &mut Context<Self>) {
        match self.selected_text() {
            Some(text) => {
                cx.write_to_clipboard(ClipboardItem::new_string(text));
                self.selection = None;
                cx.notify();
            },
            None => cx.propagate(),
        }
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        // Same pass-through rules as a local terminal: never intercept an IME
        // composition, native window shortcuts or the workspace's own bindings.
        let ks = &event.keystroke;
        let mods = &ks.modifiers;
        let workspace_chord = mods.platform
            || (mods.control && mods.shift)
            || (mods.control
                && mods.alt
                && matches!(ks.key.as_str(), "left" | "right" | "up" | "down"))
            || (mods.control && !mods.alt && ks.key == "tab");
        if self.marked_text.is_some()
            || crate::gpui_shell::terminal::keymap::is_native_window_shortcut(ks)
            || workspace_chord
        {
            return;
        }
        let key = runtime_key(&ks.key).or_else(|| {
            // Ctrl+letter is a control key; plain letters are text and arrive
            // through the input handler instead.
            mods.control.then(|| LETTERS.iter().copied().find(|letter| *letter == ks.key)).flatten()
        });
        if let Some(key) = key {
            self.send_key(key, mods.shift, mods.alt, mods.control, cx);
            cx.stop_propagation();
        }
    }

    fn send_key(
        &mut self,
        key: &str,
        shift: bool,
        alt: bool,
        control: bool,
        cx: &mut Context<Self>,
    ) {
        self.follow_live(cx);
        let modifiers = json!({ "shift": shift, "alt": alt, "control": control });
        self.push(Input::new("pane.send_key", json!({ "key": key, "modifiers": modifiers })), cx);
    }

    fn render_rows(&mut self, cx: &App) -> Vec<gpui::AnyElement> {
        let Some(settings) = cx.try_global::<Settings>() else { return Vec::new() };
        let palette = &settings.palette;
        let screen = &self.screen;
        let selection_range = self.selection.map(ordered);
        let selection = element::selection_fill(palette, element::themed_anchor(palette, cx));
        let mut shown = Vec::with_capacity(screen.rows.len());
        let mut layouts = Vec::with_capacity(screen.rows.len());
        let mut elements = Vec::with_capacity(screen.rows.len());
        let mut cursor_at = None;
        let painted = self.painted.clone();
        painted.replace(vec![None; screen.rows.len()]);
        for (row_ix, cells) in screen.rows.iter().enumerate() {
            let absolute = screen.first + row_ix as u64;
            let cursor = screen.cursor.filter(|(row, _)| *row == row_ix).map(|(_, col)| col);
            // Trailing blank default-background cells would only force a wrap.
            let keep = cells
                .iter()
                .rposition(|cell| !matches!(cell.text.as_str(), "" | " ") || cell.bg != DEFAULT_BG)
                .map_or(0, |last| last + 1)
                .max(cursor.map_or(0, |col| col + 1))
                .min(cells.len());
            let mut text = String::new();
            let mut highlights: Vec<(Range<usize>, HighlightStyle)> = Vec::new();
            for (col, cell) in cells[..keep].iter().enumerate() {
                let glyph = if cell.text.is_empty() { " " } else { cell.text.as_str() };
                let start = text.len();
                text.push_str(glyph);
                let mut style = cell_style(cell, palette);
                let selected = selection_range.is_some_and(|(from, to)| {
                    (absolute, start) >= from && (absolute, text.len()) <= to
                });
                if selected {
                    style.background_color = Some(selection.into());
                    if let Some(color) = palette.selection_foreground {
                        style.color = Some(color.into());
                    }
                } else if cursor == Some(col) {
                    style.background_color = Some(palette.cursor.into());
                    style.color = Some(palette.cursor_text.unwrap_or(palette.background).into());
                }
                if cursor == Some(col) {
                    cursor_at = Some((row_ix, start));
                }
                highlights.push((start..text.len(), style));
            }
            let styled = StyledText::new(text.clone()).with_highlights(highlights);
            layouts.push(styled.layout().clone());
            shown.push(text);
            let painted = painted.clone();
            elements.push(
                div()
                    .relative()
                    .w_full()
                    .flex_shrink_0()
                    .min_h(px(settings.font_size_px * LINE_SCALE))
                    .child(styled)
                    .child(
                        // Prepainted after the text, so its layout is measured.
                        canvas(
                            move |bounds, _, _| {
                                if let Some(slot) = painted.borrow_mut().get_mut(row_ix) {
                                    *slot = Some(bounds);
                                }
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .size_full(),
                    )
                    .into_any_element(),
            );
        }
        self.shown = shown;
        self.layouts = layouts;
        self.cursor_at = cursor_at;
        elements
    }
}

impl Input {
    fn new(method: &'static str, params: Value) -> Self {
        Self { method, params, fallback: None }
    }
}

/// The remote refused a paste outright (no bracketed paste, control bytes):
/// nothing reached its program, so the one-line form is safe to send.
fn refused_paste(error: &str) -> bool {
    error.contains("not ready for safe multi-line input") || error.contains("control")
}

impl EntityInputHandler for RemoteTerminalView {
    fn text_for_range(
        &mut self,
        _: Range<usize>,
        _: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        None
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection { range: 0..0, reversed: false })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked_text.as_ref().map(|text| 0..text.encode_utf16().count())
    }

    fn unmark_text(&mut self, _: &mut Window, _: &mut Context<Self>) {
        self.marked_text = None;
    }

    fn replace_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.marked_text = None;
        self.send_text(text, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        text: &str,
        _: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.marked_text = (!text.is_empty()).then(|| text.to_owned());
        cx.notify();
    }

    /// The IME candidate window follows the remote cursor as laid out here.
    fn bounds_for_range(
        &mut self,
        _: Range<usize>,
        element_bounds: Bounds<Pixels>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let line = px(cx.try_global::<Settings>().map_or(15.0, |s| s.font_size_px) * LINE_SCALE);
        let origin = self
            .cursor_at
            .filter(|(row, _)| self.painted.borrow().get(*row).is_some_and(Option::is_some))
            .and_then(|(row, byte)| self.layouts.get(row)?.position_for_index(byte))
            .unwrap_or(element_bounds.origin);
        Some(Bounds::new(origin, gpui::size(line * 0.5, line)))
    }

    fn character_index_for_point(
        &mut self,
        _: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}

impl Render for RemoteTerminalView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let language = super::config::ui_language(cx);
        let (family, size) = cx
            .try_global::<Settings>()
            .map(|settings| (settings.font_family.clone(), settings.font_size_px))
            .unwrap_or_else(|| ("monospace".to_owned(), 15.0));
        let muted = cx.theme().muted_foreground;
        let notice: Option<SharedString> = if !self.connected {
            Some(language.text(Message::RemoteSessionsNoticeDisconnected).into())
        } else {
            self.error.as_ref().map(|error| {
                language.format(Message::RemoteSessionsNoticeReadFailed, &[("error", error)]).into()
            })
        };
        let view = cx.entity();
        let focus = self.focus_handle.clone();
        v_flex()
            .id("remote-terminal")
            .track_focus(&self.focus_handle)
            .key_context(KEY_CONTEXT)
            .on_action(cx.listener(Self::on_copy))
            .on_action(cx.listener(|view, _: &PasteClipboard, _, cx| view.paste(cx)))
            .on_action(cx.listener(|view, _: &TerminalTab, _, cx| {
                view.send_key("tab", false, false, false, cx);
            }))
            .on_action(cx.listener(|view, _: &TerminalBackTab, _, cx| {
                view.send_key("tab", true, false, false, cx);
            }))
            .on_key_down(cx.listener(Self::on_key_down))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_scroll_wheel(cx.listener(Self::on_scroll_wheel))
            .size_full()
            .min_h_0()
            .overflow_hidden()
            .px_2()
            .py_1()
            .font_family(SharedString::from(family))
            .text_size(px(size))
            .line_height(px(size * LINE_SCALE))
            .when_some(notice, |root, notice| {
                root.child(div().pb_1().text_xs().text_color(muted).child(notice))
            })
            .child(
                // Bottom-aligned like a terminal: when wrapped rows overflow,
                // the oldest rows leave at the top.
                v_flex()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .justify_end()
                    .overflow_hidden()
                    .child(
                        canvas(
                            |_, _, _| {},
                            move |bounds, _, window, cx| {
                                window.handle_input(
                                    &focus,
                                    ElementInputHandler::new(bounds, view.clone()),
                                    cx,
                                );
                            },
                        )
                        .absolute()
                        .size_full(),
                    )
                    .children(self.render_rows(cx)),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> Value {
        json!({"screen_seq": 4, "screen": {
            "rows": [[["a",1,-257,-258,0]], [["b",1,-257,-258,0]]],
            "wrapped": [true, false], "cursor": [0, 1, 1],
            "history": {"first": 100, "oldest": 10, "end": 102, "live_start": 100,
                "live_rows": 2, "application_scroll": false}}})
    }

    #[test]
    fn deltas_apply_only_on_the_acknowledged_baseline() {
        let mut screen = Screen::default();
        screen.apply(&snapshot());
        assert_eq!((screen.seq, screen.cursor, screen.first), (Some(4), Some((1, 0)), 100));
        assert_eq!(screen.wrapped, [true, false]);
        screen.apply(&json!({"screen_seq": 5, "screen_delta": {
            "base": 4, "rows": [[0, [["x",1,-2,-258,1]]]], "cursor": [1, 0, 0],
            "wrapped": [false, false], "history": {"first": 91, "oldest": 11, "live_start": 101}}}));
        assert_eq!(screen.rows[0][0], Cell { text: "x".into(), fg: -2, bg: DEFAULT_BG, flags: 1 });
        assert_eq!((screen.seq, screen.cursor, screen.first), (Some(5), None, 91));
        // A delta on a baseline we never acknowledged forces a full snapshot.
        screen.apply(&json!({"screen_seq": 9, "screen_delta": {"base": 7, "rows": []}}));
        assert_eq!(screen.seq, None);
        assert_eq!(screen.rows[0][0].text, "x");
    }

    #[test]
    fn the_wheel_moves_an_absolute_anchor_and_returns_to_the_tail() {
        let mut screen = Screen::default();
        screen.apply(&snapshot());
        assert_eq!(scrolled(&screen, None, 3), Some(97));
        assert_eq!(scrolled(&screen, Some(87), 500), Some(10), "clamped to the oldest row");
        assert_eq!(scrolled(&screen, Some(95), -3), Some(98));
        assert_eq!(scrolled(&screen, Some(98), -5), None, "past the live start follows the tail");
        assert_eq!(scrolled(&screen, None, -1), None);
    }

    #[test]
    fn default_colors_are_local_and_the_background_stays_transparent() {
        let palette = Palette::default();
        assert_eq!(local_color(DEFAULT_FG, &palette), Some(palette.foreground));
        assert_eq!(local_color(DEFAULT_BG, &palette), None);
        assert_eq!(local_color(0x336699, &palette), Some(rgba(0x336699)));
        assert_eq!(local_color(-2, &palette), Some(palette.ansi[1]));
    }

    #[test]
    fn keys_split_into_named_keys_and_single_line_text() {
        assert_eq!(runtime_key("pageup"), Some("page_up"));
        assert_eq!(runtime_key("a"), None);
        assert_eq!(prompt_text("ls -la\r\nnext"), Some("ls -la  next".into()));
        assert_eq!(prompt_text(""), None);
        assert!(refused_paste("the target pane is not ready for safe multi-line input"));
        assert!(!refused_paste("machine is disconnected"));
    }
}
