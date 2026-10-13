//! One AI session running in a remote Pebrel, shown as its terminal text.
//!
//! Only the text grid crosses the link: `pane.read` with `screen` and row
//! deltas. Font, colors and background are this machine's; a default-background
//! cell stays transparent like a local terminal cell. Typing goes back in order
//! as `pane.prompt` (text, never submitted) and `pane.send_key` (named keys),
//! the same split the remote Runtime applies to every client.

use std::collections::{HashMap, VecDeque};
use std::ops::Range;
use std::time::Duration;

use gpui::prelude::FluentBuilder as _;
use gpui::{
    App, Bounds, Context, ElementInputHandler, EntityInputHandler, FocusHandle, FontStyle,
    FontWeight, HighlightStyle, InteractiveElement as _, IntoElement, KeyDownEvent,
    ParentElement as _, Pixels, Point, Render, Rgba, SharedString, StrikethroughStyle, Styled as _,
    StyledText, Task, UTF16Selection, UnderlineStyle, Window, canvas, div, px,
};
use serde_json::{Value, json};

use crate::gpui_shell::config::Settings;
use crate::gpui_shell::prelude::*;
use crate::gpui_shell::terminal::colors::Palette;
use crate::i18n::Message;
use crate::remote_machines::{self, RemoteAgent};

const POLL: Duration = Duration::from_millis(250);
/// Wire color codes for the default foreground and background (`-(index + 1)`).
const DEFAULT_FG: i32 = -257;
const DEFAULT_BG: i32 = -258;

pub struct RemoteTerminalView {
    pub focus_handle: FocusHandle,
    machine: String,
    agent: RemoteAgent,
    connected: bool,
    screen: Screen,
    error: Option<String>,
    /// Inputs not yet delivered, sent one at a time so their order holds.
    outbox: VecDeque<(&'static str, Value)>,
    sending: bool,
    marked_text: Option<String>,
    _poll: Task<()>,
}

#[derive(Default)]
struct Screen {
    rows: Vec<Vec<Cell>>,
    palette: HashMap<usize, u32>,
    cursor: Option<(usize, usize)>,
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
        self.palette = screen["palette"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|entry| Some((entry[0].as_u64()? as usize, entry[1].as_u64()? as u32)))
            .collect();
        let cursor = &screen["cursor"];
        self.cursor = (cursor[2] != 0)
            .then(|| Some((cursor[0].as_u64()? as usize, cursor[1].as_u64()? as usize)))
            .flatten();
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

/// One plain-text line for `pane.prompt`: it refuses control characters, so a
/// pasted block arrives as one line.
fn prompt_text(text: &str) -> Option<String> {
    let line: String = text
        .chars()
        .map(|ch| if ch == '\n' || ch == '\r' || ch == '\t' { ' ' } else { ch })
        .filter(|ch| !ch.is_control())
        .collect();
    (!line.is_empty()).then_some(line)
}

impl RemoteTerminalView {
    pub fn new(machine: String, agent: RemoteAgent, cx: &mut Context<Self>) -> Self {
        let poll = cx.spawn(async move |this, cx| {
            loop {
                let Ok((machine, params)) = this.update(cx, |view, _| view.read_request()) else {
                    return;
                };
                let reply = remote_machines::request(&machine, "pane.read", params).await;
                if this.update(cx, |view, cx| view.apply(reply, cx)).is_err() {
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
            error: None,
            outbox: VecDeque::new(),
            sending: false,
            marked_text: None,
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

    fn read_request(&self) -> (String, Value) {
        let params = json!({
            "window_id": self.agent.window_id,
            "pane_id": self.agent.pane_id,
            "screen": true,
            "screen_viewport": true,
            "screen_since": self.screen.seq.unwrap_or(0),
        });
        (self.machine.clone(), params)
    }

    fn apply(&mut self, reply: Result<Value, String>, cx: &mut Context<Self>) {
        match reply {
            Ok(result) => {
                self.error = None;
                self.screen.apply(&result);
            },
            Err(error) => self.error = Some(error),
        }
        cx.notify();
    }

    fn enqueue(&mut self, method: &'static str, mut params: Value, cx: &mut Context<Self>) {
        if !self.connected {
            return;
        }
        params["window_id"] = self.agent.window_id.into();
        params["pane_id"] = self.agent.pane_id.into();
        self.outbox.push_back((method, params));
        self.flush(cx);
    }

    /// Deliver the next input. A failed one is reported, not retried: its
    /// outcome on the remote is unknown, and replaying keys could repeat them.
    fn flush(&mut self, cx: &mut Context<Self>) {
        if self.sending {
            return;
        }
        let Some((method, params)) = self.outbox.pop_front() else { return };
        self.sending = true;
        let machine = self.machine.clone();
        cx.spawn(async move |this, cx| {
            let reply = remote_machines::request(&machine, method, params).await;
            let _ = this.update(cx, |view, cx| {
                view.sending = false;
                if let Err(error) = reply {
                    view.outbox.clear();
                    view.error = Some(error);
                }
                let (machine, params) = view.read_request();
                cx.spawn(async move |this, cx| {
                    let reply = remote_machines::request(&machine, "pane.read", params).await;
                    let _ = this.update(cx, |view, cx| view.apply(reply, cx));
                })
                .detach();
                view.flush(cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn send_text(&mut self, text: &str, cx: &mut Context<Self>) {
        if let Some(text) = prompt_text(text) {
            self.enqueue("pane.prompt", json!({ "text": text, "submit": false }), cx);
        }
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        // Same pass-through rules as a local terminal: never intercept an IME
        // composition, native window shortcuts or the workspace's own bindings.
        let ks = &event.keystroke;
        let mods = &ks.modifiers;
        if self.marked_text.is_some()
            || crate::gpui_shell::terminal::keymap::is_native_window_shortcut(ks)
            || mods.platform
        {
            return;
        }
        if mods.control && mods.shift {
            if ks.key == "v" {
                if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                    self.send_text(&text, cx);
                }
                cx.stop_propagation();
            }
            return;
        }
        if mods.control && mods.alt && matches!(ks.key.as_str(), "left" | "right" | "up" | "down") {
            return;
        }
        if mods.control && !mods.alt && ks.key == "tab" {
            return;
        }
        let modifiers = json!({ "shift": mods.shift, "alt": mods.alt, "control": mods.control });
        let key = runtime_key(&ks.key).or_else(|| {
            // Ctrl+letter is a control key; plain letters are text and arrive
            // through the input handler instead.
            (mods.control && ks.key.len() == 1 && ks.key.as_bytes()[0].is_ascii_lowercase())
                .then(|| ks.key.as_str())
                .and_then(|key| LETTERS.iter().copied().find(|letter| *letter == key))
        });
        if let Some(key) = key {
            self.enqueue("pane.send_key", json!({ "key": key, "modifiers": modifiers }), cx);
            cx.stop_propagation();
        }
    }

    fn render_rows(&self, cx: &App) -> Vec<gpui::AnyElement> {
        let Some(settings) = cx.try_global::<Settings>() else { return Vec::new() };
        let palette = &settings.palette;
        let focused_cursor = self.screen.cursor;
        self.screen
            .rows
            .iter()
            .enumerate()
            .map(|(row_ix, cells)| {
                let mut text = String::new();
                let mut highlights: Vec<(Range<usize>, HighlightStyle)> = Vec::new();
                for (col, cell) in cells.iter().enumerate() {
                    let glyph = if cell.text.is_empty() { " " } else { cell.text.as_str() };
                    let start = text.len();
                    text.push_str(glyph);
                    let cursor = focused_cursor == Some((row_ix, col));
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
                        style.underline =
                            Some(UnderlineStyle { thickness: px(1.0), ..Default::default() });
                    }
                    if cell.flags & 8 != 0 {
                        style.strikethrough =
                            Some(StrikethroughStyle { thickness: px(1.0), ..Default::default() });
                    }
                    if cell.flags & 16 != 0 {
                        style.fade_out = Some(0.4);
                    }
                    if cursor {
                        style.background_color = Some(palette.cursor.into());
                        style.color =
                            Some(palette.cursor_text.unwrap_or(palette.background).into());
                    }
                    highlights.push((start..text.len(), style));
                }
                div()
                    .h(px(settings.font_size_px * 1.3))
                    .whitespace_nowrap()
                    .child(StyledText::new(text).with_highlights(highlights))
                    .into_any_element()
            })
            .collect()
    }
}

const LETTERS: [&str; 26] = [
    "a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l", "m", "n", "o", "p", "q", "r", "s",
    "t", "u", "v", "w", "x", "y", "z",
];

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

    /// The IME candidate window follows the remote cursor.
    fn bounds_for_range(
        &mut self,
        _: Range<usize>,
        element_bounds: Bounds<Pixels>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let size = cx.try_global::<Settings>().map_or(15.0, |settings| settings.font_size_px);
        let (row, col) = self.screen.cursor.unwrap_or((0, 0));
        let origin = element_bounds.origin
            + Point::new(px(col as f32 * size * 0.6), px(row as f32 * size * 1.3));
        Some(Bounds::new(origin, gpui::size(px(size * 0.6), px(size * 1.3))))
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
            .key_context("RemoteTerminal")
            .on_key_down(cx.listener(Self::on_key_down))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|view, _, window, cx| {
                    window.focus(&view.focus_handle, cx);
                }),
            )
            .size_full()
            .min_h_0()
            .overflow_hidden()
            .px_2()
            .py_1()
            .font_family(SharedString::from(family))
            .text_size(px(size))
            .line_height(px(size * 1.3))
            .when_some(notice, |root, notice| {
                root.child(div().pb_1().text_xs().text_color(muted).child(notice))
            })
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
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

    #[test]
    fn deltas_apply_only_on_the_acknowledged_baseline() {
        let mut screen = Screen::default();
        screen.apply(&json!({"screen_seq": 4, "screen": {
            "rows": [[["a",1,-257,-258,0]], [["b",1,-257,-258,0]]],
            "cursor": [1, 0, 1], "palette": [[1, 0xff0000]]}}));
        assert_eq!((screen.seq, screen.cursor), (Some(4), Some((1, 0))));
        screen.apply(&json!({"screen_seq": 5, "screen_delta": {
            "base": 4, "rows": [[0, [["x",1,-2,-258,1]]]], "cursor": [0, 1, 0], "palette": []}}));
        assert_eq!(screen.rows[0][0], Cell { text: "x".into(), fg: -2, bg: DEFAULT_BG, flags: 1 });
        assert_eq!((screen.seq, screen.cursor), (Some(5), None));
        // A delta on a baseline we never acknowledged forces a full snapshot.
        screen.apply(&json!({"screen_seq": 9, "screen_delta": {"base": 7, "rows": []}}));
        assert_eq!(screen.seq, None);
        assert_eq!(screen.rows[0][0].text, "x");
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
        assert_eq!(prompt_text("\n"), Some(" ".into()));
        assert_eq!(prompt_text(""), None);
    }
}
