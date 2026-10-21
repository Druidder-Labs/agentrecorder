use std::fs;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

const SUN_PATH_MAX: usize = if cfg!(target_os = "linux") { 108 } else { 104 };

#[derive(Debug, Clone)]
pub struct Layout {
    root: PathBuf,
}

impl Layout {
    pub fn from_env() -> Result<Self> {
        if let Some(home) = std::env::var_os("AGENTRECORDER_HOME").filter(|v| !v.is_empty()) {
            return Ok(Layout { root: PathBuf::from(home) });
        }
        if let Some(state) = std::env::var_os("XDG_STATE_HOME").filter(|v| !v.is_empty()) {
            return Ok(Layout { root: PathBuf::from(state).join("agentrecorder") });
        }
        let home = std::env::var_os("HOME").context("HOME is not set; set AGENTRECORDER_HOME")?;
        Ok(Layout { root: PathBuf::from(home).join(".local/state/agentrecorder") })
    }

    pub fn at(root: impl Into<PathBuf>) -> Self {
        Layout { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn ensure(&self) -> Result<()> {
        for dir in [self.root.clone(), self.run_dir(), self.log_dir()] {
            ensure_private_dir(&dir)?;
        }
        Ok(())
    }

    pub fn db_path(&self) -> PathBuf {
        self.root.join("state.db")
    }

    pub fn run_dir(&self) -> PathBuf {
        self.root.join("run")
    }

    pub fn log_dir(&self) -> PathBuf {
        self.root.join("logs")
    }

    pub fn socket_path(&self, id: &str) -> Result<PathBuf> {
        let p = self.run_dir().join(format!("{id}.sock"));
        if p.as_os_str().len() >= SUN_PATH_MAX {
            bail!(
                "socket path {} exceeds the {SUN_PATH_MAX}-byte Unix socket limit; set AGENTRECORDER_HOME to a shorter directory",
                p.display()
            );
        }
        Ok(p)
    }

    pub fn log_path(&self, id: &str) -> PathBuf {
        self.log_dir().join(format!("{id}.log"))
    }
}

fn ensure_private_dir(dir: &Path) -> Result<()> {
    if !dir.exists() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .with_context(|| format!("creating {}", dir.display()))?;
    }
    let meta = fs::metadata(dir).with_context(|| format!("reading {}", dir.display()))?;
    let euid = nix::unistd::geteuid().as_raw();
    if meta.uid() != euid {
        bail!("{} is owned by uid {}, expected {euid}; refusing to use it", dir.display(), meta.uid());
    }
    if meta.mode() & 0o077 != 0 {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("restricting permissions on {}", dir.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_private_directories_and_tightens_loose_ones() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = Layout { root: tmp.path().join("home") };
        layout.ensure().unwrap();
        fs::set_permissions(layout.run_dir(), fs::Permissions::from_mode(0o755)).unwrap();
        layout.ensure().unwrap();
        for d in [layout.root.clone(), layout.run_dir(), layout.log_dir()] {
            assert_eq!(fs::metadata(&d).unwrap().mode() & 0o777, 0o700, "{}", d.display());
        }
    }

    #[test]
    fn rejects_socket_paths_over_the_limit() {
        let layout = Layout { root: PathBuf::from(format!("/tmp/{}", "x".repeat(120))) };
        assert!(layout.socket_path("abc123").is_err());
    }
}
