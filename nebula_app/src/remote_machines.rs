//! Machines whose Pebrel sessions are visible here because one of our SSH panes
//! reached them. SSH authentication is the only admission rule: there is no
//! discovery, pairing or approval step, and nothing is listed until the remote
//! bridge answered over an already authenticated connection.
//!
//! The registry is transport-neutral. `ssh_session::remote_runtime` owns the
//! links; the GPUI shell reads projections and sends allowlisted requests.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot, watch};

use crate::runtime_api::RuntimeTaskState;

/// One AI session running in a remote Pebrel pane.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RemoteAgent {
    pub window_id: u64,
    pub pane_id: u64,
    pub title: String,
    pub kind: String,
    pub display_name: String,
    pub session_id: Option<String>,
    pub task_state: RuntimeTaskState,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct MachineView {
    pub key: String,
    pub label: String,
    pub connected: bool,
    pub agents: Vec<RemoteAgent>,
}

pub(crate) type Reply = Result<Value, String>;
pub(crate) type LinkRequest = (String, Value, oneshot::Sender<Reply>);

struct Machine {
    connected: bool,
    agents: Vec<RemoteAgent>,
    /// Owner id and request queue of the live link; `None` while disconnected.
    link: Option<(u64, mpsc::Sender<LinkRequest>)>,
}

#[derive(Default)]
struct Registry {
    /// First-seen order, so groups do not reshuffle when one reconnects.
    order: Vec<String>,
    machines: HashMap<String, Machine>,
    labels: HashMap<String, String>,
    labels_loaded: bool,
}

#[derive(Default, Serialize, Deserialize)]
struct LabelFile {
    #[serde(default)]
    labels: HashMap<String, String>,
}

fn registry() -> &'static Mutex<Registry> {
    static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();
    REGISTRY.get_or_init(Default::default)
}

fn revision() -> &'static watch::Sender<u64> {
    static REVISION: OnceLock<watch::Sender<u64>> = OnceLock::new();
    REVISION.get_or_init(|| watch::channel(0).0)
}

fn bump() {
    revision().send_modify(|value| *value = value.wrapping_add(1));
}

fn labels_path() -> PathBuf {
    crate::display::nebula_data_dir().join("remote_machines.json")
}

fn with_registry<T>(f: impl FnOnce(&mut Registry) -> T) -> T {
    let mut registry = registry().lock().unwrap_or_else(|error| error.into_inner());
    if !registry.labels_loaded {
        registry.labels_loaded = true;
        registry.labels = std::fs::read(labels_path())
            .ok()
            .and_then(|bytes| serde_json::from_slice::<LabelFile>(&bytes).ok())
            .map(|file| file.labels)
            .unwrap_or_default();
    }
    f(&mut registry)
}

/// Every change to machines, links or labels bumps this value.
pub(crate) fn changes() -> watch::Receiver<u64> {
    revision().subscribe()
}

pub(crate) fn machines() -> Vec<MachineView> {
    with_registry(|registry| {
        registry
            .order
            .iter()
            .filter_map(|key| {
                let machine = registry.machines.get(key)?;
                Some(MachineView {
                    key: key.clone(),
                    label: registry.labels.get(key).cloned().unwrap_or_else(|| key.clone()),
                    connected: machine.connected,
                    agents: machine.agents.clone(),
                })
            })
            .collect()
    })
}

/// Rename a machine group. An empty name restores the SSH destination.
pub(crate) fn rename(key: &str, label: &str) {
    let label = label.trim();
    let file = with_registry(|registry| {
        if label.is_empty() || label == key {
            registry.labels.remove(key);
        } else {
            registry.labels.insert(key.to_owned(), label.to_owned());
        }
        LabelFile { labels: registry.labels.clone() }
    });
    bump();
    std::thread::spawn(move || {
        if let Ok(bytes) = serde_json::to_vec_pretty(&file)
            && let Err(error) = crate::atomic_file::write(&labels_path(), &bytes)
        {
            log::warn!("保存远端机器名称失败: {error}");
        }
    });
}

pub(crate) fn link_active(key: &str) -> bool {
    with_registry(|registry| {
        registry.machines.get(key).is_some_and(|machine| machine.link.is_some())
    })
}

/// A bridge answered: list the machine (first time) and route requests to `link`.
pub(crate) fn link_up(key: &str, owner: u64, link: mpsc::Sender<LinkRequest>) -> bool {
    let claimed = with_registry(|registry| {
        if registry.machines.get(key).is_some_and(|machine| machine.link.is_some()) {
            return false;
        }
        if !registry.machines.contains_key(key) {
            registry.order.push(key.to_owned());
        }
        let machine = registry.machines.entry(key.to_owned()).or_insert_with(|| Machine {
            connected: false,
            agents: Vec::new(),
            link: None,
        });
        machine.connected = true;
        machine.link = Some((owner, link));
        true
    });
    if claimed {
        bump();
    }
    claimed
}

/// Keep the last projection visible but dimmed; reconnecting refreshes it.
pub(crate) fn link_down(key: &str, owner: u64) {
    let changed = with_registry(|registry| {
        let Some(machine) = registry.machines.get_mut(key) else { return false };
        if machine.link.as_ref().is_none_or(|(id, _)| *id != owner) {
            return false;
        }
        machine.link = None;
        machine.connected = false;
        true
    });
    if changed {
        bump();
    }
}

/// Replace the agent list from one remote `runtime.snapshot` event.
pub(crate) fn update_snapshot(key: &str, owner: u64, snapshot: &Value) {
    let agents = agents_from_snapshot(snapshot);
    let changed = with_registry(|registry| {
        let Some(machine) = registry.machines.get_mut(key) else { return false };
        if machine.link.as_ref().is_none_or(|(id, _)| *id != owner) || machine.agents == agents {
            return false;
        }
        machine.agents = agents;
        true
    });
    if changed {
        bump();
    }
}

/// Send one allowlisted bridge request to a connected machine.
pub(crate) async fn request(key: &str, method: &str, params: Value) -> Reply {
    let link = with_registry(|registry| {
        registry.machines.get(key).and_then(|machine| machine.link.as_ref().map(|(_, l)| l.clone()))
    })
    .ok_or_else(|| "machine is disconnected".to_owned())?;
    let (reply, answer) = oneshot::channel();
    link.send((method.to_owned(), params, reply))
        .await
        .map_err(|_| "machine is disconnected".to_owned())?;
    answer.await.map_err(|_| "machine is disconnected".to_owned())?
}

/// Start a new Claude/Codex session in a new tab of the remote Pebrel, using the
/// remote's own verified launch command. Returns its `(window_id, pane_id)`.
pub(crate) async fn start_agent(key: &str, kind: &str) -> Result<(u64, u64), String> {
    let stamp = chrono::Local::now().format("%H%M%S");
    let mut last = String::new();
    // Agent names are unique per remote runtime; a clash only needs another suffix.
    for attempt in 0..3 {
        let name = match attempt {
            0 => format!("{kind}-{stamp}"),
            n => format!("{kind}-{stamp}-{n}"),
        };
        match request(key, "agent.start", serde_json::json!({"name": name, "kind": kind})).await {
            Ok(result) => {
                return started_target(&result).ok_or_else(|| "bad agent.start reply".into());
            },
            Err(error) if error.contains("already") => last = error,
            Err(error) => return Err(error),
        }
    }
    Err(last)
}

fn started_target(result: &Value) -> Option<(u64, u64)> {
    let action = result.get("action").unwrap_or(result);
    Some((action["window_id"].as_u64()?, action["pane_id"].as_u64()?))
}

fn agents_from_snapshot(snapshot: &Value) -> Vec<RemoteAgent> {
    let mut agents = Vec::new();
    for window in snapshot["windows"].as_array().into_iter().flatten() {
        let Some(window_id) = window["id"].as_u64() else { continue };
        for tab in window["tabs"].as_array().into_iter().flatten() {
            for pane in tab["panes"].as_array().into_iter().flatten() {
                let (Some(pane_id), Some(agent)) = (pane["id"].as_u64(), pane.get("agent")) else {
                    continue;
                };
                if !agent.is_object() {
                    continue;
                }
                let title = pane["title"].as_str().filter(|title| !title.is_empty());
                let label = tab["label"].as_str().filter(|label| !label.is_empty());
                agents.push(RemoteAgent {
                    window_id,
                    pane_id,
                    title: title.or(label).unwrap_or_default().to_owned(),
                    kind: agent["kind"].as_str().unwrap_or_default().to_owned(),
                    display_name: agent["display_name"].as_str().unwrap_or_default().to_owned(),
                    session_id: agent["session_id"].as_str().map(str::to_owned),
                    task_state: serde_json::from_value(pane["task_state"].clone())
                        .unwrap_or(RuntimeTaskState::Idle),
                });
            }
        }
    }
    agents
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn snapshot_lists_only_agent_panes_with_their_state() {
        let snapshot = json!({"windows":[{"id":1,"tabs":[
            {"label":"shell","panes":[{"id":3,"title":"zsh","task_state":"idle"}]},
            {"label":"港区","panes":[{"id":7,"title":"","task_state":"running",
                "agent":{"kind":"claude","display_name":"Claude Code","session_id":"s1"}}]}
        ]}]});
        let agents = agents_from_snapshot(&snapshot);
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].pane_id, 7);
        assert_eq!(agents[0].title, "港区");
        assert_eq!(agents[0].task_state, RuntimeTaskState::Running);
        assert_eq!(agents[0].session_id.as_deref(), Some("s1"));
    }

    #[test]
    fn started_agent_target_comes_from_the_runtime_action() {
        let reply = json!({"action":{"window_id":2,"pane_id":9,"agent":{}},"snapshot":{}});
        assert_eq!(started_target(&reply), Some((2, 9)));
        assert_eq!(started_target(&json!({"window_id":1,"pane_id":4})), Some((1, 4)));
        assert_eq!(started_target(&json!({"action":{"window_id":1}})), None);
    }

    #[test]
    fn only_the_owning_link_can_publish_or_disconnect() {
        let key = "test-owner-machine";
        let (sender, _receiver) = mpsc::channel(1);
        assert!(link_up(key, 1, sender.clone()));
        assert!(!link_up(key, 2, sender), "a second link must not replace the live one");
        let snapshot = json!({"windows":[{"id":1,"tabs":[{"label":"a","panes":[
            {"id":2,"title":"t","task_state":"finished","agent":{"kind":"codex","display_name":"Codex"}}
        ]}]}]});
        update_snapshot(key, 2, &snapshot);
        let find = || machines().into_iter().find(|machine| machine.key == key).unwrap();
        assert!(find().agents.is_empty());
        update_snapshot(key, 1, &snapshot);
        assert_eq!(find().agents.len(), 1);
        link_down(key, 2);
        assert!(find().connected);
        link_down(key, 1);
        let machine = find();
        assert!(!machine.connected);
        assert_eq!(machine.agents.len(), 1, "a dropped link keeps its last projection");
    }
}
