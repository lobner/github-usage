#!/usr/bin/env bash
# Build "GitHub Status.app" — a no-dock menu-bar agent (LSUIElement) — and offer
# to install it to /Applications.
#
# Usage: ./build/make-app.sh [--install | --no-install]
#
# With no flag it asks. --install and --no-install answer for you, which is also
# what happens when stdin isn't a terminal: it builds and stops.
#
# Installing quits any running copy first (either the one in /Applications or one
# launched from this repo — they share a bundle id), replaces the bundle, and
# relaunches it from /Applications.
set -euo pipefail

cd "$(dirname "$0")/.."

APP="GitHub Status.app"
EXE="githubstatus"
BUNDLE_ID="dk.biq.githubstatus"
DEST="/Applications"

CONTENTS="$APP/Contents"
MACOS="$CONTENTS/MacOS"

# Validate the flag before building, but leave an interactive prompt until after,
# so nothing is asked about a build that then fails.
answer=""
case "${1:-}" in
--install) answer=y ;;
--no-install) answer=n ;;
"") ;;
*)
	echo "unknown argument: $1 (expected --install or --no-install)" >&2
	exit 2
	;;
esac

rm -rf "$APP"
mkdir -p "$MACOS"

echo "Building binary…"
# systray links Cocoa, so cgo must be enabled (it is by default on macOS).
CGO_ENABLED=1 go build -trimpath -ldflags "-s -w" -o "$MACOS/$EXE" .

cp build/Info.plist "$CONTENTS/Info.plist"

# Ad-hoc sign with our own identifier. The Go linker already applies an ad-hoc
# signature, but it identifies every binary it ever produces as "a.out" — and
# macOS Background Task Management keys login items by signing identity, so two
# unsigned Go apps look like the same item to it and registering one evicts the
# other's "Open at Login" entry. Signing with -i gives this bundle a distinct,
# stable identity. Still not a Developer ID and still not notarised: this buys
# identity, not trust.
codesign --force --sign - --identifier "$BUNDLE_ID" "$APP" >/dev/null 2>&1 ||
	echo "  warning: could not ad-hoc sign; Open at Login may clash with sibling apps" >&2

echo "Built \"${APP}\""

# Match the process by executable name, not with pgrep -f: a full-command-line
# match also hits any shell whose arguments happen to mention the bundle path,
# including the one running this script.
running() { pgrep -x "$EXE" >/dev/null 2>&1; }

# wait_gone polls for up to 5 s, since quitting is asynchronous.
wait_gone() {
	for _ in $(seq 20); do
		running || return 0
		sleep 0.25
	done
	return 1
}

quit_running() {
	running || return 0
	echo "Quitting the running app…"
	osascript -e "quit app id \"${BUNDLE_ID}\"" >/dev/null 2>&1 || true
	wait_gone && return 0

	echo "  it did not quit on its own; terminating it"
	pkill -x "$EXE" >/dev/null 2>&1 || true
	wait_gone && return 0

	echo "  still running — quit it from its menu, then re-run this script" >&2
	return 1
}

if [[ -z "$answer" ]]; then
	if [[ -t 0 ]]; then
		read -r -p "Install to ${DEST} and relaunch? [Y/n] " answer || answer=n
	else
		answer=n # non-interactive: build only, don't touch /Applications
	fi
fi

case "${answer:-y}" in
[Yy]* | "")
	quit_running
	echo "Installing to ${DEST}/${APP}…"
	rm -rf "${DEST:?}/${APP:?}"
	if ! cp -R "$APP" "$DEST/"; then
		echo "Could not write to ${DEST} — copy it yourself, or re-run with sudo." >&2
		exit 1
	fi
	open "${DEST}/${APP}"
	echo "Installed and relaunched \"${DEST}/${APP}\""
	;;
*)
	echo "Not installed. To do it yourself:"
	echo "  cp -R \"${APP}\" ${DEST}/ && open \"${DEST}/${APP}\""
	echo "Or just try this build in place:  open \"${APP}\""
	;;
esac
