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
    async fn close(&mut self) {
        self.log.lock().unwrap().push(format!("close:{}", self.id));
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
    runtime.close().await;
    runtime.close().await;
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
