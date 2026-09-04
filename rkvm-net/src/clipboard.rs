use serde::{Deserialize, Serialize};
use std::collections::hash_map::DefaultHasher;
use std::fmt::{self, Debug, Formatter};
use std::future;
use std::hash::{Hash, Hasher};
use std::io::{Error, ErrorKind};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::mpsc::{self, Receiver, Sender};
use tokio::time;

pub const MAX_SIZE: usize = 4 * 1024 * 1024;

const MAX_MIME_LENGTH: usize = 255;
const POLL_INTERVAL: Duration = Duration::from_secs(1);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
const PREFERRED: &[&str] = &["image/png", "text/plain;charset=utf-8", "text/plain"];

#[derive(Deserialize, Serialize, Clone, PartialEq, Eq)]
pub struct Data {
    pub mime: String,
    pub data: Vec<u8>,
}

impl Debug for Data {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({} bytes)", self.mime, self.data.len())
    }
}

#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "kebab-case")]
pub struct Config {
    pub list_types: String,
    pub read: String,
    pub write: String,
}

// Split in two so that reading changes and applying them can happen in different
// branches of the same select.
pub struct Changes(Option<Receiver<Data>>);

pub struct Applier(Option<Sender<Data>>);

pub fn new(config: Option<Config>) -> (Changes, Applier) {
    let config = match config {
        Some(config) => config,
        None => return (Changes(None), Applier(None)),
    };

    let (changed_sender, changed) = mpsc::channel(1);
    let (apply, apply_receiver) = mpsc::channel(1);

    tokio::spawn(run(config, changed_sender, apply_receiver));

    (Changes(Some(changed)), Applier(Some(apply)))
}

impl Changes {
    pub async fn next(&mut self) -> Data {
        match &mut self.0 {
            Some(changed) => match changed.recv().await {
                Some(data) => data,
                None => future::pending().await,
            },
            None => future::pending().await,
        }
    }

    pub fn try_next(&mut self) -> Option<Data> {
        self.0.as_mut().and_then(|changed| changed.try_recv().ok())
    }
}

impl Applier {
    pub fn apply(&self, data: Data) {
        if let Some(apply) = &self.0 {
            let _ = apply.try_send(data);
        }
    }
}

async fn run(config: Config, changed: Sender<Data>, mut apply: Receiver<Data>) {
    let mut interval = time::interval(POLL_INTERVAL);
    let mut last = None;

    loop {
        tokio::select! {
            _ = interval.tick() => {
                let data = match read(&config).await {
                    Ok(Some(data)) => data,
                    Ok(None) => continue,
                    Err(err) => {
                        tracing::debug!("Failed to read clipboard: {}", err);
                        continue;
                    }
                };

                let hash = hash(&data);
                if last == Some(hash) {
                    continue;
                }

                last = Some(hash);
                tracing::debug!(data = ?data, "Clipboard changed");

                if changed.send(data).await.is_err() {
                    break;
                }
            }
            data = apply.recv() => {
                let data = match data {
                    Some(data) => data,
                    None => break,
                };

                if data.mime.len() > MAX_MIME_LENGTH || data.data.len() > MAX_SIZE {
                    tracing::warn!("Ignoring oversized clipboard contents");
                    continue;
                }

                let hash = hash(&data);
                if last == Some(hash) {
                    continue;
                }

                last = Some(hash);
                tracing::debug!(data = ?data, "Applying clipboard");

                if let Err(err) = write(&config, &data).await {
                    tracing::warn!("Failed to write clipboard: {}", err);
                }
            }
        }
    }
}

async fn read(config: &Config) -> Result<Option<Data>, Error> {
    let types = command(&config.list_types, None).await?;
    let types = String::from_utf8_lossy(&types).into_owned();

    let mime = match pick(&types) {
        Some(mime) => mime,
        None => return Ok(None),
    };

    let data = command(&config.read.replace("{type}", &quote(mime)), None).await?;
    if data.is_empty() {
        return Ok(None);
    }

    if data.len() > MAX_SIZE {
        tracing::debug!(mime = %mime, size = data.len(), "Clipboard contents too large to share");
        return Ok(None);
    }

    Ok(Some(Data {
        mime: mime.to_owned(),
        data,
    }))
}

async fn write(config: &Config, data: &Data) -> Result<(), Error> {
    let command = config.write.replace("{type}", &quote(&data.mime));
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(&command)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    let mut stdin = child.stdin.take().unwrap();
    let result = time::timeout(COMMAND_TIMEOUT, async {
        stdin.write_all(&data.data).await?;
        drop(stdin);

        child.wait().await
    })
    .await;

    match result {
        Ok(result) => result.map(drop),
        Err(_) => Err(Error::new(ErrorKind::TimedOut, "Clipboard command timed out")),
    }
}

async fn command(command: &str, input: Option<&[u8]>) -> Result<Vec<u8>, Error> {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(command)
        .stdin(match input {
            Some(_) => Stdio::piped(),
            None => Stdio::null(),
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;

    if let Some(input) = input {
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(input).await?;
    }

    let output = match time::timeout(COMMAND_TIMEOUT, child.wait_with_output()).await {
        Ok(output) => output?,
        Err(_) => return Err(Error::new(ErrorKind::TimedOut, "Clipboard command timed out")),
    };

    match output.status.success() {
        true => Ok(output.stdout),
        false => Err(Error::new(
            ErrorKind::Other,
            format!("Clipboard command failed: {}", output.status),
        )),
    }
}

// X11 and Wayland both report pseudo targets such as TIMESTAMP alongside real types.
fn pick(types: &str) -> Option<&str> {
    let types = || {
        types
            .lines()
            .map(str::trim)
            .filter(|line| line.contains('/'))
    };

    for preferred in PREFERRED {
        if let Some(mime) = types().find(|mime| mime.eq_ignore_ascii_case(preferred)) {
            return Some(mime);
        }
    }

    types()
        .find(|mime| mime.starts_with("image/"))
        .or_else(|| types().next())
}

fn quote(data: &str) -> String {
    format!("'{}'", data.replace('\'', r"'\''"))
}

fn hash(data: &Data) -> u64 {
    let mut hasher = DefaultHasher::new();
    data.mime.hash(&mut hasher);
    data.data.hash(&mut hasher);

    hasher.finish()
}
