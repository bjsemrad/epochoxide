use crate::config::Config;
use anyhow::{Context, Result};
use std::{fs, path::PathBuf, process::Command};

const UNIT_NAME: &str = "epochoxide.service";

pub fn install(config_arg: Option<&str>, config: &Config) -> Result<()> {
    let dir = dirs::config_dir()
        .context("could not determine config directory")?
        .join("systemd/user");
    fs::create_dir_all(&dir)?;

    let unit_path = dir.join(UNIT_NAME);
    let exe = std::env::current_exe().context("could not determine current executable")?;
    let mut args = vec![quote_systemd_arg(&exe.display().to_string())];
    if let Some(config_arg) = config_arg {
        args.push("--config".to_string());
        args.push(quote_systemd_arg(config_arg));
    }
    args.push("serve".to_string());
    args.push("--socket".to_string());
    args.push(quote_systemd_arg(&config.socket));

    let unit = format!(
        "[Unit]\nDescription=EpochOxide desktop shell data provider\nDocumentation=file://{readme}\nAfter=graphical-session.target\nPartOf=graphical-session.target\n\n[Service]\nType=simple\nExecStart={exec}\nRestart=on-failure\nRestartSec=1\nEnvironment=RUST_LOG=info\n\n[Install]\nWantedBy=graphical-session.target\n",
        readme = std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")).join("README.md").display(),
        exec = args.join(" "),
    );

    fs::write(&unit_path, unit).with_context(|| format!("writing {}", unit_path.display()))?;
    systemctl(&["daemon-reload"])?;
    println!("installed {}", unit_path.display());
    println!("run: systemctl --user enable --now {UNIT_NAME}");
    Ok(())
}

pub fn systemctl(args: &[&str]) -> Result<()> {
    let status = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .status()
        .with_context(|| format!("running systemctl --user {}", args.join(" ")))?;
    if !status.success() {
        anyhow::bail!("systemctl --user {} failed", args.join(" "));
    }
    Ok(())
}

fn quote_systemd_arg(arg: &str) -> String {
    if arg
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'_' | b'-' | b':' | b'@'))
    {
        arg.to_string()
    } else {
        format!("\"{}\"", arg.replace('\\', "\\\\").replace('"', "\\\""))
    }
}
