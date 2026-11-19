//! Stores: the place where the served repositories live.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use crate::repository::{self, LookupError};

/// Reports whether `path` names a zip archive store, by its extension.
fn is_archive(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("zip"))
}

/// Fails for a zip archive store, which gitserve does not support yet.
fn refuse_archive(path: &Path) -> anyhow::Result<()> {
    if is_archive(path) {
        anyhow::bail!("{}: zip archive stores are not supported yet", path.display());
    }
    Ok(())
}

/// Creates an empty store at `path`, along with its missing parent directories.
pub fn init_store(path: &Path) -> anyhow::Result<()> {
    if fs::symlink_metadata(path).is_ok() {
        anyhow::bail!("{} already exists", path.display());
    }
    refuse_archive(path)?;
    fs::create_dir_all(path)?;
    Ok(())
}

/// Creates an empty store at `path` when nothing exists there yet, and reports whether it did.
pub fn ensure_store(path: &Path) -> anyhow::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(false),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => init_store(path).map(|()| true),
        Err(err) => Err(err.into()),
    }
}

/// A directory holding bare repositories, which it serves under their path relative to it.
///
/// Each repository has its own lock: reads share it, a push takes it alone, since the storage does not guard
/// against concurrent writers.
#[derive(Debug)]
pub struct Store {
    root: PathBuf,
    locks: Mutex<HashMap<PathBuf, Arc<RwLock<()>>>>,
}

impl Store {
    /// Creates the store of the directory `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            locks: Mutex::default(),
        }
    }

    /// Opens the existing store at `path`.
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        refuse_archive(path)?;
        match fs::metadata(path) {
            Ok(meta) if meta.is_dir() => Ok(Self::new(path)),
            Ok(_) => anyhow::bail!("{} is neither a directory nor a zip archive", path.display()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                anyhow::bail!("{} does not exist", path.display())
            }
            Err(err) => Err(err.into()),
        }
    }

    /// The directory of the store.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Creates an empty bare repository named `name`, with `HEAD` pointing to `branch`.
    pub fn create_repository(&self, name: &str, branch: &str) -> anyhow::Result<()> {
        repository::create_repository(&self.root, name, branch)
    }

    /// Deletes the repository served under `name`, once the requests on it are over, and returns its directory.
    pub fn delete_repository(&self, name: &str) -> anyhow::Result<String> {
        self.change(name, |_| repository::delete_repository(&self.root, name))?
    }

    /// Returns the directories of the repositories of the store, in lexical order.
    pub fn repositories(&self) -> anyhow::Result<Vec<String>> {
        repository::list_repositories(&self.root)
    }

    /// Runs `access` on the directory of the repository served under `name`, which other reads may access at the
    /// same time, but no push.
    ///
    /// It blocks until the access is granted: call it outside of the async runtime.
    pub fn view<T>(&self, name: &str, access: impl FnOnce(&Path) -> T) -> Result<T, LookupError> {
        let dir = repository::find_repository(&self.root, name)?;
        let lock = self.lock(&dir);
        let _guard = lock.read().unwrap_or_else(PoisonError::into_inner);
        Ok(access(&dir))
    }

    /// Runs `access` on the directory of the repository served under `name`, which no other access reaches at the
    /// same time.
    ///
    /// It blocks until the access is granted: call it outside of the async runtime.
    pub fn change<T>(&self, name: &str, access: impl FnOnce(&Path) -> T) -> Result<T, LookupError> {
        let dir = repository::find_repository(&self.root, name)?;
        let lock = self.lock(&dir);
        let _guard = lock.write().unwrap_or_else(PoisonError::into_inner);
        Ok(access(&dir))
    }

    /// Returns the lock of the repository in `dir`. The lock is not held across an access, and a panic of an
    /// access leaves the repository as Git would after a crash, so a poisoned lock is still used.
    ///
    /// Locks are keyed by the canonical path of the repository, so that every spelling of one directory gets its
    /// lock: names differing by case on a case-insensitive file system, or reaching it through a symbolic link.
    fn lock(&self, dir: &Path) -> Arc<RwLock<()>> {
        let key = fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
        let mut locks = self.locks.lock().unwrap_or_else(PoisonError::into_inner);
        Arc::clone(locks.entry(key).or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;
    use std::time::Duration;

    fn make_repository(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(dir.join("config"), "").unwrap();
    }

    #[test]
    fn stores_are_initialized_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a/b/store");

        assert!(Store::open(&path).unwrap_err().to_string().ends_with("does not exist"));
        assert!(ensure_store(&path).unwrap());
        assert!(path.is_dir());
        assert!(!ensure_store(&path).unwrap());
        assert!(init_store(&path).unwrap_err().to_string().ends_with("already exists"));
        assert_eq!(Store::open(&path).unwrap().root(), path);

        fs::write(dir.path().join("file"), "").unwrap();
        let error = Store::open(&dir.path().join("file")).unwrap_err().to_string();
        assert!(error.ends_with("is neither a directory nor a zip archive"), "{error}");
    }

    #[test]
    fn archives_are_refused_for_now() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("repos.ZIP");

        assert!(init_store(&path).unwrap_err().to_string().contains("not supported yet"));
        assert!(ensure_store(&path).is_err());
        assert!(Store::open(&path).is_err());
        assert!(!path.exists());
    }

    #[test]
    fn repositories_are_created_listed_and_deleted() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::new(root.path());

        store.create_repository("b.git", "main").unwrap();
        store.create_repository("a/repo", "main").unwrap();
        assert_eq!(store.repositories().unwrap(), ["a/repo", "b.git"]);
        assert_eq!(store.delete_repository("b").unwrap(), "b.git");
        assert_eq!(store.repositories().unwrap(), ["a/repo"]);
        assert!(store.delete_repository("b").is_err());
    }

    #[test]
    fn accesses_find_the_repository() {
        let root = tempfile::tempdir().unwrap();
        make_repository(&root.path().join("repo.git"));
        let store = Store::new(root.path());

        assert_eq!(store.view("repo", Path::to_path_buf), Ok(root.path().join("repo.git")));
        assert_eq!(
            store.change("repo.git", Path::to_path_buf),
            Ok(root.path().join("repo.git"))
        );
        assert_eq!(
            store.view("missing", |_| ()),
            Err(LookupError::NotFound("missing".to_string()))
        );
    }

    #[test]
    fn names_of_one_repository_share_its_lock() {
        let root = tempfile::tempdir().unwrap();
        make_repository(&root.path().join("repo.git"));
        let store = Store::new(root.path());

        store.view("repo", |_| ()).unwrap();
        store.view("repo.git", |_| ()).unwrap();
        assert_eq!(store.locks.lock().unwrap().len(), 1);
    }

    #[test]
    fn spellings_of_one_directory_share_its_lock() {
        let root = tempfile::tempdir().unwrap();
        make_repository(&root.path().join("repo"));
        let store = Store::new(root.path());

        store.view("repo", |_| ()).unwrap();
        // Only a case-insensitive file system finds the repository under another case.
        if repository::is_repository(&root.path().join("REPO")) {
            store.view("REPO", |_| ()).unwrap();
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(root.path().join("repo"), root.path().join("link")).unwrap();
            store.view("link", |_| ()).unwrap();
        }
        assert_eq!(store.locks.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_change_excludes_other_accesses() {
        let root = tempfile::tempdir().unwrap();
        make_repository(&root.path().join("repo"));
        let store = Store::new(root.path());
        let inside = AtomicUsize::new(0);
        let start = Barrier::new(4);

        let (store, inside, start) = (&store, &inside, &start);
        thread::scope(|scope| {
            for writer in [true, true, false, false] {
                scope.spawn(move || {
                    start.wait();
                    let access = |_: &Path| {
                        let others = inside.fetch_add(1, Ordering::SeqCst);
                        thread::sleep(Duration::from_millis(50));
                        inside.fetch_sub(1, Ordering::SeqCst);
                        others
                    };
                    let others = if writer {
                        store.change("repo", access).unwrap()
                    } else {
                        store.view("repo", access).unwrap()
                    };
                    if writer {
                        assert_eq!(others, 0, "a change ran beside another access");
                    }
                });
            }
        });
    }
}
