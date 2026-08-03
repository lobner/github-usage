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
	"strings"
	"time"
)

// DefaultURL is GitHub's internal Copilot entitlement endpoint.
const DefaultURL = "https://api.github.com/copilot_internal/user"

const userAgent = "github-status-tracker/1.0 (+https://github.com)"

// ErrNoToken means we could not find a GitHub token to authenticate with, which
// is a setup problem rather than a transient failure.
var ErrNoToken = errors.New("no GitHub token: set GH_TOKEN or run `gh auth login`")

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
func Token(ctx context.Context) (string, error) {
	for _, key := range []string{"GH_TOKEN", "GITHUB_TOKEN"} {
		if v := strings.TrimSpace(os.Getenv(key)); v != "" {
			return v, nil
		}
	}

	// `gh auth token` reads the keyring or hosts.yml as appropriate, which keeps
	// this package out of the business of parsing credential stores.
	out, err := exec.CommandContext(ctx, "gh", "auth", "token").Output()
	if err != nil {
		return "", ErrNoToken
	}
	if token := strings.TrimSpace(string(out)); token != "" {
		return token, nil
	}
	return "", ErrNoToken
}
