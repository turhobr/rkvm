use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;

const RUNTIME_PATH: &str = "/run/user";

// rkvm runs as root, with no desktop of its own. Anything it wants to show has to
// go to a logged in user's session, found here rather than configured by hand.
pub struct Session {
    pub uid: u32,
    pub runtime: PathBuf,
}

impl Session {
    pub fn find() -> Option<Self> {
        let mut sessions = fs::read_dir(RUNTIME_PATH)
            .ok()?
            .flatten()
            .filter_map(|entry| {
                let runtime = entry.path();
                let uid = runtime.file_name()?.to_str()?.parse().ok()?;

                // Below 1000 are system users, which have no desktop.
                (uid >= 1000).then_some(Self { uid, runtime })
            })
            .collect::<Vec<_>>();

        // Prefer whoever logged in last.
        sessions.sort_by_key(|session| {
            fs::metadata(&session.runtime)
                .map(|metadata| metadata.mtime())
                .unwrap_or(0)
        });

        sessions.pop()
    }

    pub fn bus(&self) -> Option<String> {
        let path = self.runtime.join("bus");

        path.exists()
            .then(|| format!("unix:path={}", path.display()))
    }

    pub fn wayland(&self) -> Option<PathBuf> {
        let mut sockets = fs::read_dir(&self.runtime)
            .ok()?
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .map_or(false, |name| {
                        name.starts_with("wayland-") && !name.ends_with(".lock")
                    })
            })
            .collect::<Vec<_>>();

        sockets.sort();
        sockets.pop()
    }
}
