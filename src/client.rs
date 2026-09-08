use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

pub struct StreamClient {
    writer: UnixStream,
    reader: BufReader<UnixStream>,
}

impl StreamClient {
    pub fn connect(socket: &str) -> Result<Self> {
        let stream = UnixStream::connect(socket).context("connecting to daemon socket")?;
        stream.set_write_timeout(Some(Duration::from_millis(1500)))?;
        stream.set_read_timeout(Some(Duration::from_millis(1500)))?;
        let reader = BufReader::new(stream.try_clone()?);
        Ok(Self {
            writer: stream,
            reader,
        })
    }

    pub fn send(&mut self, payload: &Value) -> Result<()> {
        writeln!(self.writer, "{}", serde_json::to_string(payload)?)?;
        Ok(())
    }

    pub fn next(&mut self) -> Result<Option<Value>> {
        loop {
            let mut line = String::new();
            match self.reader.read_line(&mut line) {
                Ok(0) => return Ok(None),
                Ok(_) => {
                    if line.trim().is_empty() {
                        return Ok(None);
                    }
                    let value: Value = serde_json::from_str(line.trim())?;
                    if value.get("ok").and_then(|v| v.as_bool()) == Some(false) {
                        let error = value
                            .get("error")
                            .and_then(|v| v.as_str())
                            .unwrap_or("daemon request failed");
                        bail!(error.to_string());
                    }
                    return Ok(Some(value.get("data").cloned().unwrap_or(value)));
                }
                Err(err)
                    if err.kind() == ErrorKind::WouldBlock || err.kind() == ErrorKind::TimedOut =>
                {
                    continue
                }
                Err(err) => return Err(err).context("reading daemon response"),
            }
        }
    }
}

pub fn request(socket: &str, payload: Value) -> Result<Option<Value>> {
    let mut client = match StreamClient::connect(socket) {
        Ok(client) => client,
        Err(_) => return Ok(None),
    };
    client.send(&payload)?;
    client.next()
}

pub fn stream_query(
    socket: &str,
    providers: &[String],
    query: &str,
    limit: usize,
    exact: bool,
) -> Result<Vec<(String, Vec<Value>)>> {
    let mut client = StreamClient::connect(socket)?;
    client.send(&serde_json::json!({
        "type": "query",
        "providers": providers,
        "query": query,
        "limit": limit,
        "exact": exact,
        "stream": true,
    }))?;
    let mut batches = Vec::new();
    while let Some(data) = client.next()? {
        match data.get("type").and_then(|v| v.as_str()) {
            Some("query_batch") => {
                let provider = data
                    .get("provider")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                let items = data
                    .get("items")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                batches.push((provider, items));
            }
            Some("done") => break,
            _ => {}
        }
    }
    Ok(batches)
}

pub fn subscribe(
    socket: &str,
    providers: &[String],
) -> Result<impl Iterator<Item = Result<Value>>> {
    let mut client = StreamClient::connect(socket)?;
    client.send(&serde_json::json!({ "type": "subscribe", "providers": providers }))?;
    let initial = client.next()?;
    let _ = initial;
    Ok(std::iter::from_fn(move || match client.next() {
        Ok(Some(data)) => Some(Ok(data)),
        Ok(None) => None,
        Err(err) => Some(Err(err)),
    }))
}
