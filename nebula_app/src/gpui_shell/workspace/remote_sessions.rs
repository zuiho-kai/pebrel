//! Sidebar groups for remote machines reached over SSH, one group per machine.
//!
//! A group appears by itself the first time an SSH pane finds a Pebrel on the
//! other side (`remote_machines`). Clicking a session shows its conversation in
//! the content area without opening a local tab; any local tab change leaves it.
//! The group header's `+` starts a new Claude/Codex session on that machine and
//! opens it as soon as the remote reports it.
use std::collections::HashSet;

use super::*;
use crate::gpui_shell::remote_terminal::RemoteTerminalView;
use crate::remote_machines::{self, MachineView, RemoteAgent};
use crate::runtime_api::RuntimeTaskState;
use gpui_component::menu::PopupMenuItem;

#[derive(Default)]
pub(super) struct RemoteSessions {
    machines: Vec<MachineView>,
    view: Option<Entity<RemoteTerminalView>>,
    /// `(active, tabs.len())` when the view opened; any other local navigation
    /// path that moves `active` returns the content area to the local tab.
    shown_over: (usize, usize),
    collapsed: HashSet<String>,
    rename: Option<MachineRename>,
    watching: bool,
    /// A session this window started remotely: open it once the machine lists it.
    pending_open: Option<PendingOpen>,
}

struct PendingOpen {
    key: String,
    target: (u64, u64),
    since: std::time::Instant,
}

/// The remote lists a new agent pane within a snapshot or two; give up after this.
const PENDING_OPEN_LIMIT: std::time::Duration = std::time::Duration::from_secs(60);

struct MachineRename {
    key: String,
    input: Entity<InputState>,
    _subscription: Subscription,
}

impl NebulaWorkspace {
    fn start_remote_machine_watch(window: &mut Window, cx: &mut Context<Self>) {
        let mut changes = remote_machines::changes();
        cx.spawn_in(window, async move |this, cx| {
            loop {
                if this
                    .update_in(cx, |this, window, cx| this.refresh_remote_machines(window, cx))
                    .is_err()
                {
                    return;
                }
                if changes.changed().await.is_err() {
                    return;
                }
            }
        })
        .detach();
    }

    fn refresh_remote_machines(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.remote.machines = remote_machines::machines();
        self.open_pending_remote_session(window, cx);
        if let Some(view) = self.remote.view.clone() {
            let (key, target) = {
                let view = view.read(cx);
                (view.machine().to_owned(), view.target())
            };
            if let Some(machine) = self.remote.machines.iter().find(|machine| machine.key == key) {
                let agent = machine
                    .agents
                    .iter()
                    .find(|agent| (agent.window_id, agent.pane_id) == target)
                    .cloned();
                let connected = machine.connected;
                view.update(cx, |view, cx| view.sync(agent.as_ref(), connected, cx));
            }
        }
        cx.notify();
    }

    /// The remote conversation shown in place of the active local tab, if any.
    pub(super) fn remote_content(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        // Each window follows the registry from its first frame on.
        if !self.remote.watching {
            self.remote.watching = true;
            Self::start_remote_machine_watch(window, cx);
        }
        if self.remote.view.is_some() && self.remote.shown_over != (self.active, self.tabs.len()) {
            self.remote.view = None;
        }
        self.remote.view.clone().map(gpui::IntoElement::into_any_element)
    }

    pub(super) fn remote_view_shown(&self) -> bool {
        self.remote.view.is_some()
    }

    fn remote_focus(&self, cx: &App) -> Option<gpui::FocusHandle> {
        self.remote.view.as_ref().map(|view| view.read(cx).focus_handle.clone())
    }

    /// While a remote conversation covers the content area, keyboard focus must
    /// not return to the hidden terminal underneath it.
    pub(super) fn focus_remote(&self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(focus) = self.remote_focus(cx).filter(|_| !self.settings_open) else {
            return false;
        };
        window.defer(cx, move |window, cx| window.focus(&focus, cx));
        true
    }

    pub(super) fn close_remote_view(&mut self, cx: &mut Context<Self>) {
        if self.remote.view.take().is_some() {
            cx.notify();
        }
    }

    fn open_remote_session(
        &mut self,
        key: &str,
        agent: &RemoteAgent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.leave_settings(window, cx);
        let same = self.remote.view.as_ref().is_some_and(|view| {
            let view = view.read(cx);
            view.machine() == key && view.target() == (agent.window_id, agent.pane_id)
        });
        if !same {
            let (key, agent) = (key.to_owned(), agent.clone());
            self.remote.view = Some(cx.new(|cx| RemoteTerminalView::new(key, agent, cx)));
        }
        self.remote.shown_over = (self.active, self.tabs.len());
        if let Some(focus) = self.remote_focus(cx) {
            window.defer(cx, move |window, cx| window.focus(&focus, cx));
        }
        cx.notify();
    }

    fn start_remote_agent(
        &mut self,
        key: String,
        kind: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.spawn_in(window, async move |this, cx| {
            let started = remote_machines::start_agent(&key, kind).await;
            let _ = this.update_in(cx, |this, window, cx| match started {
                Ok(target) => {
                    let since = std::time::Instant::now();
                    this.remote.pending_open = Some(PendingOpen { key, target, since });
                    this.open_pending_remote_session(window, cx);
                },
                Err(error) => {
                    let message = crate::gpui_shell::config::ui_language(cx).format(
                        crate::i18n::Message::RemoteSessionsStartFailed,
                        &[("error", &error)],
                    );
                    let notification = gpui_component::notification::Notification::error(message);
                    crate::gpui_shell::toast::push_notification(window, cx, notification, None);
                },
            });
        })
        .detach();
    }

    /// Open the session started by `start_remote_agent` once the machine lists
    /// its pane. A lost link or a slow listing drops the wait.
    fn open_pending_remote_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pending) = self.remote.pending_open.as_ref() else { return };
        let machine = self.remote.machines.iter().find(|machine| machine.key == pending.key);
        if machine.is_none_or(|machine| !machine.connected)
            || pending.since.elapsed() > PENDING_OPEN_LIMIT
        {
            self.remote.pending_open = None;
            return;
        }
        let agent = machine.and_then(|machine| {
            machine.agents.iter().find(|agent| (agent.window_id, agent.pane_id) == pending.target)
        });
        if let Some(agent) = agent.cloned() {
            let key = pending.key.clone();
            self.remote.pending_open = None;
            self.open_remote_session(&key, &agent, window, cx);
        }
    }

    fn toggle_remote_machine(&mut self, key: &str, cx: &mut Context<Self>) {
        // Clicks inside the inline rename input bubble here; they edit, not fold.
        if self.remote.rename.as_ref().is_some_and(|edit| edit.key == key) {
            return;
        }
        if !self.remote.collapsed.remove(key) {
            self.remote.collapsed.insert(key.to_owned());
        }
        cx.notify();
    }

    fn begin_machine_rename(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) {
        let current = self
            .remote
            .machines
            .iter()
            .find(|machine| machine.key == key)
            .map_or_else(|| key.to_owned(), |machine| machine.label.clone());
        // Start empty with the current name as placeholder: the right-click that
        // opens the editor also releases over it, which would clear a selection.
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(current));
        let subscription = cx.subscribe_in(
            &input,
            window,
            |this: &mut Self, input: &Entity<InputState>, event: &InputEvent, window, cx| {
                if this.remote.rename.as_ref().is_none_or(|edit| edit.input != *input) {
                    return;
                }
                match event {
                    InputEvent::PressEnter { .. } => this.finish_machine_rename(true, window, cx),
                    InputEvent::Blur => this.finish_machine_rename(false, window, cx),
                    _ => {},
                }
            },
        );
        input.update(cx, |state, cx| state.focus(window, cx));
        self.remote.rename =
            Some(MachineRename { key: key.to_owned(), input, _subscription: subscription });
        cx.notify();
    }

    fn finish_machine_rename(&mut self, commit: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(rename) = self.remote.rename.take() else { return };
        let value = rename.input.read(cx).value();
        // Enter on an empty field keeps the current name; the registry treats
        // the SSH destination itself as "no custom name".
        if commit && !value.trim().is_empty() {
            remote_machines::rename(&rename.key, &value);
        }
        self.focus_active(window, cx);
        cx.notify();
    }

    /// One group per machine below the local TABS list. Returns whether any
    /// row shows a spinner, so the caller keeps the shared spinner animating.
    pub(super) fn render_remote_sections(
        &self,
        row_w: f32,
        row_height: f32,
        label_px: f32,
        cx: &mut Context<Self>,
    ) -> (Vec<gpui::AnyElement>, bool) {
        let language = crate::gpui_shell::config::ui_language(cx);
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let faint = crate::gpui_shell::theme::faint_ink(cx);
        let (active_bg, active_fg, hover_bg) =
            (theme.sidebar_accent, theme.sidebar_accent_foreground, theme.list_hover);
        let (primary, warning, danger, chip_bg) =
            (theme.primary, theme.warning, theme.danger, theme.muted);
        let chrome_family = theme.mono_font_family.clone();
        let symbol_family: SharedString = crate::font_install::REQUIRED_FONT_FAMILY.into();
        let shown = self.remote.view.as_ref().map(|view| {
            let view = view.read(cx);
            (view.machine().to_owned(), view.target())
        });
        let mut running = false;
        let mut sections = Vec::new();
        for (machine_ix, machine) in self.remote.machines.iter().enumerate() {
            let collapsed = self.remote.collapsed.contains(&machine.key);
            let renaming = self.remote.rename.as_ref().filter(|edit| edit.key == machine.key);
            let toggle_key = machine.key.clone();
            let rename_key = machine.key.clone();
            let title: gpui::AnyElement = match renaming {
                Some(edit) => div()
                    .flex_1()
                    .min_w_0()
                    .child(Input::new(&edit.input).xsmall())
                    .into_any_element(),
                // The hint sits on the name only, so it never covers the `+` menu.
                None => div()
                    .id(("remote-machine-title", machine_ix))
                    .min_w_0()
                    .truncate()
                    .tooltip(|window, cx| {
                        let hint = crate::gpui_shell::config::ui_language(cx)
                            .text(crate::i18n::Message::RemoteSessionsRenameHint);
                        gpui_component::tooltip::Tooltip::new(hint).build(window, cx)
                    })
                    .text_size(px(label_px * SIDEBAR_TITLE_SCALE))
                    .text_color(muted)
                    .child(SharedString::from(machine.label.clone()))
                    .into_any_element(),
            };
            let header = h_flex()
                .id(("remote-machine", machine_ix))
                .w_full()
                .h(px(row_height))
                .mt_2()
                .pl_2()
                .items_center()
                .cursor_pointer()
                .font_family(chrome_family.clone())
                .when(!machine.connected, |row| row.opacity(0.5))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.toggle_remote_machine(&toggle_key, cx);
                }))
                // A click folds the group, which moves this header (groups sit
                // below the flexible TABS list), so a double-click cannot rename.
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.begin_machine_rename(&rename_key, window, cx);
                    }),
                )
                .child(
                    h_flex().w(px(24.0)).h_full().flex_shrink_0().items_center().child(
                        Icon::new(if collapsed {
                            IconName::ChevronRight
                        } else {
                            IconName::ChevronDown
                        })
                        .xsmall()
                        .text_color(muted),
                    ),
                )
                .child(title)
                .child(
                    h_flex()
                        .ml_2()
                        .flex_shrink_0()
                        .h(px((label_px * 1.22 * SIDEBAR_TITLE_SCALE * 1.18).max(11.0)))
                        .min_w(px(label_px * 0.62 + 8.0))
                        .px_2()
                        .justify_center()
                        .items_center()
                        .rounded_full()
                        .bg(chip_bg)
                        .text_size(px(label_px * SIDEBAR_TITLE_SCALE))
                        .text_color(faint)
                        .child(SharedString::from(machine.agents.len().to_string())),
                )
                .child(div().flex_1())
                .when(machine.connected, |row| {
                    let owner = cx.entity().downgrade();
                    let start_key = machine.key.clone();
                    row.child(
                        Button::new(("remote-machine-new", machine_ix))
                            .icon(Icon::new(IconName::Plus).xsmall())
                            .ghost()
                            .xsmall()
                            .mr_1()
                            .tooltip(language.text(crate::i18n::Message::RemoteSessionsNewSession))
                            .dropdown_menu(move |menu, _, _| {
                                ["claude", "codex"].into_iter().fold(menu, |menu, kind| {
                                    let (owner, key) = (owner.clone(), start_key.clone());
                                    let label = match kind {
                                        "claude" => crate::i18n::Message::RemoteSessionsNewClaude,
                                        _ => crate::i18n::Message::RemoteSessionsNewCodex,
                                    };
                                    menu.item(PopupMenuItem::new(language.text(label)).on_click(
                                        move |_, window, cx| {
                                            let key = key.clone();
                                            let _ = owner.update(cx, |this, cx| {
                                                this.start_remote_agent(key, kind, window, cx)
                                            });
                                        },
                                    ))
                                })
                            }),
                    )
                })
                .when(!machine.connected, |row| {
                    row.child(
                        div()
                            .ml_2()
                            .flex_shrink_0()
                            .text_size(px(label_px * SIDEBAR_TAG_SCALE))
                            .text_color(muted)
                            .child(language.text(crate::i18n::Message::RemoteSessionsDisconnected)),
                    )
                });
            sections.push(header.into_any_element());
            if collapsed {
                continue;
            }
            for (agent_ix, agent) in machine.agents.iter().enumerate() {
                let active = shown.as_ref().is_some_and(|(key, target)| {
                    *key == machine.key && *target == (agent.window_id, agent.pane_id)
                });
                let status_color = if active { active_fg } else { muted };
                let status: Option<gpui::AnyElement> = match agent.task_state {
                    _ if !machine.connected => None,
                    RuntimeTaskState::Running => {
                        running = true;
                        let (track, head) =
                            crate::gpui_shell::theme::sidebar_spinner_colors(cx, active);
                        Some(Self::spinner(self.spinner_phase, track, head).into_any_element())
                    },
                    RuntimeTaskState::WaitingInput => Some(
                        Self::waiting_hand(symbol_family.clone(), label_px, primary)
                            .into_any_element(),
                    ),
                    RuntimeTaskState::Attention => Some(
                        Self::waiting_hand(symbol_family.clone(), label_px, warning)
                            .into_any_element(),
                    ),
                    RuntimeTaskState::Finished => {
                        Some(div().size(px(6.0)).rounded_full().bg(primary).into_any_element())
                    },
                    RuntimeTaskState::Failed => Some(
                        Icon::new(IconName::CircleX).xsmall().text_color(danger).into_any_element(),
                    ),
                    RuntimeTaskState::Idle => None,
                };
                let title = if agent.title.is_empty() {
                    agent.display_name.clone()
                } else {
                    agent.title.clone()
                };
                let (key, target) = (machine.key.clone(), agent.clone());
                let row = h_flex()
                    .id(("remote-session", machine_ix * 4096 + agent_ix))
                    .w(px(row_w))
                    .overflow_hidden()
                    .gap_2()
                    .px_2()
                    .h(px(row_height))
                    .items_center()
                    .rounded(px(crate::display::UI_CORNER_RADIUS_LOGICAL))
                    .font_weight(FontWeight::NORMAL)
                    .font_family(chrome_family.clone())
                    .cursor_pointer()
                    .when(!machine.connected, |row| row.opacity(0.5))
                    .when(active, |row| row.bg(active_bg).text_color(active_fg))
                    .when(!active, |row| row.text_color(muted).hover(|style| style.bg(hover_bg)))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_remote_session(&key, &target, window, cx);
                    }))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(label_px))
                            .child(SharedString::from(title)),
                    )
                    .child(
                        h_flex()
                            .flex_shrink_0()
                            .items_center()
                            .text_color(status_color)
                            .children(status),
                    );
                sections.push(row.into_any_element());
            }
        }
        (sections, running)
    }
}
