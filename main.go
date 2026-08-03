// Command githubstatus is a macOS menu-bar app that watches GitHub's incident
// feed. When an incident appears the menu-bar square blinks red and a
// Notification Centre banner is posted. Opening the menu acknowledges the
// incident: the blinking stops, the icon settles on a red notification dot, and
// the incident titles can be read (and clicked) in the menu itself.
//
// On first launch from its .app bundle it offers to open at login, registering
// itself as a login item if accepted (see internal/login).
//
// Environment overrides:
//
//	FEED_URL      feed to poll (default https://www.githubstatus.com/history.atom;
//	              a file:// URL works for offline testing)
//	POLL_SECONDS  polling interval in seconds (default 60, minimum 10)
package main

import (
	"context"
	"fmt"
	"os"
	"os/exec"
	"strconv"
	"sync"
	"time"

	"fyne.io/systray"

	"githubstatus/internal/feed"
	"githubstatus/internal/icon"
	"githubstatus/internal/login"
	"githubstatus/internal/notify"
)

const (
	defaultPollInterval = 60 * time.Second
	maxIncidentItems    = 6
	blinkInterval       = 700 * time.Millisecond
	statusPageURL       = "https://www.githubstatus.com"
)

var (
	feedURL      = envOr("FEED_URL", feed.DefaultURL)
	pollInterval = pollIntervalFromEnv()

	baseIcon     []byte
	alertIcon    []byte
	incidentIcon []byte

	mStatus    *systray.MenuItem
	mIncidents []*systray.MenuItem
	mLastCheck *systray.MenuItem
	mLogin     *systray.MenuItem

	refreshNow = make(chan struct{}, 1)

	// Icon state, owned by iconLoop. The poll goroutine reports what the feed
	// says; opening the menu acknowledges an alert.
	iconUpdates = make(chan iconUpdate, 1)
	iconAck     = make(chan struct{}, 1)

	// Poll-goroutine-only state.
	lastOngoing = map[string]bool{}
	firstRun    = true
	etag        string

	// Shared between the poll goroutine (writer) and click goroutines (readers).
	mu          sync.Mutex
	currentURLs = make([]string, maxIncidentItems)
)

func main() {
	systray.Run(onReady, onExit)
}

func onReady() {
	baseIcon = icon.BaseTemplatePNG()
	alertIcon = icon.AlertPNG()
	incidentIcon = icon.IncidentPNG()

	systray.SetTemplateIcon(baseIcon, baseIcon)
	systray.SetTooltip("GitHub Status — checking…")

	mStatus = systray.AddMenuItem("Checking GitHub status…", "")
	mStatus.Disable()
	systray.AddSeparator()

	// Pre-allocate a fixed pool of incident rows (systray can't reliably remove
	// items), shown/hidden and relabelled on each poll.
	for i := 0; i < maxIncidentItems; i++ {
		mi := systray.AddMenuItem("", "")
		mi.Hide()
		mIncidents = append(mIncidents, mi)
		go func(idx int) {
			for range mi.ClickedCh {
				openURL(incidentURL(idx))
			}
		}(i)
	}
	systray.AddSeparator()

	mOpenPage := systray.AddMenuItem("Open GitHub Status page", "Open githubstatus.com")
	mRefresh := systray.AddMenuItem("Refresh now", "Check the feed immediately")
	mLastCheck = systray.AddMenuItem("", "")
	mLastCheck.Disable()
	systray.AddSeparator()

	mLogin = systray.AddMenuItemCheckbox("Launch at Login",
		"Open GitHub Status automatically when you log in", login.Enabled())
	if login.BundlePath() == "" || !login.Supported() {
		mLogin.Hide() // nothing to register: not a bundle, or macOS 12 or earlier
	}
	systray.AddSeparator()

	mQuit := systray.AddMenuItem("Quit", "Quit GitHub Status")

	go func() {
		for range mOpenPage.ClickedCh {
			openURL(statusPageURL)
		}
	}()
	go func() {
		for range mRefresh.ClickedCh {
			triggerRefresh()
		}
	}()
	go func() {
		for range mLogin.ClickedCh {
			toggleLaunchAtLogin()
		}
	}()
	go func() {
		<-mQuit.ClickedCh
		systray.Quit()
	}()

	// Opening the menu means the user has seen the alert: stop the blinking.
	go func() {
		for range systray.TrayOpenedCh {
			select {
			case iconAck <- struct{}{}:
			default: // an acknowledgement is already queued
			}
		}
	}()

	go iconLoop()
	go pollLoop()
	go offerLaunchAtLogin()
}

func onExit() {}

// offerLaunchAtLogin asks — once — whether the app should open at login, and
// registers it as a login item if so. It stays quiet when there is nothing to
// offer: when the user has already answered either way, when the binary is not
// running from its .app bundle, or on a macOS without SMAppService.
func offerLaunchAtLogin() {
	if login.BundlePath() == "" || !login.Supported() || login.Answered() {
		return
	}

	yes, err := notify.Confirm(
		"Launch GitHub Status at login?",
		"GitHub Status can start automatically when you log in, so it keeps watching for incidents without you having to open it.",
		"Launch at Login", "Not Now")
	if err != nil {
		return // the dialog itself failed; try again next launch rather than guess
	}
	if !yes {
		_ = login.RecordAnswer(false)
		return
	}

	if err := login.Enable(); err != nil {
		// Don't record the answer, so the offer is made again next launch.
		_ = notify.Banner("GitHub Status", "Could not open at login: "+err.Error())
		return
	}
	_ = login.RecordAnswer(true)
	mLogin.Check()
	warnIfNeedsApproval()
}

// toggleLaunchAtLogin flips the Launch at Login checkbox. The checkbox is only
// ticked once the change has actually gone through, so a failure leaves the menu
// showing what is really the case.
func toggleLaunchAtLogin() {
	if mLogin.Checked() {
		if err := login.Disable(); err != nil {
			_ = notify.Banner("GitHub Status", "Could not stop opening at login: "+err.Error())
			return
		}
		_ = login.RecordAnswer(false)
		mLogin.Uncheck()
		return
	}

	if err := login.Enable(); err != nil {
		_ = notify.Banner("GitHub Status", "Could not open at login: "+err.Error())
		return
	}
	_ = login.RecordAnswer(true)
	mLogin.Check()
	warnIfNeedsApproval()
}

// warnIfNeedsApproval covers the case where the user has previously switched the
// login item off in System Settings: registering succeeds, but macOS keeps it
// held back until they switch it on there.
func warnIfNeedsApproval() {
	if login.NeedsApproval() {
		_ = notify.Banner("GitHub Status",
			"Switch GitHub Status on under System Settings → General → Login Items to finish enabling it.")
	}
}

// iconUpdate is what the latest poll saw: whether anything is ongoing, and
// whether any of it is new since the previous poll.
type iconUpdate struct{ ongoing, isNew bool }

type iconState int

const (
	stateOK    iconState = iota // no incidents: plain template icon
	stateAlert                  // unacknowledged incident: blinking red square
	stateAck                    // seen by the user: static red notification dot
)

// iconLoop owns the menu-bar icon. It is the only writer, so the blink can never
// race with a state change and leave the wrong icon on screen.
func iconLoop() {
	st := stateOK
	red := false

	setRed := func(on bool) {
		red = on
		if on {
			systray.SetIcon(alertIcon)
		} else {
			systray.SetTemplateIcon(baseIcon, baseIcon)
		}
	}
	apply := func(s iconState) {
		st = s
		switch s {
		case stateOK:
			setRed(false)
		case stateAlert:
			setRed(true) // start the blink lit, so it is seen immediately
		case stateAck:
			red = false
			systray.SetIcon(incidentIcon)
		}
	}

	t := time.NewTicker(blinkInterval)
	defer t.Stop()

	for {
		select {
		case u := <-iconUpdates:
			switch {
			case !u.ongoing:
				if st != stateOK {
					apply(stateOK)
				}
			case u.isNew || st == stateOK:
				// A fresh incident re-arms the blink even if an earlier one
				// has already been acknowledged.
				apply(stateAlert)
			}
		case <-iconAck:
			if st == stateAlert {
				apply(stateAck)
			}
		case <-t.C:
			if st == stateAlert {
				setRed(!red)
			}
		}
	}
}

func pollLoop() {
	check() // immediate first check
	t := time.NewTicker(pollInterval)
	defer t.Stop()
	for {
		select {
		case <-t.C:
			check()
		case <-refreshNow:
			check()
		}
	}
}

func check() {
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()

	body, newETag, notModified, err := feed.Fetch(ctx, feedURL, etag)
	now := time.Now()

	if err != nil {
		// Keep the last known state; just note that the refresh failed.
		mLastCheck.SetTitle("Last check failed " + now.Format("15:04:05"))
		systray.SetTooltip("GitHub Status — offline (tried " + now.Format("15:04") + ")")
		return
	}
	if notModified {
		mLastCheck.SetTitle("Last checked " + now.Format("15:04:05") + " (no change)")
		return
	}
	etag = newETag

	incidents, err := feed.ParseAtom(body)
	if err != nil {
		mLastCheck.SetTitle("Parse error " + now.Format("15:04:05"))
		return
	}

	ongoing := feed.Ongoing(incidents)
	updateMenu(ongoing)
	isNew := notifyNew(ongoing)
	iconUpdates <- iconUpdate{ongoing: len(ongoing) > 0, isNew: isNew}
	mLastCheck.SetTitle("Last checked " + now.Format("15:04:05"))
}

// updateMenu relabels the menu contents. The menu-bar icon itself is left to
// iconLoop, which blinks it until the user opens the menu.
func updateMenu(ongoing []feed.Incident) {
	mu.Lock()
	defer mu.Unlock()

	for i := range currentURLs {
		currentURLs[i] = ""
	}

	if len(ongoing) == 0 {
		systray.SetTooltip("GitHub Status — all systems operational")
		mStatus.SetTitle("✓  All systems operational")
		for _, mi := range mIncidents {
			mi.Hide()
		}
		return
	}

	systray.SetTooltip(fmt.Sprintf("GitHub Status — %d ongoing incident(s)", len(ongoing)))

	status := fmt.Sprintf("●  %d ongoing incidents", len(ongoing))
	if len(ongoing) == 1 {
		status = "●  1 ongoing incident"
	}
	if len(ongoing) > maxIncidentItems {
		status += fmt.Sprintf("  (showing %d)", maxIncidentItems)
	}
	mStatus.SetTitle(status)

	for i, mi := range mIncidents {
		if i >= len(ongoing) {
			mi.Hide()
			continue
		}
		inc := ongoing[i]
		label := inc.Title
		if inc.LatestStatus != "" {
			label = inc.LatestStatus + " — " + inc.Title
		}
		mi.SetTitle("   " + truncate(label, 64))
		mi.SetTooltip(fmt.Sprintf("%s (%s)", inc.Title, inc.LatestStatus))
		currentURLs[i] = inc.URL
		mi.Show()
	}
}

// notifyNew fires a banner for each incident that has appeared since the last
// poll, and reports whether there was any. The first poll only establishes a
// baseline (no banner), so launching the app during an existing incident does
// not spam notifications — the icon still blinks for it.
func notifyNew(ongoing []feed.Incident) bool {
	cur := make(map[string]bool, len(ongoing))
	for _, inc := range ongoing {
		cur[inc.ID] = true
	}

	isNew := false
	for _, inc := range ongoing {
		if !lastOngoing[inc.ID] {
			isNew = true
			if !firstRun {
				_ = notify.Banner("GitHub incident", inc.Title)
			}
		}
	}
	if !firstRun && len(cur) == 0 && len(lastOngoing) > 0 {
		_ = notify.Banner("GitHub Status", "All incidents resolved")
	}

	lastOngoing = cur
	firstRun = false
	return isNew
}

func triggerRefresh() {
	select {
	case refreshNow <- struct{}{}:
	default: // a refresh is already queued
	}
}

func incidentURL(i int) string {
	mu.Lock()
	defer mu.Unlock()
	if i < 0 || i >= len(currentURLs) {
		return ""
	}
	return currentURLs[i]
}

func openURL(u string) {
	if u == "" {
		return
	}
	_ = exec.Command("open", u).Start()
}

func truncate(s string, n int) string {
	r := []rune(s)
	if len(r) <= n {
		return s
	}
	if n < 1 {
		return ""
	}
	return string(r[:n-1]) + "…"
}

func envOr(key, def string) string {
	if v := os.Getenv(key); v != "" {
		return v
	}
	return def
}

func pollIntervalFromEnv() time.Duration {
	if v := os.Getenv("POLL_SECONDS"); v != "" {
		if n, err := strconv.Atoi(v); err == nil && n >= 10 {
			return time.Duration(n) * time.Second
		}
	}
	return defaultPollInterval
}
