use crate::{CasError, Result};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct CasPaths {
    pub app_home: PathBuf,
    pub registry_path: PathBuf,
    pub accounts_dir: PathBuf,
    pub lock_path: PathBuf,
    pub codex_home: PathBuf,
    pub codex_auth_path: PathBuf,
    pub codex_config_path: PathBuf,
}

impl CasPaths {
    pub fn discover() -> Result<Self> {
        let home = dirs::home_dir().ok_or(CasError::HomeUnavailable)?;
        let codex_home = std::env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".codex"));
        Ok(Self::from_codex_home(codex_home))
    }

    pub fn from_codex_home(codex_home: PathBuf) -> Self {
        let app_home = codex_home.join("cas");
        Self {
            registry_path: app_home.join("registry.json"),
            accounts_dir: app_home.join("accounts"),
            lock_path: app_home.join("state.lock"),
            codex_auth_path: codex_home.join("auth.json"),
            codex_config_path: codex_home.join("config.toml"),
            app_home,
            codex_home,
        }
    }

    pub fn ensure(&self) -> Result<()> {
        self.validate_layout()?;
        let codex_home_created = ensure_private_dir(&self.codex_home)?;
        let _ = ensure_private_dir(&self.app_home)?;
        let _ = ensure_private_dir(&self.accounts_dir)?;
        for dir in [&self.app_home, &self.accounts_dir] {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
        if codex_home_created {
            std::fs::set_permissions(&self.codex_home, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }

    pub fn ensure_account_dir(&self, id: &str) -> Result<PathBuf> {
        let dir = self.account_dir(id);
        let _ = ensure_private_dir(&dir)?;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
        Ok(dir)
    }

    pub fn account_auth_path(&self, id: &str) -> PathBuf {
        self.accounts_dir.join(id).join("auth.json")
    }

    pub fn account_dir(&self, id: &str) -> PathBuf {
        self.accounts_dir.join(id)
    }

    pub fn app_home(&self) -> &Path {
        &self.app_home
    }

    fn validate_layout(&self) -> Result<()> {
        if self.app_home.as_os_str().is_empty() || self.app_home.parent().is_none() {
            return Err(CasError::Verification(
                "refusing to use a filesystem root as CAS state directory".into(),
            ));
        }
        if self.codex_home.as_os_str().is_empty() || self.codex_home.parent().is_none() {
            return Err(CasError::Verification(
                "refusing to use a filesystem root as CODEX_HOME".into(),
            ));
        }
        if self.app_home != self.codex_home.join("cas")
            || self.registry_path != self.app_home.join("registry.json")
            || self.accounts_dir != self.app_home.join("accounts")
            || self.lock_path != self.app_home.join("state.lock")
            || self.codex_auth_path != self.codex_home.join("auth.json")
            || self.codex_config_path != self.codex_home.join("config.toml")
        {
            return Err(CasError::Verification(
                "CAS path layout is internally inconsistent".into(),
            ));
        }
        Ok(())
    }
}

fn ensure_private_dir(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(CasError::Verification(format!(
                    "refusing to use non-directory or symlink state path: {}",
                    path.display()
                )));
            }
            Ok(false)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(path)?;
            let metadata = std::fs::symlink_metadata(path)?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(CasError::Verification(format!(
                    "refusing to use non-directory or symlink state path: {}",
                    path.display()
                )));
            }
            Ok(true)
        }
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_filesystem_root_as_state_home() {
        let paths = CasPaths::from_codex_home(PathBuf::from("/"));
        assert!(matches!(
            paths.ensure(),
            Err(CasError::Verification(message)) if message.contains("filesystem root")
        ));
    }

    #[test]
    fn rejects_symlink_state_home() {
        let outer = tempfile::tempdir().unwrap();
        let target = outer.path().join("target");
        std::fs::create_dir(&target).unwrap();
        let link = outer.path().join("state-link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let paths = CasPaths::from_codex_home(link);
        assert!(matches!(
            paths.ensure(),
            Err(CasError::Verification(message)) if message.contains("symlink")
        ));
    }

    #[test]
    fn state_lives_under_codex_home() {
        let outer = tempfile::tempdir().unwrap();
        let codex_home = outer.path().join(".codex");
        let paths = CasPaths::from_codex_home(codex_home.clone());
        assert_eq!(paths.app_home, codex_home.join("cas"));
        assert_eq!(paths.registry_path, codex_home.join("cas/registry.json"));
        assert_eq!(paths.accounts_dir, codex_home.join("cas/accounts"));
        assert_eq!(paths.codex_auth_path, codex_home.join("auth.json"));
    }
}
