//! Stable OS sidecar lease: keep this file across every database replacement.
//! Acquisition is bounded; a reset refuses while a PIR connection can still write.
use super::*;
use std::fs::{File, OpenOptions};

fn open(path: &Path) -> Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path.with_extension("sqlite.ownership-lock"))
        .map_err(|_| EngineError::Message("native_pir_database_busy".into()))
}
pub(super) fn shared(path: &Path) -> Result<File> {
    let file = open(path)?;
    file.try_lock_shared()
        .map_err(|_| EngineError::Message("native_pir_database_busy".into()))?;
    Ok(file)
}
pub(super) fn exclusive(path: &Path) -> Result<File> {
    let file = open(path)?;
    file.try_lock()
        .map_err(|_| EngineError::Message("native_pir_database_busy".into()))?;
    Ok(file)
}

/// Stable ownership outside the wallet: seed-only setup must not gain a database
/// sidecar that looks like retained wallet state. Never unlink this lease file.
pub(super) fn bootstrap(root: &Path) -> Result<std::fs::File> {
    let busy = || EngineError::Message("native_pir_database_busy".into());
    let canonical = match std::fs::canonicalize(root) {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // A dangling root alias must not choose a different lock from its target.
            if std::fs::symlink_metadata(root).is_ok() {
                return Err(busy());
            }
            let parent = root
                .parent()
                .filter(|path| !path.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."));
            std::fs::create_dir_all(parent).map_err(|_| busy())?;
            std::fs::canonicalize(parent)
                .map_err(|_| busy())?
                .join(root.file_name().ok_or_else(busy)?)
        }
        Err(_) => return Err(busy()),
    };
    let parent = canonical.parent().ok_or_else(busy)?;
    let directory = parent.join(".z-engine-bootstrap-locks");
    if canonical == directory {
        return Err(busy());
    }
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    match builder.create(&directory) {
        Ok(()) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))
                    .map_err(|_| busy())?;
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(_) => return Err(busy()),
    }
    let metadata = std::fs::symlink_metadata(&directory).map_err(|_| busy())?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(busy());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o7777 != 0o700 {
            return Err(busy());
        }
    }
    // Preserve the filesystem's own case/normalization alias semantics, including
    // before the wallet exists. Hashing a differently-cased name would not do so.
    let path = directory.join(canonical.file_name().ok_or_else(busy)?);
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = match options.open(&path) {
        Ok(file) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                file.set_permissions(std::fs::Permissions::from_mode(0o600))
                    .map_err(|_| busy())?;
            }
            file
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let metadata = std::fs::symlink_metadata(&path).map_err(|_| busy())?;
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                return Err(busy());
            }
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .map_err(|_| busy())?
        }
        Err(_) => return Err(busy()),
    };
    let metadata = std::fs::symlink_metadata(&path).map_err(|_| busy())?;
    let opened = file.metadata().map_err(|_| busy())?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || !opened.is_file() {
        return Err(busy());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if metadata.permissions().mode() & 0o7777 != 0o600
            || metadata.dev() != opened.dev()
            || metadata.ino() != opened.ino()
        {
            return Err(busy());
        }
    }
    // Same bounded OS lock semantics as database_lease; the handle owns it through
    // seed authentication, account creation, Pending publication and metadata.
    file.try_lock().map_err(|_| busy())?;
    Ok(file)
}
