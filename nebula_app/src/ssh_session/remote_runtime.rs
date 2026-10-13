//! Remote Pebrel sessions over an SSH pane's own authenticated connection.
//!
//! After a shell is ready, the pane offers its connection to one holder task.
//! The holder opens a separate exec channel that runs the remote
//! `pebrel mobile-bridge` (view-only) and feeds `remote_machines`. Several panes
//! to the same destination share one link; when the owner pane closes, another
//! live pane takes over. Nothing reauthenticates or reaches the connection pool.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use russh::ChannelMsg;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot, watch};

use super::{ClientSession, SessionError, SharedSession};
use crate::remote_machines::{self, LinkRequest, Reply};

/// Locate a Pebrel build that has the bridge, then serve it. Non-interactive SSH
/// shells rarely have the app on PATH, so the usual install locations follow.
/// `sh -c` keeps the probe POSIX even when the login shell is fish or zsh.
const PROBE: &str = "sh -c 'for p in \"$(command -v pebrel 2>/dev/null)\" \
\"$HOME/Applications/Pebrel.app/Contents/MacOS/pebrel\" \
/Applications/Pebrel.app/Contents/MacOS/pebrel \"$HOME/.local/bin/pebrel\" \
/usr/local/bin/pebrel /usr/bin/pebrel; do \
if [ -n \"$p\" ] && [ -x \"$p\" ] && \"$p\" help mobile-bridge >/dev/null 2>&1; then \
exec \"$p\" mobile-bridge; fi; done; exit 127'";

const READY_BUDGET: Duration = Duration::from_secs(10);
const RETRY_DELAY: Duration = Duration::from_secs(5);
const MAX_LINE: usize = 4 * 1024 * 1024;

/// Dropped with the pane's SSH session; its holder then stops or hands over.
pub(super) struct RemoteScope(watch::Sender<bool>);

impl Drop for RemoteScope {
    fn drop(&mut self) {
        self.0.send_replace(true);
    }
}

pub(super) fn offer(destination: &str, session: &SharedSession) -> RemoteScope {
    let (sender, closed) = watch::channel(false);
    let key = destination.trim().to_owned();
    let session = Arc::downgrade(session);
    tokio::spawn(hold(key, session, closed));
    RemoteScope(sender)
}

enum Outcome {
    /// The remote has no Pebrel with a bridge, or it is not running.
    Absent,
    /// A link was established and later ended.
    Ended,
}

async fn hold(key: String, session: Weak<ClientSession>, mut closed: watch::Receiver<bool>) {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let owner = NEXT.fetch_add(1, Ordering::Relaxed);
    let mut changes = remote_machines::changes();
    loop {
        if *closed.borrow() || session.upgrade().is_none_or(|session| session.is_closed()) {
            return;
        }
        if remote_machines::link_active(&key) {
            tokio::select! {
                _ = closed.changed() => {},
                _ = changes.changed() => {},
            }
            continue;
        }
        let Some(live) = session.upgrade() else { return };
        let outcome = tokio::select! {
            biased;
            _ = closed.changed() => return,
            outcome = link(&key, owner, &live) => outcome,
        };
        drop(live);
        match outcome {
            Ok(Outcome::Absent) => return,
            Ok(Outcome::Ended) => {},
            Err(error) => log::info!("远端 Pebrel 连接结束（{key}）: {error}"),
        }
        tokio::select! {
            _ = closed.changed() => return,
            _ = tokio::time::sleep(RETRY_DELAY) => {},
        }
    }
}

async fn link(key: &str, owner: u64, session: &ClientSession) -> Result<Outcome, SessionError> {
    let channel = session.channel_open_session().await?;
    let mut channel = super::lifecycle::own_channel(channel);
    channel.exec(true, PROBE).await?;
    let mut lines = Lines::default();
    let ready = tokio::time::timeout(READY_BUDGET, async {
        while let Some(message) = channel.wait().await {
            match message {
                ChannelMsg::Data { data } => {
                    for line in lines.push(&data)? {
                        if serde_json::from_slice::<Value>(&line)
                            .is_ok_and(|value| value["type"] == "mobile.ready")
                        {
                            return Ok::<_, SessionError>(true);
                        }
                    }
                },
                ChannelMsg::Failure | ChannelMsg::ExitStatus { .. } => return Ok(false),
                ChannelMsg::Eof | ChannelMsg::Close => return Ok(false),
                _ => {},
            }
        }
        Ok(false)
    })
    .await;
    if !matches!(ready, Ok(Ok(true))) {
        let _ = channel.finish().await;
        return Ok(Outcome::Absent);
    }

    let (requests, mut incoming) = mpsc::channel::<LinkRequest>(16);
    if !remote_machines::link_up(key, owner, requests) {
        let _ = channel.finish().await;
        return Ok(Outcome::Ended);
    }
    let result = serve(key, owner, &mut channel, &mut lines, &mut incoming).await;
    remote_machines::link_down(key, owner);
    let _ = channel.finish().await;
    result.map(|()| Outcome::Ended)
}

async fn serve(
    key: &str,
    owner: u64,
    channel: &mut super::lifecycle::ShellChannel,
    lines: &mut Lines,
    incoming: &mut mpsc::Receiver<LinkRequest>,
) -> Result<(), SessionError> {
    let mut pending: Vec<(String, oneshot::Sender<Reply>)> = Vec::new();
    let mut next = 0_u64;
    send_line(channel, &json!({"id":"subscribe","method":"events.subscribe","params":{}})).await?;
    loop {
        tokio::select! {
            message = channel.wait() => {
                let Some(message) = message else { return Ok(()) };
                match message {
                    ChannelMsg::Data { data } => {
                        for line in lines.push(&data)? {
                            dispatch(key, owner, &line, &mut pending);
                        }
                    },
                    ChannelMsg::Eof | ChannelMsg::Close | ChannelMsg::ExitStatus { .. } => {
                        return Ok(());
                    },
                    _ => {},
                }
            },
            request = incoming.recv() => {
                let Some((method, params, reply)) = request else { return Ok(()) };
                next += 1;
                let id = format!("r{next}");
                send_line(channel, &json!({"id":id,"method":method,"params":params})).await?;
                pending.retain(|(_, reply)| !reply.is_closed());
                pending.push((id, reply));
            },
        }
    }
}

fn dispatch(
    key: &str,
    owner: u64,
    line: &[u8],
    pending: &mut Vec<(String, oneshot::Sender<Reply>)>,
) {
    let Ok(value) = serde_json::from_slice::<Value>(line) else { return };
    if value["event"] == "runtime.snapshot" {
        remote_machines::update_snapshot(key, owner, &value["data"]);
        return;
    }
    let Some(id) = value["id"].as_str() else { return };
    let Some(index) = pending.iter().position(|(pending, _)| pending == id) else { return };
    let (_, reply) = pending.swap_remove(index);
    let answer = if value["ok"] == true {
        Ok(value["result"].clone())
    } else {
        Err(value["error"]["message"].as_str().unwrap_or("remote request failed").to_owned())
    };
    let _ = reply.send(answer);
}

async fn send_line(
    channel: &super::lifecycle::ShellChannel,
    value: &Value,
) -> Result<(), SessionError> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    channel.data_bytes(bytes).await?;
    Ok(())
}

/// Newline-framed JSON with a bound on any single line.
#[derive(Default)]
struct Lines(Vec<u8>);

impl Lines {
    fn push(&mut self, data: &[u8]) -> Result<Vec<Vec<u8>>, SessionError> {
        self.0.extend_from_slice(data);
        let mut lines = Vec::new();
        while let Some(end) = self.0.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = self.0.drain(..=end).collect();
            lines.push(line[..line.len() - 1].to_vec());
        }
        if self.0.len() > MAX_LINE {
            return Err("remote bridge line exceeds its budget".into());
        }
        Ok(lines)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_split_across_packets_and_reject_unbounded_input() {
        let mut lines = Lines::default();
        assert!(lines.push(b"{\"a\"").unwrap().is_empty());
        assert_eq!(
            lines.push(b":1}\n{\"b\":2}\n{").unwrap(),
            [b"{\"a\":1}".to_vec(), b"{\"b\":2}".to_vec()]
        );
        assert!(lines.push(&vec![b'x'; MAX_LINE + 1]).is_err());
    }

    #[test]
    fn replies_resolve_only_their_own_request() {
        let (first, mut first_answer) = oneshot::channel();
        let (second, mut second_answer) = oneshot::channel();
        let mut pending = vec![("r1".to_owned(), first), ("r2".to_owned(), second)];
        dispatch(
            "unused",
            0,
            br#"{"id":"r2","ok":false,"error":{"message":"nope"}}"#,
            &mut pending,
        );
        assert_eq!(second_answer.try_recv().unwrap(), Err("nope".to_owned()));
        assert!(first_answer.try_recv().is_err());
        assert_eq!(pending.len(), 1);
    }

    #[test]
    fn probe_stays_inside_one_single_quoted_posix_script() {
        assert_eq!(PROBE.matches('\'').count(), 2);
        assert!(PROBE.contains("mobile-bridge; fi"));
        assert!(!PROBE.contains("--allow-input"), "remote sessions are view-only");
    }
}
