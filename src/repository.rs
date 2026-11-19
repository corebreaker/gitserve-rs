//! Repositories: bare Git repositories, served under a slash-separated name.

use std::fs;
use std::path::{Path, PathBuf};

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
}
