pub mod error;
pub mod history;
pub mod keybindings;
pub mod keys;
pub mod settings;
pub mod sqlite;
pub mod state;

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

pub use error::{Error, Result};
use fastab_util::directories;
use fd_lock::RwLock as FileRwLock;
use parking_lot::{MappedRwLockReadGuard, MappedRwLockWriteGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};
use serde_json::Value;
pub use settings::{Settings, SettingsProvider};
pub use state::{State, StateProvider};
use thiserror::Error;
use tracing::error;

pub type Map = serde_json::Map<String, Value>;

static SETTINGS_FILE_LOCK: RwLock<()> = RwLock::new(());

static SETTINGS_DATA: RwLock<Option<Map>> = RwLock::new(None);

/// Set while `SETTINGS_DATA` is not what the settings file holds, because the
/// file did not load. See [`JsonStore::ensure_loaded`].
static SETTINGS_LOAD_FAILED: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Clone)]
pub enum Backend {
    Global,
    Memory(Map),
}

pub enum ReadGuard<'a, T> {
    Global(RwLockReadGuard<'a, Option<T>>),
    Memory(&'a T),
}

impl<'a, T> ReadGuard<'a, T> {
    pub fn map<U, F: FnOnce(&T) -> &U>(self, f: F) -> MappedReadGuard<'a, U> {
        match self {
            ReadGuard::Global(guard) => {
                MappedReadGuard::Global(RwLockReadGuard::<'a, Option<T>>::map(guard, |data: &Option<T>| {
                    f(data.as_ref().expect("global backend is not used"))
                }))
            },
            ReadGuard::Memory(data) => MappedReadGuard::Memory(f(data)),
        }
    }

    pub fn try_map<U, F: FnOnce(&T) -> Option<&U>>(self, f: F) -> Option<MappedReadGuard<'a, U>> {
        match self {
            ReadGuard::Global(guard) => RwLockReadGuard::<'a, Option<T>>::try_map(guard, |data: &Option<T>| {
                f(data.as_ref().expect("global backend is not used"))
            })
            .ok()
            .map(MappedReadGuard::Global),
            ReadGuard::Memory(data) => f(data).map(MappedReadGuard::Memory),
        }
    }
}

impl<T> std::ops::Deref for ReadGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        match self {
            ReadGuard::Global(guard) => guard.as_ref().expect("global backend is not used"),
            ReadGuard::Memory(data) => data,
        }
    }
}

pub enum MappedReadGuard<'a, T> {
    Global(MappedRwLockReadGuard<'a, T>),
    Memory(&'a T),
}

impl<T> std::ops::Deref for MappedReadGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        match self {
            MappedReadGuard::Global(guard) => guard,
            MappedReadGuard::Memory(data) => data,
        }
    }
}

pub enum WriteGuard<'a, T> {
    Global(RwLockWriteGuard<'a, Option<T>>),
    Memory(&'a mut T),
}

impl<'a, T> WriteGuard<'a, T> {
    pub fn map<U, F: FnOnce(&mut T) -> &mut U>(self, f: F) -> MappedWriteGuard<'a, U> {
        match self {
            WriteGuard::Global(guard) => {
                MappedWriteGuard::Global(RwLockWriteGuard::<'a, Option<T>>::map(guard, |data: &mut Option<T>| {
                    f(data.as_mut().expect("global backend is not used"))
                }))
            },
            WriteGuard::Memory(data) => MappedWriteGuard::Memory(f(data)),
        }
    }

    pub fn try_map<U, F: FnOnce(&mut T) -> Option<&mut U>>(self, f: F) -> Option<MappedWriteGuard<'a, U>> {
        match self {
            WriteGuard::Global(guard) => RwLockWriteGuard::<'a, Option<T>>::try_map(guard, |data: &mut Option<T>| {
                f(data.as_mut().expect("global backend is not used"))
            })
            .ok()
            .map(MappedWriteGuard::Global),
            WriteGuard::Memory(data) => f(data).map(MappedWriteGuard::Memory),
        }
    }
}

impl<T> std::ops::Deref for WriteGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        match self {
            WriteGuard::Global(guard) => guard.as_ref().expect("global backend is not used"),
            WriteGuard::Memory(data) => data,
        }
    }
}

impl<T> std::ops::DerefMut for WriteGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        match self {
            WriteGuard::Global(guard) => guard.as_mut().expect("global backend is not used"),
            WriteGuard::Memory(data) => data,
        }
    }
}

pub enum MappedWriteGuard<'a, T> {
    Global(MappedRwLockWriteGuard<'a, T>),
    Memory(&'a mut T),
}

impl<T> std::ops::Deref for MappedWriteGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        match self {
            MappedWriteGuard::Global(guard) => guard,
            MappedWriteGuard::Memory(data) => data,
        }
    }
}

impl<T> std::ops::DerefMut for MappedWriteGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        match self {
            MappedWriteGuard::Global(guard) => guard,
            MappedWriteGuard::Memory(data) => data,
        }
    }
}

pub trait JsonStore: Sized {
    /// Path to the file
    fn path() -> Result<PathBuf>;

    /// In mem lock on the file
    fn file_lock() -> &'static RwLock<()>;

    /// [RwLock] on the data, [None] if not using the global backend
    fn data_lock() -> &'static RwLock<Option<Map>>;

    /// Set while the global backend's data is not the file's contents,
    /// because the file did not load.
    fn load_failed() -> &'static AtomicBool;

    fn new_from_backend(backend: Backend) -> Self;

    fn map(&self) -> ReadGuard<'_, Map>;

    fn map_mut(&mut self) -> WriteGuard<'_, Map>;

    fn load() -> Result<Self> {
        let is_global = Self::data_lock().read().as_ref().is_some();
        if is_global {
            Ok(Self::new_from_backend(Backend::Global))
        } else {
            Ok(Self::new_from_backend(Backend::Memory(Self::load_from_file()?)))
        }
    }

    fn load_from_file() -> Result<Map> {
        let path = Self::path()?;
        let _lock_guard = Self::file_lock().write();
        read_json_object(&path)
    }

    /// Makes `map`, just read from the file, the global backend's data.
    fn replace_global(map: Map) {
        *Self::data_lock().write() = Some(map);
        Self::load_failed().store(false, Ordering::Release);
    }

    /// Records that the file no longer loads, so the global backend's data is
    /// not what it holds. A watcher that fails to re-read a changed file calls
    /// this; the data it already has stays in use for reads.
    fn mark_load_failed() {
        Self::load_failed().store(true, Ordering::Release);
    }

    /// Call before changing the global backend's data.
    ///
    /// While the file has not loaded, that data stands in for it (an empty map
    /// after a failed start, the last good copy after a failed re-read), and
    /// saving it would replace the user's file: a typo in a hand edit used to
    /// cost every other setting the next time anything was written, which the
    /// desktop app did on every launch. So the file is read again first. Once
    /// it loads, its contents are the data to change; until then the change
    /// is refused and nothing is written, as it always was for a process that
    /// reads the file per call (`ftab settings`).
    fn ensure_loaded() -> Result<()> {
        if Self::load_failed().load(Ordering::Acquire) {
            let map = Self::load_from_file()
                .inspect_err(|err| error!(%err, "Not saving a setting: the settings file does not load"))?;
            Self::replace_global(map);
        }
        Ok(())
    }

    /// Loads data from file into global backend
    fn load_into_global() -> Result<()> {
        match Self::load_from_file() {
            Ok(json) => {
                Self::replace_global(json);
                Ok(())
            },
            Err(err) => {
                // Run on defaults, but never write them back: every new
                // terminal tab loads this file, so a hand-edit typo or a read
                // racing a writer would otherwise wipe the user's settings.
                *Self::data_lock().write() = Some(Map::new());
                Self::mark_load_failed();

                let backup = match (&err, Self::path()) {
                    (Error::JsonError(_), Ok(path)) => {
                        let _lock_guard = Self::file_lock().write();
                        back_up_unreadable_json(&path)
                    },
                    _ => None,
                };
                error!(%err, ?backup, "Failed to load json file into global backend");

                Err(err)
            },
        }
    }

    fn save_to_file(&self) -> Result<()> {
        // A caller that skipped `ensure_loaded` still must not save stand-in
        // data over the file.
        if Self::load_failed().load(Ordering::Acquire) {
            return Err(Error::FileNotLoaded);
        }
        let path = Self::path()?;
        let _lock_guard = Self::file_lock().write();
        write_json_object_atomically(&path, &self.map())
    }

    fn set(&mut self, key: impl Into<String>, value: impl Into<serde_json::Value>) {
        self.map_mut().insert(key.into(), value.into());
    }

    fn get(&self, key: impl AsRef<str>) -> Option<MappedReadGuard<'_, Value>> {
        self.map().try_map(|data| data.get(key.as_ref()))
    }

    fn remove(&mut self, key: impl AsRef<str>) -> Option<Value> {
        self.map_mut().remove(key.as_ref())
    }

    fn get_mut(&mut self, key: impl Into<String>) -> Option<MappedWriteGuard<'_, Value>> {
        self.map_mut().try_map(|data| data.get_mut(&key.into()))
    }

    fn get_bool(&self, key: impl AsRef<str>) -> Option<bool> {
        self.get(key).and_then(|value| value.as_bool())
    }

    fn get_bool_or(&self, key: impl AsRef<str>, default: bool) -> bool {
        self.get_bool(key).unwrap_or(default)
    }

    fn get_string(&self, key: impl AsRef<str>) -> Option<String> {
        self.get(key).and_then(|value| value.as_str().map(|s| s.into()))
    }

    fn get_string_or(&self, key: impl AsRef<str>, default: String) -> String {
        self.get_string(key).unwrap_or(default)
    }

    fn get_int(&self, key: impl AsRef<str>) -> Option<i64> {
        self.get(key).and_then(|value| value.as_i64())
    }

    fn get_int_or(&self, key: impl AsRef<str>, default: i64) -> i64 {
        self.get_int(key).unwrap_or(default)
    }
}

/// Temp files sit next to the data file as `.<name>.<random>.tmp`.
const TEMP_SUFFIX: &str = ".tmp";
/// A live writer's temp file is milliseconds old; anything this old was left
/// by a writer that died before its rename.
const STALE_TEMP_AGE: Duration = Duration::from_secs(60);
/// Unparsable versions kept as `<name>.invalid-<hash>`, newest first.
const KEPT_INVALID_BACKUPS: usize = 3;

/// Follows `path` through symlinks to the file that holds the data, also when
/// that file does not exist yet. A dotfile-managed link keeps pointing at its
/// target: writes land there, and a dangling link gets its target created
/// rather than failing every load and save.
pub fn resolve_data_file(path: &Path) -> std::io::Result<PathBuf> {
    let mut current = path.to_path_buf();
    for _ in 0..32 {
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                let link = fs::read_link(&current)?;
                // `join` keeps an absolute link target as it is.
                current = match current.parent() {
                    Some(parent) => parent.join(link),
                    None => link,
                };
            },
            _ => return Ok(current),
        }
    }
    Err(std::io::Error::other("too many levels of symbolic links"))
}

/// Reads a JSON object file, creating it as `{}` when it is missing.
///
/// Creation uses `create_new`, so it can never truncate a file another process
/// wrote in the meantime. An empty file reads as `{}`: it is what an older,
/// non-atomic writer leaves mid-write, and it holds nothing to lose.
fn read_json_object(path: &Path) -> Result<Map> {
    let path = resolve_data_file(path)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut create = File::options();
    create.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        create.mode(0o600);
    }
    match create.open(&path) {
        Ok(mut file) => {
            file.write_all(b"{}")?;
            return Ok(Map::new());
        },
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {},
        Err(err) => return Err(err.into()),
    }

    // Writers replace the file by rename, so the shared lock only matters
    // against an older binary that still rewrites it in place.
    let file = FileRwLock::new(File::open(&path)?);
    let read = file.read()?;
    let mut reader: &File = &read;
    let mut bytes = Vec::new();
    // `fs::read` would reopen the path outside the lock held on `file`.
    #[allow(clippy::verbose_file_reads)]
    reader.read_to_end(&mut bytes)?;
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(Map::new());
    }
    Ok(serde_json::from_slice(&bytes)?)
}

/// Replaces `path` with `map` through a sibling temp file and `rename`, so a
/// reader in another process sees either the old file or the new one, never a
/// truncated one. A symlinked file (dotfile managers) has its target replaced
/// and the link kept; an existing file keeps its permissions.
fn write_json_object_atomically(path: &Path, map: &Map) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(map)?;

    let target = resolve_data_file(path)?;
    let (Some(dir), Some(name)) = (target.parent(), target.file_name()) else {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "settings path has no file name").into());
    };
    fs::create_dir_all(dir)?;
    let prefix = format!(".{}.", name.to_string_lossy());
    remove_stale_temp_files(dir, &prefix);
    let permissions = fs::metadata(&target).ok().map(|metadata| metadata.permissions());

    // Created 0600 and removed on drop, so every early return cleans up.
    let mut temp = tempfile::Builder::new()
        .prefix(&prefix)
        .suffix(TEMP_SUFFIX)
        .tempfile_in(dir)?;
    temp.write_all(&bytes)?;
    if let Some(permissions) = permissions {
        temp.as_file().set_permissions(permissions)?;
    }
    flush_to_storage(temp.as_file())?;
    temp.persist(&target).map_err(|err| err.error)?;
    Ok(())
}

/// Puts the data on storage ahead of the rename, so a kernel crash cannot
/// leave a renamed but empty file. Not `File::sync_all`: on macOS that is
/// `F_FULLFSYNC`, a drive-cache flush, and the Settings window writes on the
/// UI thread.
fn flush_to_storage(file: &File) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        loop {
            // SAFETY: `file` keeps the descriptor open for the whole call.
            if unsafe { libc::fsync(file.as_raw_fd()) } == 0 {
                return Ok(());
            }
            let err = std::io::Error::last_os_error();
            if err.kind() != std::io::ErrorKind::Interrupted {
                return Err(err);
            }
        }
    }
    #[cfg(not(unix))]
    {
        file.sync_all()
    }
}

/// A writer killed between creating its temp file and the rename leaves the
/// file behind, and nothing else would ever remove it.
fn remove_stale_temp_files(dir: &Path, prefix: &str) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();
        if !file_name.starts_with(prefix) || !file_name.ends_with(TEMP_SUFFIX) {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age >= STALE_TEMP_AGE);
        if stale {
            fs::remove_file(entry.path()).ok();
        }
    }
}

/// Copies an unparsable file aside before anything can overwrite it. The name
/// is derived from the content, so every terminal tab that hits the same broken
/// file reuses one backup instead of adding another.
fn back_up_unreadable_json(path: &Path) -> Option<PathBuf> {
    use std::hash::{DefaultHasher, Hash, Hasher};

    let bytes = fs::read(path).ok()?;
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    let name = path.file_name()?.to_string_lossy();
    let backup = path.with_file_name(format!("{name}.invalid-{:016x}", hasher.finish()));
    if !backup.exists() {
        fs::write(&backup, &bytes).ok()?;
        if let Some(dir) = path.parent() {
            prune_invalid_backups(dir, &name);
        }
    }
    Some(backup)
}

/// Keeps the newest few backups. A file re-saved with a different typo each
/// time would otherwise add one backup per broken version.
fn prune_invalid_backups(dir: &Path, name: &str) {
    let prefix = format!("{name}.invalid-");
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut backups: Vec<(SystemTime, PathBuf)> = entries
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().starts_with(&prefix))
        .filter_map(|entry| Some((entry.metadata().ok()?.modified().ok()?, entry.path())))
        .collect();
    backups.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    for (_, backup) in backups.into_iter().skip(KEPT_INVALID_BACKUPS) {
        fs::remove_file(backup).ok();
    }
}

pub struct OldSettings {
    pub(crate) inner: Backend,
}

impl JsonStore for OldSettings {
    fn path() -> Result<PathBuf> {
        Ok(directories::settings_path()?)
    }

    fn file_lock() -> &'static RwLock<()> {
        &SETTINGS_FILE_LOCK
    }

    fn data_lock() -> &'static RwLock<Option<Map>> {
        &SETTINGS_DATA
    }

    fn load_failed() -> &'static AtomicBool {
        &SETTINGS_LOAD_FAILED
    }

    fn new_from_backend(backend: Backend) -> Self {
        match backend {
            Backend::Global => Self { inner: Backend::Global },
            Backend::Memory(map) => Self {
                inner: Backend::Memory(map),
            },
        }
    }

    fn map(&self) -> ReadGuard<'_, Map> {
        match &self.inner {
            Backend::Global => ReadGuard::Global(Self::data_lock().read()),
            Backend::Memory(map) => ReadGuard::Memory(map),
        }
    }

    fn map_mut(&mut self) -> WriteGuard<'_, Map> {
        match &mut self.inner {
            Backend::Global => WriteGuard::Global(Self::data_lock().write()),
            Backend::Memory(map) => WriteGuard::Memory(map),
        }
    }
}

// #[cfg(test)]
// mod test {
//     use std::path::Path;

//     use super::*;

//     fn test_store_type(path: &Path, store: JsonType) {
//         let mut local_json = LocalJson::load_file(store).unwrap();
//         assert_eq!(fs::read_to_string(path).unwrap(), "");
//         assert_eq!(local_json.inner, serde_json::Map::new());
//         local_json.save().unwrap();
//         assert_eq!(fs::read_to_string(path).unwrap(), "{}");

//         local_json.set("a", 123);
//         local_json.set("b", "hello");
//         local_json.set("c", false);
//         local_json.save().unwrap();
//         assert_eq!(
//             fs::read_to_string(path).unwrap(),
//             "{\n  \"a\": 123,\n  \"b\": \"hello\",\n  \"c\": false\n}"
//         );

//         local_json.remove("a").unwrap();
//         local_json.save().unwrap();
//         assert_eq!(
//             fs::read_to_string(path).unwrap(),
//             "{\n  \"b\": \"hello\",\n  \"c\": false\n}"
//         );
//         assert_eq!(local_json.get("b").unwrap(), "hello");

//         fs::write(path, "invalid json").unwrap();
//         assert!(matches!(
//             LocalJson::load_file(store).unwrap_err(),
//             Error::SettingsNotObject
//         ));
//     }

//     #[fig_test::test]
//     fn test_settings_raw() {
//         let path = tempfile::tempdir().unwrap().into_path().join("local.json");
//         std::env::set_var("FIG_DIRECTORIES_SETTINGS_PATH", &path);
//         test_store_type(&path, JsonType::Settings);
//     }

//     #[fig_test::test]
//     fn test_state_raw() {
//         let path = tempfile::tempdir().unwrap().into_path().join("local.json");
//         std::env::set_var("FIG_DIRECTORIES_STATE_PATH", &path);
//         test_store_type(&path, JsonType::State);
//     }
// }

#[cfg(test)]
mod file_tests {
    use super::*;

    fn object(json: &str) -> Map {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn a_missing_file_is_created_as_an_empty_object() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/settings.json");
        assert_eq!(read_json_object(&path).unwrap(), Map::new());
        assert_eq!(fs::read_to_string(path).unwrap(), "{}");
    }

    #[test]
    fn an_existing_file_is_never_truncated_by_the_create_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        fs::write(&path, r#"{"a": 1}"#).unwrap();
        assert_eq!(read_json_object(&path).unwrap(), object(r#"{"a": 1}"#));
        assert_eq!(fs::read_to_string(path).unwrap(), r#"{"a": 1}"#);
    }

    #[test]
    fn an_empty_file_reads_as_an_empty_object() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        fs::write(&path, "  \n").unwrap();
        assert_eq!(read_json_object(&path).unwrap(), Map::new());
    }

    /// A store with its own file and statics, so one test owns all of its
    /// global state.
    struct LoadStore {
        inner: Backend,
    }

    static LOAD_STORE_PATH: RwLock<Option<PathBuf>> = RwLock::new(None);
    static LOAD_STORE_FILE_LOCK: RwLock<()> = RwLock::new(());
    static LOAD_STORE_DATA: RwLock<Option<Map>> = RwLock::new(None);
    static LOAD_STORE_FAILED: AtomicBool = AtomicBool::new(false);

    impl JsonStore for LoadStore {
        fn path() -> Result<PathBuf> {
            Ok(LOAD_STORE_PATH.read().clone().expect("the test set a path"))
        }

        fn file_lock() -> &'static RwLock<()> {
            &LOAD_STORE_FILE_LOCK
        }

        fn data_lock() -> &'static RwLock<Option<Map>> {
            &LOAD_STORE_DATA
        }

        fn load_failed() -> &'static AtomicBool {
            &LOAD_STORE_FAILED
        }

        fn new_from_backend(backend: Backend) -> Self {
            Self { inner: backend }
        }

        fn map(&self) -> ReadGuard<'_, Map> {
            match &self.inner {
                Backend::Global => ReadGuard::Global(Self::data_lock().read()),
                Backend::Memory(map) => ReadGuard::Memory(map),
            }
        }

        fn map_mut(&mut self) -> WriteGuard<'_, Map> {
            match &mut self.inner {
                Backend::Global => WriteGuard::Global(Self::data_lock().write()),
                Backend::Memory(map) => WriteGuard::Memory(map),
            }
        }
    }

    /// What `Settings::set_value` does for the real store.
    fn set_in_load_store(key: &str, value: bool) -> Result<()> {
        LoadStore::ensure_loaded()?;
        let mut store = LoadStore::load()?;
        store.set(key, value);
        store.save_to_file()
    }

    /// The desktop app sets a key on every launch. With a typo in a hand-edited
    /// file that write used to save the defaults it was running on over
    /// everything else in the file.
    #[test]
    fn a_file_that_did_not_load_is_not_written_until_it_does() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        *LOAD_STORE_PATH.write() = Some(path.clone());
        let broken = "{\"theme\": \"dark\", \"keep\": 1,}";
        fs::write(&path, broken).unwrap();

        assert!(matches!(LoadStore::load_into_global(), Err(Error::JsonError(_))));
        assert_eq!(LoadStore::load().unwrap().get_string("theme"), None, "runs on defaults");
        assert!(matches!(
            set_in_load_store("app.launchOnStartup", true),
            Err(Error::JsonError(_))
        ));
        // A caller that changes the data without asking first cannot save it either.
        let mut store = LoadStore::load().unwrap();
        store.set("stray", true);
        assert!(matches!(store.save_to_file(), Err(Error::FileNotLoaded)));
        assert_eq!(fs::read_to_string(&path).unwrap(), broken);

        // Once the typo is fixed the file is the data again, and writes resume.
        fs::write(&path, "{\"theme\": \"dark\", \"keep\": 1}").unwrap();
        set_in_load_store("app.launchOnStartup", true).unwrap();
        assert_eq!(
            read_json_object(&path).unwrap(),
            object(r#"{"theme": "dark", "keep": 1, "app.launchOnStartup": true}"#),
            "nothing set while the file was unreadable leaks into it"
        );

        // A watcher that fails to re-read an edit keeps its last good copy for
        // reads, and does not save that copy over the edit.
        let edited = "{\"theme\": \"light\",";
        fs::write(&path, edited).unwrap();
        LoadStore::mark_load_failed();
        assert_eq!(LoadStore::load().unwrap().get_string("theme").as_deref(), Some("dark"));
        assert!(set_in_load_store("other", true).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), edited);
    }

    #[test]
    fn invalid_json_is_an_error_and_is_left_in_place_with_one_backup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let broken = "{\"a\": 1,}";
        fs::write(&path, broken).unwrap();
        assert!(matches!(read_json_object(&path), Err(Error::JsonError(_))));

        let first = back_up_unreadable_json(&path).unwrap();
        let second = back_up_unreadable_json(&path).unwrap();
        assert_eq!(first, second);
        assert_eq!(fs::read_to_string(first).unwrap(), broken);
        assert_eq!(fs::read_to_string(path).unwrap(), broken);
    }

    #[test]
    fn atomic_write_replaces_content_and_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        fs::write(&path, r#"{"old": true}"#).unwrap();
        write_json_object_atomically(&path, &object(r#"{"new": 1}"#)).unwrap();
        assert_eq!(read_json_object(&path).unwrap(), object(r#"{"new": 1}"#));
        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("settings.json")]);
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_keeps_permissions_and_a_symlinked_file() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("dotfiles-settings.json");
        fs::write(&target, "{}").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o640)).unwrap();
        let link = dir.path().join("settings.json");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        write_json_object_atomically(&link, &object(r#"{"a": "b"}"#)).unwrap();

        assert!(fs::symlink_metadata(link).unwrap().file_type().is_symlink());
        assert_eq!(read_json_object(&target).unwrap(), object(r#"{"a": "b"}"#));
        assert_eq!(fs::metadata(target).unwrap().permissions().mode() & 0o777, 0o640);
    }

    #[cfg(unix)]
    #[test]
    fn a_dangling_symlink_gets_its_target_created_and_written() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("dotfiles/settings.json");
        let link = dir.path().join("settings.json");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        assert_eq!(read_json_object(&link).unwrap(), Map::new());
        assert_eq!(fs::read_to_string(&target).unwrap(), "{}");

        write_json_object_atomically(&link, &object(r#"{"a": 1}"#)).unwrap();
        assert!(fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert_eq!(read_json_object(&target).unwrap(), object(r#"{"a": 1}"#));
    }

    #[test]
    fn a_write_removes_only_stale_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let stale = dir.path().join(".settings.json.dead.tmp");
        let live = dir.path().join(".settings.json.live.tmp");
        let unrelated = dir.path().join(".other.json.dead.tmp");
        for file in [&stale, &live, &unrelated] {
            fs::write(file, "").unwrap();
        }
        let old = SystemTime::now() - STALE_TEMP_AGE - Duration::from_secs(1);
        for file in [&stale, &unrelated] {
            File::options()
                .write(true)
                .open(file)
                .unwrap()
                .set_modified(old)
                .unwrap();
        }

        write_json_object_atomically(&path, &Map::new()).unwrap();

        assert!(!stale.exists());
        assert!(live.exists());
        assert!(unrelated.exists());
    }

    #[test]
    fn only_the_newest_invalid_backups_are_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let now = SystemTime::now();
        for age in 1..=5u64 {
            let backup = dir.path().join(format!("settings.json.invalid-old{age}"));
            fs::write(&backup, "x").unwrap();
            File::options()
                .write(true)
                .open(&backup)
                .unwrap()
                .set_modified(now - Duration::from_secs(age * 10))
                .unwrap();
        }
        fs::write(&path, "{").unwrap();

        let backup = back_up_unreadable_json(&path).unwrap();

        let mut kept: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("settings.json.invalid-"))
            .collect();
        kept.sort();
        let mut expected = vec![
            backup.file_name().unwrap().to_string_lossy().into_owned(),
            "settings.json.invalid-old1".to_owned(),
            "settings.json.invalid-old2".to_owned(),
        ];
        expected.sort();
        assert_eq!(kept, expected);
    }

    #[cfg(unix)]
    #[test]
    fn a_new_file_is_private() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        write_json_object_atomically(&path, &Map::new()).unwrap();
        assert_eq!(fs::metadata(path).unwrap().permissions().mode() & 0o777, 0o600);
    }
}
