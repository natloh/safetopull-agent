//! Files under the data directory are replaced whole: a reader, or the agent after a crash, sees
//! the previous content or the new content, never part of either.

use std::io;
use std::path::Path;

use tokio::io::AsyncWriteExt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
    /// Readable by the owner only, from the moment the file exists.
    Private,
    Shared,
}

pub async fn write_atomically(path: &Path, contents: &[u8], visibility: Visibility) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let mut staging = path.as_os_str().to_owned();
    staging.push(".tmp");
    let staging = Path::new(&staging);

    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);

    #[cfg(unix)]
    if visibility == Visibility::Private {
        options.mode(0o600);
    }
    #[cfg(not(unix))]
    let _ = visibility;

    let mut file = options.open(staging).await?;
    file.write_all(contents).await?;
    file.sync_all().await?;
    drop(file);

    tokio::fs::rename(staging, path).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn replaces_the_file_and_leaves_no_staging_file_behind() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("nested").join("link.json");

        write_atomically(&path, b"first", Visibility::Private).await.unwrap();
        write_atomically(&path, b"second", Visibility::Private).await.unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), b"second");
        assert_eq!(std::fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_private_file_is_never_readable_by_others() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("link.json");

        write_atomically(&path, b"token", Visibility::Private).await.unwrap();

        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    }
}
