mod api;
mod awake;
mod capture;
mod client;
mod compositor;
mod config;
mod fuzzy;
mod hardware;
mod history;
mod icons;
mod localsend;
mod nix;
mod notify;
mod power;
mod providers;
mod server;
mod service;
mod tailscale;
mod terminal;
mod types;

use anyhow::Result;
use clap::{Parser, Subcommand};
use config::Config;
use providers::Registry;
use serde_json::json;

#[derive(Parser)]
#[command(
    name = "epochoxide",
    version,
    about = "Fast desktop shell data provider"
)]
struct Cli {
    #[arg(long)]
    config: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Serve {
        #[arg(long)]
        socket: Option<String>,
    },
    Query {
        #[arg(long, value_delimiter = ',')]
        providers: Vec<String>,
        #[arg(long, default_value = "")]
        query: String,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        #[arg(long, default_value_t = false)]
        exact: bool,
        #[arg(long, default_value_t = false)]
        stream: bool,
    },
    Activate {
        #[arg(long)]
        provider: String,
        #[arg(long)]
        identifier: String,
        #[arg(long)]
        action: String,
        #[arg(long, default_value = "")]
        query: String,
        #[arg(long, default_value = "")]
        arguments: String,
    },
    ListProviders,
    /// Call a method on the Epoch API, e.g. `compositor.windows`.
    Api {
        /// Method name as `group.method`, or `api.describe` to list the contract.
        method: String,
        /// Parameters as a JSON object.
        #[arg(long, default_value = "{}")]
        params: String,
        /// Contract major version to assert against.
        #[arg(long)]
        version: Option<u32>,
    },
    Menu {
        name: String,
    },
    Subscribe {
        #[arg(long, value_delimiter = ',')]
        providers: Vec<String>,
    },
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
}

#[derive(Subcommand)]
enum ServiceAction {
    Install,
    Enable,
    Disable,
    Start,
    Stop,
    Restart,
    Status,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let config = Config::load(cli.config.as_deref())?;
    // Capture settings are read by both the daemon and the one-shot `api` command, which answers
    // in-process when no daemon is running, so they are installed before either path runs.
    capture::configure(&config);
    nix::configure(&config);
    hardware::configure(&config);

    match cli.command {
        Command::Serve { socket } => {
            let socket = socket.unwrap_or_else(|| config.socket.clone());
            let config_path = Config::resolved_path(cli.config.as_deref());
            // Accepting transfers means binding a port and answering discovery, so a failure here
            // is reported and stepped over rather than stopping the daemon: everything else still
            // works without it.
            localsend::configure(&config);
            // Checking for flake updates is a background job with a timer, so it only runs under
            // the daemon; the one-shot CLI answers from whatever the daemon last found.
            nix::watch();
            if config.localsend_receive {
                if let Err(err) = localsend::start_receiver() {
                    eprintln!("localsend: not accepting transfers: {err:#}");
                }
            }
            server::serve(&socket, config_path, move || Registry::new(config.clone()))
        }
        Command::Query {
            providers,
            query,
            limit,
            exact,
            stream,
        } => {
            if stream {
                match client::stream_query(&config.socket, &providers, &query, limit, exact) {
                    Ok(batches) => {
                        println!("{}", serde_json::to_string_pretty(&batches)?);
                        return Ok(());
                    }
                    Err(err) => eprintln!("stream query failed, falling back: {err}"),
                }
            }
            if let Some(response) = client::request(
                &config.socket,
                serde_json::json!({
                    "type": "query",
                    "providers": providers.clone(),
                    "query": query.clone(),
                    "limit": limit,
                    "exact": exact,
                }),
            )? {
                println!("{}", serde_json::to_string_pretty(&response)?);
                return Ok(());
            }
            let registry = Registry::new(config.clone())?;
            let items = registry.query(&providers, &query, limit, exact);
            println!("{}", serde_json::to_string_pretty(&items)?);
            Ok(())
        }
        Command::Activate {
            provider,
            identifier,
            action,
            query,
            arguments,
        } => {
            if let Some(response) = client::request(
                &config.socket,
                serde_json::json!({
                    "type": "activate",
                    "provider": provider.clone(),
                    "identifier": identifier.clone(),
                    "action": action.clone(),
                    "query": query.clone(),
                    "arguments": arguments.clone(),
                }),
            )? {
                println!("{}", serde_json::to_string_pretty(&response)?);
                return Ok(());
            }
            let registry = Registry::new(config.clone())?;
            registry.activate(&provider, &identifier, &action, &query, &arguments)?;
            println!("{}", json!({"ok": true}));
            Ok(())
        }
        Command::Api {
            method,
            params,
            version,
        } => {
            let params: serde_json::Value = serde_json::from_str(&params)
                .map_err(|err| anyhow::anyhow!("--params must be JSON: {err}"))?;
            // Prefer the warm daemon; fall back to answering in-process so the CLI still works
            // with no daemon running.
            // A streaming method never returns; print each payload as it arrives. It needs a
            // daemon -- there is nothing to hold the stream open in a one-shot CLI process.
            if api::is_streaming(&method) {
                match client::api_stream(&config.socket, &method, &params) {
                    Ok(payloads) => {
                        for payload in payloads {
                            println!("{}", serde_json::to_string(&payload?)?);
                        }
                        return Ok(());
                    }
                    Err(err) => {
                        anyhow::bail!("{method} needs a running daemon: {err}");
                    }
                }
            }

            let answered = client::request_envelope(
                &config.socket,
                serde_json::json!({
                    "type": "api",
                    "method": method.clone(),
                    "params": params.clone(),
                    "version": version,
                }),
            )?;
            let local = || match api::dispatch(&method, &params, version) {
                Ok(data) => (true, data),
                Err(err) => (false, err.to_value()),
            };
            let (ok, data) = match answered {
                Some(envelope) => {
                    let ok = envelope
                        .get("ok")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false);
                    let data = envelope
                        .get("data")
                        .cloned()
                        .unwrap_or(serde_json::Value::Null);
                    // A daemon predating the API rejects the request as unparseable rather than
                    // answering with a structured error, so its reply carries no `code`. That is
                    // version skew, not a real failure: answer in-process rather than reporting
                    // the daemon's parse error to the user.
                    if !ok && data.get("code").is_none() {
                        local()
                    } else {
                        (ok, data)
                    }
                }
                // No daemon at all: answer in-process so the CLI works on a cold machine.
                None => local(),
            };
            if ok {
                println!("{}", serde_json::to_string_pretty(&data)?);
                Ok(())
            } else {
                eprintln!("{}", serde_json::to_string_pretty(&data)?);
                std::process::exit(1);
            }
        }
        Command::ListProviders => {
            if let Some(response) =
                client::request(&config.socket, serde_json::json!({"type": "providers"}))?
            {
                println!("{}", serde_json::to_string_pretty(&response)?);
                return Ok(());
            }
            let registry = Registry::new(config.clone())?;
            println!("{}", serde_json::to_string_pretty(&registry.providers())?);
            Ok(())
        }
        Command::Menu { name } => {
            if let Some(response) = client::request(
                &config.socket,
                serde_json::json!({"type": "menu", "menu": name.clone()}),
            )? {
                println!("{}", serde_json::to_string_pretty(&response)?);
                return Ok(());
            }
            let registry = Registry::new(config.clone())?;
            let items = registry.menu(&name);
            println!("{}", serde_json::to_string_pretty(&items)?);
            Ok(())
        }
        Command::Subscribe { providers } => {
            match client::subscribe(&config.socket, &providers) {
                Ok(events) => {
                    for event in events {
                        let event = event?;
                        println!("{}", serde_json::to_string(&event)?);
                    }
                    return Ok(());
                }
                Err(_) => {
                    let registry = Registry::new(config.clone())?;
                    for event in registry.events() {
                        println!("{}", serde_json::to_string(&event)?);
                    }
                }
            }
            Ok(())
        }
        Command::Service { action } => match action {
            ServiceAction::Install => service::install(cli.config.as_deref(), &config),
            ServiceAction::Enable => service::systemctl(&["enable", "epochoxide.service"]),
            ServiceAction::Disable => service::systemctl(&["disable", "epochoxide.service"]),
            ServiceAction::Start => service::systemctl(&["start", "epochoxide.service"]),
            ServiceAction::Stop => service::systemctl(&["stop", "epochoxide.service"]),
            ServiceAction::Restart => service::systemctl(&["restart", "epochoxide.service"]),
            ServiceAction::Status => service::systemctl(&["status", "epochoxide.service"]),
        },
    }
}
