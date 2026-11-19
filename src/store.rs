//! Stores: the place where the served repositories live, a directory or a zip archive.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use tempfile::TempDir;

use crate::archive::{self, Stamp};
use crate::repository::{self, LookupError};

/// Reports whether `path` names a zip archive store, by its extension.
pub fn is_archive(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("zip"))
}

/// Creates an empty store at `path`, along with its missing parent directories: a directory, or a zip archive
/// holding nothing but its marker when the name ends with `.zip`.
pub fn init_store(path: &Path) -> anyhow::Result<()> {
    if fs::symlink_metadata(path).is_ok() {
        anyhow::bail!("{} already exists", path.display());
    }

    if is_archive(path) {
        archive::create_archive(path)
    } else {
        fs::create_dir_all(path)?;
        Ok(())
    }
}

/// Creates an empty store at `path` when nothing exists there yet, and reports whether it did.
pub fn ensure_store(path: &Path) -> anyhow::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(false),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => init_store(path).map(|()| true),
        Err(err) => Err(err.into()),
    }
}

/// Why an access to a repository did not run.
#[derive(Debug, thiserror::Error)]
pub enum AccessError {
    /// No repository is served under the name.
    #[error(transparent)]
    Lookup(#[from] LookupError),
    /// The archive of the store cannot be loaded.
    #[error("{0:#}")]
    Load(anyhow::Error),
}

/// What a change gives back: the result of the access, and whether the store recorded what it changed.
#[derive(Debug)]
pub struct Changed<T> {
    /// What the access returned.
    pub value: T,
    /// Whether the change reached the file of an archive store, always `Ok` for a directory store.
    ///
    /// When it failed, the change is discarded: the next access reloads the archive from its file.
    pub saved: anyhow::Result<()>,
}

/// What an operation on several repositories gives back.
#[derive(Debug, Default)]
pub struct Applied {
    /// What each successful operation reported, in the order of the names.
    pub done: Vec<String>,
    /// The failures, each naming its repository.
    pub errors: Vec<anyhow::Error>,
}

/// Holds the bare repositories that gitserve serves, under their path relative to the store.
#[derive(Debug)]
pub struct Store {
    backend: Backend,
}

#[derive(Debug)]
enum Backend {
    Directory(Directory),
    Archive(Archive),
}

/// A directory store. Each repository has its own lock: reads share it, a push takes it alone, since the storage
/// does not guard against concurrent writers.
#[derive(Debug)]
struct Directory {
    root: PathBuf,
    locks: Mutex<HashMap<PathBuf, Arc<RwLock<()>>>>,
}

/// An archive store: the archive is extracted into a temporary directory, its image, and written back to its file
/// after each change. Writing the archive covers every repository, so the whole image has a single lock.
#[derive(Debug)]
struct Archive {
    path: PathBuf,
    image: RwLock<Image>,
}

/// The extracted image of an archive.
#[derive(Debug)]
struct Image {
    dir: TempDir,
    /// The state of the archive file that the image reflects. `None` makes the next access reload the file.
    stamp: Option<Stamp>,
}

impl Store {
    /// Creates the store of the directory `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            backend: Backend::Directory(Directory {
                root: root.into(),
                locks: Mutex::default(),
            }),
        }
    }

    /// Opens the existing store at `path`: a directory, or a zip archive when its name ends with `.zip`,
    /// which is loaded at once.
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let meta = match fs::metadata(path) {
            Ok(meta) => meta,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                anyhow::bail!("{} does not exist", path.display())
            }
            Err(err) => return Err(err.into()),
        };

        if is_archive(path) && meta.is_file() {
            let image = Archive::load(path)?;
            Ok(Self {
                backend: Backend::Archive(Archive {
                    path: path.to_path_buf(),
                    image: RwLock::new(image),
                }),
            })
        } else if meta.is_dir() {
            Ok(Self::new(path))
        } else {
            anyhow::bail!("{} is neither a directory nor a zip archive", path.display())
        }
    }

    /// Runs `access` on the directory of the repository served under `name`, which other reads may access at the
    /// same time, but no change.
    ///
    /// It blocks until the access is granted: call it outside of the async runtime.
    pub fn view<T>(&self, name: &str, access: impl FnOnce(&Path) -> T) -> Result<T, AccessError> {
        match &self.backend {
            Backend::Directory(directory) => {
                let dir = repository::find_repository(&directory.root, name)?;
                let lock = directory.lock(&dir);
                let _guard = lock.read().unwrap_or_else(PoisonError::into_inner);
                Ok(access(&dir))
            }
            Backend::Archive(archive) => archive.read(|root| {
                let dir = repository::find_repository(root, name)?;
                Ok(access(&dir))
            })?,
        }
    }

    /// Runs `access` on the directory of the repository served under `name`, which no other access reaches at the
    /// same time, then records the change: an archive store is written back to its file.
    ///
    /// It blocks until the access is granted: call it outside of the async runtime.
    pub fn change<T>(&self, name: &str, access: impl FnOnce(&Path) -> T) -> Result<Changed<T>, AccessError> {
        match &self.backend {
            Backend::Directory(directory) => {
                let dir = repository::find_repository(&directory.root, name)?;
                let lock = directory.lock(&dir);
                let _guard = lock.write().unwrap_or_else(PoisonError::into_inner);
                Ok(Changed {
                    value: access(&dir),
                    saved: Ok(()),
                })
            }
            Backend::Archive(archive) => archive.write(|root| {
                let dir = repository::find_repository(root, name)?;
                Ok(access(&dir))
            }),
        }
    }

    /// Creates empty bare repositories named `names`, with `HEAD` pointing to `branch`, going on after a failure.
    /// Each success reports `Created repository <name>`.
    pub fn create_repositories(&self, names: &[String], branch: &str) -> Applied {
        self.apply(names, false, |root, name| {
            repository::create_repository(root, name, branch)?;
            Ok(format!("Created repository {name}"))
        })
    }

    /// Deletes the repositories served under `names`, going on after a failure. Each success reports
    /// `Deleted repository <dir>`.
    ///
    /// In a directory store, a deletion waits for the requests that this `Store` serves on the repository. It cannot
    /// wait for those of another process, such as a `serve` running on the same directory: there, the deletion
    /// renames the repository away first, so that it vanishes at once. On Unix, a request already reading it then
    /// fails if it opens another file of it; on Windows, the rename fails while a file of it is open, and so does
    /// the deletion. An archive store is only changed by the process that writes its file.
    pub fn delete_repositories(&self, names: &[String]) -> Applied {
        self.apply(names, true, |root, name| {
            let dir = repository::delete_repository(root, name)?;
            Ok(format!("Deleted repository {dir}"))
        })
    }

    /// Runs `operation` on each name, with the root of the repositories, going on after a failure.
    ///
    /// In a directory store, an operation on an existing repository holds its lock when `lock_repository` is set.
    /// An archive store runs every operation in a single change, then writes its file once if any succeeded:
    /// when that write fails, nothing is done.
    fn apply(
        &self,
        names: &[String],
        lock_repository: bool,
        operation: impl Fn(&Path, &str) -> anyhow::Result<String>,
    ) -> Applied {
        let mut applied = Applied::default();
        let mut run = |root: &Path, name: &str| match operation(root, name) {
            Ok(report) => applied.done.push(report),
            Err(err) => applied.errors.push(err.context(name.to_string())),
        };

        match &self.backend {
            Backend::Directory(directory) => {
                for name in names {
                    let lock = match repository::find_repository(&directory.root, name) {
                        Ok(dir) if lock_repository => Some(directory.lock(&dir)),
                        _ => None,
                    };
                    let _guard = lock
                        .as_ref()
                        .map(|lock| lock.write().unwrap_or_else(PoisonError::into_inner));
                    run(&directory.root, name);
                }
            }
            Backend::Archive(archive) => {
                let mut image = archive.image.write().unwrap_or_else(PoisonError::into_inner);
                if let Err(err) = archive.reload_if_changed(&mut image) {
                    applied.errors.push(err);
                    return applied;
                }

                for name in names {
                    run(image.dir.path(), name);
                }
                if !applied.done.is_empty()
                    && let Err(err) = archive.persist(&mut image)
                {
                    applied.done.clear();
                    applied.errors.push(err);
                }
            }
        }

        applied
    }

    /// Returns the directories of the repositories of the store, in lexical order.
    pub fn repositories(&self) -> anyhow::Result<Vec<String>> {
        match &self.backend {
            Backend::Directory(directory) => repository::list_repositories(&directory.root),
            Backend::Archive(archive) => archive.read(repository::list_repositories).map_err(|err| match err {
                AccessError::Load(err) => err,
                AccessError::Lookup(err) => err.into(),
            })?,
        }
    }

    /// Waits for the change in progress, if any, and prevents any other from starting, so that the process can
    /// exit without interrupting the write of an archive.
    pub fn close(&self) {
        if let Backend::Archive(archive) = &self.backend {
            std::mem::forget(archive.image.write().unwrap_or_else(PoisonError::into_inner));
        }
    }
}

impl Directory {
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

impl Archive {
    /// Extracts the archive at `path` into a new image.
    fn load(path: &Path) -> anyhow::Result<Image> {
        let dir = tempfile::Builder::new().prefix("gitserve-").tempdir()?;
        let stamp = archive::read_archive(path, dir.path())?;
        Ok(Image {
            dir,
            stamp: Some(stamp),
        })
    }

    /// Reports whether `image` reflects the archive file, which another gitserve command may have written.
    fn is_fresh(&self, image: &Image) -> bool {
        image.stamp.is_some() && archive::stamp(&self.path).ok() == image.stamp
    }

    /// Reloads the archive into `image` when its file changed since it was last read or written.
    fn reload_if_changed(&self, image: &mut Image) -> anyhow::Result<()> {
        if !self.is_fresh(image) {
            *image = Self::load(&self.path)?;
        }
        Ok(())
    }

    /// Writes `image` back to the archive file. When the write fails, the image holds changes that the file lacks:
    /// the next access then reloads the file, so that these changes are discarded rather than served.
    fn persist(&self, image: &mut Image) -> anyhow::Result<()> {
        match archive::write_archive(&self.path, image.dir.path()) {
            Ok(stamp) => {
                image.stamp = Some(stamp);
                Ok(())
            }
            Err(err) => {
                image.stamp = None;
                Err(err)
            }
        }
    }

    /// Runs `access` on the root of the image, shared with other reads. Reloading a changed archive replaces the
    /// image: it needs exclusive access, under which `access` then runs.
    fn read<T>(&self, access: impl FnOnce(&Path) -> T) -> Result<T, AccessError> {
        {
            let image = self.image.read().unwrap_or_else(PoisonError::into_inner);
            if self.is_fresh(&image) {
                return Ok(access(image.dir.path()));
            }
        }

        let mut image = self.image.write().unwrap_or_else(PoisonError::into_inner);
        self.reload_if_changed(&mut image).map_err(AccessError::Load)?;
        Ok(access(image.dir.path()))
    }

    /// Runs `access` on the root of the image, with exclusive access, then writes the image back to the file.
    fn write<T>(&self, access: impl FnOnce(&Path) -> Result<T, AccessError>) -> Result<Changed<T>, AccessError> {
        let mut image = self.image.write().unwrap_or_else(PoisonError::into_inner);
        self.reload_if_changed(&mut image).map_err(AccessError::Load)?;
        let value = access(image.dir.path())?;
        let saved = self.persist(&mut image);
        Ok(Changed { value, saved })
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
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        fs::write(dir.join("config"), "").unwrap();
    }

    fn names(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| name.to_string()).collect()
    }

    fn locks(store: &Store) -> usize {
        match &store.backend {
            Backend::Directory(directory) => directory.locks.lock().unwrap().len(),
            Backend::Archive(_) => panic!("not a directory store"),
        }
    }

    /// Returns a directory store and an archive store, both empty, in `dir`.
    fn both_stores(dir: &Path) -> [Store; 2] {
        let directory = dir.join("store");
        let archive = dir.join("store.zip");
        init_store(&directory).unwrap();
        init_store(&archive).unwrap();
        [Store::open(&directory).unwrap(), Store::open(&archive).unwrap()]
    }

    #[test]
    fn stores_are_initialized_once() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["a/b/store", "a/b/store.zip", "a/b/upper.ZIP"] {
            let path = dir.path().join(name);
            assert!(Store::open(&path).unwrap_err().to_string().ends_with("does not exist"));
            assert!(ensure_store(&path).unwrap());
            assert!(!ensure_store(&path).unwrap());
            assert!(init_store(&path).unwrap_err().to_string().ends_with("already exists"));
            assert!(Store::open(&path).unwrap().repositories().unwrap().is_empty());
        }
        assert!(dir.path().join("a/b/store").is_dir());
        assert!(dir.path().join("a/b/store.zip").is_file());

        fs::write(dir.path().join("file"), "").unwrap();
        let error = Store::open(&dir.path().join("file")).unwrap_err().to_string();
        assert!(error.ends_with("is neither a directory nor a zip archive"), "{error}");
    }

    #[test]
    fn repositories_are_created_listed_and_deleted() {
        let dir = tempfile::tempdir().unwrap();
        for store in both_stores(dir.path()) {
            let created = store.create_repositories(&names(&["b.git", "a/repo", "b"]), "main");
            assert_eq!(created.done, ["Created repository b.git", "Created repository a/repo"]);
            assert_eq!(created.errors.len(), 1);
            assert!(format!("{:#}", created.errors[0]).starts_with("b: repository b.git already exists"));
            assert_eq!(store.repositories().unwrap(), ["a/repo", "b.git"]);

            let deleted = store.delete_repositories(&names(&["b", "b"]));
            assert_eq!(deleted.done, ["Deleted repository b.git"]);
            assert_eq!(deleted.errors.len(), 1);
            assert_eq!(store.repositories().unwrap(), ["a/repo"]);
        }
    }

    #[test]
    fn archive_changes_reach_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("repos.zip");
        init_store(&path).unwrap();
        let store = Store::open(&path).unwrap();
        store.create_repositories(&names(&["repo.git"]), "main");

        let changed = store
            .change("repo", |dir| fs::write(dir.join("description"), "changed"))
            .unwrap();
        assert!(changed.value.is_ok() && changed.saved.is_ok());

        let reopened = Store::open(&path).unwrap();
        assert_eq!(reopened.repositories().unwrap(), ["repo.git"]);
        let description = reopened.view("repo", |dir| fs::read_to_string(dir.join("description")).unwrap());
        assert_eq!(description.unwrap(), "changed");
    }

    #[test]
    fn an_archive_changed_on_disk_is_reloaded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("repos.zip");
        init_store(&path).unwrap();
        let server = Store::open(&path).unwrap();
        assert!(server.repositories().unwrap().is_empty());

        // Another gitserve command changes the archive.
        Store::open(&path)
            .unwrap()
            .create_repositories(&names(&["new"]), "main");
        assert_eq!(server.repositories().unwrap(), ["new"]);
        assert!(server.view("new", |_| ()).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_write_discards_the_change() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("parent");
        let path = parent.join("repos.zip");
        init_store(&path).unwrap();
        let store = Store::open(&path).unwrap();
        store.create_repositories(&names(&["kept"]), "main");

        // The archive cannot be replaced while its directory is read-only.
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o555)).unwrap();
        let created = store.create_repositories(&names(&["lost"]), "main");
        let changed = store
            .change("kept", |dir| fs::write(dir.join("description"), "lost"))
            .unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o755)).unwrap();

        assert!(created.done.is_empty());
        assert!(format!("{:#}", created.errors[0]).contains("cannot write the archive"));
        assert!(changed.saved.is_err());
        assert_eq!(store.repositories().unwrap(), ["kept"]);
        let description = store.view("kept", |dir| dir.join("description").exists()).unwrap();
        assert!(!description);
    }

    #[test]
    fn accesses_find_the_repository() {
        let dir = tempfile::tempdir().unwrap();
        for store in both_stores(dir.path()) {
            store.create_repositories(&names(&["repo.git"]), "main");
            let name = |dir: &Path| dir.file_name().unwrap().to_string_lossy().into_owned();

            assert_eq!(store.view("repo", name).unwrap(), "repo.git");
            assert_eq!(store.change("repo.git", name).unwrap().value, "repo.git");
            assert!(matches!(
                store.view("missing", |_| ()),
                Err(AccessError::Lookup(LookupError::NotFound(_)))
            ));
            assert!(matches!(
                store.change("missing", |_| ()),
                Err(AccessError::Lookup(LookupError::NotFound(_)))
            ));
        }
    }

    #[test]
    fn names_of_one_repository_share_its_lock() {
        let root = tempfile::tempdir().unwrap();
        make_repository(&root.path().join("repo.git"));
        let store = Store::new(root.path());

        store.view("repo", |_| ()).unwrap();
        store.view("repo.git", |_| ()).unwrap();
        assert_eq!(locks(&store), 1);
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
        assert_eq!(locks(&store), 1);
    }

    #[test]
    fn a_change_excludes_other_accesses() {
        let dir = tempfile::tempdir().unwrap();
        for store in both_stores(dir.path()) {
            store.create_repositories(&names(&["repo", "other"]), "main");
            let inside = AtomicUsize::new(0);
            let start = Barrier::new(4);

            let (store, inside, start) = (&store, &inside, &start);
            thread::scope(|scope| {
                for (writer, name) in [(true, "repo"), (true, "repo"), (false, "repo"), (false, "other")] {
                    scope.spawn(move || {
                        start.wait();
                        // Only the accesses to `repo` are counted: in a directory store, `other` has a lock of its
                        // own, while an archive store also keeps it out of a change.
                        let counted = name == "repo";
                        let access = |_: &Path| {
                            let others = if counted {
                                inside.fetch_add(1, Ordering::SeqCst)
                            } else {
                                0
                            };
                            thread::sleep(Duration::from_millis(50));
                            if counted {
                                inside.fetch_sub(1, Ordering::SeqCst);
                            }
                            others
                        };
                        if writer {
                            let others = store.change(name, access).unwrap().value;
                            assert_eq!(others, 0, "a change ran beside another access of its repository");
                        } else {
                            store.view(name, access).unwrap();
                        }
                    });
                }
            });
        }
    }
}
