use rkvm_input::abs::{AbsAxis, AbsInfo};
use rkvm_input::event::Event;
use rkvm_input::key::{Key, KeyEvent};
use rkvm_input::monitor::Monitor;
use rkvm_input::rel::RelAxis;
use rkvm_input::sync::SyncEvent;
use rkvm_net::auth::{AuthChallenge, AuthResponse, AuthStatus};
use rkvm_net::message::Message;
use rkvm_net::version::Version;
use rkvm_net::{Pong, Update};
use slab::Slab;
use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::io::{self, ErrorKind};
use std::net::SocketAddr;
use std::time::Instant;
use thiserror::Error;
use tokio::io::{AsyncWriteExt, BufStream};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::mpsc::{self, Receiver, Sender};
use tokio::time;
use tokio_rustls::TlsAcceptor;
use tracing::Instrument;

#[derive(Error, Debug)]
pub enum Error {
    #[error("Network error: {0}")]
    Network(io::Error),
    #[error("Input error: {0}")]
    Input(io::Error),
    #[error("Event queue overflow")]
    Overflow,
}

pub async fn run(
    listen: SocketAddr,
    acceptor: TlsAcceptor,
    password: &str,
    switch_keys: &HashSet<Key>,
    propagate_switch_keys: bool,
) -> Result<(), Error> {
    let listener = TcpListener::bind(&listen).await.map_err(Error::Network)?;
    tracing::info!("Listening on {}", listen);

    let mut monitor = Monitor::new();
    let mut devices = Slab::<Device>::new();
    let mut clients = Slab::<(Sender<_>, SocketAddr)>::new();
    let mut current = 0;
    let mut previous = 0;
    let mut changed = false;
    let mut pressed_keys = HashSet::new();
    let mut deferred = Vec::new();
    let mut propagated = HashMap::new();
    let mut suppressed = false;

    let (events_sender, mut events_receiver) = mpsc::channel(1);
    let (authenticated_sender, mut authenticated_receiver) = mpsc::channel(1);

    loop {
        let event = async { events_receiver.recv().await.unwrap() };
        let authenticated = async { authenticated_receiver.recv().await.unwrap() };

        tokio::select! {
            result = listener.accept() => {
                let (stream, addr) = result.map_err(Error::Network)?;
                let acceptor = acceptor.clone();
                let password = password.to_owned();
                let authenticated_sender = authenticated_sender.clone();

                let (sender, receiver) = mpsc::channel(1);

                let span = tracing::info_span!("connection", addr = %addr);
                tokio::spawn(
                    async move {
                        tracing::info!("Connected");

                        let result = client(
                            sender,
                            addr,
                            authenticated_sender,
                            receiver,
                            stream,
                            acceptor,
                            &password,
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
            (sender, addr) = authenticated => {
                // Remove dead clients.
                clients.retain(|_, (client, _)| !client.is_closed());
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

                if alive {
                    clients.insert((sender, addr));
                    tracing::info!(addr = %addr, "Registered client");
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

                for (_, (sender, _)) in &clients {
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

                    let _ = sender.send(update).await;
                }

                let (interceptor_sender, mut interceptor_receiver) = mpsc::channel(32);
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
                            event = interceptor_receiver.recv() => {
                                let event = match event {
                                    Some(event) => event,
                                    None => break,
                                };

                                match interceptor.write(&event).await {
                                    Ok(()) => {},
                                    Err(err) => {
                                        let _ = events_sender.send((id, Err(err))).await;
                                        break;
                                    }
                                }

                                tracing::trace!(id = %id, "Wrote an event to device");
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

                    if let Event::Key(KeyEvent { key, down }) = event {
                        if switch_keys.contains(&key) {
                            press = true;

                            match down {
                                true => pressed_keys.insert(key),
                                false => pressed_keys.remove(&key),
                            };
                        }
                    }

                    // Who to send this event to.
                    let mut idx = current;
                    let mut switched = false;

                    if press {
                        if pressed_keys.len() == switch_keys.len() {
                            switched = true;

                            // Slab keys are sparse, so the cycle has to span the highest one rather than the count.
                            let end = clients.iter().map(|(key, _)| key + 2).max().unwrap_or(1);
                            loop {
                                current = (current + 1) % end;
                                if current == 0 || clients.contains(current - 1) {
                                    break;
                                }
                            }

                            previous = idx;
                            changed = true;

                            if current != 0 {
                                tracing::info!(idx = %current, addr = %clients[current - 1].1, "Switched client");
                            } else {
                                tracing::info!(idx = %current, "Switched client");
                            }
                        } else if changed {
                            idx = previous;

                            if pressed_keys.is_empty() {
                                changed = false;
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
                            match devices[id].sender.try_send(event) {
                                Ok(()) | Err(TrySendError::Closed(_)) => {},
                                Err(TrySendError::Full(_)) => return Err(Error::Overflow),
                            }
                        }

                        continue;
                    }

                    for (id, event) in events {
                        if clients[idx - 1].0.send(Update::Event { id, event }).await.is_err() {
                            clients.remove(idx - 1);

                            if current == idx {
                                current = 0;
                            }

                            break;
                        }
                    }
                }
                Err(err) if err.kind() == ErrorKind::BrokenPipe => {
                    for (_, (sender, _)) in &clients {
                        let _ = sender.send(Update::DestroyDevice { id }).await;
                    }
                    devices.remove(id);

                    deferred.retain(|(device, _)| *device != id);
                    propagated.retain(|_, device| *device != id);

                    tracing::info!(id = %id, "Destroyed device");
                }
                Err(err) => return Err(Error::Input(err)),
            }
        }
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
    sender: Sender<Event>,
}

#[derive(Error, Debug)]
enum ClientError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("Incompatible client version (got {client}, expected {server})")]
    Version { server: Version, client: Version },
    #[error("Invalid password")]
    Auth,
    #[error(transparent)]
    Rand(#[from] rand::Error),
}

async fn client(
    sender: Sender<Update>,
    addr: SocketAddr,
    authenticated: Sender<(Sender<Update>, SocketAddr)>,
    mut receiver: Receiver<Update>,
    stream: TcpStream,
    acceptor: TlsAcceptor,
    password: &str,
) -> Result<(), ClientError> {
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

    tracing::info!("Authenticated successfully");

    if authenticated.send((sender, addr)).await.is_err() {
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
        rkvm_net::timeout(rkvm_net::WRITE_TIMEOUT, async {
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
            rkvm_net::timeout(rkvm_net::READ_TIMEOUT, Pong::decode(&mut stream)).await?;
            let duration = start.elapsed();

            tracing::debug!(duration = ?duration, "Received pong");
        }

        tracing::trace!("Wrote an update");
    }

    Ok(())
}
