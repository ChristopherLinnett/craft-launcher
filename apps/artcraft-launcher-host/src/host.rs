//! What the server knows and does: the installed builds, the latest releases, and the jobs that
//! install, update and remove builds. One job per app at a time; each runs on its own thread and
//! reports its phase here, where `api/state` reads it.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use artcraft_launcher_core::hosted::{AppAction, AppRequest, CheckRequest, CheckState, HostState, HostedApp, HostedJob, JobStarted, JobState};
use artcraft_launcher_core::platform::pick_asset;
use artcraft_launcher_core::protocol::{CANCELLED, CONNECTION_DROPPED, JobPhase, LatestRelease, PAUSED, PausedDownload};
use artcraft_launcher_core::{CraftApp, Platform, SUITE, status};
use artcraft_launcher_native::http::{self, Http, Stop};
use artcraft_launcher_native::install::download_verified;
use artcraft_launcher_native::source::{GitHubSource, ReleaseSource};

use crate::auth::Auth;
use crate::store::{Manifest, Site, Store};

/// A release check isn't repeated sooner than this unless asked to (Refresh)…
const FRESH_FOR: i64 = 15 * 60;
/// …and not sooner than this even then, so a busy Refresh button can't spend GitHub's quota.
const FORCED_AFTER: i64 = 60;
/// Downloads left behind: partial ones are kept this long for a resume, finished ones this long.
const KEEP_PARTIAL: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const KEEP_FINISHED: Duration = Duration::from_secs(24 * 60 * 60);

/// Why the server won't do what was asked: an HTTP status and words for the user.
#[derive(Debug, PartialEq, Eq)]
pub struct Refusal {
    pub status: u16,
    pub message: String,
}

impl Refusal {
    fn new(status: u16, message: impl Into<String>) -> Self {
        Self { status, message: message.into() }
    }
}

struct Job {
    id: u64,
    state: JobState,
    /// While it runs: how to pause or cancel it.
    stop: Option<Arc<Stop>>,
}

/// A download that stopped part-way, kept to be resumed.
struct Paused {
    download: PausedDownload,
    file: PathBuf,
    size: u64,
}

#[derive(Default)]
struct Inner {
    manifest: Manifest,
    /// By app id and track (pre-releases or not).
    latest: BTreeMap<(String, bool), LatestRelease>,
    jobs: BTreeMap<String, Job>,
    paused: BTreeMap<String, Paused>,
    next_job: u64,
    check: CheckState,
    /// When each track was last looked up (Unix seconds).
    checked: BTreeMap<bool, i64>,
}

pub struct Host {
    pub auth: Auth,
    /// The launcher's own web build, served at the root.
    pub launcher: PathBuf,
    store: Store,
    http: Http,
    source: GitHubSource,
    inner: Mutex<Inner>,
}

impl Host {
    pub fn open(data: PathBuf, launcher: PathBuf) -> Result<Arc<Self>, String> {
        if !launcher.join("index.html").is_file() {
            return Err(format!("{} has no index.html: put ArtCraft Launcher's web build there", launcher.display()));
        }
        let auth = Auth::load(&data)?;
        let (store, manifest) = Store::open(data)?;
        let http = Http::new()?;
        http::clean_downloads(&store.downloads(), KEEP_PARTIAL, KEEP_FINISHED);
        let source = GitHubSource::new(http.clone(), store.release_cache());
        let inner = Inner { manifest, next_job: 1, ..Inner::default() };
        let host = Arc::new(Self { auth, launcher, store, http, source, inner: Mutex::new(inner) });
        host.check(CheckRequest::default());
        Ok(host)
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The folder an installed app is served from.
    pub fn site(&self, app: &str) -> Option<PathBuf> {
        self.lock().manifest.apps.get(app).map(|site| self.store.site_root(app, site))
    }

    /// Everything the launcher shows, with the latest releases of one track.
    pub fn state(&self, prereleases: bool) -> HostState {
        let inner = self.lock();
        let apps = SUITE
            .iter()
            .map(|app| HostedApp {
                id: app.id.to_string(),
                installed: inner.manifest.apps.get(app.id).map(|site| site.version.clone()),
                latest: inner.latest.get(&(app.id.to_string(), prereleases)).cloned(),
                paused: inner.paused.get(app.id).map(|p| p.download),
                job: inner.jobs.get(app.id).map(|job| HostedJob { id: job.id, state: job.state.clone() }),
            })
            .collect();
        HostState { apps, check: inner.check.clone() }
    }

    /// Looks up the latest releases in the background, unless the last answers are fresh.
    pub fn check(self: &Arc<Self>, request: CheckRequest) {
        let now = artcraft_launcher_native::now();
        {
            let mut inner = self.lock();
            let fresh_for = if request.force { FORCED_AFTER } else { FRESH_FOR };
            if inner.check.running || inner.checked.get(&request.prereleases).is_some_and(|t| now - t < fresh_for) {
                return;
            }
            inner.check.running = true;
        }
        let host = Arc::clone(self);
        let spawned = std::thread::Builder::new().name("release-check".into()).spawn(move || host.run_check(request.prereleases));
        if let Err(e) = spawned {
            log::warn!("couldn't start the release check ({e})");
            self.lock().check.running = false;
        }
    }

    fn run_check(&self, prereleases: bool) {
        let mut source = None;
        for app in &SUITE {
            let answer = self.source.latest(app.repo, prereleases);
            source = Some(answer.source);
            self.lock().latest.insert((app.id.to_string(), prereleases), answer.latest);
        }
        self.source.save();
        let now = artcraft_launcher_native::now();
        let rate_limited_until = self.source.rate_limited_until();
        let mut inner = self.lock();
        inner.checked.insert(prereleases, now);
        inner.check = CheckState { running: false, finished_at: Some(now), source, rate_limited_until };
    }

    /// Starts an install, update or removal (answering with its job), or pauses or cancels one.
    pub fn act(self: &Arc<Self>, app: &str, action: AppAction, request: AppRequest) -> Result<Option<JobStarted>, Refusal> {
        let app = CraftApp::by_id(app).ok_or_else(|| Refusal::new(404, format!("there's no Craft App called {app}")))?;
        match action {
            AppAction::Install | AppAction::Remove => self.start(app, action, request).map(Some),
            AppAction::Pause => {
                if let Some(stop) = self.lock().jobs.get(app.id).and_then(|job| job.stop.clone()) {
                    stop.pause();
                }
                Ok(None)
            }
            AppAction::Cancel => {
                let mut inner = self.lock();
                match inner.jobs.get(app.id).and_then(|job| job.stop.clone()) {
                    Some(stop) => stop.cancel(),
                    None => {
                        if let Some(paused) = inner.paused.remove(app.id) {
                            http::discard(&paused.file, paused.size);
                        }
                    }
                }
                Ok(None)
            }
        }
    }

    fn start(self: &Arc<Self>, app: &'static CraftApp, action: AppAction, request: AppRequest) -> Result<JobStarted, Refusal> {
        let remove = action == AppAction::Remove;
        let (job, stop) = {
            let mut inner = self.lock();
            if inner.jobs.get(app.id).is_some_and(|job| job.stop.is_some()) {
                return Err(Refusal::new(409, format!("{} is busy. Wait for it to finish.", app.name)));
            }
            if remove && !inner.manifest.apps.contains_key(app.id) {
                return Err(Refusal::new(409, format!("{} isn't installed", app.name)));
            }
            let id = inner.next_job;
            inner.next_job = id.saturating_add(1);
            let stop = Arc::new(Stop::default());
            let phase = if remove { JobPhase::Removing } else { JobPhase::Downloading { done: 0, total: 0 } };
            inner.jobs.insert(app.id.to_string(), Job { id, state: JobState::Running(phase), stop: Some(Arc::clone(&stop)) });
            (id, stop)
        };
        let host = Arc::clone(self);
        let spawned = std::thread::Builder::new().name(format!("job-{}", app.id)).spawn(move || {
            let result = if remove { host.remove(app) } else { host.install(app, request.prereleases, &stop, job) };
            host.finish(app, job, result);
        });
        if let Err(e) = spawned {
            self.finish(app, job, Err(format!("couldn't start ({e})")));
        }
        Ok(JobStarted { job })
    }

    fn set_phase(&self, app: &str, job: u64, phase: JobPhase) {
        if let Some(running) = self.lock().jobs.get_mut(app).filter(|j| j.id == job) {
            running.state = JobState::Running(phase);
        }
    }

    fn finish(&self, app: &CraftApp, job: u64, result: Result<String, String>) {
        match &result {
            Ok(message) => log::info!("{message}"),
            Err(e) if e == PAUSED || e == CANCELLED => log::info!("{}: {e}", app.id),
            Err(e) => log::warn!("{}: {e}", app.id),
        }
        if let Some(done) = self.lock().jobs.get_mut(app.id).filter(|j| j.id == job) {
            done.state = JobState::Finished(result);
            done.stop = None;
        }
    }

    fn install(&self, app: &'static CraftApp, prereleases: bool, stop: &Stop, job: u64) -> Result<String, String> {
        let release = match self.source.latest(app.repo, prereleases).latest {
            LatestRelease::Found(release) => release,
            LatestRelease::None => return Err(format!("{} has no published release yet", app.name)),
            LatestRelease::Failed(why) => return Err(format!("couldn't look up {}'s latest release ({why})", app.name)),
        };
        self.lock().latest.insert((app.id.to_string(), prereleases), LatestRelease::Found(release.clone()));
        let stems: Vec<&str> = app.stems().collect();
        let (asset, _) = pick_asset(&release, &stems, Platform::WEB, Platform::WEB.default_kinds())
            .ok_or_else(|| format!("{} {} has no browser build", app.name, release.version_label()))?;
        let version = release.version.clone().ok_or_else(|| format!("{}'s release {} has no version number", app.name, release.tag))?;
        let file = self.store.downloads().join(&asset.name);
        self.lock().paused.remove(app.id);
        let downloaded = download_verified(&self.http, &release, asset, &file, stop, &mut |phase| self.set_phase(app.id, job, phase));
        match downloaded {
            Ok(()) => {}
            Err(e) if e == PAUSED => {
                if let Some(done) = http::partial_bytes(&file, asset.size) {
                    let download = PausedDownload { done, total: asset.size };
                    self.lock().paused.insert(app.id.to_string(), Paused { download, file, size: asset.size });
                }
                // Paused by someone, or by a connection that kept dropping.
                return Err(if stop.check().is_ok() { CONNECTION_DROPPED.into() } else { e });
            }
            Err(e) => return Err(e),
        }
        self.set_phase(app.id, job, JobPhase::Installing);
        let folder = format!("{version}-{job}");
        let unpacked = self.store.unpack(app.id, &file, &folder);
        let _ = std::fs::remove_file(&file);
        unpacked?;
        let previous = {
            let mut inner = self.lock();
            let previous = inner.manifest.apps.insert(app.id.to_string(), Site { version, folder: folder.clone() });
            self.store.save(&inner.manifest)?;
            previous
        };
        // Keep the build before this one: pages opened a moment ago still load their files from it.
        let keep: Vec<&str> = std::iter::once(folder.as_str()).chain(previous.as_ref().map(|p| p.folder.as_str())).collect();
        self.store.prune(app.id, &keep);
        Ok(status::installed_message(app, previous.is_some(), &release))
    }

    fn remove(&self, app: &CraftApp) -> Result<String, String> {
        {
            let mut inner = self.lock();
            if inner.manifest.apps.remove(app.id).is_none() {
                return Err(format!("{} isn't installed", app.name));
            }
            self.store.save(&inner.manifest)?;
        }
        self.store.remove(app.id)?;
        Ok(status::removed_message(app))
    }
}
