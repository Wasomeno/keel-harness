//! Permission gate.
//!
//! The run loop never executes a tool on its own authority. `AllowAll` and
//! `DenyAll` are for tests and explicit opt-in; `Ask` sends a request to the
//! host, which owns the UI, and waits for a decision. A missing or dropped host
//! is a denial, never an implicit allow.

use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PermissionDecision {
    Allow,
    Deny { reason: String },
}

impl PermissionDecision {
    pub fn deny(reason: impl Into<String>) -> Self {
        Self::Deny {
            reason: reason.into(),
        }
    }

    pub fn is_allowed(&self) -> bool {
        matches!(self, Self::Allow)
    }
}

pub struct PermissionRequest {
    pub tool: String,
    pub summary: String,
    pub arguments: Value,
    pub responder: oneshot::Sender<PermissionDecision>,
}

impl PermissionRequest {
    pub fn respond(self, decision: PermissionDecision) {
        let _ = self.responder.send(decision);
    }
}

#[derive(Clone, Debug)]
pub enum PermissionGate {
    AllowAll,
    DenyAll,
    Ask(mpsc::Sender<PermissionRequest>),
}

impl PermissionGate {
    /// Create a gate plus the receiver the host must service.
    pub fn ask() -> (Self, mpsc::Receiver<PermissionRequest>) {
        let (sender, receiver) = mpsc::channel(8);
        (Self::Ask(sender), receiver)
    }

    pub async fn check(
        &self,
        tool: &str,
        summary: &str,
        arguments: &Value,
        cancel: &CancellationToken,
    ) -> PermissionDecision {
        match self {
            Self::AllowAll => PermissionDecision::Allow,
            Self::DenyAll => PermissionDecision::deny("tool use is disabled"),
            Self::Ask(sender) => {
                let (responder, response) = oneshot::channel();
                let request = PermissionRequest {
                    tool: tool.to_owned(),
                    summary: summary.to_owned(),
                    arguments: arguments.clone(),
                    responder,
                };
                if sender.send(request).await.is_err() {
                    return PermissionDecision::deny("no permission handler is attached");
                }
                tokio::select! {
                    _ = cancel.cancelled() => PermissionDecision::deny("cancelled"),
                    decision = response => decision
                        .unwrap_or_else(|_| PermissionDecision::deny("permission handler dropped")),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn allow_all_never_asks() {
        let decision = PermissionGate::AllowAll
            .check("bash", "bash: ls", &Value::Null, &CancellationToken::new())
            .await;
        assert_eq!(decision, PermissionDecision::Allow);
    }

    #[tokio::test]
    async fn deny_all_refuses() {
        let decision = PermissionGate::DenyAll
            .check("bash", "bash: ls", &Value::Null, &CancellationToken::new())
            .await;
        assert!(!decision.is_allowed());
    }

    #[tokio::test]
    async fn ask_round_trips_through_the_host() {
        let (gate, mut receiver) = PermissionGate::ask();
        let host = tokio::spawn(async move {
            let request = receiver.recv().await.unwrap();
            assert_eq!(request.tool, "bash");
            request.respond(PermissionDecision::Allow);
        });

        let decision = gate
            .check(
                "bash",
                "bash: ls",
                &serde_json::json!({"command": "ls"}),
                &CancellationToken::new(),
            )
            .await;

        host.await.unwrap();
        assert_eq!(decision, PermissionDecision::Allow);
    }

    #[tokio::test]
    async fn a_cancelled_ask_is_a_denial() {
        let (gate, _receiver) = PermissionGate::ask();
        let cancel = CancellationToken::new();
        cancel.cancel();

        let decision = gate.check("bash", "bash: ls", &Value::Null, &cancel).await;

        assert!(!decision.is_allowed());
    }

    #[tokio::test]
    async fn a_missing_host_is_a_denial() {
        let (gate, receiver) = PermissionGate::ask();
        drop(receiver);

        let decision = gate
            .check("bash", "bash: ls", &Value::Null, &CancellationToken::new())
            .await;

        assert!(!decision.is_allowed());
    }
}
