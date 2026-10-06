//! One-time migration from the project's previous name, `serialwrap`.
//!
//! The data directory (recordings and device profiles) and the config
//! directory (`rules.toml`) are keyed by the application name, so after the
//! rename a daemon would start with empty history and no write-gate policy
//! on a machine that had been running the old build. [`migrate_legacy_dirs`]
//! moves each legacy directory to its new location on first start, but only
//! when the new one doesn't exist yet: it never merges into, or overwrites,
//! anything already recorded under the new name.
//!
//! The old daemon must not be running. If it still holds a recorder lock in
//! the legacy data directory, moving that directory out from under it would
//! split one device's history across two places, so the migration is skipped
//! with a message instead.

use std::fs::{self, OpenOptions};
use std::io;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

/// The application name the data and config directories used before the rename.
pub const LEGACY_APP_NAME: &str = "serialwrap";

/// What [`migrate_dir`] did with one legacy directory.
#[derive(Debug, PartialEq, Eq)]
pub enum MigrateOutcome {
    /// No legacy directory: nothing to do.
    NoLegacy,
    /// The new directory already exists; the legacy one was left untouched.
    AlreadyMigrated,
    /// The legacy directory was renamed to the new path.
    Moved,
    /// A process still holds a recorder lock under the legacy directory.
    LegacyInUse,
}

/// Move `legacy` to `current` if `legacy` exists and `current` does not.
pub fn migrate_dir(legacy: &Path, current: &Path) -> io::Result<MigrateOutcome> {
    if !legacy.is_dir() {
        return Ok(MigrateOutcome::NoLegacy);
    }
    if current.exists() {
        return Ok(MigrateOutcome::AlreadyMigrated);
    }
    if any_recorder_lock_held(legacy)? {
        return Ok(MigrateOutcome::LegacyInUse);
    }
    if let Some(parent) = current.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::rename(legacy, current)?;
    Ok(MigrateOutcome::Moved)
}

/// Whether some process holds the `flock` a [`crate::recorder::Recorder`]
/// takes on `<data_dir>/devices/<id>/.lock`.
fn any_recorder_lock_held(data_dir: &Path) -> io::Result<bool> {
    let devices = data_dir.join("devices");
    let entries = match fs::read_dir(&devices) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let lock_path = entry?.path().join(".lock");
        let file = match OpenOptions::new().write(true).open(&lock_path) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        // Probe and release immediately; the lock is dropped with `file`.
        let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if ret != 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::WouldBlock {
                return Ok(true);
            }
            return Err(err);
        }
    }
    Ok(false)
}

/// The legacy and current `(data_dir, config_dir)` pairs for this platform.
/// On macOS both resolve to the same `Application Support` directory, so the
/// pairs are deduplicated by the caller.
fn dir_pairs() -> Vec<(PathBuf, PathBuf)> {
    let (Some(old), Some(new)) = (
        directories::ProjectDirs::from("", "", LEGACY_APP_NAME),
        directories::ProjectDirs::from("", "", "serialwarden"),
    ) else {
        return Vec::new();
    };
    let mut pairs = vec![(old.data_dir().to_path_buf(), new.data_dir().to_path_buf())];
    if old.config_dir() != old.data_dir() {
        pairs.push((
            old.config_dir().to_path_buf(),
            new.config_dir().to_path_buf(),
        ));
    }
    pairs
}

/// Run [`migrate_dir`] for the data and config directories, reporting each
/// move or skip on stderr. Never fails daemon startup: a migration problem
/// leaves the legacy data in place, where it can still be moved by hand.
pub fn migrate_legacy_dirs() {
    for (legacy, current) in dir_pairs() {
        match migrate_dir(&legacy, &current) {
            Ok(MigrateOutcome::Moved) => eprintln!(
                "serialwardend: migrated {} to {} (renamed from serialwrap)",
                legacy.display(),
                current.display()
            ),
            Ok(MigrateOutcome::LegacyInUse) => eprintln!(
                "serialwardend: not migrating {}: a running serialwrap daemon still holds it; \
                 stop it (`serialwrap service uninstall`) and restart serialwarden",
                legacy.display()
            ),
            Ok(MigrateOutcome::NoLegacy | MigrateOutcome::AlreadyMigrated) => {}
            Err(e) => eprintln!(
                "serialwardend: could not migrate {} to {}: {e}; move it by hand to keep its \
                 history",
                legacy.display(),
                current.display()
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn moves_the_legacy_dir_when_the_new_one_is_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let legacy = tmp.path().join("serialwrap");
        let current = tmp.path().join("nested").join("serialwarden");
        write(&legacy.join("devices/dev-1/segments/0.jsonl"), "{}\n");

        assert_eq!(
            migrate_dir(&legacy, &current).unwrap(),
            MigrateOutcome::Moved
        );
        assert!(!legacy.exists());
        assert_eq!(
            fs::read_to_string(current.join("devices/dev-1/segments/0.jsonl")).unwrap(),
            "{}\n"
        );
    }

    #[test]
    fn never_touches_an_existing_new_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let legacy = tmp.path().join("serialwrap");
        let current = tmp.path().join("serialwarden");
        write(&legacy.join("rules.toml"), "old");
        write(&current.join("rules.toml"), "new");

        assert_eq!(
            migrate_dir(&legacy, &current).unwrap(),
            MigrateOutcome::AlreadyMigrated
        );
        assert_eq!(
            fs::read_to_string(legacy.join("rules.toml")).unwrap(),
            "old"
        );
        assert_eq!(
            fs::read_to_string(current.join("rules.toml")).unwrap(),
            "new"
        );
    }

    #[test]
    fn no_legacy_dir_is_a_no_op() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            migrate_dir(
                &tmp.path().join("serialwrap"),
                &tmp.path().join("serialwarden")
            )
            .unwrap(),
            MigrateOutcome::NoLegacy
        );
        assert!(!tmp.path().join("serialwarden").exists());
    }

    #[test]
    fn skips_while_a_recorder_lock_is_held() {
        let tmp = tempfile::tempdir().unwrap();
        let legacy = tmp.path().join("serialwrap");
        let current = tmp.path().join("serialwarden");
        let lock_path = legacy.join("devices/dev-1/.lock");
        write(&lock_path, "");
        let holder = OpenOptions::new().write(true).open(&lock_path).unwrap();
        let ret = unsafe { libc::flock(holder.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        assert_eq!(ret, 0);

        assert_eq!(
            migrate_dir(&legacy, &current).unwrap(),
            MigrateOutcome::LegacyInUse
        );
        assert!(legacy.exists());
        assert!(!current.exists());

        drop(holder);
        assert_eq!(
            migrate_dir(&legacy, &current).unwrap(),
            MigrateOutcome::Moved
        );
    }
}
