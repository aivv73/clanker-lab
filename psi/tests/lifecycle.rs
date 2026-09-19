use async_trait::async_trait;
use psi::core::{Context, Plugin, Runtime};
use std::sync::{Arc, Mutex};
struct Probe {
    id: &'static str,
    provides: &'static [&'static str],
    requires: &'static [&'static str],
    fail: bool,
    log: Arc<Mutex<Vec<String>>>,
}
#[async_trait]
impl Plugin for Probe {
    fn id(&self) -> &'static str {
        self.id
    }
    fn provides(&self) -> &'static [&'static str] {
        self.provides
    }
    fn requires(&self) -> &'static [&'static str] {
        self.requires
    }
    async fn start(&mut self, mut context: Context<'_>) -> Result<(), String> {
        self.log.lock().unwrap().push(format!("start:{}", self.id));
        for name in self.requires {
            context.dependency::<usize>(name)?;
        }
        for name in self.provides {
            context.register(name, 1usize)?;
        }
        if self.fail {
            Err("injected".into())
        } else {
            Ok(())
        }
    }
    async fn close(&mut self) -> Result<(), String> {
        self.log.lock().unwrap().push(format!("close:{}", self.id));
        if self.id == "close-fails" {
            Err("injected close failure".into())
        } else {
            Ok(())
        }
    }
}
fn plugins(log: &Arc<Mutex<Vec<String>>>, fail: bool) -> Vec<Box<dyn Plugin>> {
    vec![
        Box::new(Probe {
            id: "loop",
            provides: &["loop"],
            requires: &["model"],
            fail,
            log: log.clone(),
        }),
        Box::new(Probe {
            id: "model",
            provides: &["model"],
            requires: &[],
            fail: false,
            log: log.clone(),
        }),
    ]
}
#[tokio::test]
async fn dependency_order_and_repeatable_cleanup() {
    let log = Arc::new(Mutex::new(vec![]));
    let mut runtime = Runtime::start(plugins(&log, false)).await.unwrap();
    runtime.close().await.unwrap();
    runtime.close().await.unwrap();
    assert!(runtime.service::<usize>("model").is_err());
    assert_eq!(
        *log.lock().unwrap(),
        ["start:model", "start:loop", "close:loop", "close:model"]
    );
}
#[tokio::test]
async fn partial_start_is_cleaned_up() {
    let log = Arc::new(Mutex::new(vec![]));
    assert!(Runtime::start(plugins(&log, true)).await.is_err());
    assert_eq!(
        *log.lock().unwrap(),
        ["start:model", "start:loop", "close:loop", "close:model"]
    );
}
#[tokio::test]
async fn invalid_graph_has_no_startup_side_effects() {
    let log = Arc::new(Mutex::new(vec![]));
    let mut p = plugins(&log, false);
    p.pop();
    assert!(Runtime::start(p).await.is_err());
    let mut p = plugins(&log, false);
    p.extend(plugins(&log, false));
    assert!(Runtime::start(p).await.is_err());
    assert!(log.lock().unwrap().is_empty());
}

#[tokio::test]
async fn providers_remain_available_until_consumers_close() {
    let log = Arc::new(Mutex::new(vec![]));
    let mut runtime = Runtime::start(plugins(&log, false)).await.unwrap();
    assert_eq!(
        runtime.stop_plugin("model").await.unwrap_err(),
        "provider_in_use"
    );
    assert_eq!(*runtime.service::<usize>("model").unwrap(), 1);
    runtime.stop_plugin("loop").await.unwrap();
    runtime.stop_plugin("loop").await.unwrap();
    runtime.stop_plugin("model").await.unwrap();
    runtime.close().await.unwrap();
    assert_eq!(
        *log.lock().unwrap(),
        ["start:model", "start:loop", "close:loop", "close:model"]
    );
}

#[tokio::test]
async fn cycle_and_distinct_duplicate_providers_never_start() {
    let log = Arc::new(Mutex::new(vec![]));
    for (provides, requires) in [(&["model"][..], &[][..]), (&["cycle"][..], &["cycle"][..])] {
        let mut p = plugins(&log, false);
        p.push(Box::new(Probe {
            id: "other",
            provides,
            requires,
            fail: false,
            log: log.clone(),
        }));
        assert!(Runtime::start(p).await.is_err());
    }
    assert!(log.lock().unwrap().is_empty());
}

#[tokio::test]
async fn session_cleanup_drops_only_owned_work_and_prevents_respawn() {
    use psi::work::Owner;
    struct Resource(Arc<Mutex<Vec<&'static str>>>, &'static str);
    impl Drop for Resource {
        fn drop(&mut self) {
            self.0.lock().unwrap().push(self.1);
        }
    }
    let log = Arc::new(Mutex::new(vec![]));
    let dropped = Arc::new(Mutex::new(vec![]));
    let mut runtime = Runtime::start(plugins(&log, false)).await.unwrap();
    let spawner = runtime.spawner("model").unwrap();
    for (owner, name) in [
        (Owner::Runtime, "shared"),
        (Owner::Session("a".into()), "session"),
        (
            Owner::Turn {
                session: "a".into(),
                turn: "t".into(),
            },
            "turn",
        ),
        (
            Owner::Invocation {
                session: "a".into(),
                turn: "t".into(),
                invocation: "i".into(),
            },
            "invocation",
        ),
        (Owner::Session("b".into()), "other"),
    ] {
        let resource = Resource(dropped.clone(), name);
        spawner
            .spawn(owner, async move {
                let _resource = resource;
                std::future::pending::<()>().await;
            })
            .unwrap();
    }
    runtime.work().close_owner(Owner::Session("a".into())).await;
    runtime.work().close_owner(Owner::Session("a".into())).await;
    let mut actual = dropped.lock().unwrap().clone();
    actual.sort();
    assert_eq!(actual, ["invocation", "session", "turn"]);
    assert_eq!(runtime.work().count(), 2);
    assert!(spawner.spawn(Owner::Session("a".into()), async {}).is_err());
    assert_eq!(*runtime.service::<usize>("model").unwrap(), 1);
    runtime.close().await.unwrap();
    assert_eq!(runtime.work().count(), 0);
    assert_eq!(dropped.lock().unwrap().len(), 5);
    assert!(spawner.spawn(Owner::Runtime, async {}).is_err());
}

#[tokio::test]
async fn shutdown_failure_is_reported_after_remaining_cleanup() {
    let log = Arc::new(Mutex::new(vec![]));
    let mut p = plugins(&log, false);
    p.push(Box::new(Probe {
        id: "close-fails",
        provides: &["consumer"],
        requires: &["loop"],
        fail: false,
        log: log.clone(),
    }));
    let mut runtime = Runtime::start(p).await.unwrap();
    assert!(
        runtime
            .close()
            .await
            .unwrap_err()
            .contains("close-fails: injected close failure")
    );
    assert!(runtime.service::<usize>("consumer").is_err());
    assert!(runtime.service::<usize>("loop").is_err());
    assert!(runtime.service::<usize>("model").is_err());
    assert_eq!(
        &log.lock().unwrap()[3..],
        ["close:close-fails", "close:loop", "close:model"]
    );
    runtime.close().await.unwrap();
}
