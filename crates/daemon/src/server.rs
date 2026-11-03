//! Daemon life cycle: claim the endpoint, read config, open the store, register modules,
//! migrate, recover, start, serve, stop. Generic over the transport and the system layer,
//! which the platform crate supplies.

use std::fmt;
use std::future::Future;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread::JoinHandle;

use proto::commands::module::CORE;
use proto::names;
use serde_json::json;
use tokio::sync::oneshot;

use crate::access::TrustPolicy;
use crate::bus::Bus;
use crate::config::Config;
use crate::dispatch::{CORE_SPEC, Dispatcher, daemon_info};
use crate::dlog;
use crate::module::{Event, Module};
use crate::registry::Registry;
use crate::sqlite::SqliteLibrary;
use crate::store::{Store, now_ms};
use crate::sys::Sys;
use crate::transport::{BindError, Transport, serve};

/// Everything the daemon needs from outside. The platform crate builds the real one
/// (`platform::daemon_config`); tests swap in fakes.
#[derive(Clone)]
pub struct DaemonConfig {
    /// Pipe name, socket path … whatever the transport understands.
    pub endpoint: String,
    /// Holds the database.
    pub data_dir: PathBuf,
    /// The JSON config file (module switches). Its location is platform convention.
    pub config_file: PathBuf,
    pub sys: Sys,
    pub sqlite: Arc<dyn SqliteLibrary>,
    pub trust: TrustPolicy,
}

#[derive(Debug)]
pub enum DaemonError {
    AlreadyRunning(String),
    Endpoint(io::Error),
    DataDir(io::Error),
    Store(String),
    Registry(String),
    Runtime(String),
}

impl fmt::Display for DaemonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DaemonError::AlreadyRunning(ep) => write!(f, "another daemon already owns {ep}"),
            DaemonError::Endpoint(e) => write!(f, "endpoint: {e}"),
            DaemonError::DataDir(e) => write!(f, "data folder: {e}"),
            DaemonError::Store(e) => write!(f, "database: {e}"),
            DaemonError::Registry(e) => write!(f, "modules: {e}"),
            DaemonError::Runtime(e) => write!(f, "runtime: {e}"),
        }
    }
}

impl std::error::Error for DaemonError {}

/// Synchronous part of startup. The endpoint must already be claimed so two daemons never
/// recover concurrently.
///
/// A broken config file does not stop startup: the database is opened and recovery runs as
/// usual, then no module is started and `core.status` reports the error. Starting modules
/// with default switches instead could switch on a module the user had switched off.
pub fn start(config: &DaemonConfig, modules: Vec<Box<dyn Module>>) -> Result<Arc<Dispatcher>, DaemonError> {
    std::fs::create_dir_all(&config.data_dir).map_err(DaemonError::DataDir)?;
    let settings = Config::load(&config.config_file);
    if let Err(e) = &settings {
        dlog!("ERROR", "config file is broken, no module will be started (fix it and restart): {e}");
    }
    let store = Arc::new(
        Store::open(&config.data_dir.join(names::DB_FILE), config.sqlite.as_ref())
            .map_err(|e| DaemonError::Store(e.to_string()))?,
    );
    let bus = Bus::new();
    let registry = Registry::new(&CORE_SPEC, modules, store.clone(), bus.clone(), config.sys.clone(), settings)
        .map_err(DaemonError::Registry)?;
    for w in registry.config_warnings() {
        dlog!("WARN", "{w}");
    }
    registry.migrate_all();
    let recovery = registry.recover();
    if recovery != Default::default() {
        dlog!(
            "INFO",
            "recovery: {} restored, {} adopted, {} failed, {} orphaned",
            recovery.restored,
            recovery.adopted,
            recovery.failed,
            recovery.orphaned
        );
        for e in &recovery.errors {
            dlog!("WARN", "recovery: {e}");
        }
    }
    registry.start_all();
    bus.publish(Event { topic: format!("{CORE}.started"), source: CORE.into(), ts: now_ms(), data: json!({}) });
    let info = daemon_info(&config.endpoint, config.data_dir.clone());
    Ok(Arc::new(Dispatcher::new(registry, store, info, recovery, config.trust.clone(), config.sys.system.clone())))
}

async fn run_inner<T: Transport>(
    transport: T,
    config: DaemonConfig,
    modules: Vec<Box<dyn Module>>,
    shutdown: impl Future<Output = ()>,
    on_ready: impl FnOnce(Option<Arc<Dispatcher>>),
) -> Result<(), DaemonError> {
    let setup = (|| {
        let listener = transport.bind(&config.endpoint).map_err(|e| match e {
            BindError::AlreadyRunning => DaemonError::AlreadyRunning(config.endpoint.clone()),
            BindError::Io(e) => DaemonError::Endpoint(e),
        })?;
        let dispatcher = start(&config, modules)?;
        Ok::<_, DaemonError>((listener, dispatcher))
    })();
    let (listener, dispatcher) = match setup {
        Ok(v) => v,
        Err(e) => {
            on_ready(None);
            return Err(e);
        }
    };
    on_ready(Some(dispatcher.clone()));
    serve(listener, dispatcher.clone(), shutdown).await;
    dispatcher.shutdown();
    Ok(())
}

/// Runs the daemon until `shutdown` resolves. Must be called inside a tokio runtime with
/// I/O enabled.
pub async fn run<T: Transport>(
    transport: T,
    config: DaemonConfig,
    modules: Vec<Box<dyn Module>>,
    shutdown: impl Future<Output = ()>,
) -> Result<(), DaemonError> {
    let endpoint = config.endpoint.clone();
    let data = config.data_dir.clone();
    run_inner(transport, config, modules, shutdown, |ready| {
        if ready.is_some() {
            dlog!("INFO", "listening on {endpoint}, data in {}", data.display());
        }
    })
    .await
}

/// A daemon running on its own thread and runtime, for tests and embedding.
pub struct BackgroundDaemon {
    dispatcher: Arc<Dispatcher>,
    stop: Option<oneshot::Sender<()>>,
    thread: Option<JoinHandle<Result<(), DaemonError>>>,
}

/// Starts a daemon in the background and returns once it accepts connections.
pub fn spawn<T: Transport>(
    transport: T,
    config: DaemonConfig,
    modules: Vec<Box<dyn Module>>,
) -> Result<BackgroundDaemon, DaemonError> {
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (stop_tx, stop_rx) = oneshot::channel::<()>();
    let thread = std::thread::Builder::new()
        .name("daemon".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| DaemonError::Runtime(e.to_string()))?;
            rt.block_on(run_inner(
                transport,
                config,
                modules,
                async {
                    let _ = stop_rx.await;
                },
                move |ready| {
                    let _ = ready_tx.send(ready);
                },
            ))
        })
        .map_err(|e| DaemonError::Runtime(e.to_string()))?;
    match ready_rx.recv() {
        Ok(Some(dispatcher)) => Ok(BackgroundDaemon { dispatcher, stop: Some(stop_tx), thread: Some(thread) }),
        _ => match thread.join() {
            Ok(Err(e)) => Err(e),
            Ok(Ok(())) => Err(DaemonError::Runtime("daemon exited during startup".into())),
            Err(_) => Err(DaemonError::Runtime("daemon thread panicked during startup".into())),
        },
    }
}

impl BackgroundDaemon {
    pub fn dispatcher(&self) -> &Arc<Dispatcher> {
        &self.dispatcher
    }

    /// Stops serving, stops the modules and waits for the thread.
    pub fn stop(mut self) -> Result<(), DaemonError> {
        self.stop_inner()
    }

    fn stop_inner(&mut self) -> Result<(), DaemonError> {
        if let Some(tx) = self.stop.take() {
            let _ = tx.send(());
        }
        match self.thread.take().map(JoinHandle::join) {
            Some(Ok(r)) => r,
            Some(Err(_)) => Err(DaemonError::Runtime("daemon thread panicked".into())),
            None => Ok(()),
        }
    }
}

impl Drop for BackgroundDaemon {
    fn drop(&mut self) {
        let _ = self.stop_inner();
    }
}
