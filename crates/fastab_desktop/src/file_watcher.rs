use std::path::{Path, PathBuf};

use fastab_settings::JsonStore;
use fastab_util::directories;
use notify::{EventKind, RecursiveMode, Watcher};
use serde_json::{Map, Value};
use tracing::{debug, error, trace};

use crate::Event;
use crate::EventLoopProxy;
use crate::notification_bus::NOTIFICATION_BUS;

/// The file a save replaces when `settings_path` is a symlink, as the watcher
/// reports it, or `None` when the path is the file itself.
///
/// A save renames a temp file over the link's target, in the target's
/// directory, so nothing happens in the directory that holds the link. Without
/// a second watch there, a change one process saved (the Settings window) never
/// reached the other (the desktop app), and each kept saving its own stale
/// copy of the whole file over the other's.
fn linked_data_file(settings_path: &Path) -> Option<PathBuf> {
    let target = fastab_settings::resolve_data_file(settings_path).ok()?;
    if target == settings_path {
        return None;
    }
    // Events carry real paths. The target's directory exists, or a save
    // creates it; the file itself may not exist yet.
    let dir = target.parent()?.canonicalize().ok()?;
    Some(dir.join(target.file_name()?))
}

pub async fn setup_listeners(proxy: EventLoopProxy) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

    let mut watcher = notify::recommended_watcher(move |res| match res {
        Ok(event) => {
            if let Err(err) = tx.send(event) {
                error!(%err, "failed to send notify event");
            }
        },
        Err(err) => error!(%err, "notify watcher"),
    })
    .unwrap();

    let settings_path = match directories::settings_path() {
        Ok(settings_path) => match settings_path.parent() {
            Some(settings_dir) => match watcher.watch(settings_dir, RecursiveMode::NonRecursive) {
                Ok(()) => {
                    trace!("watching settings file at {settings_dir:?}");
                    Some(settings_path)
                },
                Err(err) => {
                    error!(%err, "failed to watch settings dir");
                    None
                },
            },
            None => {
                error!("failed to get settings file dir");
                None
            },
        },
        Err(err) => {
            error!(%err, "failed to get settings file path");
            None
        },
    };

    let linked_file = settings_path.as_deref().and_then(linked_data_file);
    if let Some(dir) = linked_file.as_deref().and_then(Path::parent) {
        match watcher.watch(dir, RecursiveMode::NonRecursive) {
            Ok(()) => trace!("watching the settings file's link target in {dir:?}"),
            Err(err) => error!(%err, "failed to watch the settings file's link target"),
        }
    }

    tokio::spawn(async move {
        let _watcher = watcher;

        let mut prev_settings = match fastab_settings::OldSettings::load_from_file() {
            Ok(map) => map,
            Err(err) => {
                error!(?err, "failed to initialize settings");
                Map::new()
            },
        };

        #[cfg(target_os = "linux")]
        {
            use crate::Event;
            use crate::event::WindowEvent;
            use crate::webview::AUTOCOMPLETE_ID;
            proxy
                .send_event(Event::WindowEvent {
                    window_id: AUTOCOMPLETE_ID,
                    window_event: WindowEvent::SetEnabled(
                        !prev_settings
                            .get("autocomplete.disable")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false),
                    ),
                })
                .map_err(|err| error!(?err, "failed initializing autocomplete.disable state"))
                .ok();
        }

        while let Some(event) = rx.recv().await {
            trace!(?event, "Settings event");

            if let Some(settings_path) = &settings_path {
                if event.paths.contains(settings_path)
                    || linked_file.as_ref().is_some_and(|file| event.paths.contains(file))
                {
                    if let EventKind::Create(_) | EventKind::Modify(_) = event.kind {
                        match fastab_settings::OldSettings::load_from_file() {
                            Ok(settings) => {
                                if settings == prev_settings {
                                    continue;
                                }
                                debug!("Settings file changed");

                                // Install the exact validated snapshot used for
                                // deduplication/notifications. A second file read
                                // can fail during an external rewrite and reset
                                // global settings while we record this as applied.
                                fastab_settings::OldSettings::replace_global(settings.clone());
                                proxy
                                    .send_event(Event::ReloadCredentials)
                                    .map_err(|err| error!(?err, "failed to refresh overlay after settings change"))
                                    .ok();

                                json_map_diff(
                                    &prev_settings,
                                    &settings,
                                    |key, value| {
                                        debug!(%key, %value, "Setting added");
                                        NOTIFICATION_BUS.send_settings_new(key, value);
                                    },
                                    |key, old, new| {
                                        debug!(%key, %old, %new, "Setting change");
                                        NOTIFICATION_BUS.send_settings_changed(key, new);
                                    },
                                    |key, value| {
                                        debug!(%key, %value, "Setting removed");
                                        NOTIFICATION_BUS.send_settings_remove(key);
                                    },
                                );

                                prev_settings = settings;
                            },
                            Err(err) => {
                                // Keep reading the last good copy, but do not
                                // save it over a file that is being edited.
                                fastab_settings::OldSettings::mark_load_failed();
                                error!(%err, "Failed to get settings");
                            },
                        }
                    }
                }
            }
        }
    });
}

#[cfg(all(test, unix))]
mod tests {
    use super::linked_data_file;

    #[test]
    fn a_symlinked_settings_file_is_also_watched_where_its_data_lives() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("app")).unwrap();
        std::fs::create_dir_all(root.join("dotfiles")).unwrap();
        let settings = root.join("app/settings.json");

        // A plain file, or none yet, is watched where it is.
        assert_eq!(linked_data_file(&settings), None);
        std::fs::write(&settings, "{}").unwrap();
        assert_eq!(linked_data_file(&settings), None);

        // A relative link, as dotfile managers write them.
        std::fs::remove_file(&settings).unwrap();
        std::fs::write(root.join("dotfiles/settings.json"), "{}").unwrap();
        std::os::unix::fs::symlink("../dotfiles/settings.json", &settings).unwrap();
        assert_eq!(linked_data_file(&settings), Some(root.join("dotfiles/settings.json")));

        // A link whose target is not there yet: a save creates it.
        std::fs::remove_file(root.join("dotfiles/settings.json")).unwrap();
        assert_eq!(linked_data_file(&settings), Some(root.join("dotfiles/settings.json")));
    }
}

// Diffs the old and new settings and calls the appropriate callbacks
fn json_map_diff(
    map_a: &Map<String, Value>,
    map_b: &Map<String, Value>,
    on_new: impl Fn(&str, &Value),
    on_changed: impl Fn(&str, &Value, &Value),
    on_removed: impl Fn(&str, &Value),
) {
    for (key, value) in map_a {
        if let Some(other_value) = map_b.get(key) {
            if value != other_value {
                on_changed(key, value, other_value);
            }
        } else {
            on_removed(key, value);
        }
    }

    for (key, value) in map_b {
        if !map_a.contains_key(key) {
            on_new(key, value);
        }
    }
}
