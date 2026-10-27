//! The HTTP side: sign-in, the launcher's `api/`, and the files.
//!
//! Every path but `/login` needs a signed-in session. `/<app>/…` serves an installed app's build;
//! everything else is ArtCraft Launcher's own web build. Files are revalidated on every load
//! (`no-cache` with an `ETag`), so an update never pairs an old page with new files, and sent
//! compressed where the build has a `.gz` copy.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use artcraft_launcher_core::hosted::{self, AppRequest, CheckRequest};
use serde::Serialize;
use tiny_http::{Header, Method, Request, Response, ResponseBox, Server};

use crate::auth::{self, SignIn};
use crate::host::Host;
use crate::store::{compressible, gz_path};
use crate::web::{self, content_type, safe_next, sign_in_page};

/// Requests served at once. A large app download holds one for as long as it takes.
const WORKERS: usize = 16;
/// The largest request body read (sign-in forms and API requests are tiny).
const MAX_BODY: u64 = 16 * 1024;

pub fn run(listen: &str, host: Arc<Host>) -> Result<(), String> {
    let server = Arc::new(Server::http(listen).map_err(|e| format!("couldn't listen on {listen} ({e})"))?);
    log::info!("serving ArtCraft Launcher on http://{listen}");
    let workers: Vec<_> = (0..WORKERS)
        .filter_map(|n| {
            let (server, host) = (Arc::clone(&server), Arc::clone(&host));
            std::thread::Builder::new()
                .name(format!("http-{n}"))
                .spawn(move || {
                    loop {
                        match server.recv() {
                            Ok(request) => handle(&host, request),
                            Err(e) => {
                                log::warn!("couldn't take a request ({e})");
                                std::thread::sleep(Duration::from_millis(100));
                            }
                        }
                    }
                })
                .map_err(|e| log::warn!("couldn't start a worker ({e})"))
                .ok()
        })
        .collect();
    if workers.is_empty() {
        return Err("couldn't start any worker".into());
    }
    for worker in workers {
        let _ = worker.join();
    }
    Ok(())
}

fn handle(host: &Arc<Host>, mut request: Request) {
    let url = request.url().to_string();
    let (path, query) = url.split_once('?').unwrap_or((url.as_str(), ""));
    let response = route(host, &mut request, path, query);
    log::debug!("{} {url} {}", request.method(), response.status_code().0);
    if let Err(e) = request.respond(response) {
        log::debug!("{url}: the browser left before the answer ({e})");
    }
}

fn route(host: &Arc<Host>, request: &mut Request, path: &str, query: &str) -> ResponseBox {
    let now = artcraft_launcher_native::now();
    let method = request.method().clone();
    match (&method, path) {
        (Method::Get | Method::Head, "/login") => return html(200, &sign_in_page(&safe_next(web::form_value(query, "next").as_deref()), None)),
        (Method::Post, "/login") => return sign_in(host, request, now),
        (_, "/logout") => return with_headers(redirect(303, "/login"), [header("Set-Cookie", &auth::ended_cookie())]),
        _ => {}
    }
    let signed_in = header_value(request, "Cookie").and_then(|c| auth::cookie_in(&c).map(str::to_string)).is_some_and(|c| host.auth.valid(&c, now));
    if !signed_in {
        return match method {
            // Pages go to the sign-in page and come back; the launcher's API calls just hear why.
            Method::Get | Method::Head if !path.starts_with("/api/") => {
                let back = if query.is_empty() { path.to_string() } else { format!("{path}?{query}") };
                redirect(303, &format!("/login?next={}", web::encode(&back)))
            }
            _ => text(401, "Sign in first."),
        };
    }
    if let Some(api) = path.strip_prefix('/').filter(|p| p.starts_with("api/")) {
        return api_call(host, request, &method, api, query);
    }
    match method {
        Method::Get => file(host, request, path, query, false),
        Method::Head => file(host, request, path, query, true),
        _ => text(405, "Only GET and HEAD here."),
    }
}

fn sign_in(host: &Host, request: &mut Request, now: i64) -> ResponseBox {
    let form = body(request);
    let next = safe_next(web::form_value(&form, "next").as_deref());
    let password = web::form_value(&form, "password").unwrap_or_default();
    let client = client(request);
    match host.auth.sign_in(&client, &password, now) {
        SignIn::Ok(cookie) => with_headers(redirect(303, &next), [header("Set-Cookie", &auth::session_cookie(&cookie))]),
        SignIn::Wrong => {
            log::info!("wrong password from {client}");
            html(401, &sign_in_page(&next, Some("That password isn't right.")))
        }
        SignIn::Throttled => {
            log::warn!("sign-in refused for {client}: too many wrong passwords");
            html(429, &sign_in_page(&next, Some("Too many wrong passwords. Wait a few minutes, then try again.")))
        }
    }
}

/// Who is signing in, for counting wrong passwords: the browser's address as the proxy in front
/// (Tailscale Funnel, a reverse proxy) reports it, else the connection's.
fn client(request: &Request) -> String {
    let peer = request.remote_addr().map(|a| a.ip());
    let forwarded = header_value(request, "X-Forwarded-For").and_then(|f| f.split(',').next().map(|ip| ip.trim().to_string()));
    match (peer, forwarded) {
        (Some(ip), Some(forwarded)) if ip.is_loopback() && !forwarded.is_empty() => forwarded,
        (Some(ip), _) => ip.to_string(),
        (None, _) => "unknown".into(),
    }
}

fn api_call(host: &Arc<Host>, request: &mut Request, method: &Method, path: &str, query: &str) -> ResponseBox {
    if *method == Method::Get && path == hosted::STATE_PATH {
        let prereleases = web::form_value(query, "prereleases").is_some_and(|v| v == "1");
        return json(200, &host.state(prereleases));
    }
    if *method != Method::Post {
        return text(404, "No such API.");
    }
    if header_value(request, hosted::REQUEST_HEADER).is_none() {
        return text(403, "That request didn't come from ArtCraft Launcher.");
    }
    let body = body(request);
    if path == hosted::CHECK_PATH {
        let check: CheckRequest = serde_json::from_str(&body).unwrap_or_default();
        host.check(check);
        return json(202, &serde_json::json!({}));
    }
    let Some((app, action)) = hosted::parse_app_action_path(path) else { return text(404, "No such API.") };
    let app_request: AppRequest = serde_json::from_str(&body).unwrap_or_default();
    match host.act(app, action, app_request) {
        Ok(Some(started)) => json(202, &started),
        Ok(None) => json(200, &serde_json::json!({})),
        Err(refusal) => text(refusal.status, &refusal.message),
    }
}

/// An installed app's file, or the launcher's.
fn file(host: &Host, request: &Request, path: &str, query: &str, head: bool) -> ResponseBox {
    let Some(segments) = web::segments(path) else { return text(400, "That path isn't allowed.") };
    let (root, rest) = match segments.split_first().and_then(|(first, rest)| host.site(first).map(|root| (root, rest))) {
        Some(found) => found,
        None => (host.launcher.clone(), segments.as_slice()),
    };
    let mut file = rest.iter().fold(root.clone(), |dir, segment| dir.join(segment));
    if file.is_dir() {
        // Relative links inside a page only work below a path that ends in `/`.
        if !path.ends_with('/') {
            let to = if query.is_empty() { format!("{path}/") } else { format!("{path}/?{query}") };
            return redirect(301, &to);
        }
        file.push("index.html");
    }
    if !inside(&root, &file) {
        return text(404, "Not found.");
    }
    send_file(request, &file, head)
}

/// Whether `file` is a regular file under `root`, with no link on the way out of it.
fn inside(root: &Path, file: &Path) -> bool {
    let (Ok(root), Ok(real)) = (root.canonicalize(), file.canonicalize()) else { return false };
    real.starts_with(&root) && std::fs::symlink_metadata(file).is_ok_and(|m| m.file_type().is_file())
}

fn send_file(request: &Request, file: &Path, head: bool) -> ResponseBox {
    let gzip = header_value(request, "Accept-Encoding").is_some_and(|a| a.contains("gzip")) && gz_path(file).is_file();
    let sent: PathBuf = if gzip { gz_path(file) } else { file.to_path_buf() };
    let Ok(opened) = File::open(&sent) else { return text(404, "Not found.") };
    let Ok(meta) = opened.metadata() else { return text(404, "Not found.") };
    let modified = meta.modified().ok().and_then(|m| m.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos());
    let etag = format!("\"{:x}-{modified:x}{}\"", meta.len(), if gzip { "-gz" } else { "" });
    let mut headers = vec![
        header("Content-Type", content_type(file)),
        header("Cache-Control", "no-cache"),
        header("ETag", &etag),
        header("X-Content-Type-Options", "nosniff"),
    ];
    if compressible(file) {
        headers.push(header("Vary", "Accept-Encoding"));
    }
    if gzip {
        headers.push(header("Content-Encoding", "gzip"));
    }
    let unchanged = header_value(request, "If-None-Match").is_some_and(|tags| tags.split(',').any(|t| t.trim() == etag));
    let response = if unchanged {
        Response::empty(304).boxed()
    } else if head {
        Response::empty(200).boxed()
    } else {
        // A known length (not chunks) lets the browser show how much of a big wasm has arrived.
        Response::from_file(opened).with_chunked_threshold(usize::MAX).boxed()
    };
    with_headers(response, headers)
}

fn body(request: &mut Request) -> String {
    let mut text = String::new();
    let _ = request.as_reader().take(MAX_BODY).read_to_string(&mut text);
    text
}

fn header_value(request: &Request, name: &str) -> Option<String> {
    request.headers().iter().find(|h| h.field.as_str().as_str().eq_ignore_ascii_case(name)).map(|h| h.value.as_str().to_string())
}

fn header(name: &str, value: &str) -> Option<Header> {
    Header::from_bytes(name.as_bytes(), value.as_bytes()).ok()
}

fn with_headers(response: ResponseBox, headers: impl IntoIterator<Item = Option<Header>>) -> ResponseBox {
    headers.into_iter().flatten().fold(response, |r, h| r.with_header(h))
}

fn text(status: u16, message: &str) -> ResponseBox {
    with_headers(Response::from_string(message).with_status_code(status).boxed(), [header("Content-Type", "text/plain; charset=utf-8")])
}

fn html(status: u16, page: &str) -> ResponseBox {
    let headers = [header("Content-Type", "text/html; charset=utf-8"), header("Cache-Control", "no-store"), header("X-Frame-Options", "DENY")];
    with_headers(Response::from_string(page).with_status_code(status).boxed(), headers)
}

fn json(status: u16, value: &impl Serialize) -> ResponseBox {
    match serde_json::to_string(value) {
        Ok(body) => with_headers(
            Response::from_string(body).with_status_code(status).boxed(),
            [header("Content-Type", "application/json"), header("Cache-Control", "no-store")],
        ),
        Err(e) => text(500, &format!("couldn't write the answer ({e})")),
    }
}

fn redirect(status: u16, to: &str) -> ResponseBox {
    with_headers(Response::empty(status).boxed(), [header("Location", to), header("Cache-Control", "no-store")])
}
