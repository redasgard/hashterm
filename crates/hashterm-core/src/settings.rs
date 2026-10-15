//! Writing settings back to `config.toml` from the Settings window.
//!
//! The config file is hand-edited and commented, so it is never regenerated:
//! only the keys the user actually touched are edited in place with
//! `toml_edit` (comments, ordering and untouched keys survive byte-for-byte).
//! A result that no longer parses as a [`Config`] is refused rather than
//! written, and the write itself is atomic and symlink-aware (dotfile
//! managers often symlink `config.toml`; renaming over the link would replace
//! the link, not the file it points to).

use crate::config::Config;
use std::path::{Path, PathBuf};
use toml_edit::{DocumentMut, Item, Table, Value as TomlValue};

#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    #[error("cannot read {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("cannot write {path}: {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("config.toml is not valid TOML ({0}); fix it by hand first")]
    Parse(String),
    #[error("that value would make config.toml invalid: {0}")]
    Invalid(String),
}

/// A value the Settings window can write.
#[derive(Debug, Clone, PartialEq)]
pub enum Setting {
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
}

/// One edit: set `section.key` to a value, or (`None`) remove the key so the
/// built-in default applies again.
#[derive(Debug, Clone, PartialEq)]
pub struct Edit {
    pub section: String,
    pub key: String,
    pub value: Option<Setting>,
}

impl Edit {
    pub fn set(section: &str, key: &str, value: Setting) -> Self {
        Self {
            section: section.into(),
            key: key.into(),
            value: Some(value),
        }
    }
    pub fn unset(section: &str, key: &str) -> Self {
        Self {
            section: section.into(),
            key: key.into(),
            value: None,
        }
    }
}

fn to_toml(v: &Setting) -> TomlValue {
    match v {
        Setting::Bool(b) => TomlValue::from(*b),
        Setting::Int(i) => TomlValue::from(*i),
        // Two decimals are plenty for opacities/percentages and keep the file
        // free of 0.8999999761581421.
        Setting::Float(f) => TomlValue::from((f * 1000.0).round() / 1000.0),
        Setting::Str(s) => TomlValue::from(s.as_str()),
    }
}

/// Apply `edits` to the text of a config file, preserving everything else.
pub fn apply_edits(text: &str, edits: &[Edit]) -> Result<String, SettingsError> {
    let mut doc: DocumentMut = text
        .parse()
        .map_err(|e: toml_edit::TomlError| SettingsError::Parse(e.message().to_owned()))?;
    for edit in edits {
        match &edit.value {
            Some(v) => {
                let table = section_mut(&mut doc, &edit.section)?;
                let mut new = to_toml(v);
                if let Some(old) = table.get(&edit.key).and_then(Item::as_value) {
                    // keep a trailing `# comment` and the original spacing
                    *new.decor_mut() = old.decor().clone();
                }
                table.insert(&edit.key, Item::Value(new));
            }
            None => {
                if let Some(table) = doc.get_mut(&edit.section).and_then(Item::as_table_mut) {
                    table.remove(&edit.key);
                }
            }
        }
    }
    Ok(doc.to_string())
}

fn section_mut<'a>(doc: &'a mut DocumentMut, name: &str) -> Result<&'a mut Table, SettingsError> {
    if !doc.contains_key(name) {
        doc.insert(name, Item::Table(Table::new()));
    }
    doc.get_mut(name)
        .and_then(Item::as_table_mut)
        .ok_or_else(|| SettingsError::Invalid(format!("[{name}] is not a table")))
}

/// Edit `path` in place. A missing file starts from the commented reference
/// config. Returns `Ok(false)` when nothing changed (no write happened).
pub fn save_edits(path: &Path, edits: &[Edit]) -> Result<bool, SettingsError> {
    // Follow a symlinked config to the real file.
    let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let old = match std::fs::read_to_string(&target) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config::DEFAULT_TOML.to_owned(),
        Err(e) => {
            return Err(SettingsError::Read {
                path: target,
                source: e,
            });
        }
    };
    let new = apply_edits(&old, edits)?;
    if new == old && target.exists() {
        return Ok(false);
    }
    toml::from_str::<Config>(&new).map_err(|e| SettingsError::Invalid(e.message().to_owned()))?;

    let werr = |source| SettingsError::Write {
        path: target.clone(),
        source,
    };
    if let Some(dir) = target.parent() {
        std::fs::create_dir_all(dir).map_err(werr)?;
    }
    // One-time safety copy of the hand-written original.
    let bak = target.with_extension("toml.bak");
    if target.exists() && !bak.exists() {
        let _ = std::fs::copy(&target, &bak);
    }
    let tmp = target.with_extension("toml.hashterm-tmp");
    std::fs::write(&tmp, &new).map_err(werr)?;
    if let Ok(meta) = std::fs::metadata(&target) {
        let _ = std::fs::set_permissions(&tmp, meta.permissions());
    }
    std::fs::rename(&tmp, &target).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        werr(e)
    })?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
# my notes
[appearance]
font = \"JetBrains Mono 11\"  # keep this
padding = 6

[tabbar]
edge = \"top\"
";

    #[test]
    fn edits_in_place_and_keeps_comments() {
        let out = apply_edits(
            SAMPLE,
            &[
                Edit::set("appearance", "padding", Setting::Int(12)),
                Edit::set("tabbar", "auto_hide", Setting::Bool(true)),
            ],
        )
        .unwrap();
        assert!(out.contains("# my notes"));
        assert!(out.contains("font = \"JetBrains Mono 11\"  # keep this"));
        assert!(out.contains("padding = 12"));
        assert!(out.contains("auto_hide = true"));
        assert!(out.contains("edge = \"top\""));
    }

    #[test]
    fn trailing_comment_survives_a_changed_value() {
        let out = apply_edits(
            SAMPLE,
            &[Edit::set(
                "appearance",
                "font",
                Setting::Str("Fira Code 10".into()),
            )],
        )
        .unwrap();
        assert!(out.contains("font = \"Fira Code 10\"  # keep this"), "{out}");
    }

    #[test]
    fn new_section_is_created() {
        let out = apply_edits(
            SAMPLE,
            &[Edit::set("window", "remember_size", Setting::Bool(false))],
        )
        .unwrap();
        let cfg: Config = toml::from_str(&out).unwrap();
        assert!(!cfg.window.remember_size);
        assert!(cfg.window.remember_position);
    }

    #[test]
    fn unset_removes_the_key_so_the_default_applies() {
        let out = apply_edits(SAMPLE, &[Edit::unset("appearance", "padding")]).unwrap();
        assert!(!out.contains("padding"));
        let cfg: Config = toml::from_str(&out).unwrap();
        assert_eq!(cfg.appearance.padding, Config::default().appearance.padding);
        // removing something that isn't there is fine
        apply_edits(SAMPLE, &[Edit::unset("nope", "nothing")]).unwrap();
    }

    #[test]
    fn floats_are_tidy() {
        let out = apply_edits(
            SAMPLE,
            &[Edit::set(
                "appearance",
                "window_opacity",
                Setting::Float(0.899_999_976),
            )],
        )
        .unwrap();
        assert!(out.contains("window_opacity = 0.9\n"), "{out}");
    }

    #[test]
    fn broken_input_is_reported_not_overwritten() {
        assert!(matches!(
            apply_edits("[tabbar\nedge=", &[]),
            Err(SettingsError::Parse(_))
        ));
    }

    #[test]
    fn save_refuses_a_value_that_breaks_the_config() {
        let dir = std::env::temp_dir().join(format!("ht-settings-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, SAMPLE).unwrap();
        let bad = save_edits(
            &path,
            &[Edit::set("tabbar", "edge", Setting::Str("diagonal".into()))],
        );
        assert!(matches!(bad, Err(SettingsError::Invalid(_))));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), SAMPLE);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_writes_atomically_backs_up_once_and_follows_symlinks() {
        let dir = std::env::temp_dir().join(format!("ht-settings-link-{}", std::process::id()));
        let real = dir.join("dotfiles");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("config.toml"), SAMPLE).unwrap();
        let link = dir.join("config.toml");
        std::os::unix::fs::symlink(real.join("config.toml"), &link).unwrap();

        let changed = save_edits(&link, &[Edit::set("appearance", "padding", Setting::Int(9))]).unwrap();
        assert!(changed);
        // the symlink is still a symlink and the real file got the edit
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        let text = std::fs::read_to_string(real.join("config.toml")).unwrap();
        assert!(text.contains("padding = 9") && text.contains("# my notes"));
        // original preserved once, never overwritten by later saves
        assert_eq!(
            std::fs::read_to_string(real.join("config.toml.bak")).unwrap(),
            SAMPLE
        );
        save_edits(&link, &[Edit::set("appearance", "padding", Setting::Int(10))]).unwrap();
        assert_eq!(
            std::fs::read_to_string(real.join("config.toml.bak")).unwrap(),
            SAMPLE
        );
        // a no-op edit does not touch the file
        assert!(!save_edits(&link, &[Edit::set("appearance", "padding", Setting::Int(10))]).unwrap());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_missing_file_starts_from_the_commented_reference() {
        let dir = std::env::temp_dir().join(format!("ht-settings-new-{}", std::process::id()));
        let path = dir.join("config.toml");
        save_edits(&path, &[Edit::set("tabbar", "auto_hide", Setting::Bool(true))]).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("hashterm configuration"));
        assert!(toml::from_str::<Config>(&text).unwrap().tabbar.auto_hide);
        std::fs::remove_dir_all(&dir).ok();
    }
}
