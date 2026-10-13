//! One AI conversation running in a remote Pebrel: read it and talk to it.
//!
//! It shows the remote's own structured conversation (`conversation.read`), not
//! a mirrored terminal, so the remote window size never matters. Pages are
//! revision-checked: an unchanged conversation costs one tiny reply per poll.
//! Input goes through `conversation.send/choose/key` with the identity of the
//! last read; it is never retried, and reads begun before an input are dropped
//! so a stale `can_send` cannot reopen the composer.
//!
//! Until the agent reports its conversation id (for example while Claude asks
//! whether to trust the folder), there is no conversation to read. The view then
//! shows the pane's plain-text tail (`pane.read`) and offers the few keys such
//! startup questions need (`pane.send_key`), as the mobile terminal view does.

use std::time::Duration;

use gpui::prelude::FluentBuilder as _;
use gpui::{
    AppContext as _, Context, Entity, FocusHandle, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, ScrollHandle, SharedString, StatefulInteractiveElement as _,
    Styled as _, Subscription, Task, Window, div, px,
};
use gpui_component::text::{TextView, TextViewState};
use serde_json::{Value, json};

use crate::gpui_shell::prelude::*;
use crate::i18n::{Message, UiLanguage};
use crate::remote_machines::{self, RemoteAgent};
use crate::runtime_api::RuntimeTaskState;

const POLL: Duration = Duration::from_secs(2);
/// Terminal tail shown before the agent has a conversation to read.
const TERMINAL_LINES: usize = 40;
/// Keys offered in terminal mode, as `pane.send_key` names with their labels.
const TERMINAL_KEYS: [(&str, &str); 4] =
    [("up", "↑"), ("down", "↓"), ("enter", "Enter"), ("escape", "Esc")];
/// Distance from the end that still counts as "reading the latest message".
const STICK_SLACK: f32 = 48.0;

pub struct RemoteConversationView {
    pub focus_handle: FocusHandle,
    machine: String,
    machine_label: String,
    agent: RemoteAgent,
    connected: bool,
    revision: Option<String>,
    loaded: bool,
    error: Option<String>,
    text: Entity<TextViewState>,
    scroll: ScrollHandle,
    stick_bottom: bool,
    input: Entity<InputState>,
    /// From the last applied read: what input the remote accepts right now.
    live: Live,
    /// An input is in flight; the composer stays closed until it settles.
    sending: bool,
    /// Bumped around every input; a read started under another value is stale.
    input_seq: u64,
    input_error: Option<String>,
    _poll: Task<()>,
    _text_observer: Subscription,
    _input_events: Subscription,
}

#[derive(Default)]
struct Live {
    identity: Option<Value>,
    prompt: Option<RemotePrompt>,
    can_send: bool,
}

#[derive(Clone)]
struct RemotePrompt {
    id: String,
    text: String,
    options: Vec<String>,
}

impl Live {
    fn from_page(page: &Value) -> Self {
        let prompt = page["prompt"].as_object().map(|prompt| RemotePrompt {
            id: prompt.get("id").and_then(Value::as_str).unwrap_or_default().to_owned(),
            text: prompt.get("text").and_then(Value::as_str).unwrap_or_default().to_owned(),
            options: prompt
                .get("options")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|option| option.as_str().map(str::to_owned))
                .collect(),
        });
        Self {
            identity: page.get("identity").filter(|identity| identity.is_object()).cloned(),
            prompt: prompt.filter(|prompt| !prompt.id.is_empty()),
            can_send: page["can_send"] == true,
        }
    }
}

impl RemoteConversationView {
    pub fn new(
        machine: String,
        machine_label: String,
        agent: RemoteAgent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let text = cx.new(|cx| TextViewState::markdown("", cx));
        let placeholder = super::config::ui_language(cx).format(
            Message::RemoteSessionsInputPlaceholder,
            &[("agent", if agent.display_name.is_empty() { "Agent" } else { &agent.display_name })],
        );
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .multi_line(true)
                .auto_grow(1, 8)
                .submit_on_enter(true)
                .placeholder(placeholder)
        });
        let input_events = cx.subscribe_in(&input, window, |view, _, event, window, cx| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                view.submit(window, cx);
            }
        });
        // Markdown parses off-thread; follow its result so the view can keep
        // the newest message in sight once the new height is known.
        let text_observer = cx.observe(&text, |_, _, cx| cx.notify());
        let poll = cx.spawn(async move |this, cx| {
            loop {
                let Ok(read) = this.update(cx, |view, _| view.read_request()) else { return };
                let reply = read.run().await;
                if this.update(cx, |view, cx| view.apply(read.seq, reply, cx)).is_err() {
                    return;
                }
                cx.background_executor().timer(POLL).await;
            }
        });
        Self {
            focus_handle: cx.focus_handle(),
            machine,
            machine_label,
            agent,
            connected: true,
            revision: None,
            loaded: false,
            error: None,
            text,
            scroll: ScrollHandle::new(),
            stick_bottom: true,
            input,
            live: Live::default(),
            sending: false,
            input_seq: 0,
            input_error: None,
            _poll: poll,
            _text_observer: text_observer,
            _input_events: input_events,
        }
    }

    fn read_request(&self) -> Read {
        Read {
            machine: self.machine.clone(),
            agent: self.agent.clone(),
            revision: self.revision.clone(),
            seq: self.input_seq,
        }
    }

    /// Read again now instead of waiting for the next poll.
    fn refresh(&self, cx: &mut Context<Self>) {
        let read = self.read_request();
        cx.spawn(async move |this, cx| {
            let reply = read.run().await;
            let _ = this.update(cx, |view, cx| view.apply(read.seq, reply, cx));
        })
        .detach();
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.input.read(cx).value().trim().to_owned();
        if text.is_empty() || !self.live.can_send {
            return;
        }
        if self.send_input("conversation.send", json!({ "text": text }), cx) {
            self.input.update(cx, |input, cx| input.set_value("", window, cx));
        }
    }

    fn choose(&mut self, option: usize, cx: &mut Context<Self>) {
        let Some(prompt) = self.live.prompt.clone() else { return };
        self.send_input(
            "conversation.choose",
            json!({ "prompt_id": prompt.id, "option": option }),
            cx,
        );
    }

    /// Terminal mode only: one control key straight to the remote pane.
    fn send_terminal_key(&mut self, key: &'static str, cx: &mut Context<Self>) {
        if self.sending || !self.connected {
            return;
        }
        self.sending = true;
        self.input_error = None;
        self.input_seq += 1;
        let (machine, agent) = (self.machine.clone(), self.agent.clone());
        cx.spawn(async move |this, cx| {
            let params =
                json!({"window_id": agent.window_id, "pane_id": agent.pane_id, "key": key});
            let reply = remote_machines::request(&machine, "pane.send_key", params).await;
            let _ = this.update(cx, |view, cx| {
                view.sending = false;
                view.input_seq += 1;
                view.input_error = reply.err();
                view.refresh(cx);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn interrupt(&mut self, cx: &mut Context<Self>) {
        self.send_input("conversation.key", json!({ "key": "Esc" }), cx);
    }

    /// Deliver one input. The composer and options close until a read that
    /// started after the input settled says what the remote accepts again.
    fn send_input(&mut self, method: &'static str, params: Value, cx: &mut Context<Self>) -> bool {
        let Some(identity) = self.live.identity.clone() else { return false };
        if self.sending || !self.connected {
            return false;
        }
        self.sending = true;
        self.stick_bottom = true;
        self.input_error = None;
        self.input_seq += 1;
        self.live.can_send = false;
        self.live.prompt = None;
        let (machine, agent) = (self.machine.clone(), self.agent.clone());
        cx.spawn(async move |this, cx| {
            let reply =
                remote_machines::conversation_input(&machine, &agent, &identity, method, params)
                    .await;
            let _ = this.update(cx, |view, cx| {
                view.sending = false;
                view.input_seq += 1;
                view.input_error = reply.err();
                view.refresh(cx);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
        true
    }

    pub fn machine(&self) -> &str {
        &self.machine
    }

    pub fn target(&self) -> (u64, u64) {
        (self.agent.window_id, self.agent.pane_id)
    }

    /// Follow title/state/label changes from the machine's live projection.
    pub fn sync(
        &mut self,
        label: &str,
        agent: Option<&RemoteAgent>,
        connected: bool,
        cx: &mut Context<Self>,
    ) {
        let mut changed = self.machine_label != label || self.connected != connected;
        self.machine_label = label.to_owned();
        self.connected = connected;
        if let Some(agent) = agent
            && *agent != self.agent
        {
            if agent.session_id != self.agent.session_id {
                // A new conversation in the same pane must not keep the old page
                // nor accept input addressed to the old identity.
                self.revision = None;
                self.live = Live::default();
                self.input_seq += 1;
            }
            self.agent = agent.clone();
            changed = true;
        }
        if changed {
            cx.notify();
        }
    }

    fn apply(&mut self, seq: u64, reply: Result<Value, String>, cx: &mut Context<Self>) {
        if seq != self.input_seq || self.sending {
            return;
        }
        match reply {
            Ok(page) if self.agent.session_id.is_none() => {
                self.error = None;
                self.live = Live::default();
                let tail = page["text"].as_str().unwrap_or_default().trim_end();
                let markdown = format!("```text\n{}\n```", tail.replace("```", "`\u{200b}``"));
                self.text.update(cx, |text, cx| text.set_text(&markdown, cx));
                self.loaded = true;
            },
            Ok(page) => {
                self.error = None;
                self.live = Live::from_page(&page);
                if let Some(revision) = page["revision"].as_str() {
                    self.revision = Some(revision.to_owned());
                }
                if page["unchanged"] != true {
                    let messages = page["messages"].as_array().cloned().unwrap_or_default();
                    let earlier = page["before"].as_u64().is_some();
                    let labels = MarkdownLabels::new(super::config::ui_language(cx));
                    let markdown = conversation_markdown(
                        &messages,
                        &self.agent.display_name,
                        earlier,
                        &labels,
                    );
                    self.text.update(cx, |text, cx| text.set_text(&markdown, cx));
                    self.loaded = true;
                }
            },
            Err(error) => self.error = Some(error),
        }
        cx.notify();
    }

    fn on_scroll(&mut self, _: &gpui::ScrollWheelEvent, _: &mut Window, cx: &mut Context<Self>) {
        let offset = -f32::from(self.scroll.offset().y);
        let max = f32::from(self.scroll.max_offset().y);
        self.stick_bottom = offset >= max - STICK_SLACK;
        cx.notify();
    }
}

/// One `conversation.read`, remembering the input sequence it was started under.
struct Read {
    machine: String,
    agent: RemoteAgent,
    revision: Option<String>,
    seq: u64,
}

impl Read {
    async fn run(&self) -> Result<Value, String> {
        if self.agent.session_id.is_none() {
            let params = json!({
                "window_id": self.agent.window_id,
                "pane_id": self.agent.pane_id,
                "lines": TERMINAL_LINES,
            });
            return remote_machines::request(&self.machine, "pane.read", params).await;
        }
        remote_machines::conversation_read(&self.machine, &self.agent, None, self.revision.clone())
            .await
    }
}

pub(crate) fn state_message(state: RuntimeTaskState) -> Message {
    match state {
        RuntimeTaskState::Running => Message::RemoteSessionsStateRunning,
        RuntimeTaskState::WaitingInput => Message::RemoteSessionsStateWaitingInput,
        RuntimeTaskState::Attention => Message::RemoteSessionsStateAttention,
        RuntimeTaskState::Finished => Message::RemoteSessionsStateFinished,
        RuntimeTaskState::Failed => Message::RemoteSessionsStateFailed,
        RuntimeTaskState::Idle => Message::RemoteSessionsStateIdle,
    }
}

/// Localized words placed into the generated Markdown.
pub(crate) struct MarkdownLabels {
    you: &'static str,
    earlier: &'static str,
    empty: &'static str,
}

impl MarkdownLabels {
    pub(crate) fn new(language: UiLanguage) -> Self {
        Self {
            you: language.text(Message::RemoteSessionsYou),
            earlier: language.text(Message::RemoteSessionsEarlierHidden),
            empty: language.text(Message::RemoteSessionsEmpty),
        }
    }
}

/// One Markdown document for the whole page. Tool calls collapse to one line:
/// this view answers "what is it doing / what did it say", not full tool I/O.
pub(crate) fn conversation_markdown(
    messages: &[Value],
    agent_name: &str,
    earlier: bool,
    labels: &MarkdownLabels,
) -> String {
    let agent_name = if agent_name.is_empty() { "Agent" } else { agent_name };
    let mut out = String::new();
    if earlier {
        out.push_str(&format!("*{}*\n\n", labels.earlier));
    }
    let mut speaker: Option<bool> = None;
    let mut in_tools = false;
    for message in messages {
        let role = message["role"].as_str().unwrap_or_default();
        let user = role == "user";
        if speaker != Some(user) {
            if speaker.is_some() {
                out.push_str(if user { "\n---\n\n" } else { "\n" });
            }
            let speaker_name = if user { labels.you } else { agent_name };
            out.push_str(&format!("**{speaker_name}**\n\n"));
            speaker = Some(user);
            in_tools = false;
        }
        if role == "tool" {
            let name = message["name"].as_str().unwrap_or("tool");
            let detail = message["detail"]
                .as_str()
                .and_then(|detail| detail.lines().map(str::trim).find(|line| !line.is_empty()))
                .map(|line| truncate(line, 120));
            match detail {
                Some(detail) => out.push_str(&format!("- `{name}` · {}\n", inline_escape(&detail))),
                None => out.push_str(&format!("- `{name}`\n")),
            }
            in_tools = true;
            continue;
        }
        if in_tools {
            out.push('\n');
            in_tools = false;
        }
        let text = message["text"].as_str().unwrap_or_default().trim();
        if !text.is_empty() {
            out.push_str(text);
            out.push_str("\n\n");
        }
    }
    if out.is_empty() {
        out.push_str(&format!("*{}*", labels.empty));
    }
    out
}

fn truncate(text: &str, limit: usize) -> String {
    let mut chars = text.chars();
    let head: String = chars.by_ref().take(limit).collect();
    if chars.next().is_some() { format!("{head}…") } else { head }
}

/// Tool details are arbitrary command output; keep them from opening Markdown
/// constructs that would swallow the following lines.
fn inline_escape(text: &str) -> String {
    text.chars()
        .flat_map(|ch| match ch {
            '`' | '*' | '_' | '[' | ']' | '<' | '>' | '#' | '|' => vec!['\\', ch],
            other => vec![other],
        })
        .collect()
}

impl Render for RemoteConversationView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let language = super::config::ui_language(cx);
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        if self.stick_bottom {
            self.scroll.scroll_to_bottom();
        }
        let title = if self.agent.title.is_empty() {
            self.agent.display_name.clone()
        } else {
            self.agent.title.clone()
        };
        let header: SharedString = format!(
            "{} · {} · {}",
            self.machine_label,
            title,
            language.text(if self.connected {
                state_message(self.agent.task_state)
            } else {
                Message::RemoteSessionsDisconnected
            })
        )
        .into();
        let notice: Option<SharedString> = if !self.connected {
            Some(language.text(Message::RemoteSessionsNoticeDisconnected).into())
        } else if self.agent.session_id.is_none() {
            Some(language.text(Message::RemoteSessionsNoticeTerminal).into())
        } else if let Some(error) = &self.error {
            Some(
                language
                    .format(Message::RemoteSessionsNoticeReadFailed, &[("error", error)])
                    .into(),
            )
        } else if !self.loaded {
            Some(language.text(Message::RemoteSessionsNoticeLoading).into())
        } else {
            None
        };
        v_flex()
            .id("remote-conversation")
            .track_focus(&self.focus_handle)
            .size_full()
            .min_h_0()
            .child(
                div()
                    .px_4()
                    .py_2()
                    .flex_shrink_0()
                    .text_sm()
                    .text_color(muted)
                    .border_b_1()
                    .border_color(theme.border)
                    .child(header),
            )
            .when_some(notice, |root, notice| {
                root.child(div().px_4().py_2().text_sm().text_color(muted).child(notice))
            })
            .child(
                div()
                    .id("remote-conversation-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .on_scroll_wheel(cx.listener(Self::on_scroll))
                    .child(
                        div()
                            .px_4()
                            .py_3()
                            .max_w(px(980.0))
                            .child(TextView::new(&self.text).selectable(true)),
                    ),
            )
            .child(self.render_input(language, cx))
    }
}

impl RemoteConversationView {
    fn render_terminal_keys(
        &self,
        language: UiLanguage,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let theme = cx.theme();
        let (border, danger) = (theme.border, theme.danger);
        let usable = self.connected && !self.sending;
        let error: Option<SharedString> = self.input_error.as_ref().map(|error| {
            language.format(Message::RemoteSessionsInputFailed, &[("error", error)]).into()
        });
        v_flex()
            .flex_shrink_0()
            .px_4()
            .py_2()
            .gap_2()
            .border_t_1()
            .border_color(border)
            .when_some(error, |area, error| {
                area.child(div().text_xs().text_color(danger).child(error))
            })
            .child(h_flex().gap_2().children(TERMINAL_KEYS.into_iter().enumerate().map(
                |(index, (key, label))| {
                    Button::new(("remote-terminal-key", index))
                        .label(label)
                        .small()
                        .disabled(!usable)
                        .on_click(
                            cx.listener(move |view, _, _, cx| view.send_terminal_key(key, cx)),
                        )
                },
            )))
            .into_any_element()
    }

    /// The remote's open question (one button per option), the composer, and an
    /// interrupt while the agent works. Only what the last read allows is live.
    fn render_input(&self, language: UiLanguage, cx: &mut Context<Self>) -> gpui::AnyElement {
        if self.agent.session_id.is_none() {
            return self.render_terminal_keys(language, cx);
        }
        let theme = cx.theme();
        let (muted, border, danger) = (theme.muted_foreground, theme.border, theme.danger);
        let usable = self.connected && self.live.identity.is_some() && !self.sending;
        let working = matches!(
            self.agent.task_state,
            RuntimeTaskState::Running
                | RuntimeTaskState::WaitingInput
                | RuntimeTaskState::Attention
        );
        let hint: Option<SharedString> = if let Some(error) = &self.input_error {
            Some(language.format(Message::RemoteSessionsInputFailed, &[("error", error)]).into())
        } else if !usable || self.live.can_send || self.live.prompt.is_some() {
            None
        } else if working && self.agent.task_state != RuntimeTaskState::Running {
            Some(language.text(Message::RemoteSessionsInputTerminalOnly).into())
        } else {
            Some(language.text(Message::RemoteSessionsInputBusy).into())
        };
        let prompt = self.live.prompt.clone().filter(|_| usable);
        v_flex()
            .flex_shrink_0()
            .px_4()
            .py_2()
            .gap_2()
            .border_t_1()
            .border_color(border)
            .when_some(prompt, |area, prompt| {
                area.child(div().text_sm().child(SharedString::from(prompt.text))).child(
                    h_flex().flex_wrap().gap_2().children(
                        prompt.options.into_iter().enumerate().map(|(index, option)| {
                            Button::new(("remote-prompt-option", index))
                                .label(SharedString::from(option))
                                .small()
                                .when(index == 0, |button| button.primary())
                                .on_click(cx.listener(move |view, _, _, cx| view.choose(index, cx)))
                        }),
                    ),
                )
            })
            .when_some(hint, |area, hint| {
                area.child(
                    div()
                        .text_xs()
                        .text_color(if self.input_error.is_some() { danger } else { muted })
                        .child(hint),
                )
            })
            .child(
                h_flex()
                    .gap_2()
                    .items_end()
                    .child(
                        div().flex_1().min_w_0().child(
                            Input::new(&self.input).disabled(!(usable && self.live.can_send)),
                        ),
                    )
                    .when(usable && working, |row| {
                        row.child(
                            Button::new("remote-interrupt")
                                .label(language.text(Message::RemoteSessionsInterrupt))
                                .small()
                                .on_click(cx.listener(|view, _, _, cx| view.interrupt(cx))),
                        )
                    })
                    .child(
                        Button::new("remote-send")
                            .label(language.text(Message::RemoteSessionsSend))
                            .small()
                            .primary()
                            .disabled(!(usable && self.live.can_send))
                            .on_click(cx.listener(|view, _, window, cx| view.submit(window, cx))),
                    ),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn labels() -> MarkdownLabels {
        MarkdownLabels::new(UiLanguage::ZhCn)
    }

    #[test]
    fn turns_are_separated_and_tools_collapse_to_one_line() {
        let messages = vec![
            json!({"role":"user","text":"跑一下测试"}),
            json!({"role":"tool","name":"Bash","text":"{}","detail":"\n  ok: 12 passed\nmore"}),
            json!({"role":"assistant","text":"全部通过。"}),
            json!({"role":"user","text":"好"}),
        ];
        let markdown = conversation_markdown(&messages, "Claude Code", true, &labels());
        assert_eq!(
            markdown,
            "*更早的消息未显示*\n\n**你**\n\n跑一下测试\n\n\n**Claude Code**\n\n- `Bash` · ok: 12 passed\n\n全部通过。\n\n\n---\n\n**你**\n\n好\n\n"
        );
    }

    #[test]
    fn tool_details_cannot_open_markdown_blocks() {
        let messages = vec![json!({"role":"tool","name":"Bash","detail":"```rust *x* <b>"})];
        let markdown = conversation_markdown(&messages, "", false, &labels());
        assert!(markdown.contains("\\`\\`\\`rust \\*x\\* \\<b\\>"), "{markdown}");
        assert!(markdown.starts_with("**Agent**"));
    }

    #[test]
    fn a_read_page_decides_what_input_is_live() {
        let page = json!({
            "identity": {"kind":"claude","session_id":"s","epoch":3},
            "can_send": false,
            "prompt": {"id":"p1","text":"Allow?","options":["Yes","No"],"selected":0,"binary":false},
        });
        let live = Live::from_page(&page);
        assert_eq!(live.identity, Some(json!({"kind":"claude","session_id":"s","epoch":3})));
        assert!(!live.can_send);
        let prompt = live.prompt.expect("prompt");
        assert_eq!((prompt.id.as_str(), prompt.options.len()), ("p1", 2));

        let idle =
            Live::from_page(&json!({"identity":{"kind":"codex"},"can_send":true,"prompt":null}));
        assert!(idle.can_send && idle.prompt.is_none());
        assert!(Live::from_page(&json!({"unchanged": true})).identity.is_none());
    }
}
