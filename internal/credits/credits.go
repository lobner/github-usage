// Package credits reads your Copilot premium-request allowance from GitHub's
// internal endpoint, https://api.github.com/copilot_internal/user — the same one
// the editor extensions call to show "premium requests remaining". There is no
// documented API for this, so the shape below is what the endpoint actually
// returns rather than anything contractual.
//
// It is read-only about credentials: it uses the token the GitHub CLI has already
// stored (via `gh auth token`, which handles keyring and file storage itself), or
// GH_TOKEN / GITHUB_TOKEN if either is set. It never writes, refreshes, or logs a
// token.
package credits

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"time"
)

// DefaultURL is GitHub's internal Copilot entitlement endpoint.
const DefaultURL = "https://api.github.com/copilot_internal/user"

const userAgent = "github-status-tracker/1.0 (+https://github.com)"

// keychainService is the item the GitHub CLI creates when it stores credentials
// in the keyring rather than in a file.
const keychainService = "gh:github.com"

// ErrNoToken means the GitHub CLI is installed but has no token stored for us to
// borrow, which is a setup problem rather than a transient failure.
var ErrNoToken = errors.New("no GitHub token: run `gh auth login`, or set GH_TOKEN")

// ErrNoCLI means the gh binary was not found anywhere we look, so there is no
// stored token to read in the first place.
var ErrNoCLI = errors.New("GitHub CLI not found: install gh, or set GH_TOKEN")

// ErrNoQuota means the account has no premium-request quota to report — an
// unlimited or quota-less plan, rather than a failure.
var ErrNoQuota = errors.New("no premium-request quota on this plan")

// Quota is the premium-request allowance for the current billing period.
type Quota struct {
	PercentUsed int       // 0–100, rounded; what the menu bar shows
	Used        int       // credits consumed so far (entitlement - remaining)
	Remaining   int       // credits left
	Entitlement int       // credits in the period
	Overage     int       // credits used beyond the entitlement
	ResetsAt    time.Time // start of the next period; zero if unknown
	Plan        string    // e.g. "business", for the tooltip
}

// apiResponse is the subset of copilot_internal/user we use.
type apiResponse struct {
	CopilotPlan       string `json:"copilot_plan"`
	QuotaResetDateUTC string `json:"quota_reset_date_utc"`
	QuotaResetDate    string `json:"quota_reset_date"`
	QuotaSnapshots    struct {
		Premium struct {
			PercentRemaining float64 `json:"percent_remaining"`
			Remaining        float64 `json:"quota_remaining"`
			Entitlement      float64 `json:"entitlement"`
			OverageCount     float64 `json:"overage_count"`
			Unlimited        bool    `json:"unlimited"`
			HasQuota         bool    `json:"has_quota"`
		} `json:"premium_interactions"`
	} `json:"quota_snapshots"`
}

var httpClient = &http.Client{Timeout: 20 * time.Second}

// Fetch reads the token and queries the endpoint. A URL override (for offline
// testing) may be passed; empty means DefaultURL, and a file:// URL is read
// straight off disk.
func Fetch(ctx context.Context, rawURL string) (Quota, error) {
	if rawURL == "" {
		rawURL = DefaultURL
	}

	body, err := get(ctx, rawURL)
	if err != nil {
		return Quota{}, err
	}
	return parse(body)
}

func get(ctx context.Context, rawURL string) ([]byte, error) {
	if strings.HasPrefix(rawURL, "file://") {
		return os.ReadFile(strings.TrimPrefix(rawURL, "file://"))
	}

	token, err := Token(ctx)
	if err != nil {
		return nil, err
	}

	req, err := http.NewRequestWithContext(ctx, http.MethodGet, rawURL, nil)
	if err != nil {
		return nil, err
	}
	req.Header.Set("Authorization", "token "+token)
	req.Header.Set("Accept", "application/json")
	req.Header.Set("User-Agent", userAgent)

	resp, err := httpClient.Do(req)
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()

	switch resp.StatusCode {
	case http.StatusOK:
		return io.ReadAll(io.LimitReader(resp.Body, 1<<20))
	case http.StatusUnauthorized, http.StatusForbidden:
		// Deliberately says nothing about the token itself.
		return nil, fmt.Errorf("GitHub rejected the token (HTTP %d) — try `gh auth login`", resp.StatusCode)
	case http.StatusNotFound:
		return nil, ErrNoQuota
	default:
		return nil, fmt.Errorf("copilot endpoint returned HTTP %d", resp.StatusCode)
	}
}

func parse(body []byte) (Quota, error) {
	var r apiResponse
	if err := json.Unmarshal(body, &r); err != nil {
		return Quota{}, fmt.Errorf("parse copilot response: %w", err)
	}

	p := r.QuotaSnapshots.Premium
	if p.Unlimited || !p.HasQuota || p.Entitlement <= 0 {
		return Quota{}, ErrNoQuota
	}

	// percent_remaining is the number GitHub's own UI shows, so derive from it
	// rather than recomputing, and report it the way claude-usage does: used.
	used := 100 - int(p.PercentRemaining+0.5)
	if used < 0 {
		used = 0
	}
	if used > 100 {
		used = 100
	}

	return Quota{
		PercentUsed: used,
		Used:        int(p.Entitlement - p.Remaining + 0.5),
		Remaining:   int(p.Remaining + 0.5),
		Entitlement: int(p.Entitlement + 0.5),
		Overage:     int(p.OverageCount + 0.5),
		ResetsAt:    resetTime(r),
		Plan:        r.CopilotPlan,
	}, nil
}

// resetTime prefers the full UTC timestamp and falls back to the date-only field.
func resetTime(r apiResponse) time.Time {
	if t, err := time.Parse(time.RFC3339, strings.TrimSpace(r.QuotaResetDateUTC)); err == nil {
		return t
	}
	if t, err := time.Parse("2006-01-02", strings.TrimSpace(r.QuotaResetDate)); err == nil {
		return t
	}
	return time.Time{}
}

// Token returns the GitHub token to authenticate with: GH_TOKEN or GITHUB_TOKEN
// if set, otherwise whatever the GitHub CLI has stored. The value is never
// logged or surfaced in the UI.
//
// The CLI's own store is read three ways, because a menu-bar app launched by
// Finder or at login inherits a minimal PATH — /usr/bin:/bin and friends — which
// does not include Homebrew, where gh usually lives. Asking for "gh" by name
// therefore works in a terminal and fails in the built app.
func Token(ctx context.Context) (string, error) {
	for _, key := range []string{"GH_TOKEN", "GITHUB_TOKEN"} {
		if v := strings.TrimSpace(os.Getenv(key)); v != "" {
			return v, nil
		}
	}
	// Prefer the CLI itself: it knows where it put the token, and reading it
	// through gh raises no keychain prompt because gh owns that item.
	if t := tokenFromCLI(ctx); t != "" {
		return t, nil
	}
	// Then the stores gh writes to, for when the binary can't be found at all.
	if t := tokenFromKeychain(ctx); t != "" {
		return t, nil
	}
	if t := tokenFromHostsFile(); t != "" {
		return t, nil
	}
	// Say which of the two situations this is: "run gh auth login" is unhelpful
	// advice for someone who has already done exactly that.
	if ghPath() == "" {
		return "", ErrNoCLI
	}
	return "", ErrNoToken
}

// tokenFromCLI runs `gh auth token`, which reads the keyring or the hosts file as
// appropriate and keeps us out of the business of guessing which.
func tokenFromCLI(ctx context.Context) string {
	gh := ghPath()
	if gh == "" {
		return ""
	}
	out, err := exec.CommandContext(ctx, gh, "auth", "token").Output()
	if err != nil {
		return ""
	}
	return strings.TrimSpace(string(out))
}

// ghPath locates the gh binary without trusting PATH. The usual Homebrew prefixes
// come first because that is how gh is nearly always installed on macOS.
func ghPath() string {
	candidates := []string{
		"/opt/homebrew/bin/gh", // Homebrew on Apple silicon
		"/usr/local/bin/gh",    // Homebrew on Intel, or a manual install
		"/usr/bin/gh",
		"/opt/local/bin/gh", // MacPorts
	}
	if home, err := os.UserHomeDir(); err == nil {
		candidates = append(candidates, filepath.Join(home, ".local", "bin", "gh"))
	}
	for _, p := range candidates {
		if fi, err := os.Stat(p); err == nil && !fi.IsDir() && fi.Mode()&0o111 != 0 {
			return p
		}
	}
	// Last resort: a PATH lookup, which is what actually works when the app is
	// started from a shell rather than by Finder.
	if p, err := exec.LookPath("gh"); err == nil {
		return p
	}
	return ""
}

// tokenFromKeychain reads the item gh creates when it stores credentials in the
// keyring. /usr/bin/security is addressed absolutely for the same PATH reason.
func tokenFromKeychain(ctx context.Context) string {
	out, err := exec.CommandContext(ctx, "/usr/bin/security",
		"find-generic-password", "-s", keychainService, "-w").Output()
	if err != nil {
		return ""
	}
	return strings.TrimSpace(string(out))
}

// tokenFromHostsFile reads gh's config for the case where it stores the token in
// a file rather than the keyring. The file is small and the shape is stable, so a
// line scan avoids taking on a YAML dependency for one field.
func tokenFromHostsFile() string {
	home, err := os.UserHomeDir()
	if err != nil {
		return ""
	}
	data, err := os.ReadFile(filepath.Join(home, ".config", "gh", "hosts.yml"))
	if err != nil {
		return ""
	}
	return oauthTokenFromYAML(string(data))
}

func oauthTokenFromYAML(s string) string {
	for _, line := range strings.Split(s, "\n") {
		key, value, found := strings.Cut(strings.TrimSpace(line), ":")
		if !found || strings.TrimSpace(key) != "oauth_token" {
			continue
		}
		return strings.Trim(strings.TrimSpace(value), `"'`)
	}
	return ""
}
