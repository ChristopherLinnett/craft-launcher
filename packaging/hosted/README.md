# Hosted ArtCraft Launcher

One server for a team: ArtCraft Launcher at the root of an address, and every Craft App's browser
build at `/<app>/` (`/photocraft/`, `/filmcraft/`…), all behind one shared password. People open
the address, sign in, and press **Open** on an app; it runs in their own browser. **Install**,
**Update** and **Remove** in the launcher change what the server serves, for everyone.

## How it fits together

| Piece | What it does |
|---|---|
| `apps/artcraft-launcher-host` | The server (`artcraft-launcher-host`). Serves the launcher's web build and the installed apps, checks the team password, looks up releases and installs browser builds. Listens on loopback; put HTTPS in front. |
| `apps/artcraft-launcher-web` | The same browser launcher as everywhere. At start it asks its own server for `api/state`; when the server answers, it switches to hosted mode (`src/web/hosted.rs`), otherwise it stays the plain catalog-and-downloads page. |
| `crates/core/src/hosted.rs` | What the two say to each other, and how the server's state becomes the launcher's events. |
| `packaging/hosted/install.sh` | Installs or updates the server as a systemd service (run as root on the server). |
| `packaging/hosted/deploy.sh` | Builds and installs from a developer machine over SSH. |

Installing an app on the server picks its release's `<app>-web-<version>.zip`, downloads it from
the app's GitHub release, checks it against the release's `SHA256SUMS.txt`, unpacks it into a new
folder (with a gzipped copy of each text and wasm file) and switches to it. The build before stays
on disk until the next update, so a page opened a moment earlier still finds its files.

Apps are only installed or updated when someone presses the button: a hosted launcher never
updates on its own, and its Settings don't offer automatic updates.

## Data folder

```text
<root>/
  bin/artcraft-launcher-host
  launcher/                 the launcher's web build (replaced by each deploy)
  data/
    config.json             the password's Argon2 hash and the session-cookie key (0600)
    installed.json          which build of each app is served
    releases.json           saved release lookups
    downloads/              builds on their way, and paused downloads
    sites/<app>/<version>-<job>/
```

## Deploying

Needs: `trunk` and the `wasm32-unknown-unknown` target here; on the server, SSH access for an
account with passwordless `sudo` and Docker (the server binary is built there, in `rust:1-bookworm`).

Write a target file:

```sh
HOST=deploy@server.example
ROOT=/opt/artcraft-launcher-host
SERVICE=artcraft-launcher-host
RUN_USER=artcraft            # must exist
LISTEN=127.0.0.1:8120
PASSWORD_FILE=./team-password   # first deploy only
```

Then:

```sh
packaging/hosted/deploy.sh path/to/target.env
```

Run it again to update the launcher and the server; installed apps stay. `--skip-web` reuses the
last web build in `dist/web`.

Change the password (ends every session):

```sh
ssh deploy@server.example "sudo -u artcraft /opt/artcraft-launcher-host/bin/artcraft-launcher-host set-password --data /opt/artcraft-launcher-host/data" <<< 'the new password'
```

## In front of it

The server speaks plain HTTP on loopback. Serve it over HTTPS: WebGPU and the clipboard need a
secure context, and the session cookie is `Secure`. Any of these work:

- **Tailscale Funnel** (public) or **Tailscale Serve** (tailnet only): `tailscale funnel --bg http://127.0.0.1:8120`.
- **A reverse proxy** (Caddy, nginx) proxying the whole host to `127.0.0.1:8120`. Keep the
  host's root: the launcher lives at `/` and the apps at `/<app>/`.

Wrong passwords are limited per client, by the first `X-Forwarded-For` address when the
connection comes from loopback (the proxy), so make sure the proxy sets it.

## Security notes

- Everything but the sign-in page needs a session: the password is the only gate.
- Changes (`POST`s to `api/`) also need the `x-artcraft-launcher` header, which a cross-site
  form can't send. The session cookie is `HttpOnly`, `Secure` and `SameSite=Lax`.
- Downloads come only from `github.com/storytold/<app>/releases` and are checked against
  `SHA256SUMS.txt`. Zips are unpacked with the same checks as the desktop launcher's (no paths
  out of the folder, no links, size and file-count limits).
- Documents never reach the server: apps open and save files in each person's browser.
