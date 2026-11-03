//! A generation-aware view of one published `specs-ir` directory.
//!
//! The publisher replaces the directory at a canonical path. A lazy reader
//! must not resolve that path and blindly accept whichever generation happens
//! to be there. The snapshot records the relative tree and SHA-256 of every
//! regular file while opening the directory once. Unix lazy reads use a fresh
//! `openat` walk rooted at the canonical directory with `O_NOFOLLOW`, then
//! compare the bytes with the recorded digest. A replacement therefore either
//! reads identical content or fails closed and marks the snapshot stale; it
//! can never return a different generation's bytes.

use std::any::Any;
use std::collections::HashMap;
use std::fmt;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use sha2::{Digest, Sha256};

#[cfg(unix)]
use std::ffi::{CStr, CString};
#[cfg(unix)]
use std::fs::File;
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
#[cfg(unix)]
use std::os::unix::ffi::{OsStrExt, OsStringExt};

/// Files the opener already read while hashing the tree. The first hook
/// dispatch takes `typed-hooks.json`, avoiding a second read/hash. A request
/// that needs no hooks releases it and later dispatches use verified reads.
const CAPTURED_ON_OPEN: &[&str] = &["typed-hooks.json"];
const HASH_READ_BUFFER_SIZE: usize = 64 * 1024;

/// One opened generation of the IR directory. Cloning this value is cheap and
/// shares stale state with every Registry/NativeHooks clone.
#[derive(Clone, Debug)]
pub(crate) struct DirectorySnapshot {
    inner: Arc<SnapshotInner>,
}

#[derive(Debug)]
struct SnapshotInner {
    display_path: PathBuf,
    entries: HashMap<PathBuf, EntryKind>,
    file_digests: HashMap<PathBuf, String>,
    captured_files: CapturedFiles,
    generation: Generation,
    stale: AtomicBool,
    sidecar_index: SidecarIndexSlot,
}

/// One-shot bodies captured while hashing. Debug prints names only.
struct CapturedFiles(Mutex<HashMap<PathBuf, Vec<u8>>>);

/// One index over a sidecar, built on first use and kept for this
/// generation, so owners that come and go (the engine retires its hook owner
/// after every idle grace) do not rescan the file. `None` records a failed
/// build: callers fall back to whole-file reads.
#[derive(Default)]
struct SidecarIndexSlot(OnceLock<Option<Arc<dyn Any + Send + Sync>>>);

impl fmt::Debug for SidecarIndexSlot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self.0.get() {
            None => "<unbuilt>",
            Some(None) => "<unavailable>",
            Some(Some(_)) => "<built>",
        })
    }
}

/// A file read and verified against this generation's digest, kept open for
/// ranged reads of that same inode. A read refuses once the inode's size or
/// change times moved, which an in-place write cannot avoid.
#[cfg(unix)]
pub(crate) struct PinnedFile {
    file: File,
    identity: FileIdentity,
}

#[cfg(unix)]
#[derive(Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    inode: u64,
    size: i64,
    modified: (i64, i64),
    changed: (i64, i64),
}

#[cfg(unix)]
impl FileIdentity {
    fn of(stat: &libc::stat) -> Self {
        Self {
            inode: stat.st_ino,
            size: stat.st_size,
            modified: (stat.st_mtime, stat.st_mtime_nsec),
            changed: (stat.st_ctime, stat.st_ctime_nsec),
        }
    }
}

#[cfg(unix)]
impl PinnedFile {
    /// The inode still has the size and change times it had when verified.
    pub(crate) fn is_intact(&self) -> bool {
        fstat_fd(self.file.as_raw_fd()).is_ok_and(|stat| FileIdentity::of(&stat) == self.identity)
    }

    pub(crate) fn read_range(&self, range: std::ops::Range<usize>) -> io::Result<Vec<u8>> {
        use std::os::unix::fs::FileExt;
        if !self.is_intact() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "pinned sidecar changed after it was verified",
            ));
        }
        let mut bytes = vec![0; range.len()];
        self.file.read_exact_at(&mut bytes, range.start as u64)?;
        Ok(bytes)
    }
}

/// Release unused captured bytes on every request exit, including cancellation
/// or unwind. The shared snapshot keeps its generation and file digests.
pub(crate) struct CapturedFilesGuard(DirectorySnapshot);

impl Drop for CapturedFilesGuard {
    fn drop(&mut self) {
        self.0.release_captured_files();
    }
}

impl fmt::Debug for CapturedFiles {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0.lock() {
            Ok(map) => f.debug_list().entries(map.keys()).finish(),
            Err(_) => f.write_str("<poisoned>"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Generation {
    #[cfg(unix)]
    root_identity: RootIdentity,
    #[cfg(not(unix))]
    canonical_path: PathBuf,
    marker_digest: Option<String>,
}

#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RootIdentity {
    device: u64,
    inode: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EntryKind {
    File,
    Directory,
}

impl DirectorySnapshot {
    /// Open and validate the directory shape while recording a digest for
    /// every regular file. No per-file descriptor survives this call.
    pub(crate) fn open(path: &Path) -> io::Result<Self> {
        #[cfg(unix)]
        {
            let root_fd = open_root(path)?;
            let root_stat = fstat_fd(root_fd.as_raw_fd())?;
            let mut entries = HashMap::new();
            entries.insert(PathBuf::new(), EntryKind::Directory);
            let mut file_digests = HashMap::new();
            let mut captured_files = HashMap::new();
            capture_tree(
                root_fd.as_raw_fd(),
                Path::new(""),
                &mut entries,
                &mut file_digests,
                &mut captured_files,
            )?;
            let marker_digest = file_digests.get(Path::new(".spec-pair.json")).cloned();
            Ok(Self {
                inner: Arc::new(SnapshotInner {
                    display_path: path.to_path_buf(),
                    entries,
                    file_digests,
                    captured_files: CapturedFiles(Mutex::new(captured_files)),
                    generation: Generation {
                        root_identity: root_identity(&root_stat),
                        marker_digest,
                    },
                    stale: AtomicBool::new(false),
                    sidecar_index: SidecarIndexSlot::default(),
                }),
            })
        }

        #[cfg(not(unix))]
        {
            let metadata = std::fs::symlink_metadata(path)?;
            if !metadata.is_dir() {
                return Err(io::Error::new(
                    io::ErrorKind::NotADirectory,
                    format!("{} is not a directory", path.display()),
                ));
            }
            let canonical_path = std::fs::canonicalize(path)?;
            let mut entries = HashMap::new();
            entries.insert(PathBuf::new(), EntryKind::Directory);
            let mut file_digests = HashMap::new();
            let mut captured_files = HashMap::new();
            capture_tree_path(
                path,
                Path::new(""),
                &mut entries,
                &mut file_digests,
                &mut captured_files,
            )?;
            let marker_digest = file_digests.get(Path::new(".spec-pair.json")).cloned();
            Ok(Self {
                inner: Arc::new(SnapshotInner {
                    display_path: path.to_path_buf(),
                    entries,
                    file_digests,
                    captured_files: CapturedFiles(Mutex::new(captured_files)),
                    generation: Generation {
                        canonical_path,
                        marker_digest,
                    },
                    stale: AtomicBool::new(false),
                    sidecar_index: SidecarIndexSlot::default(),
                }),
            })
        }
    }

    pub(crate) fn display_path(&self) -> &Path {
        &self.inner.display_path
    }

    /// Read a regular file from the canonical directory and verify that it is
    /// still the file recorded when this generation was opened.
    #[allow(clippy::verbose_file_reads)]
    pub(crate) fn read_file(&self, relative: &Path) -> io::Result<Vec<u8>> {
        validate_relative_path(relative)?;
        let Some(expected) = self.inner.file_digests.get(relative) else {
            self.mark_stale();
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("snapshot file not found: {}", relative.display()),
            ));
        };

        #[cfg(unix)]
        let bytes = {
            let root_fd = match open_root(&self.inner.display_path) {
                Ok(fd) => fd,
                Err(error) => {
                    self.mark_stale();
                    return Err(error);
                },
            };
            let file = match open_relative(root_fd.as_raw_fd(), relative) {
                Ok(file) => file,
                Err(error) => {
                    self.mark_stale();
                    return Err(error);
                },
            };
            let metadata = match fstat_fd(file.as_raw_fd()) {
                Ok(metadata) => metadata,
                Err(error) => {
                    self.mark_stale();
                    return Err(error);
                },
            };
            if !is_regular_file(&metadata) {
                self.mark_stale();
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{} is not a regular file", relative.display()),
                ));
            }
            let mut file = File::from(file);
            let mut bytes = Vec::new();
            if let Err(error) = file.read_to_end(&mut bytes) {
                self.mark_stale();
                return Err(error);
            }
            bytes
        };

        #[cfg(not(unix))]
        let bytes = {
            if std::fs::canonicalize(&self.inner.display_path).ok().as_deref()
                != Some(&self.inner.generation.canonical_path)
            {
                self.mark_stale();
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "specs directory generation was replaced",
                ));
            }
            let path = self.inner.display_path.join(relative);
            let metadata = match std::fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) => {
                    self.mark_stale();
                    return Err(error);
                },
            };
            if !metadata.is_file() {
                self.mark_stale();
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{} is not a regular file", relative.display()),
                ));
            }
            match std::fs::read(path) {
                Ok(bytes) => bytes,
                Err(error) => {
                    self.mark_stale();
                    return Err(error);
                },
            }
        };

        if sha256_hex(&bytes) != *expected {
            self.mark_stale();
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("snapshot digest mismatch: {}", relative.display()),
            ));
        }
        Ok(bytes)
    }

    /// Read a file that may legitimately be absent from an older generation
    /// (for example `typed-hooks.json` before the sidecar existed). An
    /// unexpected present file is not consumed; the next request will refresh
    /// the generation.
    pub(crate) fn read_optional_file(&self, relative: &Path) -> io::Result<Option<Vec<u8>>> {
        validate_relative_path(relative)?;
        if !self.inner.file_digests.contains_key(relative) {
            return Ok(None);
        }
        self.read_file(relative).map(Some)
    }

    /// Take a body that `open` hashed and kept. Later reads go back to disk
    /// and still compare against the digest recorded at open.
    pub(crate) fn take_captured_file(&self, relative: &Path) -> Option<Vec<u8>> {
        self.inner.captured_files.0.lock().ok()?.remove(relative)
    }

    pub(crate) fn release_captured_after_request(self) -> CapturedFilesGuard {
        CapturedFilesGuard(self)
    }

    pub(crate) fn release_captured_files(&self) {
        let captured = std::mem::take(
            &mut *self
                .inner
                .captured_files
                .0
                .lock()
                .unwrap_or_else(|error| error.into_inner()),
        );
        drop(captured);
    }

    pub(crate) fn captured_bytes(&self) -> usize {
        self.inner
            .captured_files
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .values()
            .map(Vec::capacity)
            .sum()
    }

    /// Return names directly below a directory from the captured tree. This
    /// remains available even when the canonical directory is in a rename or
    /// cleanup window.
    pub(crate) fn read_dir(&self, relative: &Path) -> io::Result<Vec<std::ffi::OsString>> {
        validate_relative_path(relative)?;
        if self.kind(relative)? != EntryKind::Directory {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                format!("{} is not a directory", relative.display()),
            ));
        }
        let mut names = Vec::new();
        for path in self.inner.entries.keys() {
            if path.as_os_str().is_empty() || path == relative {
                continue;
            }
            if path.parent().unwrap_or_else(|| Path::new("")) == relative
                && let Some(name) = path.file_name()
            {
                names.push(name.to_os_string());
            }
        }
        Ok(names)
    }

    pub(crate) fn kind(&self, relative: &Path) -> io::Result<EntryKind> {
        validate_relative_path(relative)?;
        self.inner.entries.get(relative).copied().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("snapshot entry not found: {}", relative.display()),
            )
        })
    }

    pub(crate) fn is_file(&self, relative: &Path) -> bool {
        matches!(self.kind(relative), Ok(EntryKind::File))
    }

    /// The index this generation built over a sidecar, built by `build` on
    /// first use. Every owner of the snapshot shares it.
    pub(crate) fn sidecar_index<T: Any + Send + Sync>(&self, build: impl FnOnce() -> Option<T>) -> Option<Arc<T>> {
        let built = self
            .inner
            .sidecar_index
            .0
            .get_or_init(|| build().map(|index| Arc::new(index) as Arc<dyn Any + Send + Sync>));
        Arc::clone(built.as_ref()?).downcast::<T>().ok()
    }

    /// [`Self::read_file`], keeping the descriptor for ranged reads of the
    /// verified inode. Failures mark the snapshot stale, as reads do.
    #[cfg(unix)]
    pub(crate) fn pin_file(&self, relative: &Path) -> io::Result<(PinnedFile, Vec<u8>)> {
        validate_relative_path(relative)?;
        let Some(expected) = self.inner.file_digests.get(relative) else {
            self.mark_stale();
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("snapshot file not found: {}", relative.display()),
            ));
        };
        let pinned = self.read_pinned(relative, expected);
        if pinned.is_err() {
            self.mark_stale();
        }
        pinned
    }

    #[cfg(unix)]
    #[allow(clippy::verbose_file_reads)]
    fn read_pinned(&self, relative: &Path, expected: &str) -> io::Result<(PinnedFile, Vec<u8>)> {
        let root_fd = open_root(&self.inner.display_path)?;
        let fd = open_relative(root_fd.as_raw_fd(), relative)?;
        let metadata = fstat_fd(fd.as_raw_fd())?;
        if !is_regular_file(&metadata) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{} is not a regular file", relative.display()),
            ));
        }
        let mut file = File::from(fd);
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        if sha256_hex(&bytes) != expected {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("snapshot digest mismatch: {}", relative.display()),
            ));
        }
        let identity = FileIdentity::of(&metadata);
        Ok((PinnedFile { file, identity }, bytes))
    }

    /// Iterate over the digests captured from the opened directory generation.
    /// Pair verification uses these values instead of reopening and hashing the
    /// whole tree a second time. Lazy reads still verify their bytes against the
    /// same map before returning them.
    pub(crate) fn file_digests(&self) -> impl Iterator<Item = (&Path, &str)> {
        self.inner
            .file_digests
            .iter()
            .map(|(path, digest)| (path.as_path(), digest.as_str()))
    }

    pub(crate) fn mark_stale(&self) {
        self.inner.stale.store(true, Ordering::Release);
    }

    pub(crate) fn is_stale(&self) -> bool {
        self.inner.stale.load(Ordering::Acquire)
    }

    /// Detect an atomic replacement (or an in-place marker update) before a
    /// request starts. A changed generation is refreshable; the current
    /// request may still use identical files, while different files fail in
    /// `read_file` and force the next request to rebuild.
    pub(crate) fn generation_changed(&self) -> bool {
        #[cfg(unix)]
        {
            let current = open_root(&self.inner.display_path)
                .and_then(|fd| fstat_fd(fd.as_raw_fd()).map(|stat| root_identity(&stat)));
            if current.as_ref().ok() != Some(&self.inner.generation.root_identity) {
                return true;
            }
        }

        #[cfg(not(unix))]
        if std::fs::canonicalize(&self.inner.display_path).ok().as_deref()
            != Some(&self.inner.generation.canonical_path)
        {
            return true;
        }

        let Some(expected) = self.inner.generation.marker_digest.as_ref() else {
            return false;
        };
        match self.read_file(Path::new(".spec-pair.json")) {
            Ok(bytes) => sha256_hex(&bytes) != *expected,
            Err(_) => true,
        }
    }
}

#[cfg(unix)]
fn open_root(path: &Path) -> io::Result<OwnedFd> {
    let bytes = path.as_os_str().as_bytes();
    let path = CString::new(bytes).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("directory path contains NUL: {error}"),
        )
    })?;
    let flags = libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_DIRECTORY;
    let fd = unsafe { libc::open(path.as_ptr(), flags) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

#[cfg(unix)]
fn open_relative(parent_fd: i32, relative: &Path) -> io::Result<OwnedFd> {
    let mut current = duplicate_fd(parent_fd)?;
    let components = relative_components(relative)?;
    for (index, component) in components.iter().enumerate() {
        let name = CString::new(component.as_slice()).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("relative path contains NUL: {error}"),
            )
        })?;
        // The final entry may have been replaced by a FIFO since capture.
        // Open without waiting for a writer, then validate the opened object
        // with fstat in read_file; a pre-open path check would race replacement.
        let mut flags = libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK;
        if index + 1 < components.len() {
            flags |= libc::O_DIRECTORY;
        }
        let fd = unsafe { libc::openat(current.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        current = unsafe { OwnedFd::from_raw_fd(fd) };
    }
    Ok(current)
}

/// Files opened by the walk and waiting for a hashing thread. Bounds the
/// descriptors held at once beyond the directory chain.
#[cfg(unix)]
const HASH_QUEUE: usize = 16;

/// Hashing runs on up to this many threads while the walk continues.
#[cfg(unix)]
const MAX_HASH_THREADS: usize = 4;

#[cfg(unix)]
type HashedFile = (PathBuf, String, Option<Vec<u8>>);

/// Walk the tree on this thread and hash its files on a few others. Opening
/// a generation digested all ~880 IR files one after another: 126 ms of
/// the first completion after launch on CI. The walk still opens and
/// checks every entry in order; only reading and hashing are spread out.
#[cfg(unix)]
fn capture_tree(
    root_fd: i32,
    current: &Path,
    entries: &mut HashMap<PathBuf, EntryKind>,
    file_digests: &mut HashMap<PathBuf, String>,
    captured_files: &mut HashMap<PathBuf, Vec<u8>>,
) -> io::Result<()> {
    let threads = std::thread::available_parallelism().map_or(1, |count| count.get().min(MAX_HASH_THREADS));
    let (jobs, queued) = std::sync::mpsc::sync_channel::<(PathBuf, OwnedFd)>(HASH_QUEUE);
    let (hashed, results) = std::sync::mpsc::channel::<io::Result<HashedFile>>();
    let queued = Mutex::new(queued);
    std::thread::scope(|scope| {
        for _ in 0..threads {
            let hashed = hashed.clone();
            let queued = &queued;
            scope.spawn(move || {
                loop {
                    let next = queued.lock().unwrap_or_else(|error| error.into_inner()).recv();
                    let Ok((relative, fd)) = next else {
                        break;
                    };
                    if hashed.send(hash_file(relative, fd)).is_err() {
                        break;
                    }
                }
            });
        }
        drop(hashed);
        let walked = walk_tree(root_fd, current, entries, &jobs);
        drop(jobs);
        let mut failure = walked.err();
        for result in results {
            match result {
                Ok((relative, digest, captured)) => {
                    entries.insert(relative.clone(), EntryKind::File);
                    if let Some(bytes) = captured {
                        captured_files.insert(relative.clone(), bytes);
                    }
                    file_digests.insert(relative, digest);
                },
                Err(error) => {
                    failure.get_or_insert(error);
                },
            }
        }
        failure.map_or(Ok(()), Err)
    })
}

#[cfg(unix)]
fn hash_file(relative: PathBuf, fd: OwnedFd) -> io::Result<HashedFile> {
    let mut file = File::from(fd);
    let mut captured = HashMap::new();
    let digest = capture_file_digest(&mut file, &relative, &mut captured)?;
    let bytes = captured.remove(&relative);
    Ok((relative, digest, bytes))
}

#[cfg(unix)]
fn walk_tree(
    parent_fd: i32,
    current: &Path,
    entries: &mut HashMap<PathBuf, EntryKind>,
    jobs: &std::sync::mpsc::SyncSender<(PathBuf, OwnedFd)>,
) -> io::Result<()> {
    for name in read_dir_fd(parent_fd)? {
        let bytes = name.as_os_str().as_bytes();
        let c_name = CString::new(bytes).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("directory entry contains NUL: {error}"),
            )
        })?;
        let fd = unsafe {
            libc::openat(
                parent_fd,
                c_name.as_ptr(),
                // Inspect the opened object before reading it. In particular,
                // an unrelated FIFO must not block the entire tree capture.
                libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let metadata = fstat_fd(fd.as_raw_fd())?;
        let relative = current.join(&name);
        if is_directory(&metadata) {
            entries.insert(relative.clone(), EntryKind::Directory);
            walk_tree(fd.as_raw_fd(), &relative, entries, jobs)?;
        } else if is_regular_file(&metadata) {
            if jobs.send((relative, fd)).is_err() {
                return Err(io::Error::other("snapshot hashing threads stopped"));
            }
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "snapshot contains a special entry",
            ));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn read_dir_fd(fd: i32) -> io::Result<Vec<std::ffi::OsString>> {
    let duplicate = duplicate_fd(fd)?;
    let raw = duplicate.into_raw_fd();
    let dir = unsafe { libc::fdopendir(raw) };
    if dir.is_null() {
        unsafe {
            libc::close(raw);
        }
        return Err(io::Error::last_os_error());
    }
    let mut names = Vec::new();
    loop {
        // POSIX reports both end-of-directory and errors as a null pointer.
        // Clear errno immediately before each call so a null result can be
        // classified without inheriting an unrelated earlier error.
        nix::errno::Errno::clear();
        let entry = unsafe { libc::readdir(dir) };
        if entry.is_null() {
            let errno = nix::errno::Errno::last_raw();
            if errno != 0 {
                unsafe {
                    libc::closedir(dir);
                }
                return Err(io::Error::from_raw_os_error(errno));
            }
            break;
        }
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if name != b"." && name != b".." {
            names.push(std::ffi::OsString::from_vec(name.to_vec()));
        }
    }
    unsafe {
        libc::closedir(dir);
    }
    Ok(names)
}

#[cfg(unix)]
fn duplicate_fd(fd: i32) -> io::Result<OwnedFd> {
    let duplicate = unsafe { libc::dup(fd) };
    if duplicate < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(duplicate) })
}

#[cfg(unix)]
fn fstat_fd(fd: i32) -> io::Result<libc::stat> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { stat.assume_init() })
}

#[cfg(unix)]
fn root_identity(stat: &libc::stat) -> RootIdentity {
    RootIdentity {
        // `st_dev` is already `u64` on Linux and a narrower type on some other Unix
        // targets. Keep the cast so the identity stays a `u64` everywhere.
        #[allow(clippy::unnecessary_cast)]
        device: stat.st_dev as u64,
        inode: stat.st_ino,
    }
}

#[cfg(unix)]
fn is_regular_file(stat: &libc::stat) -> bool {
    (stat.st_mode & libc::S_IFMT) == libc::S_IFREG
}

#[cfg(unix)]
fn is_directory(stat: &libc::stat) -> bool {
    (stat.st_mode & libc::S_IFMT) == libc::S_IFDIR
}

#[cfg(unix)]
fn relative_components(path: &Path) -> io::Result<Vec<Vec<u8>>> {
    let mut components = Vec::new();
    for component in path.components() {
        let std::path::Component::Normal(name) = component else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unsafe relative path: {}", path.display()),
            ));
        };
        let bytes = name.as_bytes();
        if bytes.is_empty() || bytes == b"." || bytes == b".." || bytes.contains(&0) || bytes.contains(&b'/') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unsafe relative path: {}", path.display()),
            ));
        }
        components.push(bytes.to_vec());
    }
    Ok(components)
}

#[cfg(not(unix))]
fn capture_tree_path(
    root: &Path,
    current: &Path,
    entries: &mut HashMap<PathBuf, EntryKind>,
    file_digests: &mut HashMap<PathBuf, String>,
    captured_files: &mut HashMap<PathBuf, Vec<u8>>,
) -> io::Result<()> {
    for entry in std::fs::read_dir(root.join(current))? {
        let entry = entry?;
        let relative = current.join(entry.file_name());
        let metadata = std::fs::symlink_metadata(entry.path())?;
        if metadata.is_dir() {
            entries.insert(relative.clone(), EntryKind::Directory);
            capture_tree_path(root, &relative, entries, file_digests, captured_files)?;
        } else if metadata.is_file() {
            let mut file = std::fs::File::open(entry.path())?;
            let digest = capture_file_digest(&mut file, &relative, captured_files)?;
            entries.insert(relative.clone(), EntryKind::File);
            file_digests.insert(relative, digest);
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "snapshot contains a special entry",
            ));
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_relative_path(path: &Path) -> io::Result<()> {
    for component in path.components() {
        if !matches!(component, std::path::Component::Normal(_)) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unsafe relative path: {}", path.display()),
            ));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn validate_relative_path(path: &Path) -> io::Result<()> {
    relative_components(path).map(|_| ())
}

fn should_capture_on_open(relative: &Path) -> bool {
    CAPTURED_ON_OPEN.iter().any(|name| relative == Path::new(name))
}

fn capture_file_digest(
    file: &mut impl Read,
    relative: &Path,
    captured_files: &mut HashMap<PathBuf, Vec<u8>>,
) -> io::Result<String> {
    if should_capture_on_open(relative) {
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        let digest = sha256_hex(&bytes);
        captured_files.insert(relative.to_path_buf(), bytes);
        Ok(digest)
    } else {
        sha256_reader(file)
    }
}

/// Hash through EOF without retaining ordinary spec bodies during startup.
// This bounded 64 KiB scratch buffer replaces allocations as large as a spec.
#[allow(clippy::large_stack_arrays)]
fn sha256_reader(reader: &mut impl Read) -> io::Result<String> {
    let mut digest = Sha256::new();
    let mut buffer = [0; HASH_READ_BUFFER_SIZE];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => return Ok(digest_hex(&digest.finalize())),
            Ok(count) => digest.update(&buffer[..count]),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {},
            Err(error) => return Err(error),
        }
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    digest_hex(&Sha256::digest(bytes))
}

fn digest_hex(digest: &[u8]) -> String {
    let mut output = String::with_capacity(64);
    for byte in digest {
        output.push(char::from(b"0123456789abcdef"[(byte >> 4) as usize]));
        output.push(char::from(b"0123456789abcdef"[(byte & 0x0f) as usize]));
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn fifo(path: &Path) {
        let path = CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: the fixture path is a valid, terminated string.
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
    }

    #[cfg(unix)]
    fn rejects_fifo_without_a_writer(path: &Path, read: impl FnOnce() -> io::Result<()> + Send + 'static) {
        use std::os::unix::fs::OpenOptionsExt;
        use std::sync::mpsc;
        use std::time::Duration;

        let (tx, rx) = mpsc::channel();
        let reader = std::thread::spawn(move || tx.send(read()).unwrap());
        let result = rx.recv_timeout(Duration::from_secs(2));
        // Unblock a regressed blocking open before failing, so the test does
        // not leave a thread or FIFO behind for the rest of the suite.
        let writer = result.is_err().then(|| {
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(path)
                .unwrap()
        });
        reader.join().unwrap();
        drop(writer);
        assert_eq!(
            result.expect("FIFO open waited for a writer").unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_rejects_unrelated_fifo_before_reading_it() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("unrelated-file");
        fifo(&path);
        let directory = root.path().to_path_buf();
        rejects_fifo_without_a_writer(&path, move || DirectorySnapshot::open(&directory).map(|_| ()));
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_rejects_a_regular_file_replaced_with_fifo_on_lazy_read() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("git.json");
        std::fs::write(&path, "{}").unwrap();
        let snapshot = DirectorySnapshot::open(root.path()).unwrap();
        let observer = snapshot.clone();
        std::fs::remove_file(&path).unwrap();
        fifo(&path);
        rejects_fifo_without_a_writer(&path, move || snapshot.read_file(Path::new("git.json")).map(|_| ()));
        assert!(observer.is_stale());
    }

    #[cfg(unix)]
    fn open_fd_count() -> Option<usize> {
        std::fs::read_dir("/dev/fd").ok().map(|entries| entries.count())
    }

    #[test]
    #[cfg(unix)]
    fn snapshot_does_not_retain_one_descriptor_per_file() {
        let root = tempfile::tempdir().expect("snapshot root");
        for index in 0..256 {
            std::fs::write(root.path().join(format!("{index}.json")), format!("{index}\n")).expect("snapshot file");
        }
        let Some(before) = open_fd_count() else {
            return;
        };
        let snapshot = DirectorySnapshot::open(root.path()).expect("snapshot");
        let Some(during) = open_fd_count() else {
            drop(snapshot);
            return;
        };
        drop(snapshot);

        // DirectorySnapshot uses transient openat descriptors while capturing
        // the digest map. None may remain proportional to the file count.
        assert!(
            during.saturating_sub(before) < 64,
            "snapshot retained too many descriptors: before={before}, during={during}"
        );
    }

    #[test]
    fn streamed_digest_matches_empty_and_multi_block_inputs() {
        for size in [
            0,
            1,
            HASH_READ_BUFFER_SIZE - 1,
            HASH_READ_BUFFER_SIZE,
            HASH_READ_BUFFER_SIZE * 2 + 17,
        ] {
            let bytes: Vec<u8> = (0..size).map(|index| u8::try_from(index % 251).unwrap()).collect();
            assert_eq!(sha256_reader(&mut bytes.as_slice()).unwrap(), sha256_hex(&bytes));
        }
    }

    #[test]
    fn streamed_digest_retries_interruptions_and_accepts_short_reads() {
        struct ShortReader<'a> {
            bytes: &'a [u8],
            interrupt_next: bool,
            interruptions: usize,
        }

        impl Read for ShortReader<'_> {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                assert_eq!(buffer.len(), HASH_READ_BUFFER_SIZE);
                if self.interrupt_next {
                    self.interrupt_next = false;
                    self.interruptions += 1;
                    return Err(io::ErrorKind::Interrupted.into());
                }
                self.interrupt_next = true;
                let count = self.bytes.len().min(7);
                buffer[..count].copy_from_slice(&self.bytes[..count]);
                self.bytes = &self.bytes[count..];
                Ok(count)
            }
        }

        let bytes = b"a short read is not EOF, and an interruption is not a failed snapshot";
        let mut reader = ShortReader {
            bytes,
            interrupt_next: true,
            interruptions: 0,
        };
        assert_eq!(sha256_reader(&mut reader).unwrap(), sha256_hex(bytes));
        assert!(reader.interruptions > 1);
        assert!(reader.bytes.is_empty());
    }

    #[test]
    fn streamed_digest_propagates_an_error_after_partial_input() {
        struct FailingReader(bool);

        impl Read for FailingReader {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                if std::mem::take(&mut self.0) {
                    buffer[..3].copy_from_slice(b"abc");
                    return Ok(3);
                }
                Err(io::Error::new(io::ErrorKind::PermissionDenied, "fixture read failure"))
            }
        }

        let error = sha256_reader(&mut FailingReader(true)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(error.to_string(), "fixture read failure");
    }

    #[test]
    fn open_hashes_large_uncaptured_files_and_rejects_later_changes() {
        let root = tempfile::tempdir().expect("snapshot root");
        let bytes = vec![b'x'; HASH_READ_BUFFER_SIZE * 2 + 17];
        std::fs::write(root.path().join("large.json"), &bytes).unwrap();
        let snapshot = DirectorySnapshot::open(root.path()).unwrap();
        assert!(snapshot.take_captured_file(Path::new("large.json")).is_none());
        assert_eq!(snapshot.read_file(Path::new("large.json")).unwrap(), bytes);

        std::fs::write(root.path().join("large.json"), b"changed").unwrap();
        let error = snapshot.read_file(Path::new("large.json")).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(snapshot.is_stale());
    }

    #[test]
    fn open_keeps_sidecar_bytes_until_taken() {
        let root = tempfile::tempdir().expect("snapshot root");
        let sidecar = br#"{"hooks":[]}"#;
        std::fs::write(root.path().join("typed-hooks.json"), sidecar).expect("sidecar");
        std::fs::write(root.path().join("other.json"), b"{}\n").expect("other");

        let snapshot = DirectorySnapshot::open(root.path()).expect("snapshot");
        assert!(snapshot.take_captured_file(Path::new("other.json")).is_none());
        let taken = snapshot
            .take_captured_file(Path::new("typed-hooks.json"))
            .expect("captured sidecar");
        assert_eq!(taken, sidecar);
        assert!(snapshot.take_captured_file(Path::new("typed-hooks.json")).is_none());
        assert_eq!(
            snapshot.read_file(Path::new("typed-hooks.json")).expect("disk read"),
            sidecar
        );
    }
}
