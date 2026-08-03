# GitHub Status — macOS menu-bar tracker

A tiny menu-bar app that watches GitHub's incident feed and surfaces ongoing
incidents without ever taking up menu-bar width:

- **All clear** → a plain monochrome square (stays out of the way).
- **New incident** → the square **blinks red** until you look at it.
- **Acknowledged** → opening the menu stops the blinking and leaves a **red dot**
  on the icon (Teams-style) for as long as the incident is ongoing. The incident
  names are read in the dropdown itself. A later, different incident starts the
  blinking again.
- A **Notification Centre banner** when a *new* incident first appears while the
  app is running.

<img width="251" height="187" alt="Screenshot 2026-06-08 at 23 47 37" src="https://github.com/user-attachments/assets/a71b25e6-4812-422d-b700-4dd7476fc51b" />

The dropdown lists each ongoing incident as **what its newest update actually
says** — `Update — We are experiencing degraded availability for chat & agent
models in Copilot…` — rather than the generic entry title ("Incident with
Copilot"), which repeats what the icon has already told you. Long messages are cut
at a word boundary and the tooltip carries the title and the whole update. Click a
row to open that incident. Below the list: *Open GitHub Status page*, *Refresh
now*, the last-checked time, *Launch at Login*, and *Quit*.

## How it decides "ongoing"

It polls `https://www.githubstatus.com/history.atom` (every 60 s by default).
Each `<entry>` is an incident whose update history is newest-first, each update
tagged `<strong>Investigating | Identified | Update | Monitoring | Resolved</strong>`
(maintenance uses `Scheduled | In progress | Completed`). An incident is
**ongoing** iff its newest label is **not** `Resolved`/`Completed`. Polling uses
a conditional `If-None-Match` request, so unchanged feeds cost a cheap `304`.

## Build & run

Requires Go 1.22+ (this repo pins `golang 1.25.11` via `.tool-versions`).

```sh
# Run in the foreground (Ctrl-C to stop):
go run .

# Or build a no-dock .app you can double-click / add to Login Items:
./build/make-app.sh
open "GitHub Status.app"
```

`make-app.sh` produces `GitHub Status.app` with `LSUIElement=true`, so it runs
as a menu-bar-only agent (no Dock icon) and survives closing the terminal.

## Configuration

| Env var        | Default                                       | Meaning                                  |
| -------------- | --------------------------------------------- | ---------------------------------------- |
| `FEED_URL`     | `https://www.githubstatus.com/history.atom`   | Feed to poll. A `file://…` path also works (handy for testing). |
| `POLL_SECONDS` | `60`                                          | Polling interval in seconds (min 10).    |

## Launch at login

The app offers this itself: the first time you run it from the `.app` bundle it
asks *"Launch GitHub Status at login?"*, and choosing **Launch at Login**
registers the bundle as a login item via `SMAppService` (macOS 13+). It then
appears under System Settings → General → Login Items → **Open at Login**, and
you can switch it off there.

Register the copy that is going to stay put — macOS remembers the bundle by the
path it was registered from, so install to `/Applications` *first* and answer the
prompt from that copy. Answering it from the repo build means the next
`make-app.sh` (which does `rm -rf` on the bundle) leaves a dangling login item.

The offer is made once, either way. The answer is remembered in
`~/Library/Application Support/dk.biq.githubstatus/launch-at-login`; delete that
file to be asked again. It is also skipped when the binary isn't running from a
bundle (`go run .`), since login items are bundles.

Two notes on why it uses `SMAppService` rather than a LaunchAgent:

- A launchd job in `~/Library/LaunchAgents` also starts the app at login, but
  macOS classifies it as a *background item*, so it shows up under **App
  Background Activity** instead — labelled "Item from unidentified developer",
  since these builds are unsigned. `build/dk.biq.githubstatus.plist` is kept as
  that manual alternative.
- `SMAppService.mainApp.status` cannot be used to decide whether to ask: it
  reports `enabled` for a bundle that has *never* been registered, and only says
  `notRegistered` once something has explicitly unregistered it. Hence the local
  state file above.

To set it up by hand instead:

- **Login Items** — System Settings → General → Login Items → add
  `GitHub Status.app`; or
- **LaunchAgent** — copy `build/dk.biq.githubstatus.plist` to
  `~/Library/LaunchAgents/`, fix the path inside if the app isn't in
  `/Applications`, then `launchctl load ~/Library/LaunchAgents/dk.biq.githubstatus.plist`.

## Project layout

```
main.go                 systray wiring, poll loop, menu, notifications
internal/feed/          fetch (conditional GET) + parse Atom + Ongoing() filter
internal/icon/          programmatic icons (base template, red blink, red-dot incident)
internal/login/         open-at-login registration via SMAppService (cgo)
internal/notify/        Notification Centre banner + confirm dialog via osascript
build/                  Info.plist, make-app.sh, LaunchAgent plist
third_party/systray/    vendored fork of fyne.io/systray (see below)
```

The app pins a **local fork of `fyne.io/systray`** under `third_party/systray`,
wired in via a `replace` directive in `go.mod`. The only change is a one-method
patch to `show_menu` in `systray_darwin.m`: it attaches the menu to the status
item and triggers it via the button so AppKit positions the dropdown directly
below the menu bar. Upstream pops the menu at the button's zero origin, which
renders it *over* the menu-bar icons until a mouse move forces a relayout. To
re-sync the fork to a newer upstream, re-copy it from the module cache and
re-apply that patch.

## Testing

```sh
go test ./...                                              # unit tests (offline)

# Diagnostics (build-tagged, opt-in):
go test -tags livetest -run TestLive -v ./internal/feed    # hit the real feed, list ongoing incidents
ICON_DUMP_DIR=/tmp go test -tags dumpicons ./internal/icon # write the icons to /tmp to eyeball them
```

## Notes & possible extensions

- The first poll only establishes a baseline, so launching during an existing
  incident blinks the icon but does not fire a banner; banners fire for
  incidents that appear afterwards.
- Severity-based colouring (minor/major/critical) and per-component filtering
  (alert only when e.g. Actions or Copilot is affected) would need GitHub's JSON
  APIs (`/api/v2/status.json`, `/api/v2/incidents/unresolved.json`); the feed
  layer is isolated in `internal/feed` so swapping the source is straightforward.

## License

This project is licensed under the [MIT License](LICENSE).

It vendors a fork of [`fyne.io/systray`](https://github.com/fyne-io/systray)
under `third_party/systray/`, which is licensed under the Apache License 2.0 and
retains its own [`LICENSE`](third_party/systray/LICENSE).
