use crate::session::Session;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;
use zbus::zvariant::Value;
use zbus::{connection, proxy};

// Reusing the id replaces the previous popup instead of stacking them up.
static REPLACES: AtomicU32 = AtomicU32::new(0);

const TIMEOUT: Duration = Duration::from_secs(2);

#[proxy(
    interface = "org.freedesktop.Notifications",
    default_service = "org.freedesktop.Notifications",
    default_path = "/org/freedesktop/Notifications"
)]
trait Notifications {
    #[allow(clippy::too_many_arguments)]
    fn notify(
        &self,
        app_name: &str,
        replaces_id: u32,
        icon: &str,
        summary: &str,
        body: &str,
        actions: &[&str],
        hints: HashMap<&str, &Value<'_>>,
        expire_timeout: i32,
    ) -> zbus::Result<u32>;
}

pub fn show(target: &str) {
    let target = target.to_owned();

    tokio::spawn(async move {
        if let Err(err) = send(&target).await {
            tracing::debug!("Failed to notify: {}", err);
        }
    });
}

async fn send(target: &str) -> Result<(), Box<dyn std::error::Error>> {
    let session = Session::find().ok_or("No user session")?;
    let address = session.bus().ok_or("No session bus")?;

    let connection = tokio::time::timeout(
        TIMEOUT,
        connection::Builder::address(address.as_str())?.build(),
    )
    .await??;

    let proxy = NotificationsProxy::new(&connection).await?;
    let replaces = REPLACES.load(Ordering::Relaxed);

    let id = tokio::time::timeout(
        TIMEOUT,
        proxy.notify(
            "rkvm",
            replaces,
            "input-keyboard",
            "rkvm",
            &format!("now on {}", target),
            &[],
            HashMap::new(),
            1500,
        ),
    )
    .await??;

    REPLACES.store(id, Ordering::Relaxed);

    Ok(())
}
