//! Execution ownership is independent of the plugin supplying a capability.
use std::{
    collections::HashMap,
    future::Future,
    sync::{Arc, Mutex},
};
use tokio::task::JoinHandle;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Owner {
    Runtime,
    Session(String),
    Turn {
        session: String,
        turn: String,
    },
    Invocation {
        session: String,
        turn: String,
        invocation: String,
    },
}
impl Owner {
    fn contains(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Runtime, _) => true,
            (
                Self::Session(a),
                Self::Session(b)
                | Self::Turn { session: b, .. }
                | Self::Invocation { session: b, .. },
            ) => a == b,
            (
                Self::Turn {
                    session: a,
                    turn: x,
                },
                Self::Turn {
                    session: b,
                    turn: y,
                }
                | Self::Invocation {
                    session: b,
                    turn: y,
                    ..
                },
            ) => a == b && x == y,
            _ => self == other,
        }
    }
}
struct Task {
    plugin: &'static str,
    owner: Owner,
    handle: JoinHandle<()>,
}
#[derive(Default)]
struct State {
    tasks: Vec<Task>,
    closed: Vec<Owner>,
    plugins: HashMap<&'static str, bool>,
}
#[derive(Clone, Default)]
pub struct Work(Arc<Mutex<State>>);
#[derive(Clone)]
pub struct Spawner {
    work: Work,
    plugin: &'static str,
}
impl Work {
    pub(crate) fn plugin(&self, plugin: &'static str) -> Spawner {
        self.0.lock().unwrap().plugins.entry(plugin).or_insert(true);
        Spawner {
            work: self.clone(),
            plugin,
        }
    }
    /// Reopen a conversation after its previous work has been fully joined.
    /// Old turn/invocation scopes remain closed and their identities cannot repeat.
    pub fn reopen_session(&self, session: &str) {
        self.0
            .lock()
            .unwrap()
            .closed
            .retain(|owner| owner != &Owner::Session(session.to_owned()));
    }
    pub async fn close_owner(&self, owner: Owner) {
        let tasks = {
            let mut state = self.0.lock().unwrap();
            state.closed.push(owner.clone());
            let (tasks, keep) = state
                .tasks
                .drain(..)
                .partition::<Vec<_>, _>(|t| owner.contains(&t.owner));
            state.tasks = keep;
            tasks
        };
        Self::stop(tasks).await;
    }
    pub(crate) async fn close_plugin(&self, plugin: &'static str) {
        let tasks = {
            let mut state = self.0.lock().unwrap();
            state.plugins.insert(plugin, false);
            let (tasks, keep) = state
                .tasks
                .drain(..)
                .partition::<Vec<_>, _>(|t| t.plugin == plugin);
            state.tasks = keep;
            tasks
        };
        Self::stop(tasks).await;
    }
    async fn stop(tasks: Vec<Task>) {
        for task in &tasks {
            task.handle.abort();
        }
        for task in tasks {
            // Awaiting confirms that cooperative Rust futures have dropped resources.
            // It cannot forcibly terminate blocking or arbitrary in-process code.
            let _ = task.handle.await;
            eprintln!(
                "Psi work stopped: plugin={} owner={:?}",
                task.plugin, task.owner
            );
        }
    }
    pub fn count(&self) -> usize {
        self.0.lock().unwrap().tasks.len()
    }
}
impl Spawner {
    pub fn spawn(
        &self,
        owner: Owner,
        future: impl Future<Output = ()> + Send + 'static,
    ) -> Result<(), String> {
        let mut state = self.work.0.lock().unwrap();
        if state.plugins.get(self.plugin) != Some(&true)
            || state.closed.iter().any(|closed| closed.contains(&owner))
        {
            return Err("work_scope_closed".into());
        }
        state.tasks.push(Task {
            plugin: self.plugin,
            owner,
            handle: tokio::spawn(future),
        });
        Ok(())
    }
}
