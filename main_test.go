package main

import (
	"testing"
	"time"
)

func TestThousands(t *testing.T) {
	tests := []struct {
		in   int
		want string
	}{
		{0, "0"}, {7, "7"}, {855, "855"}, {1000, "1,000"}, {13500, "13,500"},
		{300000, "300,000"}, {1234567, "1,234,567"}, {-13500, "-13,500"},
	}
	for _, tt := range tests {
		if got := thousands(tt.in); got != tt.want {
			t.Errorf("thousands(%d) = %q, want %q", tt.in, got, tt.want)
		}
	}
}

func TestHumanizeReset(t *testing.T) {
	now := time.Now()
	tests := []struct {
		name string
		at   time.Time
		want string
	}{
		{"already past", now.Add(-time.Minute), "now"},
		{"within the hour", now.Add(25 * time.Minute), "in 25m"},
		{"later today", now.Add(3*time.Hour + 10*time.Minute), "in 3h 10m"},
		// The monthly premium-request window: a bare date, not a weekday+clock.
		{"a month out", time.Date(2099, 9, 1, 0, 0, 0, 0, time.UTC), "on " +
			time.Date(2099, 9, 1, 0, 0, 0, 0, time.UTC).Local().Format("2 Jan")},
	}
	for _, tt := range tests {
		if got := humanizeReset(tt.at); got != tt.want {
			t.Errorf("%s: humanizeReset = %q, want %q", tt.name, got, tt.want)
		}
	}
}

func TestTruncate(t *testing.T) {
	tests := []struct {
		name string
		in   string
		n    int
		want string
	}{
		{"short enough is untouched", "Investigating — all fine", 90, "Investigating — all fine"},
		{"exactly the limit", "abcde", 5, "abcde"},
		{
			name: "breaks at a word boundary, not mid-word",
			in:   "Update — We are experiencing degraded availability for chat & agent models in Copilot. Multiple models are impacted.",
			n:    90,
			want: "Update — We are experiencing degraded availability for chat & agent models in Copilot…",
		},
		{
			name: "an unbroken token breaks back to the previous word",
			in:   "Investigating — aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
			n:    20,
			want: "Investigating…",
		},
		{
			name: "a token too long to break around is cut hard, never emptied",
			in:   "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa bbb",
			n:    10,
			want: "aaaaaaaaa…",
		},
		{"n below one yields nothing", "anything", 0, ""},
		{"counts runes, not bytes", "æøåæøåæøå", 5, "æøåæ…"},
	}
	for _, tt := range tests {
		got := truncate(tt.in, tt.n)
		if got != tt.want {
			t.Errorf("%s:\n truncate(%q, %d)\n = %q\n want %q", tt.name, tt.in, tt.n, got, tt.want)
		}
		if r := []rune(got); len(r) > tt.n {
			t.Errorf("%s: result is %d runes, over the limit of %d", tt.name, len(r), tt.n)
		}
	}
}
