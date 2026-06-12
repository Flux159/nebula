//! Save/patch/restore ~/.nebula/config.toml around a run.
//!
//! Crash-safe by construction: the original is copied to
//! `config.toml.battletest-orig` *before* the first patch, restored on Drop,
//! and — because Drop doesn't run on SIGKILL — any *stale* backup found at
//! startup is restored first. A missing original is remembered with a magic
//! marker so restore deletes rather than recreates.

use anyhow::Context;
use std::path::PathBuf;

const MARKER_ABSENT: &str = "#NEBULA-BATTLETEST-NO-ORIGINAL\n";

pub fn nebula_home() -> PathBuf {
    if let Ok(h) = std::env::var("NEBULA_HOME") {
        return PathBuf::from(h);
    }
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".nebula")
}

pub struct ConfigGuard {
    path: PathBuf,
    bak: PathBuf,
    restored: bool,
}

impl ConfigGuard {
    pub fn take() -> anyhow::Result<Self> {
        let home = nebula_home();
        std::fs::create_dir_all(&home).ok();
        let path = home.join("config.toml");
        let bak = home.join("config.toml.battletest-orig");
        if bak.exists() {
            eprintln!(
                "battletest: found stale {} from an interrupted run — restoring it first",
                bak.display()
            );
            restore_from(&bak, &path)?;
        }
        if path.exists() {
            std::fs::copy(&path, &bak).context("back up config.toml")?;
        } else {
            std::fs::write(&bak, MARKER_ABSENT).context("write backup marker")?;
        }
        Ok(Self {
            path,
            bak,
            restored: false,
        })
    }

    /// Load current config (or empty), apply `patch`, write back.
    pub fn set(&self, patch: impl FnOnce(&mut toml::Table)) -> anyhow::Result<()> {
        let mut table: toml::Table = match std::fs::read_to_string(&self.path) {
            Ok(raw) => raw.parse().context("parse config.toml")?,
            Err(_) => toml::Table::new(),
        };
        patch(&mut table);
        std::fs::write(&self.path, toml::to_string_pretty(&table)?).context("write config.toml")?;
        Ok(())
    }

    pub fn set_max_ram(&self, mib: u64) -> anyhow::Result<()> {
        self.set(|t| {
            t.insert("max_ram_mib".into(), toml::Value::Integer(mib as i64));
        })
    }

    pub fn set_rootfs(&self, rootfs: Option<&std::path::Path>) -> anyhow::Result<()> {
        self.set(|t| match rootfs {
            Some(p) => {
                t.insert(
                    "rootfs".into(),
                    toml::Value::String(p.display().to_string()),
                );
            }
            None => {
                t.remove("rootfs");
            }
        })
    }

    pub fn restore(&mut self) -> anyhow::Result<()> {
        if self.restored {
            return Ok(());
        }
        restore_from(&self.bak, &self.path)?;
        self.restored = true;
        Ok(())
    }
}

fn restore_from(bak: &std::path::Path, path: &std::path::Path) -> anyhow::Result<()> {
    let raw = std::fs::read_to_string(bak).context("read config backup")?;
    if raw == MARKER_ABSENT {
        std::fs::remove_file(path).ok();
    } else {
        std::fs::write(path, &raw).context("restore config.toml")?;
    }
    std::fs::remove_file(bak).ok();
    Ok(())
}

impl Drop for ConfigGuard {
    fn drop(&mut self) {
        if let Err(e) = self.restore() {
            eprintln!(
                "battletest: FAILED to restore {} ({e}); original saved at {}",
                self.path.display(),
                self.bak.display()
            );
        }
    }
}
