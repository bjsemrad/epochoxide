use crate::providers::Registry;
use anyhow::{Context, Result};
use notify::Watcher;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::net::{UnixListener, UnixStream},
    path::{Path, PathBuf},
    sync::{mpsc, Arc, Condvar, Mutex},
    thread,
    time::{Duration, Instant},
};

const STREAM_QUERY_BUDGET: Duration = Duration::from_millis(250);

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Request {
    Query {
        providers: Option<Vec<String>>,
        query: String,
        limit: Option<usize>,
        exact: Option<bool>,
        stream: Option<bool>,
    },
    Activate {
        provider: String,
        identifier: String,
        action: String,
        query: Option<String>,
        arguments: Option<String>,
    },
    Providers,
    Menu {
        menu: String,
    },
    Subscribe {
        providers: Option<Vec<String>>,
    },
}

#[derive(Debug, Serialize)]
struct Response<T: Serialize> {
    ok: bool,
    data: T,
    error: Option<String>,
}

pub fn serve(
    socket: &str,
    config_path: Option<PathBuf>,
    build: impl FnOnce() -> Result<Registry> + Send + 'static,
) -> Result<()> {
    let path = Path::new(socket);
    if path.exists() {
        fs::remove_file(path).with_context(|| format!("removing stale socket {socket}"))?;
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let listener = UnixListener::bind(path).with_context(|| format!("binding {socket}"))?;

    let startup = Arc::new(Startup {
        registry: Mutex::new(None),
        ready: Condvar::new(),
    });
    let spawn_startup = Arc::clone(&startup);
    thread::spawn(move || {
        let result = build();
        let mut slot = spawn_startup.registry.lock().unwrap();
        *slot = Some(result.map(Arc::new).map_err(|e| e.to_string()));
        spawn_startup.ready.notify_all();
    });

    if let Some(config_path) = config_path {
        let watch_startup = Arc::clone(&startup);
        thread::spawn(move || watch_config(config_path, watch_startup));
    }

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let startup = Arc::clone(&startup);
                thread::spawn(move || {
                    if let Err(err) = handle_client(stream, startup) {
                        eprintln!("client error: {err}");
                    }
                });
            }
            Err(err) => eprintln!("accept error: {err}"),
        }
    }
    Ok(())
}

struct Startup {
    registry: Mutex<Option<Result<Arc<Registry>, String>>>,
    ready: Condvar,
}

impl Startup {
    fn wait_registry(&self) -> Result<Arc<Registry>> {
        let mut slot = self.registry.lock().unwrap();
        loop {
            if let Some(ref result) = *slot {
                return match result {
                    Ok(registry) => Ok(Arc::clone(registry)),
                    Err(err) => anyhow::bail!("registry init failed: {err}"),
                };
            }
            slot = self.ready.wait(slot).unwrap();
        }
    }
}

fn watch_config(path: PathBuf, startup: Arc<Startup>) {
    let Ok(registry) = startup.wait_registry() else {
        return;
    };
    let Some(parent) = path.parent() else { return };
    let (tx, rx) = mpsc::channel();
    let Ok(mut watcher) = notify::recommended_watcher(tx) else {
        return;
    };
    if watcher
        .watch(parent, notify::RecursiveMode::NonRecursive)
        .is_err()
    {
        return;
    }
    for event in rx {
        let Ok(event) = event else { continue };
        if !event.paths.iter().any(|p| p == &path) {
            continue;
        }
        match crate::config::Config::load(path.to_str()) {
            Ok(config) => registry.reload_config(config),
            Err(err) => eprintln!("config reload failed: {err}"),
        }
    }
}

fn handle_client(mut stream: UnixStream, startup: Arc<Startup>) -> Result<()> {
    let reader = BufReader::new(stream.try_clone()?);
    let (tx, rx) = mpsc::channel::<Result<Request, String>>();
    thread::spawn(move || {
        for line in reader.lines() {
            let Ok(line) = line else { break };
            if line.trim().is_empty() {
                continue;
            }
            if tx
                .send(serde_json::from_str::<Request>(&line).map_err(|e| e.to_string()))
                .is_err()
            {
                break;
            }
        }
    });

    let mut lookahead = None;
    while let Some(mut request) = lookahead.take().or_else(|| rx.recv().ok()) {
        // A burst of Query requests (fast typing outrunning processing) collapses to the
        // newest one — the rest are dropped before doing any work. Other request kinds
        // (Activate, Menu, ...) are never dropped and always run in order.
        while matches!(request, Ok(Request::Query { .. })) {
            match rx.try_recv() {
                Ok(next @ Ok(Request::Query { .. })) => request = next,
                Ok(other) => {
                    lookahead = Some(other);
                    break;
                }
                Err(_) => break,
            }
        }
        let response = match request {
            Ok(Request::Query {
                providers,
                query,
                limit,
                exact,
                stream: Some(true),
            }) => {
                let registry = startup.wait_registry()?;
                let providers = providers.unwrap_or_default();
                let batches = registry.query_batches(
                    &providers,
                    &query,
                    limit.unwrap_or(20),
                    exact.unwrap_or(false),
                );
                let start = Instant::now();
                while let Some(remaining) = STREAM_QUERY_BUDGET.checked_sub(start.elapsed()) {
                    match batches.recv_timeout(remaining) {
                        Ok((provider, items)) => {
                            let response = Response {
                                ok: true,
                                data: json!({"type": "query_batch", "provider": provider, "items": items}),
                                error: None::<String>,
                            };
                            writeln!(stream, "{}", serde_json::to_string(&response)?)?;
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => break,
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
                serde_json::to_value(Response {
                    ok: true,
                    data: json!({"type": "done"}),
                    error: None::<String>,
                })?
            }
            Ok(Request::Query {
                providers,
                query,
                limit,
                exact,
                stream: _,
            }) => {
                let registry = startup.wait_registry()?;
                let providers = providers.unwrap_or_default();
                let data = registry.query(
                    &providers,
                    &query,
                    limit.unwrap_or(20),
                    exact.unwrap_or(false),
                );
                serde_json::to_value(Response {
                    ok: true,
                    data,
                    error: None::<String>,
                })?
            }
            Ok(Request::Activate {
                provider,
                identifier,
                action,
                query,
                arguments,
            }) => {
                let registry = startup.wait_registry()?;
                match registry.activate(
                    &provider,
                    &identifier,
                    &action,
                    query.as_deref().unwrap_or_default(),
                    arguments.as_deref().unwrap_or_default(),
                ) {
                    Ok(()) => serde_json::to_value(Response {
                        ok: true,
                        data: json!({}),
                        error: None::<String>,
                    })?,
                    Err(err) => serde_json::to_value(Response {
                        ok: false,
                        data: json!({}),
                        error: Some(err.to_string()),
                    })?,
                }
            }
            Ok(Request::Providers) => {
                let registry = startup.wait_registry()?;
                serde_json::to_value(Response {
                    ok: true,
                    data: registry.providers(),
                    error: None::<String>,
                })?
            }
            Ok(Request::Menu { menu }) => {
                let registry = startup.wait_registry()?;
                serde_json::to_value(Response {
                    ok: true,
                    data: registry.menu(&menu),
                    error: None::<String>,
                })?
            }
            Ok(Request::Subscribe { providers }) => {
                let registry = startup.wait_registry()?;
                let providers = providers.unwrap_or_default();
                subscribe(&mut stream, registry, &providers)?;
                return Ok(());
            }
            Err(err) => serde_json::to_value(Response {
                ok: false,
                data: json!({}),
                error: Some(err),
            })?,
        };
        writeln!(stream, "{}", serde_json::to_string(&response)?)?;
    }
    Ok(())
}

fn subscribe(stream: &mut UnixStream, registry: Arc<Registry>, providers: &[String]) -> Result<()> {
    let initial = Response {
        ok: true,
        data: json!({"type": "subscribed", "providers": providers}),
        error: None::<String>,
    };
    writeln!(stream, "{}", serde_json::to_string(&initial)?)?;
    loop {
        let events = registry
            .events()
            .into_iter()
            .filter(|event| {
                providers.is_empty()
                    || event
                        .get("provider")
                        .and_then(|v| v.as_str())
                        .map(|p| providers.iter().any(|wanted| wanted == p))
                        .unwrap_or(false)
            })
            .collect::<Vec<_>>();
        for event in events {
            let response = Response {
                ok: true,
                data: json!({"type": "event", "event": event}),
                error: None::<String>,
            };
            writeln!(stream, "{}", serde_json::to_string(&response)?)?;
        }
        stream.flush()?;
        thread::sleep(Duration::from_millis(250));
    }
}
