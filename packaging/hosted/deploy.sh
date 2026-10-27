#!/usr/bin/env bash
# Deploy ArtCraft Launcher's hosted mode to a Linux server over SSH.
#
# Usage: packaging/hosted/deploy.sh <target.env> [--skip-web]
#
# Builds the launcher's web build here (packaging/web/package.sh: trunk), then the server binary
# on the server itself in Docker (its own architecture, no cross-compiling), and installs both as
# a systemd service with install.sh. Running it again updates both; installed apps stay.
#
# target.env (a shell file) sets:
#   HOST           SSH destination (user@host). That account needs Docker through passwordless sudo.
#   ROOT SERVICE RUN_USER LISTEN   passed to install.sh (see there)
#   PASSWORD_FILE  file holding the team password; read on the first deploy only
#   RUST_IMAGE     (optional) builder image, default rust:1-bookworm
#
# Remote commands are built here on purpose: what is escaped expands on the server.
# shellcheck disable=SC2029
set -euo pipefail
# shellcheck source=../env.sh
. "$(dirname "${BASH_SOURCE[0]}")/../env.sh"
HERE="$ROOT/packaging/hosted"

TARGET="${1:?usage: packaging/hosted/deploy.sh <target.env> [--skip-web]}"
SKIP_WEB="${2:-}"
[[ -f "$TARGET" ]] || { echo "error: $TARGET not found" >&2; exit 1; }
# Read the target in a subshell so its names can't clash with env.sh's ROOT.
# shellcheck disable=SC1090
eval "$(
  . "$TARGET"
  for name in HOST ROOT SERVICE RUN_USER LISTEN PASSWORD_FILE RUST_IMAGE; do
    printf 'T_%s=%q\n' "$name" "${!name:-}"
  done
)"
[[ -n "$T_HOST" ]] || { echo "error: $TARGET sets no HOST" >&2; exit 1; }
RUST_IMAGE="${T_RUST_IMAGE:-rust:1-bookworm}"
# On the server, under the SSH account's home.
BUILD=".cache/artcraft-launcher-host"

if [[ "$SKIP_WEB" != "--skip-web" ]]; then
  "$ROOT/packaging/web/package.sh"
fi
[[ -f "$ROOT/dist/web/index.html" ]] || { echo "error: $ROOT/dist/web/index.html missing; run without --skip-web" >&2; exit 1; }

echo "--- sending the source to $T_HOST"
ssh "$T_HOST" "mkdir -p $BUILD/src $BUILD/cargo $BUILD/staging/bin"
rsync -a --delete --exclude=/target --exclude=/dist --exclude=/.git "$ROOT/" "$T_HOST:$BUILD/src/"

echo "--- building the server on $T_HOST"
ssh "$T_HOST" "cd $BUILD && sudo docker run --rm --user \"\$(id -u):\$(id -g)\" \
  -e CARGO_HOME=/cargo -e CARGO_TARGET_DIR=/src/target -v \"\$PWD/src:/src\" -v \"\$PWD/cargo:/cargo\" -w /src \
  $RUST_IMAGE cargo build --release --locked -p artcraft-launcher-host \
  && cp src/target/release/artcraft-launcher-host staging/bin/"

echo "--- installing"
rsync -a --delete "$ROOT/dist/web/" "$T_HOST:$BUILD/staging/launcher/"
rsync -a "$HERE/install.sh" "$T_HOST:$BUILD/staging/"
password_input=/dev/null
if [[ -n "$T_PASSWORD_FILE" ]]; then password_input="$T_PASSWORD_FILE"; fi
ssh "$T_HOST" "sudo env STAGING=\"\$HOME/$BUILD/staging\" ROOT=$(printf %q "$T_ROOT") SERVICE=$(printf %q "$T_SERVICE") \
  RUN_USER=$(printf %q "$T_RUN_USER") LISTEN=$(printf %q "$T_LISTEN") HOST_PASSWORD=\"\$(head -n1)\" \
  bash \"\$HOME/$BUILD/staging/install.sh\"" <"$password_input"
