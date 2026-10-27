//! ArtCraft Launcher served by a hosted launcher.
//!
//! The server installs the Craft Apps' browser builds and serves each one at `<launcher>/<app>/`,
//! so here Install, Update and Remove ask the server ([`artcraft_launcher_core::hosted`]), Open
//! opens the app's page in a new tab, and release information comes from the server, which looks
//! it up once for everyone. The details pages' pictures still come from GitHub, as in the plain
//! web build.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use artcraft_launcher_core::Platform;
use artcraft_launcher_core::hosted::{self, AppAction, AppRequest, CheckRequest, HostState, JobStarted, Watcher};
use artcraft_launcher_core::protocol::{Backend, Capabilities, Command, Event, JobPhase};

use super::{WebBackend, body_text, fetch};

/// How often to ask the server for its state while nothing runs, and while something does.
const IDLE_MS: u64 = 30_000;
const BUSY_MS: u64 = 1_000;
/// After this launcher asks the server for something, look closely for this long.
const AFTER_REQUEST_MS: f64 = 5_000.0;

/// The server's state when this page is served by a hosted launcher, else `None`.
pub(super) async fn probe() -> Option<HostState> {
    get_state(false).await.ok()
}

/// Why the server's state couldn't be had.
enum StateError {
    /// The session ended: the page has to sign in again.
    SignedOut,
    /// Not a hosted launcher, or not reachable just now.
    Unavailable,
}

async fn get_state(prereleases: bool) -> Result<HostState, StateError> {
    let url = format!("{}?prereleases={}", hosted::STATE_PATH, u8::from(prereleases));
    let resp = fetch(&url, &same_origin("GET", None), &[]).await.map_err(|_| StateError::Unavailable)?;
    match resp.status() {
        200 => {}
        401 => return Err(StateError::SignedOut),
        _ => return Err(StateError::Unavailable),
    }
    let text = body_text(&resp).await.ok_or(StateError::Unavailable)?;
    serde_json::from_str(&text).map_err(|_| StateError::Unavailable)
}

/// `POST`s the JSON `body` to `path`: the answer's status and text.
async fn post(path: &str, body: &str) -> Result<(u16, String), String> {
    let headers = [(hosted::REQUEST_HEADER, "1"), ("Content-Type", "application/json")];
    let resp = fetch(path, &same_origin("POST", Some(body)), &headers).await.map_err(|why| format!("couldn't reach the server ({why})"))?;
    Ok((resp.status(), body_text(&resp).await.unwrap_or_default()))
}

fn same_origin(method: &str, body: Option<&str>) -> web_sys::RequestInit {
    let init = web_sys::RequestInit::new();
    init.set_method(method);
    init.set_mode(web_sys::RequestMode::SameOrigin);
    if let Some(body) = body {
        init.set_body(&wasm_bindgen::JsValue::from_str(body));
    }
    init
}

/// What a refused request tells the user: the server's own words when it gave some.
fn refusal(status: u16, text: &str) -> String {
    let text = text.trim();
    if text.is_empty() || text.len() > 300 { format!("the server answered HTTP {status}") } else { text.to_string() }
}

/// `<launcher>/<app>/`: where the server serves an installed app.
fn app_address(id: &str) -> String {
    let page = web_sys::window().and_then(|w| w.location().href().ok());
    page.and_then(|href| web_sys::Url::new_with_base(&format!("{id}/"), &href).ok()).map_or_else(|| format!("{id}/"), |url| url.href())
}

/// The session ended: reload, and the server shows its sign-in page.
fn sign_in_again() {
    if let Some(window) = web_sys::window() {
        let _ = window.location().reload();
    }
}

fn now_ms() -> f64 {
    js_sys::Date::now()
}

#[derive(Default)]
struct Shared {
    watcher: Watcher,
    prereleases: bool,
    /// A state request is on its way.
    fetching: bool,
    /// When the last state request went out (ms since the epoch).
    asked_at: f64,
    /// Look closely until then (ms since the epoch).
    eager_until: f64,
}

/// What the futures a request starts need to report back.
#[derive(Clone)]
struct Link {
    shared: Rc<RefCell<Shared>>,
    events: Rc<RefCell<Vec<Event>>>,
    ctx: egui::Context,
}

impl Link {
    fn emit(&self, event: Event) {
        self.events.borrow_mut().push(event);
        self.ctx.request_repaint();
    }

    fn report(&self, state: HostState) {
        let events = self.shared.borrow_mut().watcher.update(state, &app_address);
        self.events.borrow_mut().extend(events);
        self.ctx.request_repaint();
    }

    /// Asks for the server's state, unless a request is already on its way.
    fn refresh(&self) {
        let prereleases = {
            let mut shared = self.shared.borrow_mut();
            if shared.fetching {
                return;
            }
            shared.fetching = true;
            shared.asked_at = now_ms();
            shared.prereleases
        };
        let link = self.clone();
        wasm_bindgen_futures::spawn_local(async move {
            let state = get_state(prereleases).await;
            link.shared.borrow_mut().fetching = false;
            match state {
                Ok(state) => link.report(state),
                Err(StateError::SignedOut) => sign_in_again(),
                // Asked again at the next tick.
                Err(StateError::Unavailable) => {}
            }
        });
    }

    /// Sends one request and looks at the server's state straight after.
    fn ask(&self, path: String, body: String, answered: impl FnOnce(&Link, Result<(u16, String), String>) + 'static) {
        self.shared.borrow_mut().eager_until = now_ms() + AFTER_REQUEST_MS;
        let link = self.clone();
        wasm_bindgen_futures::spawn_local(async move {
            let answer = post(&path, &body).await;
            if matches!(answer, Ok((401, _))) {
                sign_in_again();
                return;
            }
            answered(&link, answer);
            // A look already on its way may predate this request; the eager ticks look again.
            link.refresh();
        });
    }

    fn check(&self, force: bool) {
        let prereleases = self.shared.borrow().prereleases;
        let body = serde_json::to_string(&CheckRequest { force, prereleases }).unwrap_or_default();
        self.ask(hosted::CHECK_PATH.to_string(), body, |_, answer| match answer {
            Ok((200..=299, _)) => {}
            Ok((status, text)) => log::warn!("release check refused: {}", refusal(status, &text)),
            Err(e) => log::warn!("release check: {e}"),
        });
    }

    fn act(&self, app: String, action: AppAction) {
        let prereleases = self.shared.borrow().prereleases;
        let body = serde_json::to_string(&AppRequest { prereleases }).unwrap_or_default();
        self.ask(hosted::app_action_path(&app, action), body, move |link, answer| {
            let result = match answer {
                Ok((200..=299, text)) => {
                    if let Ok(JobStarted { job }) = serde_json::from_str(&text) {
                        link.shared.borrow_mut().watcher.watch(&app, job);
                        let phase = if action == AppAction::Remove { JobPhase::Removing } else { JobPhase::Downloading { done: 0, total: 0 } };
                        link.emit(Event::Progress { app, phase });
                    }
                    return;
                }
                Ok((status, text)) => refusal(status, &text),
                Err(e) => e,
            };
            link.emit(Event::JobFinished { app, result: Err(result) });
        });
    }
}

/// The browser backend for a hosted launcher. It wraps the plain one for what doesn't change
/// (pictures, the clock).
pub(super) struct HostedBackend {
    web: WebBackend,
    link: Link,
}

impl HostedBackend {
    pub(super) fn new(web: WebBackend, first: HostState) -> Self {
        let shared = Rc::new(RefCell::new(Shared { asked_at: now_ms(), ..Shared::default() }));
        let link = Link { shared, events: web.events.clone(), ctx: web.ctx.clone() };
        link.report(first);
        Self { web, link }
    }
}

impl Backend for HostedBackend {
    fn send(&mut self, command: Command) {
        match command {
            Command::CheckForUpdates { force } => self.link.check(force),
            Command::Detect => self.link.refresh(),
            Command::Install { app } | Command::Replace { app } => self.link.act(app, AppAction::Install),
            Command::Uninstall { app } => self.link.act(app, AppAction::Remove),
            Command::Pause { app } => self.link.act(app, AppAction::Pause),
            Command::Cancel { app } => self.link.act(app, AppAction::Cancel),
            Command::Launch { app } => self.link.ctx.open_url(egui::OpenUrl::new_tab(app_address(&app))),
            Command::Configure(options) => {
                let changed = {
                    let mut shared = self.link.shared.borrow_mut();
                    let changed = shared.prereleases != options.prereleases;
                    shared.prereleases = options.prereleases;
                    changed
                };
                if changed {
                    self.link.check(false);
                }
            }
            command @ Command::FetchScreenshots { .. } => self.web.send(command),
            // Nothing on the server to show in a file manager, and the server updates the
            // launcher itself.
            Command::Reveal { .. } | Command::UpdateLauncher => {}
        }
    }

    fn poll(&mut self) -> Vec<Event> {
        let (due, every) = {
            let shared = self.link.shared.borrow();
            let now = now_ms();
            let every = if shared.watcher.busy() || now < shared.eager_until { BUSY_MS } else { IDLE_MS };
            (now - shared.asked_at >= every as f64, every)
        };
        if due {
            self.link.refresh();
        }
        // Frames stop when nothing moves; ask for one when the next look is due.
        self.link.ctx.request_repaint_after(Duration::from_millis(every));
        self.web.poll()
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities { manage_apps: true, reveal_label: "", self_update: false }
    }

    fn platform(&self) -> Option<Platform> {
        Some(Platform::WEB)
    }

    fn now(&self) -> i64 {
        self.web.now()
    }

    fn unattended_installs(&self) -> bool {
        false
    }
}
