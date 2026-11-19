//! Repositories: bare Git repositories, served under a slash-separated name.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Context;
use gix_hash::Kind;

/// Why a name does not lead to a repository.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum LookupError {
    /// The name cannot name a repository.
    #[error("invalid repository name: {0:?}")]
    InvalidName(String),
    /// No repository is served under the name.
    #[error("repository not found: {0}")]
    NotFound(String),
}

/// Checks that `name` can name a repository: a relative path, whose slash-separated elements are neither empty
/// nor `.` or `..`, and contain no backslash nor colon, which Windows would read as a path of its own.
pub fn check_name(name: &str) -> Result<(), LookupError> {
    let valid = !name.is_empty()
        && !name.contains(['\\', ':', '\0'])
        && name.split('/').all(|element| !matches!(element, "" | "." | ".."));
    if valid {
        Ok(())
    } else {
        Err(LookupError::InvalidName(name.to_string()))
    }
}

/// Reports whether `dir` holds a bare Git repository: a directory with `HEAD` and `config` files.
pub fn is_repository(dir: &Path) -> bool {
    ["HEAD", "config"]
        .iter()
        .all(|file| fs::metadata(dir.join(file)).is_ok_and(|meta| meta.is_file()))
}

/// Returns the directory of the repository served under `name` in `root`: `name` itself, or else `name` followed
/// by a `.git` suffix.
pub fn find_repository(root: &Path, name: &str) -> Result<PathBuf, LookupError> {
    check_name(name)?;
    [name.to_string(), format!("{name}.git")]
        .into_iter()
        .map(|dir| root.join(dir))
        .find(|dir| is_repository(dir))
        .ok_or_else(|| LookupError::NotFound(name.to_string()))
}

/// Returns the hash kind of the repository in `dir`, from the `extensions.objectFormat` entry of its `config`,
/// which Git writes for a SHA-256 repository only. As in Git, the last entry wins.
pub fn hash_kind(dir: &Path) -> anyhow::Result<Kind> {
    let config = fs::read_to_string(dir.join("config"))?;
    let mut section = String::new();
    let mut format = None;
    for line in config.lines() {
        let line = strip_comment(line).trim();
        if let Some(header) = line.strip_prefix('[').and_then(|rest| rest.split_once(']')) {
            section = header.0.trim().to_ascii_lowercase();
            continue;
        }

        let Some((key, value)) = line.split_once('=') else {
            continue;
        };

        if section == "extensions" && key.trim().eq_ignore_ascii_case("objectformat") {
            format = Some(value.trim().trim_matches('"').to_string());
        }
    }

    match format.as_deref() {
        None | Some("sha1") => Ok(Kind::Sha1),
        Some("sha256") => Ok(Kind::Sha256),
        Some(other) => anyhow::bail!("unknown object format {other:?} in {}", dir.display()),
    }
}

/// Returns `line` without its comment, which starts with `#` or `;` outside of a quoted string.
fn strip_comment(line: &str) -> &str {
    let mut quoted = false;
    for (at, character) in line.char_indices() {
        match character {
            '"' => quoted = !quoted,
            '#' | ';' if !quoted => return &line[..at],
            _ => {}
        }
    }

    line
}

/// The start of the names of the directories in which repositories are built or torn down. `list_repositories`
/// skips them.
pub(crate) const STAGING_PREFIX: &str = ".gitserve-";

/// Checks that `branch` can name the branch of a new repository, with the rules of `git check-ref-format --branch`.
pub fn check_branch(branch: &str) -> anyhow::Result<()> {
    // Validating the full reference cannot apply the rules on the start of the branch name: a branch name may not
    // start with a dash, which would read as an option.
    let reference = format!("refs/heads/{branch}");
    let valid = !branch.starts_with('-') && gix_validate::reference::branch_name(reference.as_str().into()).is_ok();
    if valid {
        Ok(())
    } else {
        anyhow::bail!("invalid branch name {branch:?}")
    }
}

/// Creates an empty bare repository named `name` in `root`, with `HEAD` pointing to `branch`.
///
/// The repository is built in a hidden directory next to it, then renamed into place: a server running on `root`
/// meanwhile never sees it half made.
pub fn create_repository(root: &Path, name: &str, branch: &str) -> anyhow::Result<()> {
    check_name(name)?;
    check_branch(branch)?;
    if name.split('/').any(|element| element.starts_with(STAGING_PREFIX)) {
        anyhow::bail!("{name}: the names starting with {STAGING_PREFIX} are reserved to gitserve");
    }

    if let Ok(dir) = find_repository(root, name) {
        anyhow::bail!("repository {} already exists", relative_name(root, &dir));
    }

    let target = root.join(name);
    if fs::symlink_metadata(&target).is_ok() {
        anyhow::bail!("{name} already exists");
    }

    for (end, _) in name.match_indices('/') {
        let ancestor = &name[..end];
        if is_repository(&root.join(ancestor)) {
            anyhow::bail!("{name} would be inside repository {ancestor}");
        }
    }

    let parent = target.parent().unwrap_or(root);
    fs::create_dir_all(parent).with_context(|| format!("cannot create {}", parent.display()))?;
    let staging = tempfile::Builder::new()
        .prefix(STAGING_PREFIX)
        .tempdir_in(parent)
        .with_context(|| format!("cannot create a directory in {}", parent.display()))?;
    let built = staging.path().join("repo");
    drop(storage_git_gix::GixStore::open_or_init(&built, Kind::Sha1)?);
    let head = built.join("HEAD");
    fs::write(&head, format!("ref: refs/heads/{branch}\n"))
        .with_context(|| format!("cannot write {}", head.display()))?;
    fs::rename(&built, &target).with_context(|| format!("cannot move the repository to {}", target.display()))?;
    Ok(())
}

/// Deletes the repository served under `name` in `root`, with all its content, and returns its directory relative
/// to `root`.
///
/// The repository is first renamed to a hidden directory, then deleted: a server running on `root` meanwhile sees
/// it vanish at once, not half deleted. Once renamed, the repository is gone from the store: failing to delete the
/// hidden directory only warrants a warning.
pub fn delete_repository(root: &Path, name: &str) -> anyhow::Result<String> {
    let dir = find_repository(root, name)?;
    let parent = dir.parent().unwrap_or(root);
    let trash = tempfile::Builder::new()
        .prefix(STAGING_PREFIX)
        .tempdir_in(parent)
        .with_context(|| format!("cannot create a directory in {}", parent.display()))?;
    fs::rename(&dir, trash.path().join("repo")).with_context(|| format!("cannot move {}", dir.display()))?;

    let leftover = trash.path().to_path_buf();
    if let Err(err) = trash.close() {
        log::warn!(
            "The repository {name} is deleted, but {} is left behind: {err}",
            leftover.display()
        );
    }

    Ok(relative_name(root, &dir))
}

/// Returns the directories of the repositories under `root`, relative to it, with slashes, in lexical order.
/// It does not look for repositories inside other ones, nor follows symbolic links.
pub fn list_repositories(root: &Path) -> anyhow::Result<Vec<String>> {
    let mut names = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let context = || format!("cannot read {}", dir.display());
        for entry in fs::read_dir(&dir).with_context(context)? {
            let entry = entry.with_context(context)?;
            let skipped = entry.file_name().to_string_lossy().starts_with(STAGING_PREFIX);
            if skipped || !entry.file_type().with_context(context)?.is_dir() {
                continue;
            }

            if is_repository(&entry.path()) {
                names.push(relative_name(root, &entry.path()));
            } else {
                pending.push(entry.path());
            }
        }
    }

    names.sort();
    Ok(names)
}

/// Returns the path of `dir` relative to `root`, with slashes.
fn relative_name(root: &Path, dir: &Path) -> String {
    let relative = dir.strip_prefix(root).unwrap_or(dir);
    let elements: Vec<_> = relative.iter().map(|element| element.to_string_lossy()).collect();
    elements.join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_repository(dir: &Path, config: &str) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        fs::write(dir.join("config"), config).unwrap();
    }

    #[test]
    fn check_name_accepts_relative_paths() {
        for name in ["repo", "repo.git", "team/repo", "a/b/c"] {
            assert_eq!(check_name(name), Ok(()), "{name}");
        }
    }

    #[test]
    fn check_name_rejects_invalid_names() {
        for name in [
            "", ".", "..", "/repo", "repo/", "a//b", "a/../b", "./a", "a\\b", "c:repo", "a\0b",
        ] {
            assert_eq!(
                check_name(name),
                Err(LookupError::InvalidName(name.to_string())),
                "{name:?}"
            );
        }
    }

    #[test]
    fn find_repository_tries_the_git_suffix() {
        let root = tempfile::tempdir().unwrap();
        make_repository(&root.path().join("plain"), "");
        make_repository(&root.path().join("bare.git"), "");

        assert_eq!(find_repository(root.path(), "plain"), Ok(root.path().join("plain")));
        assert_eq!(find_repository(root.path(), "bare"), Ok(root.path().join("bare.git")));
        assert_eq!(
            find_repository(root.path(), "bare.git"),
            Ok(root.path().join("bare.git"))
        );
        assert_eq!(
            find_repository(root.path(), "missing"),
            Err(LookupError::NotFound("missing".to_string()))
        );
    }

    #[test]
    fn find_repository_ignores_directories_that_are_not_repositories() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("empty")).unwrap();
        fs::create_dir_all(root.path().join("dir/HEAD")).unwrap();
        fs::write(root.path().join("dir/config"), "").unwrap();

        assert!(find_repository(root.path(), "empty").is_err());
        assert!(find_repository(root.path(), "dir").is_err());
    }

    #[test]
    fn hash_kind_reads_the_object_format() {
        let root = tempfile::tempdir().unwrap();
        let sha1 = root.path().join("sha1");
        let sha256 = root.path().join("sha256");
        let unknown = root.path().join("unknown");
        make_repository(&sha1, "[core]\n\tbare = true\n");
        make_repository(
            &sha256,
            "[core]\n\tbare = true\n[extensions]\n\tobjectFormat = sha256\n",
        );
        make_repository(&unknown, "[extensions]\n\tobjectformat = md5\n");

        assert_eq!(hash_kind(&sha1).unwrap(), Kind::Sha1);
        assert_eq!(hash_kind(&sha256).unwrap(), Kind::Sha256);
        assert!(hash_kind(&unknown).is_err());
    }

    #[test]
    fn hash_kind_follows_the_syntax_of_git_config() {
        let root = tempfile::tempdir().unwrap();
        let cases = [
            ("[extensions]\n\tobjectFormat = sha256 # SHA-256\n", Kind::Sha256),
            ("[Extensions]\n\tobjectformat = \"sha256\" ; quoted\n", Kind::Sha256),
            (
                "[extensions]\n\tobjectFormat = sha256\n\tobjectFormat = sha1\n",
                Kind::Sha1,
            ),
            ("[other]\n\tobjectFormat = sha256\n", Kind::Sha1),
            (
                "# objectFormat = sha256\n[extensions]\n; objectFormat = sha256\n",
                Kind::Sha1,
            ),
        ];
        for (at, (config, kind)) in cases.into_iter().enumerate() {
            let dir = root.path().join(at.to_string());
            make_repository(&dir, config);
            assert_eq!(hash_kind(&dir).unwrap(), kind, "{config:?}");
        }
    }

    #[test]
    fn check_branch_follows_git() {
        for branch in ["main", "master", "feature/x", "release-1.0"] {
            assert!(check_branch(branch).is_ok(), "{branch}");
        }
        for branch in ["", "HEAD", "-x", "--force", "a..b", "a b", "a~1", "x.lock", "x/"] {
            assert!(check_branch(branch).is_err(), "{branch:?}");
        }
    }

    #[test]
    fn create_repository_makes_a_bare_repository() {
        let root = tempfile::tempdir().unwrap();
        create_repository(root.path(), "team/project.git", "master").unwrap();

        let dir = root.path().join("team/project.git");
        assert!(is_repository(&dir));
        assert!(dir.join("objects/pack").is_dir());
        assert!(dir.join("refs/heads").is_dir());
        assert_eq!(
            fs::read_to_string(dir.join("HEAD")).unwrap(),
            "ref: refs/heads/master\n"
        );
        assert_eq!(hash_kind(&dir).unwrap(), Kind::Sha1);
        // Nothing is left of the directory in which the repository was built.
        assert_eq!(fs::read_dir(root.path().join("team")).unwrap().count(), 1);
    }

    #[test]
    fn create_repository_refuses_conflicts() {
        let root = tempfile::tempdir().unwrap();
        create_repository(root.path(), "repo.git", "main").unwrap();
        fs::write(root.path().join("file"), "").unwrap();

        let error = |name: &str| create_repository(root.path(), name, "main").unwrap_err().to_string();
        assert_eq!(error("repo"), "repository repo.git already exists");
        assert_eq!(error("repo.git"), "repository repo.git already exists");
        assert_eq!(error("file"), "file already exists");
        assert_eq!(
            error("repo.git/inner"),
            "repo.git/inner would be inside repository repo.git"
        );
        assert_eq!(error("../outside"), "invalid repository name: \"../outside\"");
    }

    #[test]
    fn create_repository_refuses_the_staging_prefix() {
        let root = tempfile::tempdir().unwrap();
        for name in [".gitserve-x", "team/.gitserve-x", ".gitserve-x/repo"] {
            let error = create_repository(root.path(), name, "main").unwrap_err().to_string();
            assert!(error.contains("reserved to gitserve"), "{name}: {error}");
        }
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);
    }

    #[test]
    fn create_repository_checks_the_branch() {
        let root = tempfile::tempdir().unwrap();
        assert!(create_repository(root.path(), "repo", "a..b").is_err());
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);
    }

    #[test]
    fn delete_repository_removes_it() {
        let root = tempfile::tempdir().unwrap();
        create_repository(root.path(), "team/repo.git", "main").unwrap();

        assert_eq!(delete_repository(root.path(), "team/repo").unwrap(), "team/repo.git");
        assert!(!root.path().join("team/repo.git").exists());
        assert_eq!(fs::read_dir(root.path().join("team")).unwrap().count(), 0);
        assert!(delete_repository(root.path(), "team/repo").is_err());
    }

    #[test]
    fn list_repositories_sorts_and_skips_nested_ones() {
        let root = tempfile::tempdir().unwrap();
        for name in ["b.git", "a/z", "a/c.git"] {
            create_repository(root.path(), name, "main").unwrap();
        }
        // Neither a repository inside another one, nor a directory being built, is listed.
        make_repository(&root.path().join("b.git/nested"), "");
        make_repository(&root.path().join(".gitserve-staging/repo"), "");
        fs::create_dir_all(root.path().join("empty/dir")).unwrap();

        assert_eq!(list_repositories(root.path()).unwrap(), ["a/c.git", "a/z", "b.git"]);
    }
}
