//! Starts a long-lived process with libmalloc's space-efficient setting.
//!
//! By default libmalloc keeps freed pages dirty for reuse, and on macOS 26
//! `malloc_zone_pressure_relief` reported releasing nothing. An engine-only
//! process held 23.5 MB with about 2 MB in use; with `MallocSpaceEfficient=1` it
//! held 2.9 MB for the same work, and completions took no measurably longer.
//!
//! libmalloc reads the variable before `main`, so the process has to start with
//! it. `LSEnvironment` would only cover launches through LaunchServices, and
//! fterm is started by a shell, so the process replaces itself once instead.

const SPACE_EFFICIENT: &str = "MallocSpaceEfficient";

/// Call first in `main`, before any thread starts. Replaces the process with
/// itself unless it already has the setting, and then clears the variable: the
/// shells and generators it starts should not inherit an allocator choice. If
/// the replacement fails, the process carries on with the default allocator.
pub fn start_space_efficient() {
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::process::CommandExt;

        if std::env::var_os(SPACE_EFFICIENT).is_some() {
            // SAFETY: the caller has started no thread yet, so nothing reads
            // the environment concurrently.
            unsafe { std::env::remove_var(SPACE_EFFICIENT) };
            return;
        }
        let Ok(executable) = std::env::current_exe() else {
            return;
        };
        let mut args = std::env::args_os();
        let mut command = std::process::Command::new(executable);
        // fterm's process title comes from its `argv[0]`.
        if let Some(arg0) = args.next() {
            command.arg0(arg0);
        }
        let _ = command.args(args).env(SPACE_EFFICIENT, "1").exec();
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    /// The guard that stops a second replacement, which would loop forever.
    #[test]
    fn a_process_started_with_the_setting_clears_it_and_stays() {
        // SAFETY: only this test touches this variable, and nothing reads it
        // concurrently.
        unsafe { std::env::set_var(SPACE_EFFICIENT, "1") };
        start_space_efficient();
        assert!(std::env::var_os(SPACE_EFFICIENT).is_none());
    }
}
