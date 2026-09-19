//! Lifecycle only: service ownership and dependency-ordered plugin startup/cleanup.
use crate::work::{Spawner, Work};
use async_trait::async_trait;
use std::{
    any::Any,
    collections::{HashMap, HashSet},
    sync::Arc,
};

pub type Services = HashMap<&'static str, Arc<dyn Any + Send + Sync>>;

pub struct Context<'a> {
    services: &'a mut Services,
    spawner: Spawner,
    provides: &'a [&'static str],
    requires: &'a [&'static str],
}
impl Context<'_> {
    pub fn spawner(&self) -> Spawner {
        self.spawner.clone()
    }
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
    async fn close(&mut self) -> Result<(), String>;
}

pub struct Runtime {
    plugins: Vec<Box<dyn Plugin>>,
    services: Services,
    started: Vec<usize>,
    work: Work,
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
        for plugin in &plugins {
            for service in plugin.requires() {
                if !owners.contains_key(service) {
                    return Err(format!(
                        "missing dependency: {service} required by {}",
                        plugin.id()
                    ));
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
            order.push(next.ok_or("dependency cycle")?);
        }
        let mut runtime = Self {
            plugins,
            services: HashMap::new(),
            started: Vec::new(),
            work: Work::default(),
        };
        for i in order {
            let provides = runtime.plugins[i].provides();
            let requires = runtime.plugins[i].requires();
            // Include a partially started plugin in cleanup.
            runtime.started.push(i);
            eprintln!("Psi plugin starting: {}", runtime.plugins[i].id());
            let spawner = runtime.work.plugin(runtime.plugins[i].id());
            let result = runtime.plugins[i]
                .start(Context {
                    services: &mut runtime.services,
                    spawner,
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
                let cleanup = runtime.close().await;
                return Err(match cleanup {
                    Ok(()) => error,
                    Err(cleanup) => format!("{error}; cleanup: {cleanup}"),
                });
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
    pub fn work(&self) -> Work {
        self.work.clone()
    }
    pub fn spawner(&self, service: &str) -> Result<Spawner, String> {
        self.started
            .iter()
            .find(|i| self.plugins[**i].provides().contains(&service))
            .map(|i| self.work.plugin(self.plugins[*i].id()))
            .ok_or("provider_not_active".into())
    }
    pub async fn stop_plugin(&mut self, id: &str) -> Result<(), String> {
        let Some(position) = self
            .started
            .iter()
            .position(|i| self.plugins[*i].id() == id)
        else {
            return Ok(());
        };
        let i = self.started[position];
        if self.started.iter().any(|j| {
            *j != i
                && self.plugins[*j]
                    .requires()
                    .iter()
                    .any(|s| self.plugins[i].provides().contains(s))
        }) {
            return Err("provider_in_use".into());
        }
        self.started.remove(position);
        self.close_one(i).await
    }
    async fn close_one(&mut self, i: usize) -> Result<(), String> {
        self.work.close_plugin(self.plugins[i].id()).await;
        let result = self.plugins[i].close().await;
        for name in self.plugins[i].provides() {
            self.services.remove(name);
        }
        eprintln!(
            "Psi plugin closed: {} registrations={} tasks={}",
            self.plugins[i].id(),
            self.services.len(),
            self.work.count()
        );
        result.map_err(|error| format!("{}: {error}", self.plugins[i].id()))
    }
    pub async fn close(&mut self) -> Result<(), String> {
        let mut errors = Vec::new();
        while let Some(i) = self.started.pop() {
            if let Err(error) = self.close_one(i).await {
                errors.push(error);
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }
}
