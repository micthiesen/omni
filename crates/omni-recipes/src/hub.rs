//! The in-process change feed behind `GET /api/events`: tools and the app's
//! own writes both notify it, after their transaction commits.

use tokio::sync::broadcast;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HubEvent {
    /// The library changed; `seq` is its newest sequence number.
    Library { seq: u64 },
    /// A cook session changed; `seq` is its newest event number.
    Session { id: String, seq: u32 },
}

#[derive(Clone)]
pub struct Hub {
    sender: broadcast::Sender<HubEvent>,
}

impl Default for Hub {
    fn default() -> Self {
        let (sender, _) = broadcast::channel(256);
        Self { sender }
    }
}

impl Hub {
    pub fn subscribe(&self) -> broadcast::Receiver<HubEvent> {
        self.sender.subscribe()
    }

    /// How many app clients hold the event stream open.
    pub fn clients(&self) -> usize {
        self.sender.receiver_count()
    }

    pub fn library(&self, seq: u64) {
        // No receiver is not a failure: nobody is watching.
        let _ = self.sender.send(HubEvent::Library { seq });
    }

    pub fn session(&self, id: &str, seq: u32) {
        let _ = self.sender.send(HubEvent::Session {
            id: id.to_owned(),
            seq,
        });
    }
}
