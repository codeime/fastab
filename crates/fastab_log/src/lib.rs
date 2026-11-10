use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use thiserror::Error;
use tracing::info;
use tracing::level_filters::LevelFilter;
use tracing_appender::non_blocking::{NonBlocking, NonBlockingBuilder, WorkerGuard};
use tracing_subscriber::filter::Directive;
use tracing_subscriber::prelude::*;
use tracing_subscriber::{EnvFilter, Registry, fmt};

mod rolling;

const DEFAULT_FILTER: LevelFilter = LevelFilter::ERROR;
// Slots stay allocated even when empty. The upstream 128,000-slot default
// costs about 3.9 MiB per writer on arm64; file + stdout need only 256 KiB here.
const BUFFERED_LINES_LIMIT: usize = 4096;

static Q_LOG_LEVEL_GLOBAL: Mutex<Option<String>> = Mutex::new(None);
static MAX_LEVEL: Mutex<Option<LevelFilter>> = Mutex::new(None);
static ENV_FILTER_RELOADABLE_HANDLE: Mutex<Option<tracing_subscriber::reload::Handle<EnvFilter, Registry>>> =
    Mutex::new(None);

// A logging error
#[derive(Debug, Error)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    TracingReload(#[from] tracing_subscriber::reload::Error),
}

/// Arguments to the initialize_logging function
#[derive(Debug)]
pub struct LogArgs<T: AsRef<Path>> {
    /// The log level to use. When not set, the default log level is used.
    pub log_level: Option<String>,
    /// Whether or not we log to stdout.
    pub log_to_stdout: bool,
    /// The log file path which we write logs to. When not set, we do not write to a file.
    pub log_file_path: Option<T>,
    /// Whether we should delete the log file at each launch.
    pub delete_old_log_file: bool,
}

/// The log guard maintains tracing guards which send log information to other threads.
///
/// This must be kept alive for logging to function as expected.
#[must_use]
#[derive(Debug)]
pub struct LogGuard {
    _file_guard: Option<WorkerGuard>,
    _stdout_guard: Option<WorkerGuard>,
}

fn non_blocking_writer(writer: impl std::io::Write + Send + 'static) -> (NonBlocking, WorkerGuard) {
    NonBlockingBuilder::default()
        .buffered_lines_limit(BUFFERED_LINES_LIMIT)
        // Never block the UI or PTY on a slow log sink; retain the existing
        // lossy behavior while bounding the permanently allocated queue.
        .lossy(true)
        .finish(writer)
}

/// Initialize our application level logging using the given LogArgs.
///
/// # Returns
///
/// On success, this returns a guard which must be kept alive.
#[inline]
pub fn initialize_logging<T: AsRef<Path>>(args: LogArgs<T>) -> Result<LogGuard, Error> {
    let filter_layer = create_filter_layer();
    let (reloadable_filter_layer, reloadable_handle) = tracing_subscriber::reload::Layer::new(filter_layer);
    ENV_FILTER_RELOADABLE_HANDLE.lock().unwrap().replace(reloadable_handle);

    // First we construct the file logging layer if a file name was provided.
    let (file_layer, _file_guard) = match args.log_file_path {
        Some(log_file_path) => {
            let log_path = log_file_path.as_ref();

            let file = rolling::RollingFile::new(log_path, args.delete_old_log_file)?;
            let (non_blocking, guard) = non_blocking_writer(file);
            let file_layer = fmt::layer().with_line_number(true).with_writer(non_blocking);

            (Some(file_layer), Some(guard))
        },
        None => (None, None),
    };

    // If we log to stdout, we need to add this layer to our logger.
    let (stdout_layer, _stdout_guard) = if args.log_to_stdout {
        let (non_blocking, guard) = non_blocking_writer(std::io::stdout());
        let stdout_layer = fmt::layer().with_line_number(true).with_writer(non_blocking);
        (Some(stdout_layer), Some(guard))
    } else {
        (None, None)
    };

    if let Some(level) = args.log_level {
        set_log_level(level)?;
    }

    // Finally, initialize our logging
    tracing_subscriber::registry()
        .with(reloadable_filter_layer)
        .with(file_layer)
        .with(stdout_layer)
        .init();

    Ok(LogGuard {
        _file_guard,
        _stdout_guard,
    })
}

/// Writes one ERROR record to the log file at `path`, on the calling thread.
///
/// Every other record goes through a writer thread, so one made just before
/// the process dies is still queued when it does. A panic hook uses this: the
/// shipped binaries abort on a panic, and their stderr is `/dev/null` under
/// LaunchServices, so nothing else would say what happened.
pub fn write_error_now(path: &Path, message: &str) -> std::io::Result<()> {
    use std::io::Write as _;

    let mut file = rolling::RollingFile::new(path, false)?;
    let record = format!("{} ERROR {message}\n", utc_timestamp(SystemTime::now()));
    file.write_all(record.as_bytes())
}

/// `2026-11-10T08:15:30Z`, from the standard library alone.
fn utc_timestamp(time: SystemTime) -> String {
    let seconds = time.duration_since(UNIX_EPOCH).map_or(0, |since| since.as_secs());
    let second_of_day = seconds % 86_400;
    // Days since 1970-01-01 to a calendar date: Howard Hinnant's
    // `civil_from_days`, for dates at or after the epoch.
    let days = i64::try_from(seconds / 86_400).unwrap_or(0) + 719_468;
    let era = days / 146_097;
    let day_of_era = days % 146_097;
    let year_of_era = (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        second_of_day / 3_600,
        second_of_day % 3_600 / 60,
        second_of_day % 60
    )
}

/// Get the current log level by first seeing if it is set in application, then environment, then
/// otherwise using the default
///
/// # Returns
///
/// Returns a string identifying the current log level.
pub fn get_log_level() -> String {
    Q_LOG_LEVEL_GLOBAL.lock().unwrap().clone().unwrap_or_else(|| {
        fastab_os_shim::Env::new()
            .q_log_level()
            .unwrap_or_else(|_| DEFAULT_FILTER.to_string())
    })
}

/// Set the log level to the given level.
///
/// # Returns
///
/// On success, returns the old log level.
pub fn set_log_level(level: String) -> Result<String, Error> {
    info!("Setting log level to {level:?}");

    let old_level = get_log_level();
    *Q_LOG_LEVEL_GLOBAL.lock().unwrap() = Some(level);

    let filter_layer = create_filter_layer();
    *MAX_LEVEL.lock().unwrap() = filter_layer.max_level_hint();

    ENV_FILTER_RELOADABLE_HANDLE
        .lock()
        .unwrap()
        .as_ref()
        .expect("set_log_level must not be called before logging is initialized")
        .reload(filter_layer)?;

    Ok(old_level)
}

/// Get the current max log level
///
/// # Returns
///
/// The max log level which is set every time the log level is set.
pub fn get_log_level_max() -> LevelFilter {
    let max_level = *MAX_LEVEL.lock().unwrap();
    match max_level {
        Some(level) => level,
        None => {
            let filter_layer = create_filter_layer();
            *MAX_LEVEL.lock().unwrap() = filter_layer.max_level_hint();
            filter_layer.max_level_hint().unwrap_or(DEFAULT_FILTER)
        },
    }
}

fn create_filter_layer() -> EnvFilter {
    let directive = Directive::from(DEFAULT_FILTER);

    let log_level = Q_LOG_LEVEL_GLOBAL
        .lock()
        .unwrap()
        .clone()
        .or_else(|| fastab_os_shim::Env::new().q_log_level().ok());

    match log_level {
        Some(level) => EnvFilter::builder()
            .with_default_directive(directive)
            .parse_lossy(level),
        None => EnvFilter::default().add_directive(directive),
    }
}

#[cfg(test)]
mod tests {
    use std::fs::read_to_string;
    use std::io::{self, Write};
    use std::sync::mpsc;
    use std::time::Duration;

    use tracing::{debug, error, trace, warn};

    use super::*;

    #[test]
    fn bounded_writer_drops_overflow_without_waiting_for_the_sink() {
        struct GatedWriter {
            started: Option<mpsc::Sender<()>>,
            release: mpsc::Receiver<()>,
        }

        impl Write for GatedWriter {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                if let Some(started) = self.started.take() {
                    let _ = started.send(());
                    // Dropping the sender also releases this gate on failure.
                    let _ = self.release.recv();
                }
                Ok(bytes.len())
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (mut writer, guard) = non_blocking_writer(GatedWriter {
            started: Some(started_tx),
            release: release_rx,
        });
        writer.write_all(b"hold sink").unwrap();
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();

        let (done_tx, done_rx) = mpsc::channel();
        let producer = std::thread::spawn(move || {
            for _ in 0..=BUFFERED_LINES_LIMIT {
                writer.write_all(b"queued line").unwrap();
            }
            let _ = done_tx.send(writer.error_counter().dropped_lines());
        });
        let dropped = done_rx.recv_timeout(Duration::from_secs(5));
        // Release before asserting or joining, even if a regression changed
        // the queue to blocking mode and the producer timed out above.
        let _ = release_tx.send(());
        producer.join().unwrap();
        drop(guard);
        assert_eq!(dropped.unwrap(), 1);
    }

    #[test]
    fn an_error_written_now_is_on_disk_when_the_call_returns() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("panic.log");
        write_error_now(&path, "panic on thread \"main\": boom").unwrap();
        write_error_now(&path, "second").unwrap();

        let contents = read_to_string(&path).unwrap();
        let lines: Vec<_> = contents.lines().collect();
        assert_eq!(lines.len(), 2, "{contents}");
        assert!(
            lines[0].ends_with("Z ERROR panic on thread \"main\": boom"),
            "{contents}"
        );
        assert!(lines[1].ends_with("Z ERROR second"), "{contents}");
    }

    #[test]
    fn timestamps_are_utc_calendar_dates() {
        let at = |seconds| utc_timestamp(UNIX_EPOCH + Duration::from_secs(seconds));
        assert_eq!(at(0), "1970-01-01T00:00:00Z");
        assert_eq!(at(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(at(1_700_000_000), "2023-11-14T22:13:20Z");
        assert_eq!(at(1_709_208_000), "2024-02-29T12:00:00Z");
        assert_eq!(at(4_102_444_799), "2099-12-31T23:59:59Z");
    }

    #[test]
    fn test_logging() {
        // Create a temp path for where we write logs to.
        let tempdir = tempfile::TempDir::new().unwrap();
        let log_path = tempdir.path().join("test.log");

        // Assert that initialize logging simply doesn't panic.
        let _guard = initialize_logging(LogArgs {
            log_level: Some("trace".to_owned()),
            log_to_stdout: true,
            log_file_path: Some(&log_path),
            delete_old_log_file: true,
        })
        .unwrap();

        // Test that get log level functions as expected.
        assert_eq!(get_log_level(), "trace");

        // Write some log messages out to file. (and stderr)
        trace!("abc");
        debug!("def");
        info!("ghi");
        warn!("jkl");
        error!("mno");

        // Test that set log level functions as expected.
        // This also restores the default log level.
        set_log_level(DEFAULT_FILTER.to_string()).unwrap();
        assert_eq!(get_log_level(), DEFAULT_FILTER.to_string());

        // Sleep in order to ensure logs get written to file, then assert on the contents
        std::thread::sleep(Duration::from_millis(100));
        let logs = read_to_string(&log_path).unwrap();
        for i in [
            "TRACE", "DEBUG", "INFO", "WARN", "ERROR", "abc", "def", "ghi", "jkl", "mno",
        ] {
            assert!(logs.contains(i));
        }
    }
}
