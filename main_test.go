package main

import "testing"

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
