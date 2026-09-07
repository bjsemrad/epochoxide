use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::{io::{BufRead, BufReader, Write}, os::unix::net::UnixStream, time::Duration};

pub fn request(socket: &str, payload: Value) -> Result<Option<Value>> {
    let stream = match UnixStream::connect(socket) {
        Ok(stream) => stream,
        Err(_) => return Ok(None),
    };
    stream.set_read_timeout(Some(Duration::from_millis(1500)))?;
    stream.set_write_timeout(Some(Duration::from_millis(1500)))?;

    let mut writer = stream.try_clone()?;
    writeln!(writer, "{}", serde_json::to_string(&payload)?)?;

    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).context("reading daemon response")?;
    if line.trim().is_empty() {
        return Ok(None);
    }
    let value: Value = serde_json::from_str(line.trim())?;
    if value.get("ok").and_then(|v| v.as_bool()) == Some(false) {
        let error = value.get("error").and_then(|v| v.as_str()).unwrap_or("daemon request failed");
        bail!(error.to_string());
    }
    Ok(Some(value.get("data").cloned().unwrap_or(value)))
}
