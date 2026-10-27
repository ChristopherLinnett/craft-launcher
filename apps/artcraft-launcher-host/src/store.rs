//! The server's files, under its data folder:
//!
//! ```text
//! config.json            the team password's hash and the cookie key (auth.rs)
//! installed.json         which build of each app is served ([`Manifest`])
//! releases.json          saved release lookups
//! downloads/             browser builds on their way (and paused ones)
//! sites/<app>/<folder>/  an unpacked browser build; the current one and the one before it
//! ```

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use artcraft_launcher_core::version::Version;
use artcraft_launcher_native::paths::write_atomic;
use artcraft_launcher_native::zip_archive;
use flate2::Compression;
use flate2::write::GzEncoder;
use serde::{Deserialize, Serialize};

/// Folders being unpacked start with this, so a crash's leftovers are recognised and removed.
const STAGING: &str = ".staging-";
/// Files worth sending compressed; the rest are pictures, fonts and already-compressed parts.
const COMPRESSIBLE: [&str; 8] = ["html", "js", "mjs", "css", "wasm", "json", "svg", "txt"];
/// Smaller files aren't worth compressing.
const MIN_COMPRESS_BYTES: usize = 1024;

/// Which build of each app is served.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Manifest {
    pub apps: BTreeMap<String, Site>,
}

/// One installed browser build.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Site {
    pub version: Version,
    /// Its folder in `sites/<app>/`.
    pub folder: String,
}

pub struct Store {
    data: PathBuf,
}

impl Store {
    /// Opens the data folder, keeping only the record of builds whose folders are still there,
    /// and clearing what an interrupted unpack left behind.
    pub fn open(data: PathBuf) -> Result<(Self, Manifest), String> {
        let store = Self { data };
        for dir in [store.sites(), store.downloads()] {
            std::fs::create_dir_all(&dir).map_err(|e| format!("couldn't create {} ({e})", dir.display()))?;
        }
        let mut manifest = match std::fs::read_to_string(store.manifest_path()) {
            Ok(text) => serde_json::from_str(&text).map_err(|e| format!("{} is unreadable ({e})", store.manifest_path().display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Manifest::default(),
            Err(e) => return Err(format!("couldn't read {} ({e})", store.manifest_path().display())),
        };
        manifest.apps.retain(|app, site| {
            let there = store.site_root(app, site).join("index.html").is_file();
            if !there {
                log::warn!("{app}: its build {} is gone; it counts as not installed", site.folder);
            }
            there
        });
        for (_, app_dir) in folders(&store.sites()) {
            for (name, path) in folders(&app_dir) {
                if name.starts_with(STAGING) {
                    let _ = std::fs::remove_dir_all(path);
                }
            }
        }
        Ok((store, manifest))
    }

    fn manifest_path(&self) -> PathBuf {
        self.data.join("installed.json")
    }

    fn sites(&self) -> PathBuf {
        self.data.join("sites")
    }

    pub fn downloads(&self) -> PathBuf {
        self.data.join("downloads")
    }

    pub fn release_cache(&self) -> PathBuf {
        self.data.join("releases.json")
    }

    pub fn save(&self, manifest: &Manifest) -> Result<(), String> {
        let json = serde_json::to_vec_pretty(manifest).map_err(|e| e.to_string())?;
        write_atomic(&self.manifest_path(), &json)
    }

    /// The folder `site` of `app` is served from.
    pub fn site_root(&self, app: &str, site: &Site) -> PathBuf {
        self.sites().join(app).join(&site.folder)
    }

    /// Unpacks the browser build `zip` of `app` into `sites/<app>/<folder>`, with a compressed
    /// copy of each file worth one. The build is the folder holding `index.html`: the zip's root,
    /// or its one top folder.
    pub fn unpack(&self, app: &str, zip: &Path, folder: &str) -> Result<(), String> {
        let app_dir = self.sites().join(app);
        let staging = app_dir.join(format!("{STAGING}{folder}"));
        let _ = std::fs::remove_dir_all(&staging);
        std::fs::create_dir_all(&staging).map_err(|e| format!("couldn't create {} ({e})", staging.display()))?;
        let result = (|| {
            zip_archive::extract(zip, &staging, "web build")?;
            let root = build_root(&staging)?;
            precompress(&root)?;
            let dest = app_dir.join(folder);
            let _ = std::fs::remove_dir_all(&dest);
            std::fs::rename(&root, &dest).map_err(|e| format!("couldn't put the build in place ({e})"))
        })();
        let _ = std::fs::remove_dir_all(&staging);
        result
    }

    /// Removes `app`'s builds other than those in `keep`.
    pub fn prune(&self, app: &str, keep: &[&str]) {
        for (name, path) in folders(&self.sites().join(app)) {
            if !keep.contains(&name.as_str())
                && let Err(e) = std::fs::remove_dir_all(&path)
            {
                log::warn!("{app}: couldn't remove the old build {name} ({e})");
            }
        }
    }

    /// Removes every build of `app`.
    pub fn remove(&self, app: &str) -> Result<(), String> {
        match std::fs::remove_dir_all(self.sites().join(app)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(format!("couldn't remove its files ({e})")),
            _ => Ok(()),
        }
    }
}

/// The folders directly in `dir` (not links), by name.
fn folders(dir: &Path) -> Vec<(String, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| Some((e.file_name().into_string().ok()?, e.path())))
        .collect()
}

/// The folder holding the build's `index.html`: `staging` itself, or its one folder.
fn build_root(staging: &Path) -> Result<PathBuf, String> {
    if staging.join("index.html").is_file() {
        return Ok(staging.to_path_buf());
    }
    let entries: Vec<PathBuf> = std::fs::read_dir(staging).map_err(|e| e.to_string())?.flatten().map(|e| e.path()).collect();
    match entries.as_slice() {
        [only] if only.join("index.html").is_file() => Ok(only.clone()),
        _ => Err("the web build has no index.html".into()),
    }
}

/// `file.gz`: the compressed copy served to browsers that accept gzip.
pub fn gz_path(file: &Path) -> PathBuf {
    let mut name = file.as_os_str().to_owned();
    name.push(".gz");
    PathBuf::from(name)
}

pub fn compressible(file: &Path) -> bool {
    file.extension().and_then(|e| e.to_str()).is_some_and(|e| COMPRESSIBLE.contains(&e.to_ascii_lowercase().as_str()))
}

/// Writes a `.gz` beside each compressible file under `root`, where it saves at least a tenth.
fn precompress(root: &Path) -> Result<(), String> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).map_err(|e| e.to_string())?.flatten() {
            let Ok(kind) = entry.file_type() else { continue };
            let path = entry.path();
            if kind.is_dir() {
                stack.push(path);
            } else if kind.is_file() && compressible(&path) {
                gzip_beside(&path)?;
            }
        }
    }
    Ok(())
}

fn gzip_beside(file: &Path) -> Result<(), String> {
    let data = std::fs::read(file).map_err(|e| format!("couldn't read {} ({e})", file.display()))?;
    if data.len() < MIN_COMPRESS_BYTES {
        return Ok(());
    }
    let mut encoder = GzEncoder::new(Vec::with_capacity(data.len() / 3), Compression::best());
    encoder.write_all(&data).map_err(|e| e.to_string())?;
    let gz = encoder.finish().map_err(|e| e.to_string())?;
    if gz.len().saturating_mul(10) <= data.len().saturating_mul(9) {
        std::fs::write(gz_path(file), gz).map_err(|e| format!("couldn't write {} ({e})", gz_path(file).display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use artcraft_launcher_native::zip_archive::test_zip::archive;

    fn store() -> (tempfile::TempDir, Store, Manifest) {
        let dir = tempfile::tempdir().unwrap();
        let (store, manifest) = Store::open(dir.path().join("data")).unwrap();
        (dir, store, manifest)
    }

    fn site(folder: &str) -> Site {
        Site { version: artcraft_launcher_core::version::parse("0.13.0").unwrap(), folder: folder.into() }
    }

    #[test]
    fn unpacks_a_build_from_its_top_folder_with_compressed_copies() {
        let (dir, store, _) = store();
        let big = "let x = 1;\n".repeat(500);
        let zip = archive(
            dir.path(),
            "filmcraft-web-0.13.0.zip",
            &[("filmcraft-web-0.13.0/index.html", b"<html>"), ("filmcraft-web-0.13.0/app.js", big.as_bytes())],
        );
        store.unpack("filmcraft", &zip, "0.13.0-1").unwrap();
        let root = store.site_root("filmcraft", &site("0.13.0-1"));
        assert!(root.join("index.html").is_file());
        // Small files stay as they are; big ones get a smaller copy.
        assert!(!gz_path(&root.join("index.html")).exists());
        assert!(std::fs::metadata(gz_path(&root.join("app.js"))).unwrap().len() < big.len() as u64 / 10);
        // Nothing of the unpacking is left.
        assert_eq!(folders(&dir.path().join("data/sites/filmcraft")).len(), 1);
    }

    #[test]
    fn a_zip_without_a_page_is_refused_and_leaves_nothing() {
        let (dir, store, _) = store();
        let zip = archive(dir.path(), "bad.zip", &[("a/readme.txt", b"no page"), ("b/index.html", b"<html>")]);
        assert!(store.unpack("filmcraft", &zip, "0.13.0-1").unwrap_err().contains("index.html"));
        assert!(folders(&dir.path().join("data/sites/filmcraft")).is_empty());
    }

    #[test]
    fn the_record_keeps_only_builds_that_are_there() {
        let (dir, store, mut manifest) = store();
        let zip = archive(dir.path(), "ok.zip", &[("index.html", b"<html>")]);
        store.unpack("photocraft", &zip, "1").unwrap();
        manifest.apps.insert("photocraft".into(), site("1"));
        manifest.apps.insert("filmcraft".into(), site("missing"));
        store.save(&manifest).unwrap();
        std::fs::create_dir_all(dir.path().join("data/sites/photocraft/.staging-2")).unwrap();
        let (_, reopened) = Store::open(dir.path().join("data")).unwrap();
        assert_eq!(reopened.apps.keys().collect::<Vec<_>>(), ["photocraft"]);
        assert!(!dir.path().join("data/sites/photocraft/.staging-2").exists());
        // Pruning keeps what it's told to.
        store.unpack("photocraft", &zip, "2").unwrap();
        store.prune("photocraft", &["2"]);
        assert_eq!(folders(&dir.path().join("data/sites/photocraft")).into_iter().map(|(n, _)| n).collect::<Vec<_>>(), ["2"]);
    }
}
