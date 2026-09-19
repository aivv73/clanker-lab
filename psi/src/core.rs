//! Lifecycle only: service ownership and dependency-ordered plugin startup/cleanup.
use async_trait::async_trait;
use std::{
    any::Any,
    collections::{HashMap, HashSet},
    sync::Arc,
};

pub type Services = HashMap<&'static str, Arc<dyn Any + Send + Sync>>;

pub struct Context<'a> {
    services: &'a mut Services,
    provides: &'a [&'static str],
    requires: &'a [&'static str],
}
impl Context<'_> {
    pub fn register<T: Any + Send + Sync>(
        &mut self,
        name: &'static str,
        value: T,
    ) -> Result<(), String> {
        if !self.provides.contains(&name) || self.services.contains_key(name) {
            return Err(format!("invalid registration: {name}"));
        }
        self.services.insert(name, Arc::new(value));
        Ok(())
    }
    pub fn dependency<T: Any + Send + Sync>(&self, name: &str) -> Result<Arc<T>, String> {
        if !self.requires.contains(&name) {
            return Err(format!("undeclared dependency: {name}"));
        }
        self.services
            .get(name)
            .cloned()
            .and_then(|v| v.downcast().ok())
            .ok_or_else(|| format!("missing or mistyped service: {name}"))
    }
}

#[async_trait]
pub trait Plugin: Send {
    fn id(&self) -> &'static str;
    fn provides(&self) -> &'static [&'static str];
    fn requires(&self) -> &'static [&'static str];
    async fn start(&mut self, context: Context<'_>) -> Result<(), String>;
    async fn close(&mut self);
}

pub struct Runtime {
    plugins: Vec<Box<dyn Plugin>>,
    services: Services,
    started: Vec<usize>,
}
impl Runtime {
    pub async fn start(plugins: Vec<Box<dyn Plugin>>) -> Result<Self, String> {
        // Validate the whole graph before any plugin can acquire resources.
        let mut owners = HashMap::new();
        let mut ids = HashSet::new();
        for (i, p) in plugins.iter().enumerate() {
            if !ids.insert(p.id()) {
                return Err(format!("duplicate plugin: {}", p.id()));
            }
            for service in p.provides() {
                if owners.insert(*service, i).is_some() {
                    return Err(format!("duplicate service: {service}"));
                }
            }
        }
        let mut order = Vec::new();
        while order.len() < plugins.len() {
            let next = (0..plugins.len()).find(|i| {
                !order.contains(i)
                    && plugins[*i]
                        .requires()
                        .iter()
                        .all(|s| owners.get(s).is_some_and(|owner| order.contains(owner)))
            });
            order.push(next.ok_or("missing dependency or dependency cycle")?);
        }
        let mut runtime = Self {
            plugins,
            services: HashMap::new(),
            started: Vec::new(),
        };
        for i in order {
            let provides = runtime.plugins[i].provides();
            let requires = runtime.plugins[i].requires();
            // Include a partially started plugin in cleanup.
            runtime.started.push(i);
            let result = runtime.plugins[i]
                .start(Context {
                    services: &mut runtime.services,
                    provides,
                    requires,
                })
                .await;
            let result = result.and_then(|()| {
                if provides.iter().all(|s| runtime.services.contains_key(s)) {
                    Ok(())
                } else {
                    Err("plugin did not register its declared services".into())
                }
            });
            if let Err(error) = result {
                runtime.close().await;
                return Err(error);
            }
        }
        Ok(runtime)
    }
    pub fn service<T: Any + Send + Sync>(&self, name: &str) -> Result<Arc<T>, String> {
        self.services
            .get(name)
            .cloned()
            .and_then(|v| v.downcast().ok())
            .ok_or_else(|| format!("missing service: {name}"))
    }
    pub async fn close(&mut self) {
        while let Some(i) = self.started.pop() {
            self.plugins[i].close().await;
            for name in self.plugins[i].provides() {
                self.services.remove(name);
            }
        }
    }
}
