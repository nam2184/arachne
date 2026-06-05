use std::sync::Arc;
use tokio::sync::mpsc;
use parking_lot::RwLock;
use std::collections::HashMap;

pub struct MessageBus {
    subscribers: RwLock<HashMap<String, Vec<mpsc::Sender<String>>>>,
}

impl MessageBus {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn subscribe(&self, topic: String) -> mpsc::Receiver<String> {
        let (tx, rx) = mpsc::channel(100);
        let mut subs = self.subscribers.write();
        subs.entry(topic).or_default().push(tx);
        rx
    }

    pub fn publish(&self, topic: String, message: String) {
        let subs = self.subscribers.read();
        if let Some(senders) = subs.get(&topic) {
            for sender in senders.iter() {
                let _ = sender.try_send(message.clone());
            }
        }
    }

    pub fn broadcast(&self, message: String) {
        let subs = self.subscribers.read();
        for senders in subs.values() {
            for sender in senders.iter() {
                let _ = sender.try_send(message.clone());
            }
        }
    }
}

impl Default for MessageBus {
    fn default() -> Self {
        Self {
            subscribers: RwLock::new(HashMap::new()),
        }
    }
}
