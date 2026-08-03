// Package feed fetches GitHub's StatusPage incident Atom feed
// (https://www.githubstatus.com/history.atom) and decides which incidents are
// currently ongoing.
//
// The feed lists incidents newest-first. Each <entry>'s <content> holds the
// incident's update history, also newest-first, with each update tagged by a
// <strong>…</strong> status label (Investigating / Identified / Update /
// Monitoring / Resolved for incidents; Scheduled / In progress / Completed for
// maintenance). An incident is therefore "ongoing" iff the *first* status label
// in its content is not a terminal one (Resolved / Completed). Resolved
// incidents stay in the feed but carry "Resolved" as their topmost label, so a
// simple status filter yields exactly the currently-open set.
package feed

import (
	"context"
	"encoding/xml"
	"fmt"
	"html"
	"io"
	"net/http"
	"os"
	"regexp"
	"strings"
	"time"
)

// DefaultURL is GitHub's incident-history Atom feed.
const DefaultURL = "https://www.githubstatus.com/history.atom"

const userAgent = "github-usage/1.0 (+https://github.com)"

// Incident is a single incident parsed from one Atom <entry>.
type Incident struct {
	ID           string
	Title        string
	URL          string
	LatestStatus string // newest update's label, e.g. "Investigating", "Resolved"
	LatestUpdate string // newest update's message, without its label or separator
	Updated      time.Time
}

// IsOngoing reports whether this incident is currently open: its latest status
// is something other than a terminal label. An empty/unknown-but-blank status is
// treated as not-ongoing to avoid a false alarm on an unparseable entry.
func (i Incident) IsOngoing() bool {
	switch strings.ToLower(strings.TrimSpace(i.LatestStatus)) {
	case "", "resolved", "completed":
		return false
	default:
		return true
	}
}

// Ongoing returns only the incidents that are currently open.
func Ongoing(incidents []Incident) []Incident {
	out := make([]Incident, 0, len(incidents))
	for _, inc := range incidents {
		if inc.IsOngoing() {
			out = append(out, inc)
		}
	}
	return out
}

var httpClient = &http.Client{Timeout: 15 * time.Second}

// Fetch retrieves the feed body. It supports conditional requests: pass the
// previous ETag and, if the server replies 304 Not Modified, notModified is true
// and body is nil (reuse your last parse). A file:// URL is read straight off
// disk, which is handy for offline tests.
func Fetch(ctx context.Context, rawURL, etag string) (body []byte, newETag string, notModified bool, err error) {
	if strings.HasPrefix(rawURL, "file://") {
		b, ferr := os.ReadFile(strings.TrimPrefix(rawURL, "file://"))
		return b, "", false, ferr
	}

	req, err := http.NewRequestWithContext(ctx, http.MethodGet, rawURL, nil)
	if err != nil {
		return nil, "", false, err
	}
	req.Header.Set("User-Agent", userAgent)
	req.Header.Set("Accept", "application/atom+xml, application/xml;q=0.9")
	if etag != "" {
		req.Header.Set("If-None-Match", etag)
	}

	resp, err := httpClient.Do(req)
	if err != nil {
		return nil, "", false, err
	}
	defer resp.Body.Close()

	switch resp.StatusCode {
	case http.StatusNotModified:
		return nil, etag, true, nil
	case http.StatusOK:
		b, rerr := io.ReadAll(io.LimitReader(resp.Body, 8<<20)) // cap at 8 MiB
		if rerr != nil {
			return nil, "", false, rerr
		}
		return b, resp.Header.Get("ETag"), false, nil
	default:
		return nil, "", false, fmt.Errorf("feed returned HTTP %d", resp.StatusCode)
	}
}

type feedXML struct {
	Entries []entryXML `xml:"entry"`
}

type entryXML struct {
	ID      string    `xml:"id"`
	Title   string    `xml:"title"`
	Updated string    `xml:"updated"`
	Links   []linkXML `xml:"link"`
	Content string    `xml:"content"`
}

type linkXML struct {
	Href string `xml:"href,attr"`
	Rel  string `xml:"rel,attr"`
}

// An entry's content is one <p> per update, newest first, each shaped like:
//
//	<p> <small>Aug <var …> 3</var>, <var …>09:54</var> UTC</small><br>
//	    <strong>Update</strong> - We are experiencing degraded availability … </p>
//
// updateRe takes the first one: its <strong> label and the message that follows,
// up to the end of that paragraph. strongRe is the fallback for content whose
// paragraph never closes, where the label is still worth having on its own.
var (
	updateRe = regexp.MustCompile(`(?is)<strong[^>]*>\s*(.*?)\s*</strong>(.*?)</p>`)
	strongRe = regexp.MustCompile(`(?is)<strong[^>]*>\s*(.*?)\s*</strong>`)

	tagRe     = regexp.MustCompile(`(?s)<[^>]*>`)   // links and <var>s inside a message
	leadSepRe = regexp.MustCompile(`^\s*[-–—:]\s*`) // the " - " between label and message
	spaceRe   = regexp.MustCompile(`\s+`)           // newlines and runs of spaces in the feed
)

// latestUpdate pulls the newest update's status label and message out of an
// entry's content. Either may come back empty if the content is not shaped as
// expected; callers fall back to the entry title.
func latestUpdate(content string) (status, message string) {
	if m := updateRe.FindStringSubmatch(content); m != nil {
		return clean(m[1]), clean(leadSepRe.ReplaceAllString(tagRe.ReplaceAllString(m[2], ""), ""))
	}
	if m := strongRe.FindStringSubmatch(content); m != nil {
		return clean(m[1]), ""
	}
	return "", ""
}

// clean turns a snippet of feed markup into a single tidy line of text.
func clean(s string) string {
	return strings.TrimSpace(spaceRe.ReplaceAllString(html.UnescapeString(s), " "))
}

// ParseAtom parses the feed body into incidents (in feed order, newest-first).
func ParseAtom(body []byte) ([]Incident, error) {
	var f feedXML
	if err := xml.Unmarshal(body, &f); err != nil {
		return nil, fmt.Errorf("parse atom feed: %w", err)
	}

	incidents := make([]Incident, 0, len(f.Entries))
	for _, e := range f.Entries {
		status, message := latestUpdate(e.Content)
		updated, _ := time.Parse(time.RFC3339, strings.TrimSpace(e.Updated))
		incidents = append(incidents, Incident{
			ID:           strings.TrimSpace(e.ID),
			Title:        html.UnescapeString(strings.TrimSpace(e.Title)),
			URL:          pickLink(e.Links),
			LatestStatus: status,
			LatestUpdate: message,
			Updated:      updated,
		})
	}
	return incidents, nil
}

func pickLink(links []linkXML) string {
	for _, l := range links {
		if l.Rel == "alternate" || l.Rel == "" {
			return l.Href
		}
	}
	if len(links) > 0 {
		return links[0].Href
	}
	return ""
}
