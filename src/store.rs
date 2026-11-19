//! Stores: the place where the served repositories live.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use crate::repository::{self, LookupError};

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

    /// The directory of the store.
    pub fn root(&self) -> &Path {
        &self.root
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
