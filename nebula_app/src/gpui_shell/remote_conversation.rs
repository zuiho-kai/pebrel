//! Read-only view of one AI conversation running in a remote Pebrel.
//!
//! It shows the remote's own structured conversation (`conversation.read`), not
//! a mirrored terminal, so the remote window size never matters. Pages are
//! revision-checked: an unchanged conversation costs one tiny reply per poll.

use std::time::Duration;

use gpui::prelude::FluentBuilder as _;
use gpui::{
    AppContext as _, Context, Entity, FocusHandle, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, ScrollHandle, SharedString, StatefulInteractiveElement as _,
    Styled as _, Task, Window, div, px,
};
use gpui_component::text::{TextView, TextViewState};
use serde_json::Value;

use crate::gpui_shell::prelude::*;
use crate::i18n::{Message, UiLanguage};
use crate::remote_machines::{self, RemoteAgent};
use crate::runtime_api::RuntimeTaskState;

const POLL: Duration = Duration::from_secs(2);
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
    _poll: Task<()>,
    _text_observer: gpui::Subscription,
}

impl RemoteConversationView {
    pub fn new(
        machine: String,
        machine_label: String,
        agent: RemoteAgent,
        cx: &mut Context<Self>,
    ) -> Self {
        let text = cx.new(|cx| TextViewState::markdown("", cx));
        // Markdown parses off-thread; follow its result so the view can keep
        // the newest message in sight once the new height is known.
        let text_observer = cx.observe(&text, |_, _, cx| cx.notify());
        let poll = cx.spawn(async move |this, cx| {
            loop {
                let Ok(request) = this.update(cx, |view, _| {
                    (view.machine.clone(), view.agent.clone(), view.revision.clone())
                }) else {
                    return;
                };
                let (machine, agent, revision) = request;
                let reply =
                    remote_machines::conversation_read(&machine, &agent, None, revision).await;
                if this.update(cx, |view, cx| view.apply(reply, cx)).is_err() {
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
            _poll: poll,
            _text_observer: text_observer,
        }
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
                // A new conversation in the same pane must not keep the old page.
                self.revision = None;
            }
            self.agent = agent.clone();
            changed = true;
        }
        if changed {
            cx.notify();
        }
    }

    fn apply(&mut self, reply: Result<Value, String>, cx: &mut Context<Self>) {
        match reply {
            Ok(page) => {
                self.error = None;
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
            Some(language.text(Message::RemoteSessionsNoticeNoSession).into())
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
}
