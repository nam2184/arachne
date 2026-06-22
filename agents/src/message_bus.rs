//! In-process pub/sub for the agent runtime. **Planned future API**.
//!
//! `MessageBus` is a topic-keyed fan-out of `String` messages
//! between background jobs (e.g. the compactor, the tool-result
//! pruner, child subagent completions). Today the runner uses
//! `SubagentRegistry::take_completions` for the only cross-job
//! signalling it needs; the bus will be wired up when the
//! pruner / background jobs land. Do not delete.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;
use tokio::sync::mpsc;

#[derive(Default)]
pub struct MessageBus {
    subscribers: RwLock<HashMap<String, Vec<mpsc::Sender<String>>>>,
}

impl MessageBus {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn subscribe(&self, topic: String) -> mpsc::Receiver<String> {
        let (tx, rx) = mpsc::channel(100);
        self.subscribers.write().entry(topic).or_default().push(tx);
        rx
    }

    pub fn publish(&self, topic: &str, message: String) {
        if let Some(senders) = self.subscribers.read().get(topic) {
            for sender in senders {
                let _ = sender.try_send(message.clone());
            }
        }
    }
}
