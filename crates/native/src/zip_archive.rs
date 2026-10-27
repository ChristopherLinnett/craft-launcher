//! Bounded, path-checked ZIP extraction, shared by Windows portable packages and the browser
//! builds a hosted launcher serves.

use std::path::{Component, Path, PathBuf};

/// The most entries one archive may hold.
const MAX_FILES: usize = 10_000;
/// The most bytes one archive may unpack to.
const MAX_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// Unpacks `file` into `destination`, refusing paths that leave it, links, names that collide
/// when case is ignored (Windows treats them as one file), more than [`MAX_FILES`] entries and
/// more than [`MAX_BYTES`] in all. Never writes over an existing file. `what` names the archive in
/// errors (`portable ZIP`). Returns the files written and every entry (files and folders), both
/// relative to `destination`.
pub fn extract(file: &Path, destination: &Path, what: &str) -> Result<(Vec<PathBuf>, Vec<PathBuf>), String> {
    let file = std::fs::File::open(file).map_err(|e| e.to_string())?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| format!("couldn't read the {what} ({e})"))?;
    if archive.len() > MAX_FILES {
        return Err(format!("The {what} contains too many files."));
    }
    let mut files = Vec::new();
    let mut entries = Vec::new();
    let mut total = 0u64;
    let mut seen = std::collections::BTreeSet::new();
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).map_err(|e| e.to_string())?;
        let relative = entry.enclosed_name().ok_or_else(|| format!("The {what} contains an unsafe path."))?;
        if relative.components().any(|c| !matches!(c, Component::Normal(_)))
            || entry.name().contains(['\\', ':'])
            || entry.unix_mode().is_some_and(|mode| mode & 0o170000 == 0o120000)
        {
            return Err(format!("The {what} contains an unsafe path or link."));
        }
        // Windows treats case variants as the same file; reject duplicate and colliding names.
        if !seen.insert(relative.to_string_lossy().to_lowercase()) {
            return Err(format!("The {what} contains duplicate paths."));
        }
        let target = destination.join(&relative);
        entries.push(relative.clone());
        if entry.is_dir() {
            std::fs::create_dir_all(&target).map_err(|e| e.to_string())?;
            continue;
        }
        total = total.checked_add(entry.size()).filter(|size| *size <= MAX_BYTES).ok_or_else(|| format!("The {what} is too large."))?;
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let mut output = std::fs::OpenOptions::new().write(true).create_new(true).open(&target).map_err(|e| e.to_string())?;
        use std::io::Read;
        let declared = entry.size();
        let copied = std::io::copy(&mut (&mut entry).take(declared.saturating_add(1)), &mut output).map_err(|e| e.to_string())?;
        if copied != declared {
            return Err(format!("The {what} has an unexpected unpacked size."));
        }
        files.push(relative);
    }
    Ok((files, entries))
}

/// Archives for tests (other crates' too, with the `test-support` feature). Test code: a
/// failure here should stop the test.
#[cfg(any(test, feature = "test-support"))]
#[allow(clippy::unwrap_used)]
pub mod test_zip {
    use std::io::Write;
    use std::path::{Path, PathBuf};

    /// Writes `root/name` holding `files`; names ending in `/` are folders.
    pub fn archive(root: &Path, name: &str, files: &[(&str, &[u8])]) -> PathBuf {
        let path = root.join(name);
        let mut zip = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
        for (name, content) in files {
            if name.ends_with('/') {
                zip.add_directory(*name, zip::write::SimpleFileOptions::default()).unwrap();
            } else {
                zip.start_file(*name, zip::write::SimpleFileOptions::default()).unwrap();
                zip.write_all(content).unwrap();
            }
        }
        zip.finish().unwrap();
        path
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_zip::archive;

    #[test]
    fn unpacks_a_browser_build() {
        let dir = tempfile::tempdir().unwrap();
        let zip = archive(
            dir.path(),
            "photocraft-web-0.16.0.zip",
            &[("photocraft-web-0.16.0/", b""), ("photocraft-web-0.16.0/index.html", b"<html>"), ("photocraft-web-0.16.0/app/parts.txt", b"1")],
        );
        let out = dir.path().join("out");
        std::fs::create_dir(&out).unwrap();
        let (files, entries) = extract(&zip, &out, "web build").unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(entries.len(), 3);
        assert_eq!(std::fs::read(out.join("photocraft-web-0.16.0/app/parts.txt")).unwrap(), b"1");
    }

    #[test]
    fn refuses_escapes_collisions_and_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        for (name, files) in [
            ("traversal.zip", vec![("../escape.html", b"bad".as_slice())]),
            ("colon.zip", vec![("data:stream", b"bad".as_slice())]),
            ("duplicate.zip", vec![("Index.html", b"one".as_slice()), ("index.html", b"two".as_slice())]),
        ] {
            let zip = archive(dir.path(), name, &files);
            let out = dir.path().join(name.replace('.', "-"));
            std::fs::create_dir(&out).unwrap();
            let error = extract(&zip, &out, "web build").unwrap_err();
            assert!(error.contains("web build"), "{name}: {error}");
        }
        assert!(!dir.path().join("escape.html").exists());
        // A root-anchored name stays inside the destination.
        let zip = archive(dir.path(), "absolute.zip", &[("/inside.html", b"ok")]);
        let out = dir.path().join("absolute");
        std::fs::create_dir(&out).unwrap();
        extract(&zip, &out, "web build").unwrap();
        assert!(out.join("inside.html").is_file());
        let zip = archive(dir.path(), "plain.zip", &[("index.html", b"new")]);
        let out = dir.path().join("taken");
        std::fs::create_dir(&out).unwrap();
        std::fs::write(out.join("index.html"), b"old").unwrap();
        assert!(extract(&zip, &out, "web build").is_err());
        assert_eq!(std::fs::read(out.join("index.html")).unwrap(), b"old");
    }
}
