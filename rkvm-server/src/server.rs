use crate::config::{Config, Indicator};

use rkvm_input::abs::{AbsAxis, AbsInfo};
use rkvm_input::event::Event;
use rkvm_input::key::{Key, KeyEvent};
use rkvm_input::monitor::Monitor;
use rkvm_input::rel::RelAxis;
use rkvm_input::sync::SyncEvent;
use rkvm_net::auth::{AuthChallenge, AuthResponse, AuthStatus};
use rkvm_net::clipboard;
use rkvm_net::message::Message;
use rkvm_net::version::Version;
use rkvm_net::{Pong, Update};
use slab::Slab;
use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::io::{self, ErrorKind};
use std::net::SocketAddr;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};
use thiserror::Error;
use tokio::io::{AsyncWriteExt, BufStream};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::Command;
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::mpsc::{self, Receiver, Sender};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time;
use tokio_rustls::TlsAcceptor;
use tracing::Instrument;

const MAX_NAME_LENGTH: usize = 64;

// Connections waiting to authenticate, so that a flood of them can't pile up.
const MAX_PENDING: usize = 16;

#[derive(Error, Debug)]
pub enum Error {
    #[error("Network error: {0}")]
    Network(io::Error),
    #[error("Input error: {0}")]
    Input(io::Error),
    #[error("Event queue overflow")]
    Overflow,
    #[error("Config error: {0}")]
    Config(&'static str),
}

enum Target {
    Cycle,
    Server,
    Client(String),
}

struct Shortcut {
    keys: HashSet<Key>,
    target: Target,
}

struct Client {
    sender: Sender<Update>,
    addr: SocketAddr,
    name: Option<String>,
}

impl Client {
    fn label(&self) -> String {
        self.name.clone().unwrap_or_else(|| self.addr.to_string())
    }
}

pub async fn run(config: &Config, acceptor: TlsAcceptor) -> Result<(), Error> {
    let propagate_switch_keys = config.propagate_switch_keys.unwrap_or(true);
    let password = &config.password;

    let mut shortcuts = vec![Shortcut {
        keys: config.switch_keys.iter().copied().map(Into::into).collect(),
        target: Target::Cycle,
    }];

    for (name, keys) in &config.switch_to {
        let target = match name.as_str() {
            "server" => Target::Server,
            name => Target::Client(name.to_owned()),
        };

        shortcuts.push(Shortcut {
            keys: keys.iter().copied().map(Into::into).collect(),
            target,
        });
    }

    if shortcuts.iter().any(|shortcut| shortcut.keys.is_empty()) {
        return Err(Error::Config("A switch key combination is empty"));
    }

    // A longer combination has to win over a shorter one it contains.
    shortcuts.sort_by_key(|shortcut| std::cmp::Reverse(shortcut.keys.len()));

    let switch_keys = shortcuts
        .iter()
        .flat_map(|shortcut| shortcut.keys.iter().copied())
        .collect::<HashSet<_>>();

    let listener = TcpListener::bind(&config.listen).await.map_err(Error::Network)?;
    let local_addr = listener.local_addr().map_err(Error::Network)?;

    tracing::info!("Listening on {}", local_addr);

    let (mut clipboard_changes, clipboard) = clipboard::new(config.clipboard.clone());
    let reply_timeout = match config.clipboard {
        Some(_) => rkvm_net::CLIPBOARD_TIMEOUT,
        None => rkvm_net::READ_TIMEOUT,
    };

    let mut monitor = Monitor::new(config.ignore_devices.clone());
    let mut devices = Slab::<Device>::new();
    let mut clients = Slab::<Client>::new();
    let mut current = 0;
    let mut previous = 0;
    let mut changed = false;
    let mut pressed_keys = HashMap::new();
    let mut deferred = Vec::new();
    let mut propagated = HashMap::new();
    let mut suppressed = false;
    let mut clipboard_contents = None;

    let pending = Arc::new(Semaphore::new(MAX_PENDING));

    let (events_sender, mut events_receiver) = mpsc::channel(1);
    let (authenticated_sender, mut authenticated_receiver) = mpsc::channel(1);
    let (clipboard_sender, mut clipboard_receiver) = mpsc::channel::<clipboard::Data>(1);

    loop {
        let event = async { events_receiver.recv().await.unwrap() };
        let authenticated = async { authenticated_receiver.recv().await.unwrap() };
        let shared = async { clipboard_receiver.recv().await.unwrap() };

        tokio::select! {
            data = clipboard_changes.next() => {
                for (_, client) in &clients {
                    let _ = client.sender.try_send(Update::Clipboard(data.clone()));
                }

                clipboard_contents = Some(data);
            }
            data = shared => {
                clipboard.apply(data.clone());

                for (_, client) in &clients {
                    let _ = client.sender.try_send(Update::Clipboard(data.clone()));
                }

                clipboard_contents = Some(data);
            }
            result = listener.accept() => {
                let (stream, addr) = result.map_err(Error::Network)?;

                let permit = match pending.clone().try_acquire_owned() {
                    Ok(permit) => permit,
                    Err(_) => {
                        tracing::warn!(addr = %addr, "Too many connections waiting to authenticate");
                        continue;
                    }
                };

                let acceptor = acceptor.clone();
                let password = password.to_owned();
                let authenticated_sender = authenticated_sender.clone();
                let clipboard_sender = clipboard_sender.clone();

                let (sender, receiver) = mpsc::channel(1);

                let span = tracing::info_span!("connection", addr = %addr);
                tokio::spawn(
                    async move {
                        tracing::info!("Connected");

                        let result = client(
                            sender,
                            addr,
                            authenticated_sender,
                            clipboard_sender,
                            receiver,
                            stream,
                            acceptor,
                            &password,
                            reply_timeout,
                            permit,
                        )
                        .await;

                        match result {
                            Ok(()) => tracing::info!("Disconnected"),
                            Err(err) => tracing::error!("Disconnected: {}", err),
                        }
                    }
                    .instrument(span),
                );
            }
            (sender, addr, name) = authenticated => {
                // Remove dead clients.
                clients.retain(|_, client| !client.sender.is_closed());
                if current != 0 && !clients.contains(current - 1) {
                    current = 0;
                }

                let mut alive = true;
                for (id, device) in &devices {
                    let update = Update::CreateDevice {
                        id,
                        name: device.name.clone(),
                        version: device.version,
                        vendor: device.vendor,
                        product: device.product,
                        rel: device.rel.clone(),
                        abs: device.abs.clone(),
                        keys: device.keys.clone(),
                        delay: device.delay,
                        period: device.period,
                    };

                    if sender.send(update).await.is_err() {
                        alive = false;
                        break;
                    }
                }

                if let Some(data) = alive.then_some(clipboard_contents.clone()).flatten() {
                    let _ = sender.try_send(Update::Clipboard(data));
                }

                if alive {
                    let client = Client { sender, addr, name };

                    if client.name.is_some()
                        && clients.iter().any(|(_, other)| other.name == client.name)
                    {
                        tracing::warn!(
                            name = ?client.name,
                            "Another client uses this name, switching to it is ambiguous"
                        );
                    }
                    tracing::info!(addr = %addr, name = ?client.name, "Registered client");

                    clients.insert(client);
                }
            }
            result = monitor.read() => {
                let mut interceptor = result.map_err(Error::Input)?;

                let name = interceptor.name().to_owned();
                let id = devices.vacant_key();
                let version = interceptor.version();
                let vendor = interceptor.vendor();
                let product = interceptor.product();
                let rel = interceptor.rel().collect::<HashSet<_>>();
                let abs = interceptor.abs().collect::<HashMap<_,_>>();
                let keys = interceptor.key().collect::<HashSet<_>>();
                let repeat = interceptor.repeat();

                for (_, client) in &clients {
                    let update = Update::CreateDevice {
                        id,
                        name: name.clone(),
                        version: version.clone(),
                        vendor: vendor.clone(),
                        product: product.clone(),
                        rel: rel.clone(),
                        abs: abs.clone(),
                        keys: keys.clone(),
                        delay: repeat.delay,
                        period: repeat.period,
                    };

                    let _ = client.sender.send(update).await;
                }

                let (interceptor_sender, mut interceptor_receiver) = mpsc::channel::<DeviceCommand>(32);
                devices.insert(Device {
                    name,
                    version,
                    vendor,
                    product,
                    rel,
                    abs,
                    keys,
                    delay: repeat.delay,
                    period: repeat.period,
                    sender: interceptor_sender,
                });

                let events_sender = events_sender.clone();
                tokio::spawn(async move {
                    loop {
                        tokio::select! {
                            event = interceptor.read() => {
                                if event.is_err() | events_sender.send((id, event)).await.is_err() {
                                    break;
                                }
                            }
                            command = interceptor_receiver.recv() => {
                                let command = match command {
                                    Some(command) => command,
                                    None => break,
                                };

                                match command {
                                    DeviceCommand::Event(event) => {
                                        match interceptor.write(&event).await {
                                            Ok(()) => {},
                                            Err(err) => {
                                                let _ = events_sender.send((id, Err(err))).await;
                                                break;
                                            }
                                        }

                                        tracing::trace!(id = %id, "Wrote an event to device");
                                    }
                                    DeviceCommand::CapsLockLed(on) => {
                                        if let Err(err) = interceptor.set_caps_lock_led(on) {
                                            tracing::warn!(id = %id, "Failed to set caps lock LED: {}", err);
                                        }
                                    }
                                }
                            }
                        }
                    }
                });

                let device = &devices[id];

                tracing::info!(
                    id = %id,
                    name = ?device.name,
                    vendor = %device.vendor,
                    product = %device.product,
                    version = %device.version,
                    "Registered new device"
                );
            }
            (id, result) = event => match result {
                Ok(event) => {
                    let mut press = false;
                    let mut pressed = false;

                    if let Event::Key(KeyEvent { key, down }) = event {
                        tracing::debug!(key = ?key, down = %down, "Key event");

                        if switch_keys.contains(&key) {
                            press = true;
                            pressed = down;

                            match down {
                                true => pressed_keys.insert(key, id),
                                false => pressed_keys.remove(&key),
                            };
                        }
                    }

                    // Who to send this event to.
                    let mut idx = current;
                    let mut switched = false;

                    if press {
                        let shortcut = pressed
                            .then(|| {
                                shortcuts.iter().find(|shortcut| {
                                    shortcut.keys.iter().all(|key| pressed_keys.contains_key(key))
                                })
                            })
                            .flatten();

                        match shortcut {
                            Some(shortcut) => {
                                switched = true;

                                let target = match &shortcut.target {
                                    Target::Cycle => {
                                        // Slab keys are sparse, so the cycle has to span the highest one rather than the count.
                                        let end = clients.iter().map(|(key, _)| key + 2).max().unwrap_or(1);
                                        let mut next = current;

                                        loop {
                                            next = (next + 1) % end;
                                            if next == 0 || clients.contains(next - 1) {
                                                break;
                                            }
                                        }

                                        Some(next)
                                    }
                                    Target::Server => Some(0),
                                    Target::Client(name) => clients
                                        .iter()
                                        .find(|(_, client)| client.name.as_deref() == Some(name))
                                        .map(|(key, _)| key + 1),
                                };

                                previous = idx;
                                changed = true;

                                match target {
                                    Some(target) => {
                                        current = target;

                                        if current != previous {
                                            for (idx, active) in [(previous, false), (current, true)] {
                                                if let Some(client) = idx.checked_sub(1).and_then(|idx| clients.get(idx)) {
                                                    let _ = client.sender.try_send(Update::Active(active));
                                                }
                                            }
                                        }

                                        let label = match current {
                                            0 => "server".to_owned(),
                                            current => clients[current - 1].label(),
                                        };

                                        tracing::info!(target = %label, "Switched");

                                        if let Some(command) = &config.on_switch {
                                            run_on_switch(command, &label);
                                        }

                                        if config.indicator == Indicator::CapsLock {
                                            for (_, device) in &devices {
                                                let _ = device
                                                    .sender
                                                    .try_send(DeviceCommand::CapsLockLed(current != 0));
                                            }
                                        }
                                    }
                                    None => {
                                        if let Target::Client(name) = &shortcut.target {
                                            tracing::warn!(target = %name, "Client is not connected");
                                        }
                                    }
                                }
                            }
                            None => {
                                if changed {
                                    idx = previous;

                                    if pressed_keys.is_empty() {
                                        changed = false;
                                    }
                                }
                            }
                        }
                    }

                    let mut events = Vec::new();

                    if press && !propagate_switch_keys {
                        if switched {
                            // Release the switch keys that were already let through, so that they
                            // don't stay stuck down on the other side.
                            for (key, id) in propagated.drain() {
                                events.push((id, Event::Key(KeyEvent { key, down: false })));
                                events.push((id, Event::Sync(SyncEvent::All)));
                            }

                            deferred.clear();
                            suppressed = true;
                        } else if suppressed {
                            suppressed = !pressed_keys.is_empty();
                        } else {
                            // On its own a switch key is an ordinary key, but it's not known yet
                            // whether the combination is going to be completed, so hold it back.
                            deferred.push((id, event));
                            deferred.push((id, Event::Sync(SyncEvent::All)));

                            if pressed_keys.is_empty() {
                                events.append(&mut deferred);
                            }
                        }
                    } else {
                        events.append(&mut deferred);
                        events.push((id, event));

                        if press {
                            events.push((id, Event::Sync(SyncEvent::All)));
                        }
                    }

                    if !propagate_switch_keys {
                        for (id, event) in &events {
                            if let Event::Key(KeyEvent { key, down }) = event {
                                if switch_keys.contains(key) {
                                    match *down {
                                        true => propagated.insert(*key, *id),
                                        false => propagated.remove(key),
                                    };
                                }
                            }
                        }
                    }

                    // Index 0 - special case to keep the modular arithmetic above working.
                    if idx == 0 {
                        // We do a try_send() here rather than a "blocking" send in order to prevent deadlocks.
                        // In this scenario, the interceptor task is sending events to the main task,
                        // while the main task is simultaneously sending events back to the interceptor.
                        // This creates a classic deadlock situation where both tasks are waiting for each other.
                        for (id, event) in events {
                            match devices[id].sender.try_send(DeviceCommand::Event(event)) {
                                Ok(()) | Err(TrySendError::Closed(_)) => {},
                                Err(TrySendError::Full(_)) => return Err(Error::Overflow),
                            }
                        }

                        continue;
                    }

                    if !clients.contains(idx - 1) {
                        continue;
                    }

                    for (id, event) in events {
                        if clients[idx - 1].sender.send(Update::Event { id, event }).await.is_err() {
                            clients.remove(idx - 1);

                            if current == idx {
                                current = 0;
                            }

                            break;
                        }
                    }
                }
                Err(err) if err.kind() == ErrorKind::BrokenPipe => {
                    for (_, client) in &clients {
                        let _ = client.sender.send(Update::DestroyDevice { id }).await;
                    }
                    devices.remove(id);

                    deferred.retain(|(device, _)| *device != id);
                    propagated.retain(|_, device| *device != id);
                    pressed_keys.retain(|_, device| *device != id);

                    tracing::info!(id = %id, "Destroyed device");
                }
                Err(err) => return Err(Error::Input(err)),
            }
        }
    }
}

fn run_on_switch(command: &str, target: &str) {
    let result = Command::new("sh")
        .arg("-c")
        .arg(command)
        .arg("rkvm")
        .arg(target)
        .env("RKVM_TARGET", target)
        .stdin(Stdio::null())
        .spawn();

    if let Err(err) = result {
        tracing::warn!("Failed to run on-switch command: {}", err);
    }
}

struct Device {
    name: CString,
    vendor: u16,
    product: u16,
    version: u16,
    rel: HashSet<RelAxis>,
    abs: HashMap<AbsAxis, AbsInfo>,
    keys: HashSet<Key>,
    delay: Option<i32>,
    period: Option<i32>,
    sender: Sender<DeviceCommand>,
}

enum DeviceCommand {
    Event(Event),
    CapsLockLed(bool),
}

#[derive(Error, Debug)]
enum ClientError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("Incompatible client version (got {client}, expected {server})")]
    Version { server: Version, client: Version },
    #[error("Invalid password")]
    Auth,
    #[error("Invalid client name")]
    Name,
    #[error(transparent)]
    Rand(#[from] rand::Error),
}

async fn client(
    sender: Sender<Update>,
    addr: SocketAddr,
    authenticated: Sender<(Sender<Update>, SocketAddr, Option<String>)>,
    clipboard: Sender<clipboard::Data>,
    mut receiver: Receiver<Update>,
    stream: TcpStream,
    acceptor: TlsAcceptor,
    password: &str,
    reply_timeout: Duration,
    permit: OwnedSemaphorePermit,
) -> Result<(), ClientError> {
    // Input events are tiny and latency sensitive, don't let Nagle sit on them.
    stream.set_nodelay(true)?;

    let stream = rkvm_net::timeout(rkvm_net::TLS_TIMEOUT, acceptor.accept(stream)).await?;
    tracing::info!("TLS connected");

    let mut stream = BufStream::with_capacity(1024, 1024, stream);

    rkvm_net::timeout(rkvm_net::WRITE_TIMEOUT, async {
        Version::CURRENT.encode(&mut stream).await?;
        stream.flush().await?;

        Ok(())
    })
    .await?;

    let version = rkvm_net::timeout(rkvm_net::READ_TIMEOUT, Version::decode(&mut stream)).await?;
    if version != Version::CURRENT {
        return Err(ClientError::Version {
            server: Version::CURRENT,
            client: version,
        });
    }

    let challenge = AuthChallenge::generate().await?;

    rkvm_net::timeout(rkvm_net::WRITE_TIMEOUT, async {
        challenge.encode(&mut stream).await?;
        stream.flush().await?;

        Ok(())
    })
    .await?;

    let response =
        rkvm_net::timeout(rkvm_net::READ_TIMEOUT, AuthResponse::decode(&mut stream)).await?;
    let status = match response.verify(&challenge, password) {
        true => AuthStatus::Passed,
        false => AuthStatus::Failed,
    };

    rkvm_net::timeout(rkvm_net::WRITE_TIMEOUT, async {
        status.encode(&mut stream).await?;
        stream.flush().await?;

        Ok(())
    })
    .await?;

    if status == AuthStatus::Failed {
        return Err(ClientError::Auth);
    }

    let name =
        rkvm_net::timeout(rkvm_net::READ_TIMEOUT, Option::<String>::decode(&mut stream)).await?;

    if let Some(name) = &name {
        let sane = !name.is_empty()
            && name.len() <= MAX_NAME_LENGTH
            && !name.chars().any(char::is_control);

        if !sane {
            return Err(ClientError::Name);
        }
    }

    tracing::info!("Authenticated successfully");
    drop(permit);

    if authenticated.send((sender, addr, name)).await.is_err() {
        return Ok(());
    }

    let mut interval = time::interval(rkvm_net::PING_INTERVAL);

    loop {
        let recv = receiver.recv();

        let update = tokio::select! {
            // Make sure pings have priority.
            // The client could time out otherwise.
            biased;

            _ = interval.tick() => Some(Update::Ping),
            recv = recv => recv,
        };

        let update = match update {
            Some(update) => update,
            None => break,
        };

        let start = Instant::now();
        rkvm_net::timeout(update.timeout(), async {
            update.encode(&mut stream).await?;
            stream.flush().await?;

            Ok(())
        })
        .await?;
        let duration = start.elapsed();

        if let Update::Ping = update {
            // Keeping these as debug because it's not as frequent as other updates.
            tracing::debug!(duration = ?duration, "Sent ping");

            let start = Instant::now();
            let pong =
                rkvm_net::timeout(reply_timeout, Pong::decode(&mut stream)).await?;
            let duration = start.elapsed();

            tracing::debug!(duration = ?duration, "Received pong");

            if let Some(data) = pong.clipboard {
                if clipboard.send(data).await.is_err() {
                    break;
                }
            }
        }

        tracing::trace!("Wrote an update");
    }

    Ok(())
}
