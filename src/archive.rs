//! Zip archives: the file format of an archive store, converted from and to a directory holding its image.

use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::Context;
use serde::{Deserialize, Serialize};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, DateTime, ZipArchive, ZipWriter};

use crate::repository::{self, STAGING_PREFIX};

/// The file, at the root of an archive, that records the version of the archive format.
pub const MARKER: &str = ".gitserve";

/// The version of the archive format that gitserve reads and writes.
pub const VERSION: u32 = 1;

/// The content of the marker file.
#[derive(Debug, Serialize, Deserialize)]
struct Marker {
    version: u32,
}

/// Identifies a state of a file: it changes whenever the file is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stamp {
    size: u64,
    modified: Option<SystemTime>,
}

impl Stamp {
    fn of(meta: &fs::Metadata) -> Self {
        Self {
            size: meta.len(),
            modified: meta.modified().ok(),
        }
    }
}

/// Returns the stamp of the file at `path`.
pub fn stamp(path: &Path) -> io::Result<Stamp> {
    fs::metadata(path).map(|meta| Stamp::of(&meta))
}

/// Writes the marker of the current archive format at the root of `image`.
fn write_marker(image: &Path) -> anyhow::Result<()> {
    let mut data = serde_json::to_vec(&Marker { version: VERSION })?;
    data.push(b'\n');
    fs::write(image.join(MARKER), data)?;
    Ok(())
}

/// Checks that the archive format recorded in `image` is the one gitserve knows.
///
/// An archive without marker, such as a zip made by hand from a directory store, is in the current format:
/// the marker is added to its image, and reaches the file with the next write.
fn check_marker(image: &Path) -> anyhow::Result<()> {
    let data = match fs::read(image.join(MARKER)) {
        Ok(data) => data,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return write_marker(image),
        Err(err) => return Err(err.into()),
    };

    let marker: Marker = serde_json::from_slice(&data).with_context(|| format!("invalid {MARKER} marker"))?;
    if marker.version != VERSION {
        anyhow::bail!(
            "unsupported archive format version {} (gitserve handles version {VERSION})",
            marker.version
        );
    }

    Ok(())
}

/// Extracts the archive at `path` into the empty directory `image`, and returns the stamp of the file it read.
pub fn read_archive(path: &Path, image: &Path) -> anyhow::Result<Stamp> {
    let read = || -> anyhow::Result<Stamp> {
        let file = File::open(path)?;
        let stamp = Stamp::of(&file.metadata()?);
        let mut archive = ZipArchive::new(BufReader::new(file))?;
        for index in 0..archive.len() {
            let mut entry = archive.by_index(index)?;
            let raw = entry.name()?.to_string();
            extract_entry(&mut entry, &raw, image).with_context(|| format!("entry {raw:?}"))?;
        }

        check_marker(image)?;
        Ok(stamp)
    };

    read().with_context(|| format!("cannot read the archive {}", path.display()))
}

/// Copies an entry of a zip archive, named `raw` in it, into `image`.
fn extract_entry(entry: &mut zip::read::ZipFile<'_, impl io::Read>, raw: &str, image: &Path) -> anyhow::Result<()> {
    // Some zip tools write Windows path separators.
    let name = raw.replace('\\', "/");
    let is_dir = entry.is_dir() || name.ends_with('/');
    let name = name.trim_end_matches('/');
    repository::check_name(name).map_err(|_| anyhow::anyhow!("invalid entry name"))?;

    let target = image.join(name);
    if is_dir {
        fs::create_dir_all(&target)?;
        return Ok(());
    }

    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = BufWriter::new(File::create(&target)?);
    io::copy(entry, &mut file)?;
    file.flush()?;
    Ok(())
}

/// Writes the directory `image` to the archive at `path`, and returns the stamp of the written file.
///
/// The archive is first written to a temporary file next to it, that then replaces it: an interrupted write leaves
/// the previous archive intact. The archive keeps the permissions of the file it replaces.
pub fn write_archive(path: &Path, image: &Path) -> anyhow::Result<Stamp> {
    let write = || -> anyhow::Result<Stamp> {
        let parent = path.parent().filter(|parent| !parent.as_os_str().is_empty());
        let parent = parent.unwrap_or(Path::new("."));
        let base = path.file_name().unwrap_or_default().to_string_lossy();
        let temporary = tempfile::Builder::new()
            .prefix(&format!(".{base}."))
            .suffix(".tmp")
            .tempfile_in(parent)?;

        let mut writer = ZipWriter::new(BufWriter::new(temporary));
        zip_directory(&mut writer, image, image)?;
        let temporary = writer.finish()?.into_inner().map_err(io::IntoInnerError::into_error)?;
        temporary.as_file().sync_all()?;
        if let Ok(meta) = fs::metadata(path) {
            fs::set_permissions(temporary.path(), meta.permissions())?;
        }

        temporary.persist(path).map_err(|err| err.error)?;
        Ok(stamp(path)?)
    };

    write().with_context(|| format!("cannot write the archive {}", path.display()))
}

/// Creates an empty archive at `path`, holding nothing but the marker, along with its missing parent directories.
pub fn create_archive(path: &Path) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("cannot create {}", parent.display()))?;
    }

    let image = tempfile::tempdir()?;
    write_marker(image.path())?;
    write_archive(path, image.path())?;
    Ok(())
}

/// Writes every directory and file under `dir` to `writer`, named after their path relative to `image`, in
/// lexical order. Git objects are stored as they are, since Git already compresses them; the other files are
/// deflated. The directories in which repositories are built or torn down are left out.
fn zip_directory(writer: &mut ZipWriter<impl io::Write + io::Seek>, image: &Path, dir: &Path) -> anyhow::Result<()> {
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<io::Result<_>>()?;
    entries.sort();

    for path in entries {
        let file_name = path.file_name().unwrap_or_default().to_string_lossy();
        if file_name.starts_with(STAGING_PREFIX) {
            continue;
        }

        let meta = fs::symlink_metadata(&path)?;
        let name = entry_name(image, &path);
        let options = SimpleFileOptions::default().last_modified_time(zip_time(meta.modified().ok()));
        if meta.is_dir() {
            writer.add_directory(format!("{name}/"), options.unix_permissions(0o755))?;
            zip_directory(writer, image, &path)?;
        } else if meta.is_file() {
            let method = if format!("/{name}").contains("/objects/") {
                CompressionMethod::Stored
            } else {
                CompressionMethod::Deflated
            };
            writer.start_file(name, options.unix_permissions(0o644).compression_method(method))?;
            io::copy(&mut File::open(&path)?, writer)?;
        }
    }

    Ok(())
}

/// Returns the name in an archive of the file at `path` in `image`: its relative path, with slashes.
fn entry_name(image: &Path, path: &Path) -> String {
    let relative = path.strip_prefix(image).unwrap_or(path);
    let elements: Vec<_> = relative.iter().map(|element| element.to_string_lossy()).collect();
    elements.join("/")
}

/// Converts a modification time to the time of a zip entry, which has no time zone: the time in UTC is used.
/// A time that a zip entry cannot hold, before 1980 for instance, gives the default time of zip entries.
fn zip_time(modified: Option<SystemTime>) -> DateTime {
    modified
        .map(time::OffsetDateTime::from)
        .and_then(|time| DateTime::try_from(time::PrimitiveDateTime::new(time.date(), time.time())).ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes an archive at `path` holding the entries `(name, content)`, a name ending with a slash being a
    /// directory.
    fn make_zip(path: &Path, entries: &[(&str, &str)]) {
        let mut writer = ZipWriter::new(File::create(path).unwrap());
        for (name, content) in entries {
            if name.ends_with('/') {
                writer.add_directory(*name, SimpleFileOptions::default()).unwrap();
            } else {
                writer.start_file(*name, SimpleFileOptions::default()).unwrap();
                writer.write_all(content.as_bytes()).unwrap();
            }
        }
        writer.finish().unwrap();
    }

    fn read_marker(image: &Path) -> String {
        fs::read_to_string(image.join(MARKER)).unwrap()
    }

    #[test]
    fn create_archive_writes_the_marker_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a/b/repos.zip");
        create_archive(&path).unwrap();

        let mut archive = ZipArchive::new(File::open(&path).unwrap()).unwrap();
        assert_eq!(archive.len(), 1);
        let mut marker = String::new();
        io::Read::read_to_string(&mut archive.by_name(MARKER).unwrap(), &mut marker).unwrap();
        assert_eq!(marker, "{\"version\":1}\n");
    }

    #[test]
    fn archives_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        fs::create_dir_all(source.join("team/repo.git/objects/pack")).unwrap();
        fs::create_dir_all(source.join("team/repo.git/refs/heads")).unwrap();
        fs::write(source.join("team/repo.git/HEAD"), "ref: refs/heads/main\n").unwrap();
        fs::write(source.join("team/repo.git/objects/pack/pack-1.pack"), [0u8, 1, 2, 255]).unwrap();
        write_marker(&source).unwrap();
        // A directory in which a repository is being built is left out.
        fs::create_dir_all(source.join("team/.gitserve-x/repo")).unwrap();

        let path = dir.path().join("repos.zip");
        let written = write_archive(&path, &source).unwrap();
        assert_eq!(written, stamp(&path).unwrap());

        let image = dir.path().join("image");
        fs::create_dir(&image).unwrap();
        assert_eq!(read_archive(&path, &image).unwrap(), written);
        assert_eq!(
            fs::read_to_string(image.join("team/repo.git/HEAD")).unwrap(),
            "ref: refs/heads/main\n"
        );
        assert_eq!(
            fs::read(image.join("team/repo.git/objects/pack/pack-1.pack")).unwrap(),
            [0, 1, 2, 255]
        );
        assert!(image.join("team/repo.git/refs/heads").is_dir());
        assert!(!image.join("team/.gitserve-x").exists());
    }

    #[test]
    fn objects_are_stored_and_other_files_deflated() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        fs::create_dir_all(source.join("repo/objects/ab")).unwrap();
        fs::write(source.join("repo/objects/ab/cdef"), "object").unwrap();
        fs::write(source.join("repo/config"), "[core]\n").unwrap();
        write_marker(&source).unwrap();

        let path = dir.path().join("repos.zip");
        write_archive(&path, &source).unwrap();
        let mut archive = ZipArchive::new(File::open(&path).unwrap()).unwrap();
        let method = |archive: &mut ZipArchive<File>, name: &str| archive.by_name(name).unwrap().compression();
        assert_eq!(method(&mut archive, "repo/objects/ab/cdef"), CompressionMethod::Stored);
        assert_eq!(method(&mut archive, "repo/config"), CompressionMethod::Deflated);
        assert_eq!(method(&mut archive, MARKER), CompressionMethod::Deflated);
        assert!(archive.by_name("repo/objects/").unwrap().is_dir());
    }

    #[test]
    fn an_archive_without_marker_gets_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("repos.zip");
        make_zip(&path, &[("repo\\HEAD", "ref: refs/heads/main\n"), ("repo\\refs\\", "")]);

        let image = tempfile::tempdir().unwrap();
        read_archive(&path, image.path()).unwrap();
        assert_eq!(read_marker(image.path()), "{\"version\":1}\n");
        // Windows path separators are read as slashes.
        assert!(image.path().join("repo/HEAD").is_file());
        assert!(image.path().join("repo/refs").is_dir());
    }

    #[test]
    fn other_format_versions_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        for (marker, expected) in [
            ("{\"version\":2}", "unsupported archive format version 2"),
            ("not json", "invalid .gitserve marker"),
        ] {
            let path = dir.path().join("repos.zip");
            make_zip(&path, &[(MARKER, marker)]);
            let image = tempfile::tempdir().unwrap();
            let error = format!("{:#}", read_archive(&path, image.path()).unwrap_err());
            assert!(error.contains(expected), "{error}");
        }
    }

    #[test]
    fn entries_escaping_the_image_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["../escape", "/absolute", "a/../../escape", "c:/drive", "a//b"] {
            let path = dir.path().join("repos.zip");
            make_zip(&path, &[(name, "content")]);
            let image = dir.path().join("image");
            fs::create_dir_all(&image).unwrap();
            let error = format!("{:#}", read_archive(&path, &image).unwrap_err());
            assert!(error.contains("invalid entry name"), "{name}: {error}");
            assert!(!dir.path().join("escape").exists());
            fs::remove_dir_all(&image).unwrap();
        }
    }

    #[test]
    fn a_failed_write_leaves_the_archive_intact() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("repos.zip");
        create_archive(&path).unwrap();
        let before = fs::read(&path).unwrap();

        assert!(write_archive(&path, &dir.path().join("missing")).is_err());
        assert_eq!(fs::read(&path).unwrap(), before);
        // No temporary file is left next to the archive.
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
