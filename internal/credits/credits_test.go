package credits

import (
	"errors"
	"os"
	"path/filepath"
	"strconv"
	"testing"
	"time"
)

// realShape mirrors what copilot_internal/user actually returns, including the
// fields we ignore, so a stray key or an unlimited sibling quota can't confuse
// the parser.
const realShape = `{
  "login": "octocat",
  "copilot_plan": "business",
  "quota_reset_date": "2026-09-01",
  "quota_reset_date_utc": "2026-09-01T00:00:00.000Z",
  "token_based_billing": true,
  "quota_snapshots": {
    "chat": {"percent_remaining": 100.0, "unlimited": true, "has_quota": true, "entitlement": 0},
    "completions": {"percent_remaining": 100.0, "unlimited": true, "has_quota": true, "entitlement": 0},
    "premium_interactions": {
      "overage_count": 0, "overage_permitted": true, "percent_remaining": 93.6,
      "quota_id": "premium_interactions", "quota_remaining": 12645.2, "unlimited": false,
      "has_quota": true, "credits_used": 299, "remaining": 12645, "entitlement": 13500
    }
  }
}`

func TestParse(t *testing.T) {
	q, err := parse([]byte(realShape))
	if err != nil {
		t.Fatalf("parse: %v", err)
	}

	if q.PercentUsed != 6 { // 100 - 93.6, rounded
		t.Errorf("PercentUsed = %d, want 6", q.PercentUsed)
	}
	if q.Entitlement != 13500 {
		t.Errorf("Entitlement = %d, want 13500", q.Entitlement)
	}
	if q.Remaining != 12645 {
		t.Errorf("Remaining = %d, want 12645", q.Remaining)
	}
	if q.Used != 855 { // entitlement - remaining, not the credits_used field
		t.Errorf("Used = %d, want 855", q.Used)
	}
	if q.Plan != "business" {
		t.Errorf("Plan = %q, want %q", q.Plan, "business")
	}
	want := time.Date(2026, 9, 1, 0, 0, 0, 0, time.UTC)
	if !q.ResetsAt.Equal(want) {
		t.Errorf("ResetsAt = %s, want %s", q.ResetsAt, want)
	}
}

func TestParseNoQuota(t *testing.T) {
	tests := []struct {
		name, body string
	}{
		{"unlimited plan", `{"quota_snapshots":{"premium_interactions":{"unlimited":true,"has_quota":true,"entitlement":300}}}`},
		{"has_quota false", `{"quota_snapshots":{"premium_interactions":{"has_quota":false,"entitlement":300}}}`},
		{"zero entitlement", `{"quota_snapshots":{"premium_interactions":{"has_quota":true,"entitlement":0}}}`},
		{"no snapshots at all", `{"login":"octocat"}`},
	}
	for _, tt := range tests {
		if _, err := parse([]byte(tt.body)); !errors.Is(err, ErrNoQuota) {
			t.Errorf("%s: err = %v, want ErrNoQuota", tt.name, err)
		}
	}
}

func TestParseGarbage(t *testing.T) {
	if _, err := parse([]byte("<html>not json</html>")); err == nil {
		t.Error("parse(garbage) = nil error, want a parse failure")
	}
}

// TestParseClampsPercent guards the menu-bar string: whatever the endpoint says,
// the percentage has to stay in 0–100.
func TestParseClampsPercent(t *testing.T) {
	tests := []struct {
		remaining float64
		want      int
	}{
		{100, 0}, {0, 100}, {50, 50}, {93.6, 6}, {0.4, 100}, {120, 0}, {-20, 100},
	}
	for _, tt := range tests {
		body := `{"quota_snapshots":{"premium_interactions":{"has_quota":true,"entitlement":100,` +
			`"quota_remaining":1,"percent_remaining":` + strconv.FormatFloat(tt.remaining, 'f', -1, 64) + `}}}`
		q, err := parse([]byte(body))
		if err != nil {
			t.Fatalf("percent_remaining %v: %v", tt.remaining, err)
		}
		if q.PercentUsed != tt.want {
			t.Errorf("percent_remaining %v: PercentUsed = %d, want %d", tt.remaining, q.PercentUsed, tt.want)
		}
	}
}

func TestResetTimeFallsBackToDateOnly(t *testing.T) {
	q, err := parse([]byte(`{"quota_reset_date":"2026-09-01","quota_snapshots":{"premium_interactions":
		{"has_quota":true,"entitlement":100,"quota_remaining":50,"percent_remaining":50}}}`))
	if err != nil {
		t.Fatal(err)
	}
	if want := time.Date(2026, 9, 1, 0, 0, 0, 0, time.UTC); !q.ResetsAt.Equal(want) {
		t.Errorf("ResetsAt = %s, want %s", q.ResetsAt, want)
	}
}

// TestGhPathIgnoresPATH is the regression test for the bug that made the built
// app report "no GitHub token" while `gh auth token` worked in a terminal: a
// Finder-launched app inherits a minimal PATH that excludes Homebrew, so looking
// gh up by name alone fails.
func TestGhPathIgnoresPATH(t *testing.T) {
	t.Setenv("PATH", "/nonexistent")

	got := ghPath()
	if got == "" {
		t.Skip("gh is not installed at any known location on this machine")
	}
	if _, err := os.Stat(got); err != nil {
		t.Errorf("ghPath() = %q, which does not exist: %v", got, err)
	}
	if !filepath.IsAbs(got) {
		t.Errorf("ghPath() = %q, want an absolute path", got)
	}
}

func TestOauthTokenFromYAML(t *testing.T) {
	tests := []struct {
		name, in, want string
	}{
		{
			name: "gh's file layout",
			in: "github.com:\n    users:\n        octocat:\n            oauth_token: gho_EXAMPLE\n" +
				"    git_protocol: ssh\n    user: octocat\n    oauth_token: gho_EXAMPLE\n",
			want: "gho_EXAMPLE",
		},
		{"quoted", "github.com:\n    oauth_token: \"gho_QUOTED\"\n", "gho_QUOTED"},
		{"single quoted", "github.com:\n    oauth_token: 'gho_SINGLE'\n", "gho_SINGLE"},
		{"keyring storage, no token in the file", "github.com:\n    git_protocol: ssh\n    user: octocat\n", ""},
		{"empty", "", ""},
		{"lookalike key is not matched", "github.com:\n    not_oauth_token: nope\n", ""},
	}
	for _, tt := range tests {
		if got := oauthTokenFromYAML(tt.in); got != tt.want {
			t.Errorf("%s: oauthTokenFromYAML() = %q, want %q", tt.name, got, tt.want)
		}
	}
}
