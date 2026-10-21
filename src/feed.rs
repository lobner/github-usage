//! Gets GitHub's StatusPage incident Atom feed
//! (<https://www.githubstatus.com/history.atom>) and decides which incidents are
//! ongoing now.
//!
//! The feed lists incidents newest-first. The `<content>` of each `<entry>`
//! holds the update history of the incident, also newest-first, and each update
//! has a `<strong>…</strong>` status label (Investigating / Identified / Update /
//! Monitoring / Resolved for incidents; Scheduled / In progress / Completed for
//! maintenance). An incident is therefore "ongoing" only if the *first* status
//! label in its content is not a terminal one (Resolved / Completed). Resolved
//! incidents stay in the feed with "Resolved" as their topmost label, so a
//! simple status filter gives exactly the set that is open now.

use std::sync::LazyLock;
use std::time::{Duration, Instant};

use chrono::{DateTime, FixedOffset};
use quick_xml::events::Event;
use quick_xml::{Reader, XmlVersion};
use regex::Regex;

use crate::http;

/// GitHub's incident-history Atom feed.
pub const DEFAULT_URL: &str = "https://www.githubstatus.com/history.atom";

const USER_AGENT: &str = "github-usage/1.0 (+https://github.com)";

const CLIENT_TIMEOUT: Duration = Duration::from_secs(15);
const BODY_LIMIT: u64 = 8 << 20; // 8 MiB

/// A single incident, parsed from one Atom `<entry>`.
#[derive(Debug, Clone, Default)]
#[allow(dead_code)] // updated is parsed for parity, not shown
pub struct Incident {
    pub id: String,
    pub title: String,
    pub url: String,
    /// The label of the newest update, for example "Investigating", "Resolved".
    pub latest_status: String,
    /// The message of the newest update, without its label or separator.
    pub latest_update: String,
    pub updated: Option<DateTime<FixedOffset>>,
}

impl Incident {
    /// Reports whether this incident is open now: its latest status is not a
    /// terminal label. An empty status counts as not ongoing, so an entry that
    /// cannot be parsed never raises a false alarm.
    pub fn is_ongoing(&self) -> bool {
        !matches!(
            self.latest_status.trim().to_lowercase().as_str(),
            "" | "resolved" | "completed"
        )
    }
}

/// Returns only the incidents that are open now.
pub fn ongoing(incidents: &[Incident]) -> Vec<Incident> {
    incidents
        .iter()
        .filter(|i| i.is_ongoing())
        .cloned()
        .collect()
}

/// The result of a fetch.
pub enum Fetched {
    /// A new body, with the ETag to send next time (empty if there was none).
    Body { body: Vec<u8>, etag: String },
    /// The server replied 304 Not Modified: keep the last parse.
    NotModified,
}

/// Gets the feed body. It sends a conditional request: pass the previous ETag,
/// and a 304 Not Modified reply gives `Fetched::NotModified`. A file:// URL is
/// read straight off disk, which is useful for offline tests.
pub fn fetch(url: &str, etag: &str, deadline: Instant) -> Result<Fetched, String> {
    if let Some(path) = url.strip_prefix("file://") {
        let body = std::fs::read(path).map_err(|e| http::go_open_error(path, &e))?;
        return Ok(Fetched::Body {
            body,
            etag: String::new(),
        });
    }

    let mut req = http::agent()
        .get(url)
        .header("User-Agent", USER_AGENT)
        .header("Accept", "application/atom+xml, application/xml;q=0.9");
    if !etag.is_empty() {
        req = req.header("If-None-Match", etag);
    }
    let mut resp = req
        .config()
        .timeout_global(Some(http::timeout(CLIENT_TIMEOUT, deadline)))
        .build()
        .call()
        .map_err(|e| format!("Get {url:?}: {e}"))?;

    match resp.status().as_u16() {
        304 => Ok(Fetched::NotModified),
        200 => {
            let new_etag = resp
                .headers()
                .get("ETag")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_string();
            let body =
                http::read_limited(resp.body_mut(), BODY_LIMIT).map_err(|e| e.to_string())?;
            Ok(Fetched::Body {
                body,
                etag: new_etag,
            })
        }
        code => Err(format!("feed returned HTTP {code}")),
    }
}

// The content of an entry is one <p> for each update, newest first, each in
// this shape:
//
//     <p> <small>Aug <var …> 3</var>, <var …>09:54</var> UTC</small><br>
//         <strong>Update</strong> - We are experiencing degraded availability … </p>
//
// UPDATE_RE takes the first one: its <strong> label and the message after it,
// up to the end of that paragraph. STRONG_RE is the fallback for content whose
// paragraph never closes, where the label alone is still useful.
//
// `[\t\n\f\r ]` is Go's RE2 `\s`, which is ASCII only. Rust's `\s` also matches
// Unicode spaces such as the no-break space that `&nbsp;` decodes to.
static UPDATE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)<strong[^>]*>[\t\n\f\r ]*(.*?)[\t\n\f\r ]*</strong>(.*?)</p>").unwrap()
});
static STRONG_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)<strong[^>]*>[\t\n\f\r ]*(.*?)[\t\n\f\r ]*</strong>").unwrap()
});
// Links and <var>s inside a message.
static TAG_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)<[^>]*>").unwrap());
// The " - " between the label and the message.
static LEAD_SEP_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[\t\n\f\r ]*[-–—:][\t\n\f\r ]*").unwrap());
// Newlines and runs of spaces in the feed.
static SPACE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[\t\n\f\r ]+").unwrap());

/// Takes the status label and the message of the newest update out of the
/// content of an entry. Either can be empty if the content does not have the
/// expected shape. Callers then use the entry title.
fn latest_update(content: &str) -> (String, String) {
    if let Some(m) = UPDATE_RE.captures(content) {
        let message = TAG_RE.replace_all(&m[2], "");
        let message = LEAD_SEP_RE.replace_all(&message, "");
        return (clean(&m[1]), clean(&message));
    }
    if let Some(m) = STRONG_RE.captures(content) {
        return (clean(&m[1]), String::new());
    }
    (String::new(), String::new())
}

/// Turns a piece of feed markup into a single tidy line of text.
fn clean(s: &str) -> String {
    let s = htmlize::unescape(s);
    SPACE_RE.replace_all(&s, " ").trim().to_string()
}

#[derive(Default)]
struct Entry {
    id: String,
    title: String,
    updated: String,
    content: String,
    /// (href, rel) of each <link>.
    links: Vec<(String, String)>,
}

/// The entry fields that hold text.
#[derive(Clone, Copy, PartialEq)]
enum Field {
    Id,
    Title,
    Updated,
    Content,
}

/// Parses the feed body into incidents, in feed order (newest first).
pub fn parse_atom(body: &[u8]) -> Result<Vec<Incident>, String> {
    let entries = parse_entries(body).map_err(|e| format!("parse atom feed: {e}"))?;
    Ok(entries
        .into_iter()
        .map(|e| {
            let (status, message) = latest_update(&e.content);
            Incident {
                id: e.id.trim().to_string(),
                title: htmlize::unescape(e.title.trim()).into_owned(),
                url: pick_link(&e.links),
                latest_status: status,
                latest_update: message,
                updated: DateTime::parse_from_rfc3339(e.updated.trim()).ok(),
            }
        })
        .collect())
}

/// Reads the `<entry>` children of the root element, as Go's `encoding/xml`
/// does for this shape: element names match without regard to namespace, and a
/// text field gets only the character data directly inside its element.
fn parse_entries(body: &[u8]) -> Result<Vec<Entry>, String> {
    let text = std::str::from_utf8(body).map_err(|e| e.to_string())?;
    let mut reader = Reader::from_str(text);

    let mut entries = Vec::new();
    let mut depth = 0usize; // the root element is at depth 1
    let mut entry: Option<Entry> = None;
    let mut field: Option<(Field, usize)> = None; // the field, and its depth
    let mut saw_root = false;

    let field_for = |name: &str| match name {
        "id" => Some(Field::Id),
        "title" => Some(Field::Title),
        "updated" => Some(Field::Updated),
        "content" => Some(Field::Content),
        _ => None,
    };

    loop {
        let event = reader.read_event().map_err(|e| e.to_string())?;
        match event {
            Event::Start(ref e) | Event::Empty(ref e) => {
                let empty = matches!(event, Event::Empty(_));
                depth += 1;
                saw_root = true;
                let name = e.local_name();
                let name = name.as_ref();
                if depth == 2 && name == "entry" {
                    entry = Some(Entry::default());
                } else if depth == 3
                    && let Some(en) = entry.as_mut()
                {
                    if name == "link" {
                        let mut href = String::new();
                        let mut rel = String::new();
                        for a in e.attributes() {
                            let a = a.map_err(|e| e.to_string())?;
                            let value = a
                                .normalized_value(XmlVersion::Implicit1_0)
                                .map_err(|e| e.to_string())?;
                            match a.key.local_name().as_ref() {
                                "href" => href = value.into_owned(),
                                "rel" => rel = value.into_owned(),
                                _ => {}
                            }
                        }
                        en.links.push((href, rel));
                    } else if let Some(f) = field_for(name) {
                        // A repeated element replaces the earlier value, as in Go.
                        *field_text(en, f) = String::new();
                        field = Some((f, depth));
                    }
                }
                if empty {
                    end_element(&mut depth, &mut entry, &mut entries, &mut field);
                }
            }
            Event::End(_) => end_element(&mut depth, &mut entry, &mut entries, &mut field),
            Event::Text(ref t) => {
                if let (Some((f, d)), Some(en)) = (field, entry.as_mut())
                    && d == depth
                {
                    field_text(en, f).push_str(&t.xml10_content());
                }
            }
            Event::CData(ref t) => {
                if let (Some((f, d)), Some(en)) = (field, entry.as_mut())
                    && d == depth
                {
                    field_text(en, f).push_str(&t.xml10_content());
                }
            }
            Event::GeneralRef(ref r) => {
                let resolved = match r.resolve_char_ref().map_err(|e| e.to_string())? {
                    Some(c) => c.to_string(),
                    None => quick_xml::escape::resolve_predefined_entity(r)
                        .ok_or_else(|| format!("invalid character entity &{};", &**r))?
                        .to_string(),
                };
                if let (Some((f, d)), Some(en)) = (field, entry.as_mut())
                    && d == depth
                {
                    field_text(en, f).push_str(&resolved);
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if !saw_root {
        return Err("EOF".into());
    }
    Ok(entries)
}

fn end_element(
    depth: &mut usize,
    entry: &mut Option<Entry>,
    entries: &mut Vec<Entry>,
    field: &mut Option<(Field, usize)>,
) {
    if field.is_some_and(|(_, d)| d == *depth) {
        *field = None;
    }
    if *depth == 2
        && let Some(e) = entry.take()
    {
        entries.push(e);
    }
    *depth = depth.saturating_sub(1);
}

fn field_text(e: &mut Entry, f: Field) -> &mut String {
    match f {
        Field::Id => &mut e.id,
        Field::Title => &mut e.title,
        Field::Updated => &mut e.updated,
        Field::Content => &mut e.content,
    }
}

fn pick_link(links: &[(String, String)]) -> String {
    links
        .iter()
        .find(|(_, rel)| rel == "alternate" || rel.is_empty())
        .or(links.first())
        .map(|(href, _)| href.clone())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mirrors the real feed: <content> is HTML-escaped (&lt;strong&gt;…), so this
    /// also tests the entity decoding. It contains, in order:
    ///  1. an ongoing incident   (latest label: Update)
    ///  2. a resolved incident   (latest label: Resolved)
    ///  3. completed maintenance (latest label: Completed)
    ///  4. ongoing maintenance   (latest label: In progress)
    const SAMPLE_ATOM: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <title>GitHub Status - Incident History</title>
  <updated>2026-06-08T09:08:58Z</updated>
  <entry>
    <id>tag:www.githubstatus.com,2005:Incident/71hv2q6tk693</id>
    <published>2026-06-08T09:05:44Z</published>
    <updated>2026-06-08T09:08:58Z</updated>
    <link rel="alternate" type="text/html" href="https://www.githubstatus.com/incidents/71hv2q6tk693"/>
    <title>Disruption with Claude Opus 4.7</title>
    <content type="html">&lt;p&gt;&lt;small&gt;Jun  8, 09:08 UTC&lt;/small&gt;&lt;br&gt;&lt;strong&gt;Update&lt;/strong&gt; - Degraded availability.&lt;/p&gt;&lt;p&gt;&lt;small&gt;Jun  8, 09:05 UTC&lt;/small&gt;&lt;br&gt;&lt;strong&gt;Investigating&lt;/strong&gt; - We are investigating.&lt;/p&gt;</content>
  </entry>
  <entry>
    <id>tag:www.githubstatus.com,2005:Incident/oldresolved</id>
    <updated>2026-06-01T12:00:00Z</updated>
    <link rel="alternate" type="text/html" href="https://www.githubstatus.com/incidents/oldresolved"/>
    <title>Old resolved incident</title>
    <content type="html">&lt;p&gt;&lt;strong&gt;Resolved&lt;/strong&gt; - This incident has been resolved.&lt;/p&gt;&lt;p&gt;&lt;strong&gt;Investigating&lt;/strong&gt; - We are investigating.&lt;/p&gt;</content>
  </entry>
  <entry>
    <id>tag:www.githubstatus.com,2005:Incident/maintdone</id>
    <updated>2026-05-20T12:00:00Z</updated>
    <link rel="alternate" type="text/html" href="https://www.githubstatus.com/incidents/maintdone"/>
    <title>Completed maintenance</title>
    <content type="html">&lt;p&gt;&lt;strong&gt;Completed&lt;/strong&gt; - The scheduled maintenance has been completed.&lt;/p&gt;</content>
  </entry>
  <entry>
    <id>tag:www.githubstatus.com,2005:Incident/maintinprog</id>
    <updated>2026-06-08T08:00:00Z</updated>
    <link rel="alternate" type="text/html" href="https://www.githubstatus.com/incidents/maintinprog"/>
    <title>Ongoing maintenance</title>
    <content type="html">&lt;p&gt;&lt;strong&gt;In progress&lt;/strong&gt; - Scheduled maintenance is currently in progress.&lt;/p&gt;</content>
  </entry>
</feed>"#;

    #[test]
    fn parse_atom_entries() {
        let incs = parse_atom(SAMPLE_ATOM.as_bytes()).expect("parse_atom");
        assert_eq!(incs.len(), 4);
        let first = &incs[0];
        assert_eq!(first.title, "Disruption with Claude Opus 4.7");
        assert_eq!(first.latest_status, "Update");
        assert_eq!(first.latest_update, "Degraded availability.");
        assert_eq!(
            first.url,
            "https://www.githubstatus.com/incidents/71hv2q6tk693"
        );
        assert_eq!(
            first.id,
            "tag:www.githubstatus.com,2005:Incident/71hv2q6tk693"
        );
        assert!(first.updated.is_some(), "updated[0] not parsed");
    }

    #[test]
    fn ongoing_filters_terminal_labels() {
        let incs = parse_atom(SAMPLE_ATOM.as_bytes()).expect("parse_atom");
        let titles: Vec<String> = ongoing(&incs).into_iter().map(|i| i.title).collect();
        assert_eq!(
            titles,
            ["Disruption with Claude Opus 4.7", "Ongoing maintenance"]
        );
    }

    #[test]
    fn is_ongoing_labels() {
        let cases = [
            ("Investigating", true),
            ("Identified", true),
            ("Update", true),
            ("Monitoring", true),
            ("In progress", true),
            ("Verifying", true),
            ("Resolved", false),
            ("resolved", false),     // case-insensitive
            ("  Resolved  ", false), // trimmed
            ("Completed", false),
            ("", false), // cannot be parsed → no false alarm
        ];
        for (status, want) in cases {
            let inc = Incident {
                latest_status: status.into(),
                ..Default::default()
            };
            assert_eq!(inc.is_ongoing(), want, "{status:?}");
        }
    }

    /// The real shape of GitHub's feed content: one <p> for each update newest
    /// first, with <var> tags in the timestamp, and entities that the XML
    /// decoder has already turned back into markup.
    #[test]
    fn latest_update_real_shape() {
        let content = "<p> <small>Aug <var data-var='date'> 3</var>, <var data-var='time'>09:54</var> UTC</small><br> \
            <strong>Update</strong> - We are experiencing degraded availability for chat & agent models in Copilot. \
            Multiple models are impacted. </p> <p> <small>Aug <var data-var='date'> 3</var>, \
            <var data-var='time'>09:53</var> UTC</small><br> <strong>Investigating</strong> - \
            We are investigating reports of degraded performance for Copilot </p>";
        let (status, message) = latest_update(content);
        assert_eq!(status, "Update");
        assert_eq!(
            message,
            "We are experiencing degraded availability for chat & agent models in Copilot. Multiple models are impacted."
        );
    }

    #[test]
    fn latest_update_shapes() {
        let cases = [
            (
                "link inside the message",
                r#"<p><strong>Scheduled</strong> - See <a href="https://x.test">the notice</a> for details.</p>"#,
                "Scheduled",
                "See the notice for details.",
            ),
            (
                "no separator after the label",
                "<p><strong>Resolved</strong> This incident has been resolved.</p>",
                "Resolved",
                "This incident has been resolved.",
            ),
            (
                "en dash separator",
                "<p><strong>Monitoring</strong> – We are monitoring the fix.</p>",
                "Monitoring",
                "We are monitoring the fix.",
            ),
            (
                "unclosed paragraph falls back to the label alone",
                "<p><strong>Investigating</strong> - truncated feed",
                "Investigating",
                "",
            ),
            ("no label at all", "<p>nothing useful here</p>", "", ""),
        ];
        for (name, content, status, message) in cases {
            let got = latest_update(content);
            assert_eq!(got, (status.to_string(), message.to_string()), "{name}");
        }
    }

    /// Go's `\s` is ASCII only, so a no-break space stays in the text.
    #[test]
    fn clean_keeps_no_break_space() {
        assert_eq!(clean("a&nbsp;b \n c"), "a\u{a0}b c");
        assert_eq!(clean("Tom &amp; Jerry&#39;s"), "Tom & Jerry's");
    }

    #[test]
    fn parse_atom_rejects_garbage() {
        assert!(parse_atom(b"").is_err());
        assert!(parse_atom(b"<feed><entry></feed>").is_err());
    }

    /// Diagnostic: gets the real GitHub feed. Opt-in, because it needs network
    /// access:
    ///
    ///     cargo test live_feed -- --ignored --nocapture
    #[test]
    #[ignore]
    fn live_feed() {
        let deadline = Instant::now() + Duration::from_secs(20);
        let Fetched::Body { body, etag } = fetch(DEFAULT_URL, "", deadline).expect("fetch") else {
            panic!("a request without an ETag cannot be Not Modified");
        };
        let incs = parse_atom(&body).expect("parse");
        let open = ongoing(&incs);
        println!(
            "parsed {} entries, {} ongoing (etag={etag:?})",
            incs.len(),
            open.len()
        );
        for i in &open {
            println!("ONGOING: {:<14} | {} | {}", i.latest_status, i.title, i.url);
        }
        assert!(
            !incs.is_empty(),
            "expected at least one entry in the live feed"
        );
    }
}
