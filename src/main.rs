mod client;
mod config;
mod fuzzy;
mod history;
mod icons;
mod providers;
mod service;
mod server;
mod types;

use anyhow::Result;
use clap::{Parser, Subcommand};
use config::Config;
use providers::Registry;
use serde_json::json;

#[derive(Parser)]
#[command(name = "epochoxide", version, about = "Fast desktop shell data provider")]
struct Cli {
    #[arg(long)]
    config: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Serve { #[arg(long)] socket: Option<String> },
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
    Menu { name: String },
    Subscribe {
        #[arg(long, value_delimiter = ',')]
        providers: Vec<String>,
    },
    Service { #[command(subcommand)] action: ServiceAction },
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

    match cli.command {
        Command::Serve { socket } => {
            let socket = socket.unwrap_or_else(|| config.socket.clone());
            let config_path = Config::resolved_path(cli.config.as_deref());
            server::serve(&socket, config_path, move || Registry::new(config.clone()))
        }
        Command::Query { providers, query, limit, exact, stream } => {
            if stream {
                match client::stream_query(&config.socket, &providers, &query, limit, exact) {
                    Ok(batches) => {
                        println!("{}", serde_json::to_string_pretty(&batches)?);
                        return Ok(());
                    }
                    Err(err) => eprintln!("stream query failed, falling back: {err}"),
                }
            }
            if let Some(response) = client::request(&config.socket, serde_json::json!({
                "type": "query",
                "providers": providers.clone(),
                "query": query.clone(),
                "limit": limit,
                "exact": exact,
            }))? {
                println!("{}", serde_json::to_string_pretty(&response)?);
                return Ok(());
            }
            let registry = Registry::new(config.clone())?;
            let items = registry.query(&providers, &query, limit, exact);
            println!("{}", serde_json::to_string_pretty(&items)?);
            Ok(())
        }
        Command::Activate { provider, identifier, action, query, arguments } => {
            if let Some(response) = client::request(&config.socket, serde_json::json!({
                "type": "activate",
                "provider": provider.clone(),
                "identifier": identifier.clone(),
                "action": action.clone(),
                "query": query.clone(),
                "arguments": arguments.clone(),
            }))? {
                println!("{}", serde_json::to_string_pretty(&response)?);
                return Ok(());
            }
            let registry = Registry::new(config.clone())?;
            registry.activate(&provider, &identifier, &action, &query, &arguments)?;
            println!("{}", json!({"ok": true}));
            Ok(())
        }
        Command::ListProviders => {
            if let Some(response) = client::request(&config.socket, serde_json::json!({"type": "providers"}))? {
                println!("{}", serde_json::to_string_pretty(&response)?);
                return Ok(());
            }
            let registry = Registry::new(config.clone())?;
            println!("{}", serde_json::to_string_pretty(&registry.providers())?);
            Ok(())
        }
        Command::Menu { name } => {
            if let Some(response) = client::request(&config.socket, serde_json::json!({"type": "menu", "menu": name.clone()}))? {
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
