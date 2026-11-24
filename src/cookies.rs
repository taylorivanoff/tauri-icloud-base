//! Apple cookie persistence — mirrors electron icloud-windows-base/cookies.js.
//! Session cookies (no expiry from Apple) get a ~180-day TTL so they survive quit.

use cookie::SameSite;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};
use tauri::webview::Cookie;
use tauri::{AppHandle, Manager, WebviewWindow};
use tauri_tray_base::Quitting;
#[cfg(not(windows))]
use time::{Duration, OffsetDateTime};

#[cfg(windows)]
use crate::cookies_webview2;

const SESSION_COOKIE_TTL_SECS: i64 = 180 * 24 * 60 * 60;

static APPLYING: AtomicBool = AtomicBool::new(false);
static LAST_FINGERPRINT: Mutex<String> = Mutex::new(String::new());

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredCookie {
    #[serde(default)]
    pub url: String,
    pub name: String,
    pub value: String,
    pub domain: String,
    pub path: String,
    pub secure: bool,
    #[serde(rename = "httpOnly")]
    pub http_only: bool,
    #[serde(rename = "expirationDate")]
    pub expiration_date: i64,
    #[serde(rename = "sameSite")]
    pub same_site: String,
}

pub fn cookie_file_path(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("app_data_dir: {e}"))?;
    Ok(dir.join("cookies.json"))
}

pub fn is_apple_family_domain(domain: &str) -> bool {
    let d = domain.trim_start_matches('.').to_ascii_lowercase();
    ["icloud.com", "apple.com", "icloud.com.cn", "apple.com.cn"]
        .iter()
        .any(|suffix| d == *suffix || d.ends_with(&format!(".{suffix}")))
}

/// Mainland China uses icloud.com.cn / apple.com.cn. Cookies from those hosts
/// belong to a different account region than www.icloud.com.
fn is_china_apple_domain(domain: &str) -> bool {
    let d = domain.trim_start_matches('.').to_ascii_lowercase();
    d == "icloud.com.cn"
        || d.ends_with(".icloud.com.cn")
        || d == "apple.com.cn"
        || d.ends_with(".apple.com.cn")
}

fn icloud_host_from_cookies(cookies: &[StoredCookie]) -> Option<&'static str> {
    let china_auth = cookies
        .iter()
        .any(|c| is_china_apple_domain(&c.domain) && is_apple_session_cookie_name(&c.name));
    china_auth.then_some(crate::ICLOUD_CN_HOST)
}

/// Existing installs may have a China session but no saved host preference.
/// Infer the region from the persisted auth cookies so the first launch after
/// upgrading does not navigate to www.icloud.com and force another login.
pub fn persisted_icloud_host(app: &AppHandle) -> Option<&'static str> {
    let path = cookie_file_path(app).ok()?;
    let raw = fs::read_to_string(path).ok()?;
    let cookies: Vec<StoredCookie> = serde_json::from_str(&raw).ok()?;
    icloud_host_from_cookies(&cookies)
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn ensure_parent(path: &Path) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir: {e}"))?;
    }
    Ok(())
}

/// Only a full web-session cookie proves the user is actually signed in.
///
/// Apple sets account identifiers (`MYACINFO`, `AASP`, `ACN01`, `DES`, `DSID`)
/// and device-trust cookies (`X-APPLE-WEBAUTH-HSA-TRUST`, `-USER`, `-REPAIR`,
/// PCS cookies) before a usable session exists. Treating those as auth lets a
/// logged-out or partial snapshot overwrite a valid `cookies.json`, so the app
/// looks signed out again on the next restart.
fn is_apple_session_cookie_name(name: &str) -> bool {
    let n = name.to_ascii_uppercase();
    n == "X-APPLE-WEBAUTH-TOKEN" || n == "X-APPLE-DS-WEB-SESSION-TOKEN"
}

/// Electron keeps Chromium's leading `.` on apex domain cookies. Tauri strips it.
/// Without the leading `.`, WebView2 treats the cookie as host-only and it will
/// not be sent to `www.icloud.com` / app subdomains after restore.
pub(crate) fn persist_domain(domain: &str) -> String {
    let trimmed = domain.trim();
    if trimmed.starts_with('.') {
        return trimmed.to_ascii_lowercase();
    }
    let d = trimmed.to_ascii_lowercase();
    if matches!(
        d.as_str(),
        "icloud.com" | "apple.com" | "icloud.com.cn" | "apple.com.cn"
    ) {
        format!(".{d}")
    } else {
        d
    }
}

fn normalize_stored_cookie(mut stored: StoredCookie) -> StoredCookie {
    stored.domain = persist_domain(&stored.domain);
    if stored.path.is_empty() {
        stored.path = "/".into();
    }
    if stored.url.is_empty() {
        stored.url = cookie_url(&stored.domain, &stored.path);
    }
    // Keep a future expiry so restored Apple session cookies stay persistent.
    if stored.expiration_date <= now_unix() {
        stored.expiration_date = now_unix() + SESSION_COOKIE_TTL_SECS;
    }
    stored
}

fn cookie_url(domain: &str, path: &str) -> String {
    let host = domain.trim_start_matches('.');
    let path = if path.is_empty() { "/" } else { path };
    format!("https://{host}{path}")
}

pub fn has_apple_auth(cookies: &[StoredCookie]) -> bool {
    cookies
        .iter()
        .any(|c| is_apple_session_cookie_name(&c.name))
}

/// Never overwrite on-disk auth cookies with a logged-out webview snapshot.
fn should_skip_auth_clobber(path: &Path, cookies: &[StoredCookie]) -> bool {
    let Some(existing) = read_cookie_file(path) else {
        return false;
    };
    if cookies.is_empty() && !existing.is_empty() {
        return has_apple_auth(&existing);
    }
    has_apple_auth(&existing) && !has_apple_auth(cookies)
}

fn fingerprint(cookies: &[StoredCookie]) -> String {
    let mut parts: Vec<String> = cookies
        .iter()
        .map(|c| format!("{}\n{}\n{}", c.name, c.domain, c.value))
        .collect();
    parts.sort();
    parts.join("\0")
}

fn same_site_str(cookie: &Cookie<'_>) -> String {
    match cookie.same_site() {
        Some(SameSite::None) => "no_restriction".into(),
        Some(SameSite::Strict) => "strict".into(),
        Some(SameSite::Lax) | None => "lax".into(),
    }
}

fn serialize_cookie(cookie: &Cookie<'_>) -> Option<StoredCookie> {
    let raw_domain = cookie.domain()?.to_string();
    if !is_apple_family_domain(&raw_domain) {
        return None;
    }
    let domain = persist_domain(&raw_domain);
    let path = cookie.path().unwrap_or("/").to_string();
    let expiration_date = cookie
        .expires_datetime()
        .map(|dt| dt.unix_timestamp())
        .unwrap_or_else(|| now_unix() + SESSION_COOKIE_TTL_SECS);

    Some(StoredCookie {
        url: cookie_url(&domain, &path),
        name: cookie.name().to_string(),
        value: cookie.value().to_string(),
        domain,
        path,
        secure: cookie.secure().unwrap_or(true),
        http_only: cookie.http_only().unwrap_or(false),
        expiration_date,
        same_site: same_site_str(cookie),
    })
}

fn snapshot_apple_cookies(window: &WebviewWindow) -> Result<Vec<StoredCookie>, String> {
    let all = window.cookies().map_err(|e| format!("cookies(): {e}"))?;
    Ok(all.iter().filter_map(serialize_cookie).collect())
}

#[cfg(not(windows))]
fn to_tauri_cookie(stored: &StoredCookie) -> Cookie<'static> {
    let same_site = match stored.same_site.as_str() {
        "no_restriction" | "none" => SameSite::None,
        "strict" => SameSite::Strict,
        _ => SameSite::Lax,
    };

    let expires = OffsetDateTime::from_unix_timestamp(stored.expiration_date)
        .unwrap_or_else(|_| OffsetDateTime::now_utc() + Duration::days(180));

    Cookie::build((stored.name.clone(), stored.value.clone()))
        .domain(stored.domain.clone())
        .path(stored.path.clone())
        .secure(stored.secure)
        .http_only(stored.http_only)
        .same_site(same_site)
        .expires(expires)
        .build()
}

fn apply_stored_cookies(window: &WebviewWindow, cookies: &[StoredCookie]) -> Result<usize, String> {
    #[cfg(windows)]
    {
        // Never fall back to Tauri `set_cookie` on Windows: it strips the leading
        // `.` on apex domains, creating host-only cookies that www.icloud.com
        // never receives — which looks like “logged out after restart”.
        return cookies_webview2::set_stored_cookies(window, cookies);
    }
    #[cfg(not(windows))]
    {
        let mut applied = 0usize;
        for stored in cookies {
            if stored.name.is_empty() || stored.domain.is_empty() {
                continue;
            }
            match window.set_cookie(to_tauri_cookie(stored)) {
                Ok(()) => applied += 1,
                Err(e) => eprintln!("[cookies] set failed {}: {e}", stored.name),
            }
        }
        if applied == 0 {
            Err("no cookies applied".into())
        } else {
            Ok(applied)
        }
    }
}

fn read_cookie_file(path: &Path) -> Option<Vec<StoredCookie>> {
    let raw = fs::read_to_string(path).ok()?;
    let cookies: Vec<StoredCookie> = serde_json::from_str(&raw).ok()?;
    if cookies.is_empty() {
        None
    } else {
        Some(cookies)
    }
}

/// True when the webview cookie jar contains Apple auth markers.
pub fn webview_has_apple_auth(window: &WebviewWindow) -> Result<bool, String> {
    let cookies = snapshot_apple_cookies(window)?;
    Ok(has_apple_auth(&cookies))
}

/// Restore persisted Apple cookies into the webview. Call from a non-WebView2 UI thread.
pub fn load_persisted_cookies(app: &AppHandle, window: &WebviewWindow) -> Result<bool, String> {
    let path = cookie_file_path(app)?;
    let Some(raw) = read_cookie_file(&path) else {
        return Ok(false);
    };
    let cookies: Vec<StoredCookie> = raw.into_iter().map(normalize_stored_cookie).collect();
    if cookies.is_empty() {
        return Ok(false);
    }
    let disk_has_auth = has_apple_auth(&cookies);

    APPLYING.store(true, Ordering::SeqCst);
    let result = (|| {
        let applied = apply_stored_cookies(window, &cookies)?;
        if let Ok(mut fp) = LAST_FINGERPRINT.lock() {
            *fp = fingerprint(&cookies);
        }
        // Cold boot can report a partial apply via a broken fallback; require the
        // auth markers we persisted or keep retrying.
        if disk_has_auth {
            match webview_has_apple_auth(window) {
                Ok(true) => {}
                Ok(false) => {
                    return Err("auth cookies missing in webview after restore".into());
                }
                Err(e) => {
                    return Err(format!("auth check after restore: {e}"));
                }
            }
        }
        eprintln!("[cookies] applied {applied} cookies from disk");
        Ok(true)
    })();
    APPLYING.store(false, Ordering::SeqCst);
    result
}

/// Snapshot Apple cookies from the webview and write cookies.json.
pub fn save_persisted_cookies(app: &AppHandle, window: &WebviewWindow) -> Result<(), String> {
    if APPLYING.load(Ordering::SeqCst) {
        return Ok(());
    }

    let cookies = snapshot_apple_cookies(window)?;
    let path = cookie_file_path(app)?;
    if should_skip_auth_clobber(&path, &cookies) {
        return Ok(());
    }

    let next = fingerprint(&cookies);
    {
        let mut fp = LAST_FINGERPRINT.lock().map_err(|e| e.to_string())?;
        if *fp == next {
            return Ok(());
        }
        *fp = next;
    }

    ensure_parent(&path)?;
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    let json = serde_json::to_string_pretty(&cookies).map_err(|e| e.to_string())?;
    fs::write(&tmp, json).map_err(|e| format!("write tmp: {e}"))?;
    fs::rename(&tmp, &path).map_err(|e| format!("rename: {e}"))?;
    Ok(())
}

/// Persist cookies immediately when the webview shows an authenticated session.
pub fn save_persisted_cookies_if_authed(
    app: &AppHandle,
    window: &WebviewWindow,
) -> Result<(), String> {
    if !webview_has_apple_auth(window)? {
        return Ok(());
    }
    save_persisted_cookies(app, window)
}

/// Wipe Apple session cookies + persisted cookies.json (and browsing data).
/// Call from a non-WebView2 UI thread on Windows.
pub fn clear_session(app: &AppHandle, window: &WebviewWindow) -> Result<(), String> {
    APPLYING.store(true, Ordering::SeqCst);
    let result = (|| {
        if let Ok(all) = window.cookies() {
            for cookie in all {
                let domain = cookie.domain().unwrap_or("");
                if is_apple_family_domain(domain) {
                    if let Err(e) = window.delete_cookie(cookie) {
                        eprintln!("[cookies] delete failed: {e}");
                    }
                }
            }
        }
        if let Err(e) = window.clear_all_browsing_data() {
            eprintln!("[cookies] clear browsing data: {e}");
        }

        let path = cookie_file_path(app)?;
        ensure_parent(&path)?;
        fs::write(&path, "[]").map_err(|e| format!("write cookies.json: {e}"))?;
        if let Ok(mut fp) = LAST_FINGERPRINT.lock() {
            *fp = String::new();
        }
        Ok(())
    })();
    APPLYING.store(false, Ordering::SeqCst);
    result
}

fn is_quitting(app: &AppHandle) -> bool {
    app.try_state::<Quitting>()
        .map(|q| q.get())
        .unwrap_or(false)
}

/// Periodic poll so we catch auth cookies without a cookie-changed event.
pub fn start_cookie_poller(app: AppHandle, window: WebviewWindow) {
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_secs(5));
        if is_quitting(&app) {
            break;
        }
        if APPLYING.load(Ordering::SeqCst) {
            continue;
        }
        if let Err(e) = save_persisted_cookies(&app, &window) {
            eprintln!("[cookies] poll: {e}");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_auth_cookie() -> StoredCookie {
        StoredCookie {
            url: "https://icloud.com/".into(),
            name: "X-APPLE-WEBAUTH-TOKEN".into(),
            value: "secret".into(),
            domain: ".icloud.com".into(),
            path: "/".into(),
            secure: true,
            http_only: true,
            expiration_date: 999_999_999,
            same_site: "lax".into(),
        }
    }

    fn write_cookie_file(path: &Path, cookies: &[StoredCookie]) {
        let parent = path.parent().unwrap();
        fs::create_dir_all(parent).unwrap();
        let json = serde_json::to_string_pretty(cookies).unwrap();
        fs::write(path, json).unwrap();
    }

    fn weak_cookie(name: &str) -> StoredCookie {
        StoredCookie {
            name: name.into(),
            ..sample_auth_cookie()
        }
    }

    #[test]
    fn has_apple_auth_requires_a_full_session_cookie() {
        assert!(has_apple_auth(&[sample_auth_cookie()]));
        assert!(has_apple_auth(&[weak_cookie(
            "x-apple-ds-web-session-token"
        )]));

        // Apple can leave a device-trust/partial jar after 2FA or a pending
        // Terms & Conditions prompt. Those cookies are not a web session.
        for name in [
            "MYACINFO",
            "aasp",
            "acn01",
            "DES123",
            "DSID",
            "X-APPLE-WEBAUTH-USER",
            "X-APPLE-WEBAUTH-HSA-TRUST",
            "X-APPLE-WEBAUTH-REPAIR",
            "X-APPLE-WEBAUTH-PCS-Notes",
            "X-APPLE-DS-WEB-SESSION",
            "pref",
        ] {
            assert!(
                !has_apple_auth(&[weak_cookie(name)]),
                "{name} is not proof of an iCloud session"
            );
        }
    }

    #[test]
    fn skip_auth_clobber_protects_saved_session() {
        let dir = std::env::temp_dir().join(format!("icloud-cookies-test-{}", std::process::id()));
        let path = dir.join("cookies.json");
        write_cookie_file(&path, &[sample_auth_cookie()]);

        assert!(should_skip_auth_clobber(&path, &[]));
        assert!(should_skip_auth_clobber(&path, &[weak_cookie("MYACINFO")]));
        assert!(should_skip_auth_clobber(
            &path,
            &[
                weak_cookie("X-APPLE-WEBAUTH-USER"),
                weak_cookie("X-APPLE-WEBAUTH-HSA-TRUST"),
                weak_cookie("X-APPLE-WEBAUTH-REPAIR"),
            ]
        ));
        assert!(!should_skip_auth_clobber(&path, &[sample_auth_cookie()]));

        // Weak identifiers alone must never protect a stale file; the user has
        // signed out and a fresh snapshot should be allowed to replace it.
        write_cookie_file(&path, &[weak_cookie("MYACINFO")]);
        assert!(!should_skip_auth_clobber(&path, &[]));

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn persist_domain_adds_leading_dot_for_apex_only() {
        assert_eq!(persist_domain("icloud.com"), ".icloud.com");
        assert_eq!(persist_domain(".icloud.com"), ".icloud.com");
        assert_eq!(persist_domain("www.icloud.com"), "www.icloud.com");
        assert_eq!(persist_domain("idmsa.apple.com"), "idmsa.apple.com");
        assert_eq!(persist_domain("apple.com"), ".apple.com");
        assert_eq!(persist_domain("icloud.com.cn"), ".icloud.com.cn");
        assert_eq!(persist_domain(".icloud.com.cn"), ".icloud.com.cn");
        assert_eq!(persist_domain("www.icloud.com.cn"), "www.icloud.com.cn");
        assert_eq!(persist_domain("idmsa.apple.com.cn"), "idmsa.apple.com.cn");
        assert_eq!(persist_domain("apple.com.cn"), ".apple.com.cn");
    }

    #[test]
    fn apple_family_domains_include_china_and_exclude_lookalikes() {
        for domain in [
            "www.icloud.com",
            ".icloud.com",
            "idmsa.apple.com",
            "www.icloud.com.cn",
            ".icloud.com.cn",
            "idmsa.apple.com.cn",
        ] {
            assert!(is_apple_family_domain(domain), "{domain}");
        }
        assert!(!is_apple_family_domain("icloud.com.example.com"));
        assert!(!is_apple_family_domain("notapple.com"));
    }

    #[test]
    fn china_auth_cookies_select_china_icloud_host() {
        assert_eq!(
            icloud_host_from_cookies(&[StoredCookie {
                domain: ".icloud.com.cn".into(),
                ..sample_auth_cookie()
            }]),
            Some(crate::ICLOUD_CN_HOST)
        );
        assert_eq!(
            icloud_host_from_cookies(&[StoredCookie {
                domain: "MYACINFO.example.net".into(),
                ..sample_auth_cookie()
            }]),
            None
        );
        assert_eq!(
            icloud_host_from_cookies(&[StoredCookie {
                domain: ".icloud.com.cn".into(),
                ..weak_cookie("MYACINFO")
            }]),
            None
        );
        assert_eq!(icloud_host_from_cookies(&[sample_auth_cookie()]), None);
    }

    #[test]
    fn normalize_restores_apex_dot_and_fresh_expiry() {
        let stored = normalize_stored_cookie(StoredCookie {
            url: String::new(),
            name: "X-APPLE-WEBAUTH-TOKEN".into(),
            value: "secret".into(),
            domain: "icloud.com".into(),
            path: String::new(),
            secure: true,
            http_only: true,
            expiration_date: 1,
            same_site: "lax".into(),
        });
        assert_eq!(stored.domain, ".icloud.com");
        assert_eq!(stored.path, "/");
        assert!(stored.expiration_date > now_unix());
        assert!(stored.url.starts_with("https://icloud.com"));
    }

    #[test]
    fn cookie_url_matches_electron() {
        assert_eq!(cookie_url(".icloud.com", "/"), "https://icloud.com/");
        assert_eq!(
            cookie_url("idmsa.apple.com", "/appleauth/auth"),
            "https://idmsa.apple.com/appleauth/auth"
        );
    }
}
