use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;
use notify::{Watcher, RecommendedWatcher, RecursiveMode, Event};
use parking_lot::RwLock;
use std::collections::HashMap;

pub struct WatcherService {
    watchers: RwLock<HashMap<String, notify::RecommendedWatcher>>,
    subscribers: RwLock<HashMap<String, Vec<mpsc::Sender<FileEvent>>>>,
}

#[derive(Debug, Clone)]
pub struct FileEvent {
    pub project_id: String,
    pub path: String,
    pub kind: FileEventKind,
}

#[derive(Debug, Clone)]
pub enum FileEventKind {
    Created,
    Modified,
    Deleted,
    Renamed,
}

impl WatcherService {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn watch_project(&self, project_id: String, path: PathBuf) -> Result<(), String> {
        let (tx, mut rx) = mpsc::channel(100);

        let project_id_clone = project_id.clone();
        tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                println!("File event for {}: {:?}", project_id_clone, event);
            }
        });

        let mut watcher = notify::recommended_watcher(move |res: Result<Event, notify::Error>| {
            if let Ok(event) = res {
                let kind = match event.kind {
                    notify::EventKind::Create(_) => FileEventKind::Created,
                    notify::EventKind::Modify(_) => FileEventKind::Modified,
                    notify::EventKind::Remove(_) => FileEventKind::Deleted,
                    _ => return,
                };

                for path in event.paths {
                    let file_event = FileEvent {
                        project_id: project_id.clone(),
                        path: path.to_string_lossy().to_string(),
                        kind: kind.clone(),
                    };
                }
            }
        }).map_err(|e| e.to_string())?;

        watcher.watch(&path, RecursiveMode::Recursive)
            .map_err(|e| e.to_string())?;

        self.watchers.write().insert(project_id.clone(), watcher);
        self.subscribers.write().insert(project_id, vec![tx]);

        Ok(())
    }

    pub fn unwatch_project(&self, project_id: &str) -> bool {
        self.watchers.write().remove(project_id);
        self.subscribers.write().remove(project_id);
        true
    }

    pub fn subscribe(&self, project_id: String) -> mpsc::Receiver<FileEvent> {
        let (tx, rx) = mpsc::channel(100);
        let mut subs = self.subscribers.write();
        subs.entry(project_id).or_default().push(tx);
        rx
    }
}

impl Default for WatcherService {
    fn default() -> Self {
        Self {
            watchers: RwLock::new(HashMap::new()),
            subscribers: RwLock::new(HashMap::new()),
        }
    }
}
