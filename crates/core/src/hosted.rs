//! What a hosted launcher's server and ArtCraft Launcher in the browser say to each other.
//!
//! A hosted launcher serves the Craft Apps' browser builds from one address (`/photocraft/`,
//! `/filmcraft/`…), with ArtCraft Launcher at its root. The server installs, updates and removes
//! those builds; the launcher in the browser reads [`HostState`] from [`STATE_PATH`] and asks for
//! changes by `POST`ing to [`CHECK_PATH`] and [`app_action_path`]. Paths are relative to the
//! launcher's page, so a host can serve everything under a prefix.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::protocol::{Event, JobPhase, LatestRelease, Origin, PausedDownload, Source};
use crate::status::{InstallSource, Installed};
use crate::version::Version;

/// `GET`: the [`HostState`]. `?prereleases=1` reports the pre-release track's latest releases.
pub const STATE_PATH: &str = "api/state";
/// `POST` a [`CheckRequest`]: look up the latest releases.
pub const CHECK_PATH: &str = "api/check";
/// Every `POST` carries this header (any value). A cross-site form can't set it, and a cross-site
/// script that tries needs a CORS preflight the server never grants.
pub const REQUEST_HEADER: &str = "x-artcraft-launcher";
/// First path segments the server keeps for itself, so no Craft App may use them as its id.
pub const RESERVED: [&str; 3] = ["api", "login", "logout"];

/// Everything the server knows, as [`STATE_PATH`] answers it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HostState {
    /// Every Craft App, in catalog order.
    pub apps: Vec<HostedApp>,
    pub check: CheckState,
}

/// The server's release lookup.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CheckState {
    /// A lookup is running now.
    pub running: bool,
    /// When the last lookup finished (Unix seconds).
    pub finished_at: Option<i64>,
    /// Where the last lookup's answers came from.
    pub source: Option<Source>,
    /// GitHub refused more API calls until this time (Unix seconds).
    pub rate_limited_until: Option<i64>,
}

/// One Craft App on the server.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HostedApp {
    /// [`crate::CraftApp::id`].
    pub id: String,
    /// The version installed on the server; `None` when it isn't installed.
    pub installed: Option<Version>,
    /// The latest release, once looked up.
    pub latest: Option<LatestRelease>,
    /// A download that stopped part-way and can be resumed with [`AppAction::Install`].
    pub paused: Option<PausedDownload>,
    /// The app's running job, or the last one to finish.
    pub job: Option<HostedJob>,
}

/// A job on the server: an install, update or removal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostedJob {
    /// Increases with every job the server starts, so a launcher can tell a job it watched (or
    /// started) from one that finished before it looked.
    pub id: u64,
    pub state: JobState,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum JobState {
    Running(JobPhase),
    /// `Ok`: what it did; `Err`: what went wrong (English, like the desktop backend's messages).
    Finished(Result<String, String>),
}

/// What a `POST` to [`CHECK_PATH`] carries.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CheckRequest {
    /// Look again even when the last answers are fresh (the user pressed Refresh).
    pub force: bool,
    pub prereleases: bool,
}

/// Something the launcher asks the server to do to one app.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AppAction {
    /// Install, update, or resume a paused download.
    Install,
    Remove,
    /// Stop a download, keeping what has arrived.
    Pause,
    /// Stop a download, or throw away a paused one.
    Cancel,
}

impl AppAction {
    pub const ALL: [AppAction; 4] = [Self::Install, Self::Remove, Self::Pause, Self::Cancel];

    pub fn name(self) -> &'static str {
        match self {
            Self::Install => "install",
            Self::Remove => "remove",
            Self::Pause => "pause",
            Self::Cancel => "cancel",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|a| a.name() == name)
    }
}

/// `api/apps/<app>/<action>`: where to `POST` an [`AppRequest`].
pub fn app_action_path(app: &str, action: AppAction) -> String {
    format!("api/apps/{app}/{}", action.name())
}

/// Reads an [`app_action_path`] back into its app id and action.
pub fn parse_app_action_path(path: &str) -> Option<(&str, AppAction)> {
    let (app, action) = path.strip_prefix("api/apps/")?.split_once('/')?;
    Some((app, AppAction::from_name(action)?))
}

/// What a `POST` to an [`app_action_path`] carries.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppRequest {
    /// Install from the pre-release track (Settings → pre-releases).
    pub prereleases: bool,
}

/// The answer to an [`AppAction::Install`] or [`AppAction::Remove`]: the job it started.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobStarted {
    pub job: u64,
}

/// Turns the server's successive [`HostState`]s into the [`Event`]s a backend reports: only what
/// changed, plus how each job this launcher watched ended.
#[derive(Debug, Default)]
pub struct Watcher {
    last: Option<HostState>,
    /// The job of each app this launcher saw running or started.
    watched: BTreeMap<String, u64>,
    /// A check reported as started and not yet as finished.
    check_open: bool,
    /// The finish time of the last check reported.
    reported_check: Option<i64>,
}

/// What a watched job ends with when the server no longer knows it.
pub const LOST_JOB: &str = "The server restarted before it finished. Try again.";

impl Watcher {
    /// Reports how `job` of `app` ends, even when it ends before the next state arrives.
    pub fn watch(&mut self, app: &str, job: u64) {
        self.watched.insert(app.to_string(), job);
    }

    /// Something is running on the server (a job or the release check), so ask again soon.
    pub fn busy(&self) -> bool {
        self.last
            .as_ref()
            .is_some_and(|s| s.check.running || s.apps.iter().any(|a| matches!(&a.job, Some(j) if matches!(j.state, JobState::Running(_)))))
    }

    /// The events since the last state. `address` gives an installed app's address, which the UI
    /// shows as where it lives.
    pub fn update(&mut self, state: HostState, address: &dyn Fn(&str) -> String) -> Vec<Event> {
        let mut events = Vec::new();
        let last = self.last.take();
        for app in &state.apps {
            let before = last.as_ref().and_then(|l| l.apps.iter().find(|a| a.id == app.id));
            self.app_events(app, before, address, &mut events);
        }
        let check = &state.check;
        if check.running && !self.check_open {
            self.check_open = true;
            events.push(Event::CheckStarted);
        } else if !check.running
            && let Some(at) = check.finished_at
            && (self.check_open || self.reported_check != Some(at))
        {
            self.check_open = false;
            self.reported_check = Some(at);
            events.push(Event::CheckFinished { at, source: check.source.unwrap_or(Source::GitHubApi), rate_limited_until: check.rate_limited_until });
        }
        self.last = Some(state);
        events
    }

    fn app_events(&mut self, app: &HostedApp, before: Option<&HostedApp>, address: &dyn Fn(&str) -> String, events: &mut Vec<Event>) {
        let id = || app.id.clone();
        if let Some(latest) = &app.latest
            && before.is_none_or(|b| b.latest.as_ref() != Some(latest))
        {
            events.push(Event::Latest { app: id(), latest: latest.clone(), from: Origin::GitHub });
        }
        if before.is_none_or(|b| b.installed != app.installed) {
            let installed = app.installed.clone().map(|v| Installed {
                version: Some(v),
                source: InstallSource::Launcher,
                location: address(&app.id),
                package: None,
            });
            events.push(Event::Detected { app: id(), installed });
        }
        if before.is_none_or(|b| b.paused != app.paused) {
            events.push(Event::Paused { app: id(), download: app.paused });
        }
        let watched = self.watched.get(&app.id).copied();
        match &app.job {
            Some(HostedJob { id: job, state: JobState::Running(phase) }) => {
                self.watched.insert(id(), *job);
                if before.and_then(|b| b.job.as_ref()) != app.job.as_ref() {
                    events.push(Event::Progress { app: id(), phase: *phase });
                }
            }
            Some(HostedJob { id: job, state: JobState::Finished(result) }) if watched.is_some_and(|w| *job >= w) => {
                self.watched.remove(&app.id);
                events.push(Event::JobFinished { app: id(), result: result.clone() });
            }
            // The server restarted, and lost the job with its memory.
            None if watched.is_some() => {
                self.watched.remove(&app.id);
                events.push(Event::JobFinished { app: id(), result: Err(LOST_JOB.into()) });
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SUITE;

    #[test]
    fn action_paths_read_back() {
        for action in AppAction::ALL {
            let path = app_action_path("photocraft", action);
            assert_eq!(parse_app_action_path(&path), Some(("photocraft", action)));
        }
        for bad in ["api/apps/photocraft", "api/apps/photocraft/launch", "api/state", "apps/photocraft/install"] {
            assert_eq!(parse_app_action_path(bad), None, "{bad}");
        }
    }

    #[test]
    fn no_app_takes_a_reserved_path() {
        for app in &SUITE {
            assert!(!RESERVED.contains(&app.id), "{}", app.id);
        }
        assert!(!RESERVED.contains(&crate::catalog::LAUNCHER_ID));
    }

    #[test]
    fn state_survives_json() {
        let state = HostState {
            apps: vec![HostedApp {
                id: "filmcraft".into(),
                installed: crate::version::parse("0.13.0"),
                latest: Some(LatestRelease::None),
                paused: Some(PausedDownload { done: 5, total: 10 }),
                job: Some(HostedJob { id: 3, state: JobState::Running(JobPhase::Downloading { done: 5, total: 10 }) }),
            }],
            check: CheckState { running: true, finished_at: Some(1), source: Some(Source::GitHubApi), rate_limited_until: None },
        };
        let json = serde_json::to_string(&state).unwrap();
        assert_eq!(serde_json::from_str::<HostState>(&json).unwrap(), state);
        // A newer server's extra fields don't stop an older launcher.
        assert_eq!(serde_json::from_str::<HostState>(r#"{"apps":[],"later":1}"#).unwrap(), HostState::default());
    }

    fn app(id: &str, installed: Option<&str>, job: Option<(u64, JobState)>) -> HostedApp {
        HostedApp {
            id: id.into(),
            installed: installed.and_then(crate::version::parse),
            latest: None,
            paused: None,
            job: job.map(|(id, state)| HostedJob { id, state }),
        }
    }

    fn state(apps: Vec<HostedApp>, running: bool, finished_at: Option<i64>) -> HostState {
        HostState { apps, check: CheckState { running, finished_at, source: Some(Source::GitHubApi), rate_limited_until: None } }
    }

    fn address(id: &str) -> String {
        format!("https://host/{id}/")
    }

    #[test]
    fn the_first_state_says_what_is_installed_and_when_releases_were_checked() {
        let mut w = Watcher::default();
        let events = w.update(state(vec![app("filmcraft", Some("0.13.0"), None), app("photocraft", None, None)], false, Some(7)), &address);
        let installed =
            Installed { version: crate::version::parse("0.13.0"), source: InstallSource::Launcher, location: address("filmcraft"), package: None };
        assert!(events.contains(&Event::Detected { app: "filmcraft".into(), installed: Some(installed) }));
        assert!(events.contains(&Event::Detected { app: "photocraft".into(), installed: None }));
        assert!(events.contains(&Event::CheckFinished { at: 7, source: Source::GitHubApi, rate_limited_until: None }));
        // The same state again changes nothing.
        let again = w.update(state(vec![app("filmcraft", Some("0.13.0"), None), app("photocraft", None, None)], false, Some(7)), &address);
        assert_eq!(again, Vec::new());
    }

    #[test]
    fn a_watched_job_reports_progress_and_how_it_ended() {
        let mut w = Watcher::default();
        w.update(state(vec![app("filmcraft", None, None)], false, None), &address);
        w.watch("filmcraft", 4);
        let downloading = JobState::Running(JobPhase::Downloading { done: 1, total: 2 });
        let events = w.update(state(vec![app("filmcraft", None, Some((4, downloading)))], false, None), &address);
        assert_eq!(events, vec![Event::Progress { app: "filmcraft".into(), phase: JobPhase::Downloading { done: 1, total: 2 } }]);
        assert!(w.busy());
        let done = JobState::Finished(Ok("FilmCraft installed: 0.13.0".into()));
        let events = w.update(state(vec![app("filmcraft", Some("0.13.0"), Some((4, done.clone())))], false, None), &address);
        assert!(matches!(events.first(), Some(Event::Detected { .. })), "{events:?}");
        assert_eq!(events.last(), Some(&Event::JobFinished { app: "filmcraft".into(), result: Ok("FilmCraft installed: 0.13.0".into()) }));
        assert!(!w.busy());
        // Reported once.
        assert_eq!(w.update(state(vec![app("filmcraft", Some("0.13.0"), Some((4, done)))], false, None), &address), Vec::new());
    }

    #[test]
    fn jobs_that_ended_before_this_launcher_looked_stay_quiet() {
        let mut w = Watcher::default();
        let old = JobState::Finished(Err("no space left".into()));
        let events = w.update(state(vec![app("filmcraft", None, Some((2, old)))], false, None), &address);
        assert!(!events.iter().any(|e| matches!(e, Event::JobFinished { .. })), "{events:?}");
    }

    #[test]
    fn a_job_the_server_forgot_ends_with_a_reason() {
        let mut w = Watcher::default();
        w.update(state(vec![app("filmcraft", None, None)], false, None), &address);
        w.watch("filmcraft", 9);
        let events = w.update(state(vec![app("filmcraft", None, None)], false, None), &address);
        assert_eq!(events, vec![Event::JobFinished { app: "filmcraft".into(), result: Err(LOST_JOB.into()) }]);
    }

    #[test]
    fn a_check_reports_its_start_and_finish_once() {
        let mut w = Watcher::default();
        assert_eq!(w.update(state(vec![], true, Some(1)), &address), vec![Event::CheckStarted]);
        assert_eq!(w.update(state(vec![], true, Some(1)), &address), Vec::new());
        assert_eq!(
            w.update(state(vec![], false, Some(2)), &address),
            vec![Event::CheckFinished { at: 2, source: Source::GitHubApi, rate_limited_until: None }]
        );
        assert_eq!(w.update(state(vec![], false, Some(2)), &address), Vec::new());
    }
}
