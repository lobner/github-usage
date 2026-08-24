// Command githubusage is a macOS menu-bar app with two jobs.
//
// It shows how much of your Copilot premium-request allowance is spent, as a
// percentage in the menu-bar title, with the credit counts and reset date in the
// menu (see internal/credits).
//
// It also watches GitHub's incident feed. When an incident appears a dot in front
// of the percentage blinks red and a Notification Centre banner is posted.
// Opening the menu acknowledges the incident: the blinking stops, the dot settles
// solid, and each incident's newest update can be read (and clicked) in the menu
// itself.
//
// On first launch from its .app bundle it offers to open at login, registering
// itself as a login item if accepted (see internal/login).
//
// Environment overrides:
//
//	FEED_URL       feed to poll (default https://www.githubstatus.com/history.atom;
//	               a file:// URL works for offline testing)
//	COPILOT_URL    Copilot entitlement endpoint (default api.github.com/copilot_internal/user;
//	               a file:// URL works for offline testing)
//	POLL_SECONDS   polling interval in seconds (default 60, minimum 10)
//	ALERT_PERCENT  banner when premium-request usage first reaches this % (default 80, 0=off)
//	GH_TOKEN       GitHub token to use instead of the GitHub CLI's stored one
//	               (GITHUB_TOKEN also works)
package main

import (
	"context"
	"errors"
	"fmt"
	"os"
	"os/exec"
	"runtime"
	"strconv"
	"strings"
	"sync"
	"time"

	"fyne.io/systray"

	"githubusage/internal/credits"
	"githubusage/internal/feed"
	"githubusage/internal/icon"
	"githubusage/internal/login"
	"githubusage/internal/notify"
)

const (
	defaultPollInterval = 60 * time.Second
	defaultAlertPercent = 80
	maxIncidentItems    = 6
	blinkInterval       = 700 * time.Millisecond
	statusPageURL       = "https://www.githubstatus.com"
	// Where premium-request consumption is shown for a personal account. A seat
	// billed through an organisation reports its overage under that org's
	// billing instead, but a user's own usage still appears here.
	usagePageURL = "https://github.com/settings/billing/usage"

	appName = "GitHub Usage"
	repoURL = "https://github.com/lobner/github-usage"

	// Update messages run long; past this the row is cut and the tooltip carries
	// the rest, so the menu doesn't grow to the width of a paragraph.
	incidentLabelMaxLen = 90
)

// Build stamps, set by build/make-app.sh with -ldflags. A plain `go build` or
// `go run .` leaves them at these defaults, which is what "dev" in the About row
// means.
var (
	version   = "dev"
	commit    = ""
	buildDate = ""
)

var (
	feedURL      = envOr("FEED_URL", feed.DefaultURL)
	copilotURL   = envOr("COPILOT_URL", credits.DefaultURL)
	pollInterval = pollIntervalFromEnv()
	alertPercent = alertPercentFromEnv()

	alertIcon []byte // red circle for an ongoing incident

	mCredits      *systray.MenuItem
	mCreditsCount *systray.MenuItem
	mCreditsReset *systray.MenuItem
	mStatus       *systray.MenuItem
	mIncidents    []*systray.MenuItem
	mLastCheck    *systray.MenuItem
	mLogin        *systray.MenuItem

	refreshNow = make(chan struct{}, 1)

	// Icon state, owned by iconLoop. The poll goroutine reports what the feed
	// says; opening the menu acknowledges an alert.
	iconUpdates = make(chan iconUpdate, 1)
	iconAck     = make(chan struct{}, 1)

	// Poll-goroutine-only state.
	lastOngoing     = map[string]bool{}
	firstRun        = true
	etag            string
	creditsAlerted  bool
	creditsFirstRun = true

	// Shared between the poll goroutine (writer) and click goroutines (readers).
	mu          sync.Mutex
	currentURLs = make([]string, maxIncidentItems)
)

func main() {
	systray.Run(onReady, onExit)
}

func onReady() {
	alertIcon = icon.RedDotPNG()

	// The icon slot is occupied from the start, blank until there is something
	// to show, so the percentage never moves — see icon.BlankPNG.
	systray.SetIcon(icon.BlankPNG())
	systray.SetTitle("…")
	systray.SetTooltip("GitHub Usage — checking…")

	mCredits = systray.AddMenuItem("Premium requests: …", "Copilot premium-request allowance")
	mCredits.Disable()
	mCreditsCount = systray.AddMenuItem("", "")
	mCreditsCount.Disable()
	mCreditsCount.Hide()
	mCreditsReset = systray.AddMenuItem("", "")
	mCreditsReset.Disable()
	mCreditsReset.Hide()
	systray.AddSeparator()

	// Clickable, like claude-usage's: the headline row is how you get to the
	// status page, so there is no separate menu item for it.
	mStatus = systray.AddMenuItem("Service status: checking…", "Open githubstatus.com")

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

	mOpenPage := systray.AddMenuItem("Open usage page", "Open github.com/settings/billing/usage")
	mRefresh := systray.AddMenuItem("Refresh now", "Check the feed immediately")
	mLastCheck = systray.AddMenuItem("", "")
	mLastCheck.Disable()
	systray.AddSeparator()

	mLogin = systray.AddMenuItemCheckbox("Launch at Login",
		"Open GitHub Usage automatically when you log in", login.Enabled())
	if login.BundlePath() == "" || !login.Supported() {
		mLogin.Hide() // nothing to register: not a bundle, or macOS 12 or earlier
	}
	systray.AddSeparator()

	mAbout := systray.AddMenuItem(aboutTitle(), aboutTooltip())
	mQuit := systray.AddMenuItem("Quit", "Quit GitHub Usage")

	go func() {
		for range mOpenPage.ClickedCh {
			openURL(usagePageURL)
		}
	}()
	go func() {
		for range mStatus.ClickedCh {
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
		for range mAbout.ClickedCh {
			openURL(releaseNotesURL())
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

// aboutTitle names the app and the build in one line, so the version is readable
// without clicking anything.
func aboutTitle() string { return "About " + appName + " " + version }

// aboutTooltip carries what is needed to identify a build in a bug report. The
// architecture is included because the released archives are Apple silicon only,
// and the commit because these builds are unsigned — "which build is that?" is
// otherwise unanswerable.
func aboutTooltip() string {
	parts := make([]string, 0, 4)
	if commit != "" {
		parts = append(parts, "commit "+commit)
	}
	if buildDate != "" {
		parts = append(parts, "built "+buildDate)
	}
	parts = append(parts, runtime.GOOS+"/"+runtime.GOARCH)
	return strings.Join(parts, " · ") + " — click for the release notes"
}

// releaseNotesURL points at this exact version's notes when the build came from a
// clean tag, and at the release list otherwise, since there is nothing to link a
// dev build to.
func releaseNotesURL() string {
	if strings.HasPrefix(version, "v") && !strings.ContainsAny(version, "-+") {
		return repoURL + "/releases/tag/" + version
	}
	return repoURL + "/releases"
}

// offerLaunchAtLogin asks — once — whether the app should open at login, and
// registers it as a login item if so. It stays quiet when there is nothing to
// offer: when the user has already answered either way, when the binary is not
// running from its .app bundle, or on a macOS without SMAppService.
func offerLaunchAtLogin() {
	if login.BundlePath() == "" || !login.Supported() {
		return
	}

	// Already answered: don't ask again, but do make the system match what we
	// recorded — see login.Reconcile for why that can't be read back instead.
	if login.Answered() {
		if login.Answer() {
			if login.Reconcile() {
				mLogin.Check()
			} else {
				mLogin.Uncheck()
			}
		}
		return
	}

	yes, err := notify.Confirm(
		"Launch GitHub Usage at login?",
		"GitHub Usage can start automatically when you log in, so it keeps watching for incidents without you having to open it.",
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
		_ = notify.Banner("GitHub Usage", "Could not open at login: "+err.Error())
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
			_ = notify.Banner("GitHub Usage", "Could not stop opening at login: "+err.Error())
			return
		}
		_ = login.RecordAnswer(false)
		mLogin.Uncheck()
		return
	}

	if err := login.Enable(); err != nil {
		_ = notify.Banner("GitHub Usage", "Could not open at login: "+err.Error())
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
		_ = notify.Banner("GitHub Usage",
			"Switch GitHub Usage on under System Settings → General → Login Items to finish enabling it.")
	}
}

// iconUpdate is what the latest poll saw: whether anything is ongoing, and
// whether any of it is new since the previous poll.
type iconUpdate struct{ ongoing, isNew bool }

type iconState int

const (
	stateOK    iconState = iota // no incidents: the dot slot is there but empty
	stateAlert                  // unacknowledged incident: blinking red dot
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
			systray.SetIcon(icon.BlankPNG())
		}
	}
	apply := func(s iconState) {
		st = s
		switch s {
		case stateOK:
			// All clear: blank the dot, but keep the slot. Dropping the image
			// narrows the status item, which drags every menu-bar item to its
			// left sideways — so the row is steady only if the width never
			// changes, incident or not.
			red = false
			systray.SetIcon(icon.BlankPNG())
		case stateAlert:
			setRed(true) // start the blink lit, so it is seen immediately
		case stateAck:
			red = true
			systray.SetIcon(alertIcon)
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
	poll() // immediate first check
	t := time.NewTicker(pollInterval)
	defer t.Stop()
	for {
		select {
		case <-t.C:
			poll()
		case <-refreshNow:
			poll()
		}
	}
}

func poll() {
	check()
	checkCredits()
}

// checkCredits refreshes the Copilot premium-request allowance: the percentage in
// the menu bar and the three rows at the top of the menu.
func checkCredits() {
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()

	q, err := credits.Fetch(ctx, copilotURL)
	switch {
	case errors.Is(err, credits.ErrNoQuota):
		// Unlimited or quota-less plan: nothing to meter, so stay out of the bar.
		systray.SetTitle("")
		mCredits.SetTitle("Premium requests: unlimited on this plan")
		mCreditsCount.Hide()
		mCreditsReset.Hide()
		return
	case err != nil:
		systray.SetTitle("⚠")
		mCredits.SetTitle("⚠  " + err.Error())
		mCreditsCount.Hide()
		mCreditsReset.Hide()
		return
	}

	systray.SetTitle(strconv.Itoa(q.PercentUsed) + "%")
	mCredits.SetTitle(fmt.Sprintf("Premium requests: %d%% used", q.PercentUsed))
	mCredits.SetTooltip(fmt.Sprintf("%s of %s premium-request credits used on the %s plan",
		thousands(q.Used), thousands(q.Entitlement), q.Plan))

	count := fmt.Sprintf("   %s of %s credits", thousands(q.Used), thousands(q.Entitlement))
	if q.Overage > 0 {
		count += fmt.Sprintf("  (+%s over)", thousands(q.Overage))
	}
	mCreditsCount.SetTitle(count)
	mCreditsCount.Show()

	if q.ResetsAt.IsZero() {
		mCreditsReset.Hide()
	} else {
		mCreditsReset.SetTitle("   resets " + humanizeReset(q.ResetsAt))
		mCreditsReset.Show()
	}

	notifyCreditsThreshold(q)
}

// notifyCreditsThreshold fires a banner the first time usage reaches
// alertPercent, re-arming once it drops back below (after a monthly reset). The
// first poll only establishes a baseline, so launching while already high does
// not fire one.
func notifyCreditsThreshold(q credits.Quota) {
	if alertPercent <= 0 {
		return
	}
	high := q.PercentUsed >= alertPercent

	if !creditsFirstRun && high && !creditsAlerted {
		_ = notify.Banner("GitHub Copilot",
			fmt.Sprintf("%d%% of premium requests used (%s of %s)",
				q.PercentUsed, thousands(q.Used), thousands(q.Entitlement)))
	}
	creditsAlerted = high
	creditsFirstRun = false
}

// humanizeReset renders a reset time relatively when it is close and as a date
// when it is not — the premium-request window is monthly, so most of the time
// this reads "resets on 1 Sep".
func humanizeReset(t time.Time) string {
	// Round rather than truncate, so a reset 24m59s away doesn't read "in 24m".
	// Rounding first also keeps the branches consistent: 59m40s becomes an hour
	// and takes the "1h 0m" arm rather than printing "in 60m".
	d := time.Until(t).Round(time.Minute)
	switch {
	case d <= 0:
		return "now"
	case d < time.Hour:
		return fmt.Sprintf("in %dm", int(d.Minutes()))
	case d < 24*time.Hour:
		return fmt.Sprintf("in %dh %dm", int(d.Hours()), int(d.Minutes())%60)
	case d < 7*24*time.Hour:
		return "on " + t.Local().Format("Mon 2 Jan")
	default:
		return "on " + t.Local().Format("2 Jan")
	}
}

// thousands groups an integer with thin separators, so 13500 reads as 13,500.
func thousands(n int) string {
	s := strconv.Itoa(n)
	sign := ""
	if strings.HasPrefix(s, "-") {
		sign, s = "-", s[1:]
	}
	for i := len(s) - 3; i > 0; i -= 3 {
		s = s[:i] + "," + s[i:]
	}
	return sign + s
}

func check() {
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()

	body, newETag, notModified, err := feed.Fetch(ctx, feedURL, etag)
	now := time.Now()

	if err != nil {
		// Keep the last known state; just note that the refresh failed.
		mLastCheck.SetTitle("Last check failed " + now.Format("15:04:05"))
		systray.SetTooltip("GitHub Usage — offline (tried " + now.Format("15:04") + ")")
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
		systray.SetTooltip("GitHub Usage — all systems operational")
		mStatus.SetTitle("✓  All systems operational")
		mStatus.SetTooltip("githubstatus.com reports all systems operational")
		for _, mi := range mIncidents {
			mi.Hide()
		}
		return
	}

	systray.SetTooltip(fmt.Sprintf("GitHub Usage — %d ongoing incident(s)", len(ongoing)))

	status := fmt.Sprintf("●  %d ongoing incidents", len(ongoing))
	if len(ongoing) == 1 {
		status = "●  1 ongoing incident"
	}
	if len(ongoing) > maxIncidentItems {
		status += fmt.Sprintf("  (showing %d)", maxIncidentItems)
	}
	mStatus.SetTitle(status)
	mStatus.SetTooltip("githubstatus.com — click to open the status page")

	for i, mi := range mIncidents {
		if i >= len(ongoing) {
			mi.Hide()
			continue
		}
		inc := ongoing[i]

		// Prefer what the newest update actually says. The entry title is
		// generic ("Incident with Copilot") and says no more than the icon
		// already has; the update carries the detail.
		text := inc.LatestUpdate
		if text == "" {
			text = inc.Title
		}
		label := text
		if inc.LatestStatus != "" {
			label = inc.LatestStatus + " — " + text
		}
		mi.SetTitle("   " + truncate(label, incidentLabelMaxLen))

		// The tooltip carries the title and the whole untruncated update.
		tip := fmt.Sprintf("%s (%s)", inc.Title, inc.LatestStatus)
		if inc.LatestUpdate != "" {
			tip += "\n" + inc.LatestUpdate
		}
		mi.SetTooltip(tip)
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
		_ = notify.Banner("GitHub Usage", "All incidents resolved")
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

// truncate shortens s to at most n runes, marking the cut with an ellipsis. It
// prefers to break at a word boundary, since update messages are prose and
// stopping mid-word reads as a glitch — but only when that still keeps most of
// the line, so a single very long word can't shrink the row to nothing.
func truncate(s string, n int) string {
	r := []rune(s)
	if len(r) <= n {
		return s
	}
	if n < 1 {
		return ""
	}
	const trailing = " .,;:-–—"
	hard := string(r[:n-1])
	if i := strings.LastIndex(hard, " "); i >= 0 {
		// LastIndex gives a byte offset, but the "is this still most of the
		// line?" test has to count runes, or a line of multi-byte characters
		// clears the bar on byte length alone.
		if word := strings.TrimRight(hard[:i], trailing); len([]rune(word))*5 >= (n-1)*3 {
			return word + "…"
		}
	}
	return strings.TrimRight(hard, trailing) + "…"
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

func alertPercentFromEnv() int {
	if v := os.Getenv("ALERT_PERCENT"); v != "" {
		if n, err := strconv.Atoi(v); err == nil && n >= 0 && n <= 100 {
			return n
		}
	}
	return defaultAlertPercent
}
