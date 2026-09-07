use crate::providers::Registry;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{fs, io::{BufRead, BufReader, Write}, os::unix::net::{UnixListener, UnixStream}, path::Path};

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Request {
    Query { providers: Option<Vec<String>>, query: String, limit: Option<usize>, exact: Option<bool> },
    Activate { provider: String, identifier: String, action: String, query: Option<String>, arguments: Option<String> },
    Providers,
    Menu { menu: String },
}

#[derive(Debug, Serialize)]
struct Response<T: Serialize> {
    ok: bool,
    data: T,
    error: Option<String>,
}

pub fn serve(socket: &str, mut registry: Registry) -> Result<()> {
    let path = Path::new(socket);
    if path.exists() { fs::remove_file(path).with_context(|| format!("removing stale socket {socket}"))?; }
    if let Some(parent) = path.parent() { fs::create_dir_all(parent)?; }
    let listener = UnixListener::bind(path).with_context(|| format!("binding {socket}"))?;
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => handle_client(stream, &mut registry)?,
            Err(err) => eprintln!("accept error: {err}"),
        }
    }
    Ok(())
}

fn handle_client(mut stream: UnixStream, registry: &mut Registry) -> Result<()> {
    let reader = BufReader::new(stream.try_clone()?);
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() { continue; }
        let response = match serde_json::from_str::<Request>(&line) {
            Ok(Request::Query { providers, query, limit, exact }) => {
                let providers = providers.unwrap_or_default();
                let data = registry.query(&providers, &query, limit.unwrap_or(20), exact.unwrap_or(false));
                serde_json::to_value(Response { ok: true, data, error: None::<String> })?
            }
            Ok(Request::Activate { provider, identifier, action, query, arguments }) => {
                match registry.activate(&provider, &identifier, &action, query.as_deref().unwrap_or_default(), arguments.as_deref().unwrap_or_default()) {
                    Ok(()) => serde_json::to_value(Response { ok: true, data: json!({}), error: None::<String> })?,
                    Err(err) => serde_json::to_value(Response { ok: false, data: json!({}), error: Some(err.to_string()) })?,
                }
            }
            Ok(Request::Providers) => serde_json::to_value(Response { ok: true, data: registry.providers(), error: None::<String> })?,
            Ok(Request::Menu { menu }) => serde_json::to_value(Response { ok: true, data: registry.menu(&menu), error: None::<String> })?,
            Err(err) => serde_json::to_value(Response { ok: false, data: json!({}), error: Some(err.to_string()) })?,
        };
        writeln!(stream, "{}", serde_json::to_string(&response)?)?;
    }
    Ok(())
}
