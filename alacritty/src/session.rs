//! Restore open windows, their tab directories, and their selected tabs.

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, BufReader};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use log::warn;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tempfile::{Builder, NamedTempFile};

// Window managers send independent close requests for a group close.
const CLOSE_GROUP_INTERVAL: Duration = Duration::from_millis(750);
const INSTANCE_PREFIX: &str = "instance-";

#[derive(Deserialize, Serialize)]
pub struct WindowSession {
    // OsString preserves Unix paths that are not valid UTF-8.
    directories: Vec<Option<OsString>>,
    pub active_tab: usize,
}

impl WindowSession {
    pub fn new(directories: impl Iterator<Item = Option<PathBuf>>, active_tab: usize) -> Self {
        let directories = directories.map(|path| path.map(PathBuf::into_os_string)).collect();
        Self { directories, active_tab }
    }

    pub fn into_directories(self) -> impl ExactSizeIterator<Item = Option<PathBuf>> {
        self.directories
            .into_iter()
            .map(|path| path.map(PathBuf::from).filter(|path| path.is_absolute() && path.is_dir()))
    }
}

#[derive(Deserialize, Serialize)]
#[serde(untagged)]
enum SessionFile {
    Windows { windows: Vec<WindowSession> },
    SingleWindow(WindowSession),
    Directories(Vec<Option<OsString>>),
}

#[derive(Deserialize, Serialize)]
struct ClosedWindow {
    at: SystemTime,
    window: WindowSession,
}

/// Membership in the running session shared by Alacritty processes using the same state directory.
pub struct Session {
    path: PathBuf,
    instance: NamedTempFile,
    closed: Vec<ClosedWindow>,
}

impl Session {
    pub fn start(path: PathBuf) -> io::Result<(Self, Vec<WindowSession>)> {
        let registry = registry_path(&path);
        fs::create_dir_all(&registry)?;
        let _lock = lock_registry(&registry)?;
        let mut windows = Vec::new();
        if !has_live_instances(&registry)? {
            remove_pending(&registry)?;
            match load_windows(&path) {
                Ok(saved_windows) => windows = saved_windows,
                Err(err) => warn!("Unable to load terminal session: {err}"),
            }
        }

        let instance = Builder::new().prefix(INSTANCE_PREFIX).tempfile_in(&registry)?;
        instance.as_file().lock()?;
        Ok((Self { path, instance, closed: Vec::new() }, windows))
    }

    pub fn record_close(&mut self, window: WindowSession, at: SystemTime) {
        self.closed.retain(|close| in_close_group(close.at, at));
        self.closed.push(ClosedWindow { at, window });
    }

    pub fn finish(self) -> io::Result<()> {
        let Self { path, instance, mut closed } = self;
        let registry = registry_path(&path);
        let _lock = lock_registry(&registry)?;
        instance.close()?;
        if closed.is_empty() {
            return Ok(());
        }

        let pending_path = registry.join("pending.json");
        match read_json::<Vec<ClosedWindow>>(&pending_path) {
            Ok(Some(pending)) => closed.extend(pending),
            Ok(None) => (),
            Err(err) => warn!("Unable to read pending terminal windows: {err}"),
        }
        closed.sort_by_key(|close| close.at);
        let latest = closed.last().unwrap().at;
        closed.retain(|close| in_close_group(close.at, latest));

        if has_live_instances(&registry)? {
            // Other processes still have windows open. Leave these candidates for the final exit.
            write_json(&pending_path, &closed)
        } else {
            let windows = closed.into_iter().map(|close| close.window).collect();
            write_json(&path, &SessionFile::Windows { windows })?;
            remove_pending(&registry)
        }
    }
}

pub fn path() -> Option<PathBuf> {
    xdg::BaseDirectories::with_prefix("alacritty").get_state_file("session.json")
}

fn registry_path(path: &Path) -> PathBuf {
    path.with_extension("instances")
}

fn lock_registry(registry: &Path) -> io::Result<File> {
    let lock = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(registry.join("lock"))?;
    lock.lock()?;
    Ok(lock)
}

fn has_live_instances(registry: &Path) -> io::Result<bool> {
    let mut live = false;
    for entry in fs::read_dir(registry)? {
        let entry = entry?;
        if !entry.file_name().as_encoded_bytes().starts_with(INSTANCE_PREFIX.as_bytes()) {
            continue;
        }
        let instance_path = entry.path();
        let file = match OpenOptions::new().write(true).open(&instance_path) {
            Ok(file) => file,
            // An instance can fail to start and drop its membership outside the registry lock.
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(err) => return Err(err),
        };
        match file.try_lock() {
            Ok(()) => match fs::remove_file(instance_path) {
                Ok(()) => (),
                Err(err) if err.kind() == io::ErrorKind::NotFound => (),
                Err(err) => return Err(err),
            },
            Err(TryLockError::WouldBlock) => live = true,
            Err(TryLockError::Error(err)) => return Err(err),
        }
    }
    Ok(live)
}

fn remove_pending(registry: &Path) -> io::Result<()> {
    match fs::remove_file(registry.join("pending.json")) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

fn in_close_group(close: SystemTime, latest: SystemTime) -> bool {
    latest.duration_since(close).is_ok_and(|elapsed| elapsed <= CLOSE_GROUP_INTERVAL)
}

fn load_windows(path: &Path) -> io::Result<Vec<WindowSession>> {
    let mut windows = match read_json(path)? {
        Some(SessionFile::Windows { windows }) => windows,
        Some(SessionFile::SingleWindow(window)) => vec![window],
        Some(SessionFile::Directories(directories)) => {
            vec![WindowSession { directories, active_tab: 0 }]
        },
        None => Vec::new(),
    };
    windows.retain(|window| !window.directories.is_empty());
    Ok(windows)
}

fn read_json<T: DeserializeOwned>(path: &Path) -> io::Result<Option<T>> {
    match File::open(path) {
        Ok(file) => Ok(Some(serde_json::from_reader(BufReader::new(file))?)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

fn write_json(path: &Path, value: &impl Serialize) -> io::Result<()> {
    let parent = path.parent().expect("session file has a parent directory");
    let mut file = NamedTempFile::new_in(parent)?;
    serde_json::to_writer(file.as_file_mut(), value)?;
    file.persist(path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::ffi::OsStringExt;

    use super::*;

    #[test]
    fn restores_selection_and_keeps_tabs_for_missing_paths() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first tab");
        let missing = dir.path().join("deleted");
        let last = dir.path().join(OsString::from_vec(b"last\xff".to_vec()));
        let path = dir.path().join("session.json");
        for path in [&first, &missing, &last] {
            fs::create_dir(path).unwrap();
        }

        let (mut session, windows) = Session::start(path.clone()).unwrap();
        assert!(windows.is_empty());
        let directories = [Some(last.clone()), Some(missing.clone()), Some(first.clone())];
        session.record_close(WindowSession::new(directories.into_iter(), 1), SystemTime::now());
        session.finish().unwrap();
        fs::remove_dir(missing).unwrap();

        let (_, mut windows) = Session::start(path).unwrap();
        let window = windows.pop().unwrap();
        assert_eq!(window.active_tab, 1);
        assert_eq!(
            window.into_directories().collect::<Vec<_>>(),
            vec![Some(last), None, Some(first)]
        );
    }

    #[test]
    fn previous_single_window_formats_are_readable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        let directories = vec![Some(dir.path().as_os_str().to_owned()), None];
        write_json(&path, &directories).unwrap();
        let mut windows = load_windows(&path).unwrap();
        let window = windows.pop().unwrap();
        assert_eq!(window.active_tab, 0);
        assert_eq!(window.directories, directories);

        write_json(&path, &WindowSession { directories: directories.clone(), active_tab: 1 })
            .unwrap();
        let mut windows = load_windows(&path).unwrap();
        let window = windows.pop().unwrap();
        assert_eq!(window.active_tab, 1);
        assert_eq!(window.directories, directories);
    }

    #[test]
    fn overlapping_instances_restore_only_the_final_close_group() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        let (mut early, _) = Session::start(path.clone()).unwrap();
        let (mut first, _) = Session::start(path.clone()).unwrap();
        let (mut last, _) = Session::start(path.clone()).unwrap();
        let now = SystemTime::now();
        let snapshot =
            |active_tab| WindowSession { directories: vec![None, None, None], active_tab };

        early.record_close(snapshot(0), now - CLOSE_GROUP_INTERVAL * 2);
        early.finish().unwrap();
        first.record_close(snapshot(1), now - Duration::from_millis(100));
        last.record_close(snapshot(2), now);
        last.finish().unwrap();
        assert!(!path.exists());
        first.finish().unwrap();

        let (_, windows) = Session::start(path.clone()).unwrap();
        assert_eq!(windows.iter().map(|window| window.active_tab).collect::<Vec<_>>(), vec![1, 2]);
        let (primary, _) = Session::start(path.clone()).unwrap();
        let (_, windows) = Session::start(path).unwrap();
        assert!(windows.is_empty());
        drop(primary);
    }
}
