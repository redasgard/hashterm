//! On-disk layout and atomic persistence of dumps.
//!
//! ~/.local/share/hashterm/
//!   sessions/<name>/manifest.json + panes/*.scrollback.zst   (named, kept)
//!   autosave/<timestamp>/...                                 (rotated)
//!
//! A save is built in a `.tmp.*` sibling dir then rename()d into place, so an
//! interrupted save never corrupts an existing one. Scrollbacks may hold
//! secrets: dirs 0700, files 0600.

use crate::schema::{DumpManifest, SCHEMA_VERSION};
use std::fs;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// A save is untrusted input (it may be shared or dropped in). Bound it so a
/// hostile manifest can't exhaust memory or stall restore with a huge fan-out.
const MAX_MANIFEST_BYTES: u64 = 4 * 1024 * 1024;
const MAX_SESSIONS: usize = 256;
const MAX_WINDOWS: usize = 256;
const MAX_PANES: usize = 256;

/// Create a directory (and parents) with mode 0700 from the start — never
/// world-visible even briefly (avoids the create-then-chmod TOCTOU).
fn create_dir_700(path: &Path) -> std::io::Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
}

/// Tighten an existing dir to 0700, but only if it is a real directory — never
/// follow a planted symlink to chmod its target.
fn harden_existing_dir(path: &Path) {
    if let Ok(md) = fs::symlink_metadata(path)
        && md.file_type().is_dir()
    {
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("bad manifest {path}: {source}")]
    Manifest {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error(
        "save '{name}' has schema v{found}, this hashterm understands up to v{SCHEMA_VERSION}; upgrade hashterm"
    )]
    TooNew { name: String, found: u32 },
    #[error("no save named '{0}'")]
    NotFound(String),
    #[error("invalid save name '{0}' (no slashes or dots allowed)")]
    BadName(String),
    #[error("save '{name}' is too large or has too many {what}")]
    TooLarge { name: String, what: &'static str },
}

fn io_err(path: &Path) -> impl FnOnce(std::io::Error) -> StoreError + '_ {
    move |source| StoreError::Io {
        path: path.into(),
        source,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaveMeta {
    pub name: String,
    pub created_at: String,
    /// Local-time display ("YYYY-MM-DD HH:MM"); falls back to created_at (UTC)
    /// for saves written before this field existed.
    pub created_local: String,
    pub session_count: usize,
    pub auto: bool,
}

#[derive(Debug, Clone)]
pub struct SessionStore {
    root: PathBuf,
}

impl SessionStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn default_root() -> PathBuf {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share")
            })
            .join("hashterm")
    }

    fn validate_name(name: &str) -> Result<(), StoreError> {
        if name.is_empty() || name.starts_with('.') || name.contains('/') || name.contains('\0') {
            return Err(StoreError::BadName(name.into()));
        }
        Ok(())
    }

    fn dir_for(&self, name: &str, auto: bool) -> PathBuf {
        self.root
            .join(if auto { "autosave" } else { "sessions" })
            .join(name)
    }

    /// Directory in which a new save should be assembled; caller writes
    /// manifest.json + panes/ inside, then calls `commit`.
    pub fn begin(&self, name: &str, auto: bool) -> Result<PathBuf, StoreError> {
        Self::validate_name(name)?;
        let parent = self.root.join(if auto { "autosave" } else { "sessions" });
        create_dir_700(&parent).map_err(io_err(&parent))?;
        // Tighten root/parent if a prior version created them world-readable,
        // without following a symlink planted at either path.
        harden_existing_dir(&self.root);
        harden_existing_dir(&parent);
        let tmp = parent.join(format!(".tmp.{}.{}", name, std::process::id()));
        if tmp.exists() {
            fs::remove_dir_all(&tmp).map_err(io_err(&tmp))?;
        }
        create_dir_700(&tmp.join("panes")).map_err(io_err(&tmp))?;
        Ok(tmp)
    }

    /// Atomically move a completed tmp save into place, replacing any previous
    /// save of the same name.
    pub fn commit(&self, tmp: &Path, name: &str, auto: bool) -> Result<PathBuf, StoreError> {
        let dest = self.dir_for(name, auto);
        if dest.exists() {
            // Include the PID so concurrent commits of the same save name
            // don't collide on the graveyard dir.
            let graveyard = dest.with_file_name(format!(".old.{}.{}", name, std::process::id()));
            fs::rename(&dest, &graveyard).map_err(io_err(&dest))?;
            fs::rename(tmp, &dest).map_err(io_err(tmp))?;
            let _ = fs::remove_dir_all(&graveyard);
        } else {
            fs::rename(tmp, &dest).map_err(io_err(tmp))?;
        }
        if auto {
            let link = self.root.join("autosave/latest");
            let _ = fs::remove_file(&link);
            let _ = std::os::unix::fs::symlink(name, &link);
        }
        Ok(dest)
    }

    pub fn write_manifest(dir: &Path, manifest: &DumpManifest) -> Result<(), StoreError> {
        let path = dir.join("manifest.json");
        let json = serde_json::to_vec_pretty(manifest).map_err(|source| StoreError::Manifest {
            path: path.clone(),
            source,
        })?;
        // Create 0600 up-front so scrollback-bearing manifests are never even
        // briefly world-readable (was create-then-chmod).
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)
            .map_err(io_err(&path))?;
        f.write_all(&json).map_err(io_err(&path))?;
        Ok(())
    }

    pub fn load(&self, name: &str, auto: bool) -> Result<(DumpManifest, PathBuf), StoreError> {
        Self::validate_name(name)?;
        let dir = self.dir_for(name, auto);
        let path = dir.join("manifest.json");
        // Refuse an oversized manifest before allocating it.
        if let Ok(md) = fs::metadata(&path)
            && md.len() > MAX_MANIFEST_BYTES
        {
            return Err(StoreError::TooLarge {
                name: name.into(),
                what: "manifest bytes",
            });
        }
        let bytes = fs::read(&path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                StoreError::NotFound(name.into())
            } else {
                StoreError::Io {
                    path: path.clone(),
                    source: e,
                }
            }
        })?;
        let manifest: DumpManifest =
            serde_json::from_slice(&bytes).map_err(|source| StoreError::Manifest {
                path: path.clone(),
                source,
            })?;
        if manifest.schema_version > SCHEMA_VERSION {
            return Err(StoreError::TooNew {
                name: name.into(),
                found: manifest.schema_version,
            });
        }
        // Bound the fan-out so restore can't be stalled by a hostile save
        // (also caps the sequential per-pane wait in restore).
        if manifest.sessions.len() > MAX_SESSIONS {
            return Err(StoreError::TooLarge {
                name: name.into(),
                what: "sessions",
            });
        }
        for s in &manifest.sessions {
            if s.windows.len() > MAX_WINDOWS
                || s.windows.iter().any(|w| w.panes.len() > MAX_PANES)
            {
                return Err(StoreError::TooLarge {
                    name: name.into(),
                    what: "windows/panes",
                });
            }
        }
        Ok((manifest, dir))
    }

    pub fn list(&self) -> Result<Vec<SaveMeta>, StoreError> {
        let mut out = Vec::new();
        for (sub, auto) in [("sessions", false), ("autosave", true)] {
            let dir = self.root.join(sub);
            let entries = match fs::read_dir(&dir) {
                Ok(e) => e,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(io_err(&dir)(e)),
            };
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') || name == "latest" || !entry.path().is_dir() {
                    continue;
                }
                if let Ok((m, _)) = self.load(&name, auto) {
                    let created_local = if m.created_local.is_empty() {
                        m.created_at.clone()
                    } else {
                        m.created_local
                    };
                    out.push(SaveMeta {
                        name,
                        created_at: m.created_at,
                        created_local,
                        session_count: m.sessions.len(),
                        auto,
                    });
                }
            }
        }
        out.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        Ok(out)
    }

    pub fn delete(&self, name: &str, auto: bool) -> Result<(), StoreError> {
        Self::validate_name(name)?;
        let dir = self.dir_for(name, auto);
        if !dir.exists() {
            return Err(StoreError::NotFound(name.into()));
        }
        fs::remove_dir_all(&dir).map_err(io_err(&dir))
    }

    /// Resolve a user-facing save name to (concrete name, is_autosave):
    /// "latest" follows the autosave symlink; otherwise a named save wins,
    /// falling back to an autosave with that exact name.
    pub fn resolve(&self, name: &str) -> Option<(String, bool)> {
        if name == "latest" {
            let target = fs::read_link(self.root.join("autosave/latest")).ok()?;
            // The symlink must name a single save directory, never a path.
            if target.components().count() != 1 {
                return None;
            }
            return Some((target.to_string_lossy().into_owned(), true));
        }
        if self.root.join("sessions").join(name).is_dir() {
            return Some((name.to_owned(), false));
        }
        if self.root.join("autosave").join(name).is_dir() {
            return Some((name.to_owned(), true));
        }
        None
    }

    /// Keep the newest `keep` autosaves (by directory name, which is a
    /// sortable timestamp), delete the rest.
    pub fn prune_autosaves(&self, keep: usize) -> Result<(), StoreError> {
        let dir = self.root.join("autosave");
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(io_err(&dir)(e)),
        };
        let mut names: Vec<String> = entries
            .flatten()
            // file_type() does not follow symlinks: skips the `latest` link.
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| !n.starts_with('.'))
            .collect();
        names.sort();
        let excess = names.len().saturating_sub(keep);
        for name in names.into_iter().take(excess) {
            let _ = fs::remove_dir_all(dir.join(name));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::*;

    fn manifest() -> DumpManifest {
        DumpManifest {
            schema_version: SCHEMA_VERSION,
            created_at: "2026-08-14T10:00:00Z".into(),
            created_local: "2026-08-14 10:00".into(),
            tmux_version: "3.5a".into(),
            client_size: (80, 24),
            sessions: vec![],
        }
    }

    fn scratch() -> SessionStore {
        let dir = std::env::temp_dir().join(format!(
            "hashterm-store-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        SessionStore::new(dir)
    }

    #[test]
    fn save_load_roundtrip_atomic() {
        let store = scratch();
        let tmp = store.begin("work", false).unwrap();
        SessionStore::write_manifest(&tmp, &manifest()).unwrap();
        store.commit(&tmp, "work", false).unwrap();
        let (m, dir) = store.load("work", false).unwrap();
        assert_eq!(m.tmux_version, "3.5a");
        assert!(dir.join("panes").is_dir());
        assert_eq!(store.list().unwrap().len(), 1);
        store.delete("work", false).unwrap();
        assert!(matches!(
            store.load("work", false),
            Err(StoreError::NotFound(_))
        ));
    }

    #[test]
    fn newer_schema_rejected() {
        let store = scratch();
        let tmp = store.begin("future", false).unwrap();
        let mut m = manifest();
        m.schema_version = SCHEMA_VERSION + 1;
        SessionStore::write_manifest(&tmp, &m).unwrap();
        store.commit(&tmp, "future", false).unwrap();
        assert!(matches!(
            store.load("future", false),
            Err(StoreError::TooNew { .. })
        ));
    }

    #[test]
    fn autosave_rotation() {
        let store = scratch();
        for i in 0..5 {
            let name = format!("2026-08-14T10-00-0{i}");
            let tmp = store.begin(&name, true).unwrap();
            SessionStore::write_manifest(&tmp, &manifest()).unwrap();
            store.commit(&tmp, &name, true).unwrap();
        }
        store.prune_autosaves(2).unwrap();
        let autos: Vec<_> = store
            .list()
            .unwrap()
            .into_iter()
            .filter(|m| m.auto)
            .collect();
        assert_eq!(autos.len(), 2);
        assert_eq!(autos[0].name, "2026-08-14T10-00-04");
    }

    #[test]
    fn path_traversal_rejected() {
        let store = scratch();
        assert!(matches!(
            store.begin("../evil", false),
            Err(StoreError::BadName(_))
        ));
        assert!(matches!(
            store.load(".hidden", false),
            Err(StoreError::BadName(_))
        ));
    }
}
