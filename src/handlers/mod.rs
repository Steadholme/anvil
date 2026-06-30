//! HTTP handlers + shared server-render helpers.
//!
//! `health` is the unauthenticated liveness probe; `pipelines` carries the operator console and the
//! SSO-gated create-pipeline / trigger-run / view-run flow.
//!
//! The shared design tokens / CSS are embedded (via `include_str!`) and inlined into every page,
//! matching the HOLDFAST enterprise brand (the same dark command-center look as the rest of the
//! estate): brand shield, dark gradient app-bar, indigo accent, status pills.

pub mod health;
pub mod pipelines;

use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Response};

/// Embedded design system, inlined into each rendered page's `<style>`.
pub const APP_CSS: &str = include_str!("../../static/app.css");

/// Cross-subdomain gateway logout (Anvil lives at ci.w33d.xyz; the IdP is at id.w33d.xyz).
pub const LOGOUT_URL: &str = "https://id.w33d.xyz/_gw/auth/logout";

/// The HOLDFAST shield glyph (small, for the app-bar brand lockup).
pub const SHIELD_SVG: &str = r##"<svg viewBox="0 0 48 48" fill="none" xmlns="http://www.w3.org/2000/svg"><defs><linearGradient id="hf-shield-sm" x1="8" y1="4" x2="40" y2="44" gradientUnits="userSpaceOnUse"><stop stop-color="#818CF8"/><stop offset="1" stop-color="#4F46E5"/></linearGradient></defs><path d="M24 4 8 9.5V22c0 11 7 17.4 16 21.5C33 39.4 40 33 40 22V9.5L24 4Z" fill="url(#hf-shield-sm)"/><rect x="20" y="19" width="8" height="13" rx="1" fill="#fff" fill-opacity="0.92"/><path d="M20 19v-2.5a4 4 0 0 1 8 0V19" stroke="#fff" stroke-width="2" stroke-opacity="0.92" fill="none"/></svg>"##;

/// Minimal HTML escaping for text/attribute interpolation (defense-in-depth on every field).
pub fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
}

/// Render the shared app-bar: shield + HOLDFAST wordmark + page title on the left; the signed-in
/// email + a Logout link to the gateway on the right.
pub fn topbar(page_title: &str, email: &str) -> String {
    format!(
        r#"<header class="topbar">
  <div class="topbar__inner">
    <a class="brand" href="/" aria-label="HOLDFAST Anvil">
      <span class="brand__glyph" aria-hidden="true">{shield}</span>
      <span class="brand__word">HOLDFAST</span>
      <span class="brand__svc">Anvil</span>
    </a>
    <div class="topbar__right">
      <span class="topbar__title">{title}</span>
      <span class="user-email">{email}</span>
      <a class="btn btn-ghost btn-sm" href="{logout}">Log out</a>
    </div>
  </div>
</header>"#,
        shield = SHIELD_SVG,
        title = esc(page_title),
        email = esc(email),
        logout = LOGOUT_URL,
    )
}

/// A status pill (`queued` / `running` / `success` / `failed`) with brand status colors.
pub fn status_pill(status: &str) -> String {
    let cls = match status {
        "success" => "pill pill--success",
        "failed" => "pill pill--failed",
        "running" => "pill pill--running",
        _ => "pill pill--queued",
    };
    format!(r#"<span class="{cls}">{label}</span>"#, label = esc(status))
}

/// Format epoch seconds as a compact UTC datetime `Mon D, YYYY HH:MM`. `0` (unset) renders `—`.
pub fn fmt_ts(secs: i64) -> String {
    if secs <= 0 {
        return "—".to_string();
    }
    match time::OffsetDateTime::from_unix_timestamp(secs) {
        Ok(dt) => format!(
            "{} {}, {} {:02}:{:02}",
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

/// A small, branded HTML error page (used by [`crate::error::AppError`]).
pub fn error_page(status: StatusCode, message: &str) -> String {
    let code = status.as_u16();
    let reason = status.canonical_reason().unwrap_or("Error");
    format!(
        r#"<!DOCTYPE html>
<html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="color-scheme" content="dark">
<title>{code} {reason} · Anvil</title><style>{css}</style></head>
<body class="page-console">
{topbar}
<main class="console">
  <div class="error-card">
    <div class="error-card__code">{code}</div>
    <h1 class="error-card__title">{reason}</h1>
    <p class="error-card__msg">{msg}</p>
    <a class="btn btn-primary" href="/">Back to the console</a>
  </div>
</main>
</body></html>"#,
        css = APP_CSS,
        topbar = topbar("Anvil", "—"),
        code = code,
        reason = esc(reason),
        msg = esc(message),
    )
}
