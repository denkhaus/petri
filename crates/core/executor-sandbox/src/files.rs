//! Workspace file I/O for an environment whose workspace is a directory on
//! this machine: a host scope's own, or the bind-mount source of a container's.
//! Both backends answer `read_file` and `write_file` from here, so the
//! missing-file and oversized-read rules are the same on each.

use std::io::ErrorKind;
use std::path::Path;

use executor::EnvError;
use tokio::fs;
use tokio::io::AsyncReadExt as _;

/// The whole file at `relative` under `root`, or `None` when it does not exist.
pub(crate) async fn read(root: &Path, relative: &Path) -> Result<Option<Vec<u8>>, EnvError> {
    match fs::read(root.join(relative)).await {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(EnvError::workspace("read", relative.display(), error)),
    }
}

/// Like [`read`], but a file over `limit` bytes is an error, not a truncation.
pub(crate) async fn read_limited(
    root: &Path,
    relative: &Path,
    limit: usize,
) -> Result<Option<Vec<u8>>, EnvError> {
    let file = match fs::File::open(root.join(relative)).await {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(EnvError::workspace("open", relative.display(), error)),
    };
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(|error| EnvError::workspace("read", relative.display(), error))?;
    if bytes.len() > limit {
        return Err(EnvError::workspace(
            "read",
            relative.display(),
            executor::oversized_read(limit),
        ));
    }
    Ok(Some(bytes))
}

/// Writes `contents` to `relative` under `root`, creating parent directories.
pub(crate) async fn write(root: &Path, relative: &Path, contents: &[u8]) -> Result<(), EnvError> {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .await
            .map_err(|error| EnvError::workspace("create", parent.display(), error))?;
    }
    fs::write(&path, contents)
        .await
        .map_err(|error| EnvError::workspace("write", relative.display(), error))
}
