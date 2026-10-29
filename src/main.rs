//! GitHub Usage is a macOS menu-bar app with two jobs.
//!
//! It shows how much of your Copilot premium-request allowance is spent, as a
//! percentage in the menu-bar title, with the credit counts and the reset date
//! in the menu (see credits.rs).
//!
//! It also watches GitHub's incident feed. When an incident appears, a dot in
//! front of the percentage blinks red and a Notification Centre banner is
//! posted. Opening the menu acknowledges the incident: the blinking stops, the
//! dot stays solid, and the newest update of each incident can be read (and
//! clicked) in the menu itself.
//!
//! On the first launch from its .app bundle it offers to open at login, and
//! registers itself as a login item if the user agrees (see login.rs).
//!
//! Environment overrides:
//!
//!     FEED_URL       feed to poll (default https://www.githubstatus.com/history.atom;
//!                    a file:// URL works for offline testing)
//!     COPILOT_URL    Copilot entitlement endpoint (default api.github.com/copilot_internal/user;
//!                    a file:// URL works for offline testing)
//!     POLL_SECONDS   polling interval in seconds (default 60, minimum 10)
//!     ALERT_PERCENT  banner when premium-request usage first reaches this % (default 80, 0=off)
//!     GH_TOKEN       GitHub token to use instead of the GitHub CLI's stored one
//!                    (GITHUB_TOKEN also works)

mod credits;
mod feed;
mod http;
mod icon;
mod json;
mod login;
mod notify;
mod tray;

use std::collections::HashSet;
use std::process::{Command, Stdio};
use std::sync::{LazyLock, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use chrono::{DateTime, Local, TimeDelta, Utc};
use crossbeam_channel::{Receiver, Sender, bounded, select, tick};

use tray::MenuItem;

const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(60);
const DEFAULT_ALERT_PERCENT: i64 = 80;
const MAX_INCIDENT_ITEMS: usize = 6;
const BLINK_INTERVAL: Duration = Duration::from_millis(700);
const STATUS_PAGE_URL: &str = "https://www.githubstatus.com";
// Where premium-request use is shown for a personal account. A seat billed
// through an organisation reports its overage under the billing of that org
// instead, but the user's own use still appears here.
const USAGE_PAGE_URL: &str = "https://github.com/settings/billing/usage";

const APP_NAME: &str = "GitHub Usage";
const REPO_URL: &str = "https://github.com/lobner/github-usage";

// Update messages can be long. After this many characters the row is cut and
// the tooltip carries the rest, so the menu does not grow to the width of a
// paragraph.
const INCIDENT_LABEL_MAX_LEN: usize = 90;

// Build stamps, set by build/make-app.sh through the environment of `cargo
// build`. A plain `cargo build` or `cargo run` leaves them at these defaults,
// which is what "dev" in the About row means.
const VERSION: &str = match option_env!("GITHUBUSAGE_VERSION") {
    Some(v) => v,
    None => "dev",
};
const COMMIT: &str = match option_env!("GITHUBUSAGE_COMMIT") {
    Some(v) => v,
    None => "",
};
const BUILD_DATE: &str = match option_env!("GITHUBUSAGE_BUILD_DATE") {
    Some(v) => v,
    None => "",
};

/// The environment overrides, read once at startup.
struct Config {
    feed_url: String,
    copilot_url: String,
    poll_interval: Duration,
    alert_percent: i64,
}

static CONFIG: LazyLock<Config> = LazyLock::new(|| Config {
    feed_url: env_or("FEED_URL", feed::DEFAULT_URL),
    copilot_url: env_or("COPILOT_URL", credits::DEFAULT_URL),
    poll_interval: poll_interval_from_env(),
    alert_percent: alert_percent_from_env(),
});

/// The rows that change after startup.
struct Menu {
    credits: MenuItem,
    credits_count: MenuItem,
    credits_reset: MenuItem,
    status: MenuItem,
    incidents: Vec<MenuItem>,
    last_check: MenuItem,
    login: MenuItem,
}

static MENU: OnceLock<Menu> = OnceLock::new();

fn menu() -> &'static Menu {
    MENU.get().expect("the menu is built in on_ready")
}

type Chan<T> = LazyLock<(Sender<T>, Receiver<T>)>;

static REFRESH_NOW: Chan<()> = LazyLock::new(|| bounded(1));

// The icon state, owned by icon_loop. The poll thread reports what the feed
// says. Opening the menu acknowledges an alert.
static ICON_UPDATES: Chan<IconUpdate> = LazyLock::new(|| bounded(1));
static ICON_ACK: Chan<()> = LazyLock::new(|| bounded(1));

// Shared between the poll thread (writer) and the click threads (readers).
static CURRENT_URLS: Mutex<[String; MAX_INCIDENT_ITEMS]> =
    Mutex::new([const { String::new() }; MAX_INCIDENT_ITEMS]);

fn main() {
    tray::run(on_ready);
}

fn on_ready() {
    tray::set_title("…");
    tray::set_tooltip("GitHub Usage — checking…");

    let credits = tray::add_item("Premium requests: …", "Copilot premium-request allowance");
    credits.disable();
    let credits_count = tray::add_item("", "");
    credits_count.disable();
    credits_count.hide();
    let credits_reset = tray::add_item("", "");
    credits_reset.disable();
    credits_reset.hide();
    tray::add_separator();

    // Clickable, as in claude-usage: the headline row is how you get to the
    // status page, so there is no separate menu item for it.
    let status = tray::add_item("Service status: checking…", "Open githubstatus.com");

    // A fixed pool of incident rows, shown, hidden and relabelled on each poll.
    let mut incidents = Vec::with_capacity(MAX_INCIDENT_ITEMS);
    for idx in 0..MAX_INCIDENT_ITEMS {
        let mi = tray::add_item("", "");
        mi.hide();
        on_click(&mi, move || open_url(&incident_url(idx)));
        incidents.push(mi);
    }
    tray::add_separator();

    let open_page = tray::add_item("Open usage page", "Open github.com/settings/billing/usage");
    let refresh = tray::add_item("Refresh now", "Check the feed immediately");
    let last_check = tray::add_item("", "");
    last_check.disable();
    tray::add_separator();

    let login = tray::add_checkbox(
        "Launch at Login",
        "Open GitHub Usage automatically when you log in",
        login::enabled(),
    );
    if login::bundle_path().is_none() || !login::supported() {
        login.hide(); // nothing to register: not a bundle, or macOS 12 or earlier
    }
    tray::add_separator();

    let about = tray::add_item(&about_title(), &about_tooltip());
    let quit = tray::add_item("Quit", "Quit GitHub Usage");

    on_click(&open_page, || open_url(USAGE_PAGE_URL));
    on_click(&status, || open_url(STATUS_PAGE_URL));
    on_click(&refresh, trigger_refresh);
    on_click(&login, toggle_launch_at_login);
    on_click(&about, || open_url(&release_notes_url()));
    thread::spawn(move || {
        let _ = quit.clicked().recv();
        tray::quit();
    });

    // Opening the menu means the user has seen the alert: stop the blinking.
    tray::on_menu_will_open(|| {
        let _ = ICON_ACK.0.try_send(()); // or an acknowledgement is already queued
    });

    let _ = MENU.set(Menu {
        credits,
        credits_count,
        credits_reset,
        status,
        incidents,
        last_check,
        login,
    });

    thread::spawn(icon_loop);
    thread::spawn(|| Poller::default().run());
    thread::spawn(offer_launch_at_login);
}

/// Runs `f` on its own thread for each click on `item`, one click at a time.
fn on_click(item: &MenuItem, f: impl Fn() + Send + 'static) {
    let clicks = item.clicked().clone();
    thread::spawn(move || {
        for () in clicks.iter() {
            f();
        }
    });
}

/// Names the app and the build in one line, so the version is visible without
/// a click.
fn about_title() -> String {
    format!("About {APP_NAME} {VERSION}")
}

/// Carries what is necessary to identify a build in a bug report. The
/// architecture is there because the released archives are for Apple silicon
/// only, and the commit because these builds are unsigned - "which build is
/// that?" otherwise has no answer.
fn about_tooltip() -> String {
    let mut parts = Vec::with_capacity(4);
    if !COMMIT.is_empty() {
        parts.push(format!("commit {COMMIT}"));
    }
    if !BUILD_DATE.is_empty() {
        parts.push(format!("built {BUILD_DATE}"));
    }
    parts.push(platform());
    parts.join(" · ") + " — click for the release notes"
}

/// The platform in Go's GOOS/GOARCH words, which the About tooltip has always
/// used.
fn platform() -> String {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    };
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "amd64",
        other => other,
    };
    format!("{os}/{arch}")
}

/// Points at the notes of this exact version when the build came from a clean
/// tag, and at the release list otherwise, since there is nothing to link a
/// dev build to.
fn release_notes_url() -> String {
    release_notes_url_for(VERSION)
}

fn release_notes_url_for(version: &str) -> String {
    if version.starts_with('v') && !version.contains(['-', '+']) {
        format!("{REPO_URL}/releases/tag/{version}")
    } else {
        format!("{REPO_URL}/releases")
    }
}

/// Asks - once - whether the app should open at login, and registers it as a
/// login item if so. It stays quiet when there is nothing to offer: when the
/// user has already answered either way, when the binary is not running from
/// its .app bundle, or on a macOS without SMAppService.
fn offer_launch_at_login() {
    if login::bundle_path().is_none() || !login::supported() {
        return;
    }
    let m = menu();

    // Already answered: do not ask again, but make the system match what we
    // recorded - see login::reconcile for why it cannot be read back instead.
    if login::answered() {
        if login::answer() {
            if login::reconcile() {
                m.login.check();
            } else {
                m.login.uncheck();
            }
        }
        return;
    }

    let Ok(yes) = notify::confirm(
        "Launch GitHub Usage at login?",
        "GitHub Usage can start automatically when you log in, so it keeps watching for incidents without you having to open it.",
        "Launch at Login",
        "Not Now",
    ) else {
        return; // the dialog itself failed; try again next launch, do not guess
    };
    if !yes {
        let _ = login::record_answer(false);
        return;
    }

    if let Err(e) = login::enable() {
        // Do not record the answer, so the offer is made again next launch.
        let _ = notify::banner("GitHub Usage", &format!("Could not open at login: {e}"));
        return;
    }
    let _ = login::record_answer(true);
    m.login.check();
    warn_if_needs_approval();
}

/// Flips the Launch at Login checkbox. The checkbox changes only after the
/// change has gone through, so a failure leaves the menu showing the truth.
fn toggle_launch_at_login() {
    let m = menu();
    if m.login.checked() {
        if let Err(e) = login::disable() {
            let _ = notify::banner(
                "GitHub Usage",
                &format!("Could not stop opening at login: {e}"),
            );
            return;
        }
        let _ = login::record_answer(false);
        m.login.uncheck();
        return;
    }

    if let Err(e) = login::enable() {
        let _ = notify::banner("GitHub Usage", &format!("Could not open at login: {e}"));
        return;
    }
    let _ = login::record_answer(true);
    m.login.check();
    warn_if_needs_approval();
}

/// Covers the case where the user has switched the login item off in System
/// Settings before: the registration succeeds, but macOS holds it back until
/// they switch it on there.
fn warn_if_needs_approval() {
    if login::needs_approval() {
        let _ = notify::banner(
            "GitHub Usage",
            "Switch GitHub Usage on under System Settings → General → Login Items to finish enabling it.",
        );
    }
}

/// What the latest poll saw: whether anything is ongoing, and whether any of it
/// is new since the previous poll.
struct IconUpdate {
    ongoing: bool,
    is_new: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum IconState {
    /// No incidents: the dot slot is there but empty.
    Ok,
    /// An incident the user has not seen yet: a blinking red dot.
    Alert,
    /// Seen by the user: a steady red notification dot.
    Ack,
}

/// Owns the menu-bar icon. It is the only writer, so the blink can never race
/// a state change and leave the wrong icon on screen.
fn icon_loop() {
    let mut st = IconState::Ok;
    let mut red = false;

    let set_red = |red: &mut bool, on: bool| {
        *red = on;
        if on {
            tray::set_icon(Some(icon::red_dot_png()));
        } else {
            // The blink is off, the incident is not over: a transparent image
            // of the same size holds the icon slot so the percentage does not
            // move.
            tray::set_icon(Some(icon::blank_png()));
        }
    };
    let apply = |st: &mut IconState, red: &mut bool, next: IconState| {
        *st = next;
        match next {
            IconState::Ok => {
                // All clear: remove the icon, so it is text only again.
                *red = false;
                tray::set_icon(None);
            }
            IconState::Alert => set_red(red, true), // start lit, so it is seen immediately
            IconState::Ack => {
                *red = true;
                tray::set_icon(Some(icon::red_dot_png()));
            }
        }
    };

    let ticker = tick(BLINK_INTERVAL);
    loop {
        select! {
            recv(ICON_UPDATES.1) -> u => {
                let Ok(u) = u else { return };
                if !u.ongoing {
                    if st != IconState::Ok {
                        apply(&mut st, &mut red, IconState::Ok);
                    }
                } else if u.is_new || st == IconState::Ok {
                    // A new incident starts the blink again, even if an earlier
                    // one has already been acknowledged.
                    apply(&mut st, &mut red, IconState::Alert);
                }
            }
            recv(ICON_ACK.1) -> _ => {
                if st == IconState::Alert {
                    apply(&mut st, &mut red, IconState::Ack);
                }
            }
            recv(ticker) -> _ => {
                if st == IconState::Alert {
                    let on = !red;
                    set_red(&mut red, on);
                }
            }
        }
    }
}

/// The state of the poll thread. Nothing else reads or writes it.
struct Poller {
    last_ongoing: HashSet<String>,
    first_run: bool,
    etag: String,
    credits_alerted: bool,
    credits_first_run: bool,
}

impl Default for Poller {
    fn default() -> Self {
        Poller {
            last_ongoing: HashSet::new(),
            first_run: true,
            etag: String::new(),
            credits_alerted: false,
            credits_first_run: true,
        }
    }
}

impl Poller {
    fn run(mut self) {
        self.poll(); // the first check is immediate
        let ticker = tick(CONFIG.poll_interval);
        loop {
            select! {
                recv(ticker) -> _ => self.poll(),
                recv(REFRESH_NOW.1) -> _ => self.poll(),
            }
        }
    }

    fn poll(&mut self) {
        self.check();
        self.check_credits();
    }

    /// Refreshes the Copilot premium-request allowance: the percentage in the
    /// menu bar and the three rows at the top of the menu.
    fn check_credits(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(20);
        let m = menu();

        let q = match credits::fetch(&CONFIG.copilot_url, deadline) {
            Ok(q) => q,
            Err(credits::Error::NoQuota) => {
                // An unlimited or quota-less plan: nothing to meter, so stay out
                // of the bar.
                tray::set_title("");
                m.credits
                    .set_title("Premium requests: unlimited on this plan");
                m.credits_count.hide();
                m.credits_reset.hide();
                return;
            }
            Err(err) => {
                tray::set_title("⚠");
                m.credits.set_title(&format!("⚠  {err}"));
                m.credits_count.hide();
                m.credits_reset.hide();
                return;
            }
        };

        tray::set_title(&format!("{}%", q.percent_used));
        m.credits
            .set_title(&format!("Premium requests: {}% used", q.percent_used));
        m.credits.set_tooltip(&format!(
            "{} of {} premium-request credits used on the {} plan",
            thousands(q.used),
            thousands(q.entitlement),
            q.plan
        ));

        let mut count = format!(
            "   {} of {} credits",
            thousands(q.used),
            thousands(q.entitlement)
        );
        if q.overage > 0 {
            count += &format!("  (+{} over)", thousands(q.overage));
        }
        m.credits_count.set_title(&count);
        m.credits_count.show();

        match q.resets_at {
            None => m.credits_reset.hide(),
            Some(t) => {
                m.credits_reset
                    .set_title(&format!("   resets {}", humanize_reset(t, Utc::now())));
                m.credits_reset.show();
            }
        }

        self.notify_credits_threshold(&q);
    }

    /// Sends a banner the first time the use reaches the alert percentage, and
    /// arms again after it drops back below (after a monthly reset). The first
    /// poll only sets a baseline, so a launch while already high sends none.
    fn notify_credits_threshold(&mut self, q: &credits::Quota) {
        let alert = CONFIG.alert_percent;
        if alert <= 0 {
            return;
        }
        let high = q.percent_used >= alert;

        if !self.credits_first_run && high && !self.credits_alerted {
            let _ = notify::banner(
                "GitHub Copilot",
                &format!(
                    "{}% of premium requests used ({} of {})",
                    q.percent_used,
                    thousands(q.used),
                    thousands(q.entitlement)
                ),
            );
        }
        self.credits_alerted = high;
        self.credits_first_run = false;
    }

    fn check(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(20);
        let m = menu();

        let result = feed::fetch(&CONFIG.feed_url, &self.etag, deadline);
        let now = Local::now();

        let body = match result {
            Err(_) => {
                // Keep the last known state. Only note that the refresh failed.
                m.last_check
                    .set_title(&format!("Last check failed {}", now.format("%H:%M:%S")));
                tray::set_tooltip(&format!(
                    "GitHub Usage — offline (tried {})",
                    now.format("%H:%M")
                ));
                return;
            }
            Ok(feed::Fetched::NotModified) => {
                m.last_check.set_title(&format!(
                    "Last checked {} (no change)",
                    now.format("%H:%M:%S")
                ));
                return;
            }
            Ok(feed::Fetched::Body { body, etag }) => {
                self.etag = etag;
                body
            }
        };

        let Ok(incidents) = feed::parse_atom(&body) else {
            m.last_check
                .set_title(&format!("Parse error {}", now.format("%H:%M:%S")));
            return;
        };

        let ongoing = feed::ongoing(&incidents);
        update_menu(&ongoing);
        let is_new = self.notify_new(&ongoing);
        let _ = ICON_UPDATES.0.send(IconUpdate {
            ongoing: !ongoing.is_empty(),
            is_new,
        });
        m.last_check
            .set_title(&format!("Last checked {}", now.format("%H:%M:%S")));
    }

    /// Sends a banner for each incident that has appeared since the last poll,
    /// and reports whether there was one. The first poll only sets a baseline
    /// (no banner), so a launch during an existing incident does not spam
    /// notifications - the icon still blinks for it.
    fn notify_new(&mut self, ongoing: &[feed::Incident]) -> bool {
        let cur: HashSet<String> = ongoing.iter().map(|i| i.id.clone()).collect();

        let mut is_new = false;
        for inc in ongoing {
            if !self.last_ongoing.contains(&inc.id) {
                is_new = true;
                if !self.first_run {
                    let _ = notify::banner("GitHub incident", &inc.title);
                }
            }
        }
        if !self.first_run && cur.is_empty() && !self.last_ongoing.is_empty() {
            let _ = notify::banner("GitHub Usage", "All incidents resolved");
        }

        self.last_ongoing = cur;
        self.first_run = false;
        is_new
    }
}

/// Shows a reset time relative to now when it is close, and as a date when it
/// is not. The premium-request window is monthly, so most of the time this
/// reads "resets on 1 Sep".
fn humanize_reset(t: DateTime<Utc>, now: DateTime<Utc>) -> String {
    // Round, do not truncate, so a reset 24m59s away does not read "in 24m".
    // Rounding first also keeps the branches consistent: 59m40s becomes an hour
    // and takes the "1h 0m" arm instead of printing "in 60m".
    let minutes = round_to_minutes(t - now);
    if minutes <= 0 {
        "now".into()
    } else if minutes < 60 {
        format!("in {minutes}m")
    } else if minutes < 24 * 60 {
        format!("in {}h {}m", minutes / 60, minutes % 60)
    } else if minutes < 7 * 24 * 60 {
        format!("on {}", t.with_timezone(&Local).format("%a %-d %b"))
    } else {
        format!("on {}", t.with_timezone(&Local).format("%-d %b"))
    }
}

/// Rounds to whole minutes, halfway values away from zero, as Go's
/// `d.Round(time.Minute)` does.
fn round_to_minutes(d: TimeDelta) -> i64 {
    const MINUTE: i128 = 60_000_000_000;
    let ns = d.num_seconds() as i128 * 1_000_000_000 + d.subsec_nanos() as i128;
    let rounded = if ns >= 0 {
        (ns + MINUTE / 2) / MINUTE
    } else {
        -((-ns + MINUTE / 2) / MINUTE)
    };
    rounded as i64
}

/// Groups an integer with thin separators, so 13500 reads as 13,500.
fn thousands(n: i64) -> String {
    let s = n.unsigned_abs().to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3 + 1);
    if n < 0 {
        out.push('-');
    }
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Relabels the menu contents. The menu-bar icon itself is left to icon_loop,
/// which blinks it until the user opens the menu.
fn update_menu(ongoing: &[feed::Incident]) {
    let m = menu();
    let mut urls = CURRENT_URLS.lock().unwrap();
    for u in urls.iter_mut() {
        u.clear();
    }

    if ongoing.is_empty() {
        tray::set_tooltip("GitHub Usage — all systems operational");
        m.status.set_title("✓  All systems operational");
        m.status
            .set_tooltip("githubstatus.com reports all systems operational");
        for mi in &m.incidents {
            mi.hide();
        }
        return;
    }

    tray::set_tooltip(&format!(
        "GitHub Usage — {} ongoing incident(s)",
        ongoing.len()
    ));

    let mut status = if ongoing.len() == 1 {
        "●  1 ongoing incident".to_string()
    } else {
        format!("●  {} ongoing incidents", ongoing.len())
    };
    if ongoing.len() > MAX_INCIDENT_ITEMS {
        status += &format!("  (showing {MAX_INCIDENT_ITEMS})");
    }
    m.status.set_title(&status);
    m.status
        .set_tooltip("githubstatus.com — click to open the status page");

    for (i, mi) in m.incidents.iter().enumerate() {
        let Some(inc) = ongoing.get(i) else {
            mi.hide();
            continue;
        };

        // Show what the newest update actually says. The entry title is
        // generic ("Incident with Copilot") and says no more than the icon
        // already does; the update carries the detail.
        let text = if inc.latest_update.is_empty() {
            &inc.title
        } else {
            &inc.latest_update
        };
        let label = if inc.latest_status.is_empty() {
            text.clone()
        } else {
            format!("{} — {}", inc.latest_status, text)
        };
        mi.set_title(&format!("   {}", truncate(&label, INCIDENT_LABEL_MAX_LEN)));

        // The tooltip carries the title and the whole update, not cut.
        let mut tip = format!("{} ({})", inc.title, inc.latest_status);
        if !inc.latest_update.is_empty() {
            tip += "\n";
            tip += &inc.latest_update;
        }
        mi.set_tooltip(&tip);
        urls[i] = inc.url.clone();
        mi.show();
    }
}

fn trigger_refresh() {
    let _ = REFRESH_NOW.0.try_send(()); // or a refresh is already queued
}

fn incident_url(i: usize) -> String {
    CURRENT_URLS
        .lock()
        .unwrap()
        .get(i)
        .cloned()
        .unwrap_or_default()
}

/// Opens `u` in the default browser, without waiting for it.
fn open_url(u: &str) {
    if u.is_empty() {
        return;
    }
    let child = Command::new("/usr/bin/open")
        .arg(u)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    if let Ok(mut child) = child {
        thread::spawn(move || child.wait());
    }
}

/// Cuts `s` to at most `n` characters, with an ellipsis at the cut. It breaks
/// at a word boundary if it can, because update messages are prose and a cut in
/// the middle of a word looks like a fault - but only when that keeps most of
/// the line, so a single very long word cannot shrink the row to nothing.
fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    if n < 1 {
        return String::new();
    }
    const TRAILING: &[char] = &[' ', '.', ',', ';', ':', '-', '–', '—'];
    let hard: String = s.chars().take(n - 1).collect();
    if let Some(i) = hard.rfind(' ') {
        // rfind gives a byte offset, but the "is this still most of the line?"
        // test must count characters, or a line of multi-byte characters
        // passes on byte length alone.
        let word = hard[..i].trim_end_matches(TRAILING);
        if word.chars().count() * 5 >= (n - 1) * 3 {
            return format!("{word}…");
        }
    }
    format!("{}…", hard.trim_end_matches(TRAILING))
}

fn env_or(key: &str, def: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| def.to_string())
}

/// Parses a whole number as Go's strconv.Atoi does: an optional sign, then
/// digits only.
fn atoi(v: &str) -> Option<i64> {
    v.parse().ok()
}

fn poll_interval_from_env() -> Duration {
    poll_interval_from(std::env::var("POLL_SECONDS").ok().as_deref())
}

fn poll_interval_from(v: Option<&str>) -> Duration {
    match v.filter(|v| !v.is_empty()).and_then(atoi) {
        Some(n) if n >= 10 => Duration::from_secs(n as u64),
        _ => DEFAULT_POLL_INTERVAL,
    }
}

fn alert_percent_from_env() -> i64 {
    alert_percent_from(std::env::var("ALERT_PERCENT").ok().as_deref())
}

fn alert_percent_from(v: Option<&str>) -> i64 {
    match v.filter(|v| !v.is_empty()).and_then(atoi) {
        Some(n) if (0..=100).contains(&n) => n,
        _ => DEFAULT_ALERT_PERCENT,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thousands_groups() {
        let cases = [
            (0, "0"),
            (7, "7"),
            (855, "855"),
            (1000, "1,000"),
            (13500, "13,500"),
            (300000, "300,000"),
            (1234567, "1,234,567"),
            (-13500, "-13,500"),
        ];
        for (n, want) in cases {
            assert_eq!(thousands(n), want, "thousands({n})");
        }
    }

    #[test]
    fn humanize_reset_forms() {
        let now = Utc::now();
        let at = |d: TimeDelta| now + d;
        assert_eq!(humanize_reset(at(TimeDelta::minutes(-1)), now), "now");
        assert_eq!(humanize_reset(at(TimeDelta::minutes(25)), now), "in 25m");
        assert_eq!(
            humanize_reset(at(TimeDelta::hours(3) + TimeDelta::minutes(10)), now),
            "in 3h 10m"
        );
        // Rounding, not truncation: 24m59s reads 25m, and 59m40s reads 1h 0m.
        assert_eq!(
            humanize_reset(at(TimeDelta::minutes(24) + TimeDelta::seconds(59)), now),
            "in 25m"
        );
        assert_eq!(
            humanize_reset(at(TimeDelta::minutes(59) + TimeDelta::seconds(40)), now),
            "in 1h 0m"
        );
        // Within a week: weekday and date.
        let soon = at(TimeDelta::days(3));
        assert_eq!(
            humanize_reset(soon, now),
            format!("on {}", soon.with_timezone(&Local).format("%a %-d %b"))
        );
        // The monthly premium-request window: a bare date, not weekday + clock.
        let far = DateTime::parse_from_rfc3339("2099-09-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(
            humanize_reset(far, now),
            format!("on {}", far.with_timezone(&Local).format("%-d %b"))
        );
    }

    #[test]
    fn round_to_minutes_matches_go() {
        assert_eq!(round_to_minutes(TimeDelta::seconds(29)), 0);
        assert_eq!(round_to_minutes(TimeDelta::seconds(30)), 1);
        assert_eq!(round_to_minutes(TimeDelta::seconds(-30)), -1);
        assert_eq!(round_to_minutes(TimeDelta::seconds(-29)), 0);
    }

    #[test]
    fn truncate_cases() {
        let cases = [
            (
                "short enough is untouched",
                "Investigating — all fine",
                90,
                "Investigating — all fine",
            ),
            ("exactly the limit", "abcde", 5, "abcde"),
            (
                "breaks at a word boundary, not mid-word",
                "Update — We are experiencing degraded availability for chat & agent models in Copilot. Multiple models are impacted.",
                90,
                "Update — We are experiencing degraded availability for chat & agent models in Copilot…",
            ),
            (
                "an unbroken token breaks back to the previous word",
                "Investigating — aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                20,
                "Investigating…",
            ),
            (
                "a token too long to break around is cut hard, never emptied",
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa bbb",
                10,
                "aaaaaaaaa…",
            ),
            ("n below one yields nothing", "anything", 0, ""),
            ("counts runes, not bytes", "æøåæøåæøå", 5, "æøåæ…"),
        ];
        for (name, input, n, want) in cases {
            let got = truncate(input, n);
            assert_eq!(got, want, "{name}");
            assert!(got.chars().count() <= n, "{name}: over the limit of {n}");
        }
    }

    #[test]
    fn release_notes_url_only_for_clean_tags() {
        assert_eq!(
            release_notes_url_for("v2.0.1"),
            "https://github.com/lobner/github-usage/releases/tag/v2.0.1"
        );
        for v in [
            "dev",
            "v2.0.1-2-gabc1234",
            "v2.0.1-dirty",
            "v2.0.1+meta",
            "abc1234",
        ] {
            assert_eq!(
                release_notes_url_for(v),
                "https://github.com/lobner/github-usage/releases",
                "{v}"
            );
        }
    }

    #[test]
    fn env_parsing() {
        assert_eq!(poll_interval_from(None), DEFAULT_POLL_INTERVAL);
        assert_eq!(poll_interval_from(Some("9")), DEFAULT_POLL_INTERVAL);
        assert_eq!(poll_interval_from(Some("10")), Duration::from_secs(10));
        assert_eq!(poll_interval_from(Some("abc")), DEFAULT_POLL_INTERVAL);

        assert_eq!(alert_percent_from(None), 80);
        assert_eq!(alert_percent_from(Some("0")), 0);
        assert_eq!(alert_percent_from(Some("100")), 100);
        assert_eq!(alert_percent_from(Some("101")), 80);
        assert_eq!(alert_percent_from(Some("-1")), 80);
    }

    #[test]
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    fn platform_uses_go_names() {
        assert_eq!(platform(), "darwin/arm64");
    }
}
