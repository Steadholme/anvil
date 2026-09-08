//! HTTP handlers + shared server-render helpers.
//!
//! `health` is the unauthenticated liveness probe; `pipelines` carries the operator console and the
//! SSO-gated create-pipeline / trigger-run / view-run flow.
//!
//! The shared design tokens / CSS are embedded (via `include_str!`) and served as one immutable asset,
//! matching the Steadholme estate brand on the shared Okta Odyssey UI kit: brand shield, a clean
//! flat app-bar (brand + "All apps" link + user chip + logout), the Odyssey primary accent, and
//! soft-tinted status pills.

pub mod health;
pub mod pipelines;

use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Response};

/// Anvil-only CSS layered after Odyssey's canonical font, tokens, and components.
pub const SERVICE_CSS: &str = include_str!("../../static/service.css");
const ERROR_HTML: &str = include_str!("../../templates/error.html");

pub const APP_CSS_PATH: &str = "/assets/anvil-20260908.css";

static APP_CSS: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Canonical Odyssey base first, the Anvil service layer second — the estate-wide design system.
pub fn app_css() -> &'static str {
    APP_CSS
        .get_or_init(|| {
            let mut css = String::with_capacity(odyssey::APP_CSS.len() + SERVICE_CSS.len() + 1);
            css.push_str(odyssey::APP_CSS);
            css.push('\n');
            css.push_str(SERVICE_CSS);
            css
        })
        .as_str()
}

pub async fn app_css_asset() -> Response {
    let mut response = app_css().into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/css; charset=utf-8"),
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=31536000, immutable"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response
}

/// Cross-subdomain gateway logout (Anvil lives at ci.w33d.xyz; the IdP is at id.w33d.xyz).
pub const LOGOUT_URL: &str = "https://sso.w33d.xyz/_gw/auth/logout";


/// Minimal HTML escaping for text/attribute interpolation (defense-in-depth on every field).
pub fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
}


/// A normalized status pill whose word and glyph remain distinguishable without color.
pub fn status_pill(status: &str) -> String {
    let state = normalized_status(status);
    let glyph = match state {
        "queued" => "…",
        "running" => "»",
        "success" => "✓",
        "failed" => "✕",
        "never" => "–",
        _ => "?",
    };
    format!(
        r#"<span class="pill pill--{state}"><span class="pill__ico" aria-hidden="true">{glyph}</span><span class="sr-only">Status: </span>{state}</span>"#
    )
}

/// Map every runtime value onto the six states the UI is allowed to claim.
pub(crate) fn normalized_status(status: &str) -> &'static str {
    match status {
        "queued" => "queued",
        "running" => "running",
        "success" => "success",
        "failed" => "failed",
        "never" => "never",
        "unknown" => "unknown",
        _ => "unknown",
    }
}

/// Format epoch seconds as a compact UTC datetime `Mon D, YYYY HH:MM UTC`.
/// `0` (unset) renders `—`; an out-of-range value remains raw and is never labelled UTC.
pub fn fmt_ts(secs: i64) -> String {
    if secs == 0 {
        return "—".to_string();
    }
    match time::OffsetDateTime::from_unix_timestamp(secs) {
        Ok(dt) => format!(
            "{} {}, {} {:02}:{:02} UTC",
            month_abbr(dt.month()),
            dt.day(),
            dt.year(),
            dt.hour(),
            dt.minute()
        ),
        Err(_) => secs.to_string(),
    }
}

/// Human duration between `start` and `end` (epoch secs). Renders `—` until a run has started.
pub fn fmt_duration(start: i64, end: i64) -> String {
    if start <= 0 {
        return "—".to_string();
    }
    let end = if end > 0 { end } else { crate::now_secs() };
    let secs = (end - start).max(0);
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
    }
}

fn month_abbr(m: time::Month) -> &'static str {
    use time::Month::*;
    match m {
        January => "Jan",
        February => "Feb",
        March => "Mar",
        April => "Apr",
        May => "May",
        June => "Jun",
        July => "Jul",
        August => "Aug",
        September => "Sep",
        October => "Oct",
        November => "Nov",
        December => "Dec",
    }
}

/// A 303 redirect (post/redirect/get).
pub fn redirect(location: &str) -> Response {
    (
        StatusCode::SEE_OTHER,
        [(
            header::LOCATION,
            HeaderValue::from_str(location).unwrap_or(HeaderValue::from_static("/")),
        )],
    )
        .into_response()
}

/// An HTML response, optionally attaching a freshly-minted CSRF `Set-Cookie`.
pub fn html_with_cookie(body: String, set_cookie: Option<String>) -> Response {
    let mut resp = Html(body).into_response();
    if let Some(c) = set_cookie {
        if let Ok(value) = HeaderValue::from_str(&c) {
            resp.headers_mut().insert(header::SET_COOKIE, value);
        }
    }
    resp
}


/// Icons used across the console chrome (inline so no asset request is needed).
pub const ICON_MARK: &str = r##"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M3 12h7l3-5 3 10 2-5h3"/></svg>"##;
pub const ICON_GRID: &str = r##"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><rect x="3" y="3" width="7" height="7" rx="1.5"/><rect x="14" y="3" width="7" height="7" rx="1.5"/><rect x="3" y="14" width="7" height="7" rx="1.5"/><rect x="14" y="14" width="7" height="7" rx="1.5"/></svg>"##;

/// The console pages, in app-bar order.
pub const NAV: [(&str, &str); 1] = [("/", "Pipelines")];

/// Render the app bar: brand lockup + host + page pills; All apps, identity and Log out.
pub fn app_bar(active: &str, email: Option<&str>) -> String {
    let mut pills = String::new();
    for (href, label) in NAV {
        pills.push_str(&format!(
            r#"<a class="surf{state}" href="{href}"{aria}>{label}</a>"#,
            state = if href == active { " is-active" } else { "" },
            href = href,
            aria = if href == active { r#" aria-current="page""# } else { "" },
            label = label,
        ));
    }
    let chip = match email {
        Some(value) if !value.is_empty() && value != "—" => {
            let initial = value
                .chars()
                .next()
                .map(|c| c.to_uppercase().to_string())
                .unwrap_or_else(|| "S".to_string());
            format!(
                r#"<span class="userchip"><span class="userchip__avatar" aria-hidden="true">{initial}</span><span class="user-email">{email}</span></span>"#,
                initial = esc(&initial),
                email = esc(value),
            )
        }
        _ => r#"<span class="user-email user-email--none">— (no gateway session)</span>"#.to_string(),
    };
    format!(
        r#"<header class="suitebar">
  <a class="suitebar__brand" href="/">
    <span class="brand-tile" aria-hidden="true">{mark}</span>
    <span class="suitebar__name"><b>Steadholme</b><span>Anvil · continuous integration</span></span>
  </a>
  <span class="suitebar__host">ci.w33d.xyz</span>
  <nav class="surfaces" aria-label="Anvil pages">{pills}</nav>
  <span class="suitebar__spacer"></span>
  <div class="suitebar__right">
    <a class="allapps" href="https://w33d.xyz">{grid}<span>All apps</span></a>
    {chip}
    <a class="btn btn-ghost btn-sm" href="{logout}">Log out</a>
  </div>
</header>"#,
        mark = ICON_MARK,
        pills = pills,
        grid = ICON_GRID,
        chip = chip,
        logout = LOGOUT_URL,
    )
}

/// The shared page footer.
pub const FOOTER: &str = r##"<footer class="v2-foot">
  <span class="v2-foot__lead">Steadholme Anvil · ci.w33d.xyz · pipelines, runs, logs</span>
  <a href="https://git.w33d.xyz">Loom</a>
  <a href="https://audit.w33d.xyz">Watchtower</a>
  <a href="https://status.w33d.xyz">Status</a>
  <a href="https://w33d.xyz">All apps</a>
</footer>"##;

/// Resolve the viewer's theme from the cookie header.
pub fn theme_of(headers: &axum::http::HeaderMap) -> &'static str {
    odyssey::resolve_theme(
        headers
            .get(header::COOKIE)
            .and_then(|value| value.to_str().ok()),
    )
}

/// Fill a page template's chrome placeholders: theme attributes, stylesheet, app bar, footer.
pub fn shell(template: &str, active: &str, theme: &str, email: Option<&str>) -> String {
    template
        .replace("{{THEME_ATTR}}", odyssey::html_theme_attr(theme))
        .replace("{{COLOR_SCHEME}}", odyssey::color_scheme_meta(theme))
        .replace("{{CSS_PATH}}", APP_CSS_PATH)
        .replace("{{APPBAR}}", &app_bar(active, email))
        .replace("{{FOOTER}}", FOOTER)
}

/// Render the branded error document as one status tile.
pub fn error_page(status: StatusCode, message: &str) -> String {
    let reason = status.canonical_reason().unwrap_or("Error");
    ERROR_HTML
        .replace("{{THEME_ATTR}}", "")
        .replace("{{COLOR_SCHEME}}", "light dark")
        .replace("{{CSS_PATH}}", APP_CSS_PATH)
        .replace("{{APPBAR}}", &app_bar("/", None))
        .replace("{{FOOTER}}", FOOTER)
        .replace("{{STATUS}}", &status.as_u16().to_string())
        .replace("{{HEADING}}", &esc(reason))
        .replace("{{MESSAGE}}", &esc(message))
}

