use crate::Result;
use atomic_write_file::AtomicWriteFile;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut options = AtomicWriteFile::options();
    options.mode(0o600);
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.commit()?;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;

    Ok(())
}

pub fn atomic_write_json<T: serde::Serialize>(path: &Path, value: &T) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(value)?;
    atomic_write(path, &bytes)
}
