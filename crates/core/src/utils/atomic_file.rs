//! Same-directory temporary write plus rename. Secret-bearing files are created with mode 0600
//! on Unix before any bytes are written.
//!
//! Several files can be replaced as one write-set: each destination is snapshotted in memory,
//! every payload is written to a sibling temp file, then the temps are renamed in order. A
//! failure restores already-replaced destinations from those snapshots. Restoration errors are
//! returned together with the original failure. Unconsumed temporary files are removed.
//!
//! Directory `fsync` after rename is best-effort: some filesystems do not support it, and a
//! sync failure does not undo a completed replacement.

use std::path::{Path, PathBuf};

use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;

pub struct FileReplace {
    pub path: PathBuf,
    pub bytes: Vec<u8>,
    pub private: bool,
}

/// Write `bytes` to `path` via a unique sibling temp file, then rename over the destination.
///
/// When `private` is set on Unix the temp file is created with mode 0600 before the payload is
/// written. Errors never include the payload.
pub async fn write_bytes(path: &Path, bytes: &[u8], private: bool) -> Result<(), String> {
    replace_files(&[FileReplace { path: path.to_path_buf(), bytes: bytes.to_vec(), private }]).await
}

/// Replace every destination as one reversible set. Already-renamed files are restored if a later
/// rename fails.
pub async fn replace_files(files: &[FileReplace]) -> Result<(), String> {
    replace_files_inner(files, None).await
}

/// Like [`replace_files`], but fails just before renaming `files[index]`. Used by tests so
/// injection cannot leak into an unrelated concurrent write.
pub async fn replace_files_failing_before(files: &[FileReplace], index: usize) -> Result<(), String> {
    replace_files_inner(files, Some(index)).await
}

async fn replace_files_inner(files: &[FileReplace], fail_before: Option<usize>) -> Result<(), String> {
    if files.is_empty() {
        return Ok(());
    }
    let mut originals = Vec::with_capacity(files.len());
    let mut tmps = Vec::with_capacity(files.len());
    for file in files {
        let original = if file.path.exists() {
            match tokio::fs::read(&file.path).await {
                Ok(bytes) => Some(bytes),
                Err(err) => {
                    remove_all(&tmps).await;
                    return Err(err.to_string());
                }
            }
        } else {
            None
        };
        originals.push(original);
        let tmp = tmp_path(&file.path);
        if let Err(err) = write_tmp(&tmp, &file.bytes, file.private).await {
            let _ = tokio::fs::remove_file(&tmp).await;
            remove_all(&tmps).await;
            return Err(err);
        }
        tmps.push(tmp);
    }
    for (index, file) in files.iter().enumerate() {
        if fail_before == Some(index) {
            return abort_replace(files, &originals, &tmps, index, "injected rename failure".to_string()).await;
        }
        if let Err(err) = rename_replace(&tmps[index], &file.path).await {
            return abort_replace(files, &originals, &tmps, index, err).await;
        }
        if let Err(err) = enforce_private(&file.path, file.private).await {
            return abort_replace(files, &originals, &tmps, index + 1, err).await;
        }
        sync_parent(&file.path).await;
    }
    Ok(())
}

async fn abort_replace(
    files: &[FileReplace],
    originals: &[Option<Vec<u8>>],
    tmps: &[PathBuf],
    renamed: usize,
    err: String,
) -> Result<(), String> {
    let restore = restore_prefix(files, originals, renamed).await;
    let leftover = if renamed < tmps.len() { &tmps[renamed..] } else { &[] };
    remove_all(leftover).await;
    match restore {
        Ok(()) => Err(err),
        Err(restore) => Err(format!("{err}; previous files were not restored: {restore}")),
    }
}

async fn restore_prefix(files: &[FileReplace], originals: &[Option<Vec<u8>>], renamed: usize) -> Result<(), String> {
    let mut errors = Vec::new();
    for index in (0..renamed).rev() {
        let result = match &originals[index] {
            Some(bytes) => write_tmp_and_rename(&files[index].path, bytes, files[index].private).await,
            None => match tokio::fs::remove_file(&files[index].path).await {
                Ok(()) => Ok(()),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(err) => Err(err.to_string()),
            },
        };
        if let Err(err) = result {
            errors.push(err);
        }
    }
    if errors.is_empty() { Ok(()) } else { Err(errors.join("; ")) }
}

async fn remove_all(paths: &[PathBuf]) {
    for path in paths {
        let _ = tokio::fs::remove_file(path).await;
    }
}

async fn write_tmp_and_rename(path: &Path, bytes: &[u8], private: bool) -> Result<(), String> {
    let tmp = tmp_path(path);
    write_tmp(&tmp, bytes, private).await?;
    rename_replace(&tmp, path).await?;
    enforce_private(path, private).await?;
    Ok(())
}

fn tmp_path(path: &Path) -> PathBuf {
    let parent = path.parent().filter(|dir| !dir.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."));
    let name = path.file_name().and_then(|name| name.to_str()).unwrap_or("file");
    parent.join(format!(".{name}.tmp-{}", crate::utils::generate_str_uid()))
}

async fn write_tmp(tmp: &Path, bytes: &[u8], private: bool) -> Result<(), String> {
    if let Some(parent) = tmp.parent() {
        tokio::fs::create_dir_all(parent).await.map_err(|err| err.to_string())?;
    }
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    if private {
        opts.mode(0o600);
    }
    #[cfg(not(unix))]
    let _ = private;
    let mut file = opts.open(tmp).await.map_err(|err| err.to_string())?;
    file.write_all(bytes).await.map_err(|err| err.to_string())?;
    file.flush().await.map_err(|err| err.to_string())?;
    file.sync_all().await.map_err(|err| err.to_string())?;
    Ok(())
}

async fn rename_replace(from: &Path, to: &Path) -> Result<(), String> {
    match tokio::fs::rename(from, to).await {
        Ok(()) => Ok(()),
        Err(err) if to.exists() => replace_existing(from, to, err).await,
        Err(err) => Err(err.to_string()),
    }
}

/// Unix `rename` replaces an existing destination. Elsewhere (notably Windows) the destination
/// is moved aside first so a failed second rename can put it back instead of leaving a hole.
#[allow(clippy::unused_async)]
async fn replace_existing(from: &Path, to: &Path, err: std::io::Error) -> Result<(), String> {
    #[cfg(unix)]
    {
        let _ = (from, to);
        Err(err.to_string())
    }
    #[cfg(not(unix))]
    {
        let _ = err;
        let backup = tmp_path(to);
        tokio::fs::rename(to, &backup).await.map_err(|err| err.to_string())?;
        match tokio::fs::rename(from, to).await {
            Ok(()) => {
                let _ = tokio::fs::remove_file(&backup).await;
                Ok(())
            }
            Err(err) => match tokio::fs::rename(&backup, to).await {
                Ok(()) => Err(err.to_string()),
                Err(restore) => Err(format!("{err}; destination was not restored: {restore}")),
            },
        }
    }
}

async fn enforce_private(path: &Path, private: bool) -> Result<(), String> {
    if !private {
        #[cfg(not(unix))]
        let _ = path;
        return Ok(());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .await
            .map_err(|err| err.to_string())?;
        let mode = tokio::fs::metadata(path).await.map_err(|err| err.to_string())?.permissions().mode() & 0o777;
        if mode != 0o600 {
            return Err("credential file mode is not 0600".to_string());
        }
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

async fn sync_parent(path: &Path) {
    let Some(parent) = path.parent().filter(|dir| !dir.as_os_str().is_empty()) else {
        return;
    };
    if let Ok(dir) = tokio::fs::File::open(parent).await {
        let _ = dir.sync_all().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(std::path::PathBuf);
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn temp() -> TempDir {
        let dir = std::env::temp_dir().join(format!("n2linkd-atomic-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    fn leftover_tmps(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
        std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp-"))
            .map(|entry| entry.path())
            .collect()
    }

    #[tokio::test]
    async fn a_failed_write_leaves_the_previous_file() {
        let dir = temp();
        let path = dir.0.join("secret.json");
        write_bytes(&path, br#"{"keep":true}"#, true).await.unwrap();
        let err = replace_files_failing_before(
            &[FileReplace { path: path.clone(), bytes: b"secret-value".to_vec(), private: true }],
            0,
        )
        .await
        .unwrap_err();
        assert!(!err.contains("secret-value"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), r#"{"keep":true}"#);
        assert!(leftover_tmps(&dir.0).is_empty(), "{:?}", leftover_tmps(&dir.0));
    }

    #[tokio::test]
    async fn a_failed_second_rename_restores_the_first_file() {
        let dir = temp();
        let flows = dir.0.join("flows.json");
        let creds = dir.0.join("flows_cred.json");
        write_bytes(&flows, b"old-flows", false).await.unwrap();
        write_bytes(&creds, b"old-creds", true).await.unwrap();
        let err = replace_files_failing_before(
            &[
                FileReplace { path: flows.clone(), bytes: b"new-flows".to_vec(), private: false },
                FileReplace { path: creds.clone(), bytes: b"secret-value".to_vec(), private: true },
            ],
            1,
        )
        .await
        .unwrap_err();
        assert!(!err.contains("secret-value"));
        assert_eq!(std::fs::read_to_string(&flows).unwrap(), "old-flows");
        assert_eq!(std::fs::read_to_string(&creds).unwrap(), "old-creds");
        assert!(leftover_tmps(&dir.0).is_empty(), "{:?}", leftover_tmps(&dir.0));
    }

    #[tokio::test]
    async fn replacement_is_complete() {
        let dir = temp();
        let path = dir.0.join("fleet.json");
        write_bytes(&path, b"one", true).await.unwrap();
        write_bytes(&path, b"two", true).await.unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "two");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }
}
