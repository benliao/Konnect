//! Pre-write backups for KiCAD project files.
//!
//! Every tool here edits KiCAD files in place. When an edit goes wrong the
//! original is already gone, and the only thing that has saved a real project
//! so far is KiCAD's own `.history/` — which only exists for files KiCAD itself
//! has saved, and only until it rotates. This module snapshots a file into a
//! store outside the project *before* it is overwritten, so a bad write is
//! recoverable regardless of what KiCAD happens to have kept.
//!
//! Backups live under `~/.konnect/backups/` with the source's absolute path
//! mirrored underneath, so the snapshots for a given file are trivial to find:
//!
//! ```text
//! ~/.konnect/backups/Users/me/pcb/proj/proj.kicad_pcb/20260725-153012-004.kicad_pcb
//! ```
//!
//! Configuration (read from the environment, never written):
//! - `KONNECT_BACKUP_DIR` — store root; defaults to `~/.konnect/backups`.
//! - `KONNECT_BACKUP_GENERATIONS` — how many snapshots to keep per file,
//!   default 10. `0` disables backups entirely.

use std::path::{Component, Path, PathBuf};

/// Snapshots kept per source file when `KONNECT_BACKUP_GENERATIONS` is unset.
const DEFAULT_GENERATIONS: usize = 10;

/// Copy `path` into the backup store, then prune old snapshots.
///
/// Returns the snapshot path, or `None` when nothing was written — the file
/// does not exist yet (a fresh write has nothing to lose), backups are
/// disabled, or the store could not be reached. A backup failure never blocks
/// the caller's write: losing the ability to undo is bad, but refusing to do
/// the user's work because a backup directory is unwritable is worse.
pub fn backup_before_write(path: &Path) -> Option<PathBuf> {
    let generations = env_generations();
    if generations == 0 || !path.is_file() {
        return None;
    }
    let root = backup_root()?;
    match snapshot(path, &root, generations) {
        Ok(p) => Some(p),
        Err(e) => {
            tracing::warn!(
                "could not back up {} before overwriting it: {e}",
                path.display()
            );
            None
        }
    }
}

/// The store directory holding every snapshot of `path`.
pub fn backup_dir_for(path: &Path, root: &Path) -> PathBuf {
    // Mirror the absolute path under the root. `..`/`.` are dropped and the
    // Windows prefix (`C:`) is flattened to a plain component so the result
    // always stays inside `root`.
    let abs = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let mut out = root.to_path_buf();
    for c in abs.components() {
        match c {
            Component::Normal(s) => out.push(s),
            Component::Prefix(p) => {
                out.push(p.as_os_str().to_string_lossy().replace([':', '\\'], "_"))
            }
            _ => {}
        }
    }
    out
}

/// Snapshot paths for `path`, oldest first.
pub fn list_backups(path: &Path, root: &Path) -> Vec<PathBuf> {
    let dir = backup_dir_for(path, root);
    let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();
    // Names are zero-padded and time-ordered, so lexical order is chronological.
    found.sort();
    found
}

/// Copy `path` into `root`, keeping at most `generations` snapshots.
fn snapshot(path: &Path, root: &Path, generations: usize) -> std::io::Result<PathBuf> {
    let dir = backup_dir_for(path, root);
    std::fs::create_dir_all(&dir)?;

    let ext = path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    let dest = dir.join(format!("{}{ext}", stamp()));

    // copy, not rename: the original must stay exactly where it is.
    std::fs::copy(path, &dest)?;

    // Prune oldest first, keeping room for the snapshot just written.
    let existing = list_backups(path, root);
    for old in existing.iter().take(existing.len().saturating_sub(generations)) {
        let _ = std::fs::remove_file(old);
    }
    Ok(dest)
}

/// A sortable, collision-resistant stamp: `<seconds>-<nanos>`, zero padded so
/// lexical order matches chronological order.
fn stamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{:012}-{:09}", now.as_secs(), now.subsec_nanos())
}

fn env_generations() -> usize {
    match std::env::var("KONNECT_BACKUP_GENERATIONS") {
        Ok(v) => v.trim().parse().unwrap_or(DEFAULT_GENERATIONS),
        Err(_) => DEFAULT_GENERATIONS,
    }
}

fn backup_root() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("KONNECT_BACKUP_DIR") {
        if !dir.trim().is_empty() {
            return Some(PathBuf::from(dir));
        }
    }
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()?;
    Some(PathBuf::from(home).join(".konnect").join("backups"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(p: &Path, s: &str) {
        std::fs::write(p, s).unwrap();
    }

    #[test]
    fn snapshots_the_previous_contents() {
        let src = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        let f = src.path().join("b.kicad_pcb");
        write(&f, "original");

        let dest = snapshot(&f, store.path(), 10).unwrap();
        write(&f, "overwritten");

        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "original");
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "overwritten");
        assert_eq!(
            dest.extension().unwrap(),
            "kicad_pcb",
            "snapshot keeps the source extension so it can be opened directly"
        );
    }

    #[test]
    fn keeps_only_the_configured_number_of_generations() {
        let src = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        let f = src.path().join("b.kicad_sch");

        for i in 0..8 {
            write(&f, &format!("v{i}"));
            snapshot(&f, store.path(), 3).unwrap();
        }

        let kept = list_backups(&f, store.path());
        assert_eq!(kept.len(), 3, "bounded to 3 generations");
        // The newest three contents survive, oldest first.
        let bodies: Vec<String> = kept
            .iter()
            .map(|p| std::fs::read_to_string(p).unwrap())
            .collect();
        assert_eq!(bodies, vec!["v5", "v6", "v7"], "oldest are pruned, not newest");
    }

    #[test]
    fn two_files_with_the_same_name_do_not_collide() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        let (fa, fb) = (a.path().join("x.kicad_pcb"), b.path().join("x.kicad_pcb"));
        write(&fa, "from-a");
        write(&fb, "from-b");

        snapshot(&fa, store.path(), 5).unwrap();
        snapshot(&fb, store.path(), 5).unwrap();

        let ba = list_backups(&fa, store.path());
        let bb = list_backups(&fb, store.path());
        assert_eq!(ba.len(), 1);
        assert_eq!(bb.len(), 1);
        assert_eq!(std::fs::read_to_string(&ba[0]).unwrap(), "from-a");
        assert_eq!(std::fs::read_to_string(&bb[0]).unwrap(), "from-b");
    }

    #[test]
    fn snapshots_stay_inside_the_store() {
        let store = tempfile::tempdir().unwrap();
        // A relative path with `..` must not escape the root.
        let dir = backup_dir_for(Path::new("../../etc/passwd"), store.path());
        assert!(
            dir.starts_with(store.path()),
            "escaped the backup root: {}",
            dir.display()
        );
    }

    #[test]
    fn nonexistent_source_is_not_backed_up() {
        let store = tempfile::tempdir().unwrap();
        let missing = store.path().join("nope.kicad_pcb");
        assert!(backup_before_write(&missing).is_none());
    }
}
