//! WebView2 CookieManager restore that keeps a leading `.` on apex domains.
//! Tauri's `cookie::Cookie::domain()` strips it, which makes host-only cookies.

use std::sync::mpsc;
use std::time::Duration;

use tauri::WebviewWindow;
use webview2_com::Microsoft::Web::WebView2::Win32::{
    ICoreWebView2_2, COREWEBVIEW2_COOKIE_SAME_SITE_KIND_LAX,
    COREWEBVIEW2_COOKIE_SAME_SITE_KIND_NONE, COREWEBVIEW2_COOKIE_SAME_SITE_KIND_STRICT,
};
use windows::core::{Interface, HSTRING};

use crate::cookies::StoredCookie;

/// Apply all cookies in a single `with_webview` hop (cold-start safe).
pub fn set_stored_cookies(
    window: &WebviewWindow,
    cookies: &[StoredCookie],
) -> Result<usize, String> {
    let cookies = cookies.to_vec();
    let (tx, rx) = mpsc::channel();
    window
        .with_webview(move |webview| {
            let result = unsafe { set_cookies_on_manager(&webview, &cookies) };
            let _ = tx.send(result);
        })
        .map_err(|e| format!("with_webview: {e}"))?;
    rx.recv_timeout(Duration::from_secs(20))
        .map_err(|e| format!("cookie set wait: {e}"))?
}

unsafe fn set_cookies_on_manager(
    webview: &tauri::webview::PlatformWebview,
    cookies: &[StoredCookie],
) -> Result<usize, String> {
    let core = webview
        .controller()
        .CoreWebView2()
        .map_err(|e| format!("CoreWebView2: {e}"))?;
    let core2: ICoreWebView2_2 = core
        .cast()
        .map_err(|e| format!("ICoreWebView2_2: {e}"))?;
    let manager = core2
        .CookieManager()
        .map_err(|e| format!("CookieManager: {e}"))?;

    let mut applied = 0usize;
    let mut errors: Vec<String> = Vec::new();

    for stored in cookies {
        if stored.name.is_empty() || stored.domain.is_empty() {
            continue;
        }
        match set_one_cookie(&manager, stored) {
            Ok(()) => applied += 1,
            Err(e) => errors.push(e),
        }
    }

    if applied == 0 {
        if errors.is_empty() {
            return Err("no cookies to apply".into());
        }
        return Err(errors.join("; "));
    }
    if !errors.is_empty() {
        eprintln!(
            "[cookies] webview2 applied {applied}, {} failed (e.g. {})",
            errors.len(),
            errors.first().map(String::as_str).unwrap_or("")
        );
    }
    Ok(applied)
}

unsafe fn set_one_cookie(
    manager: &webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2CookieManager,
    stored: &StoredCookie,
) -> Result<(), String> {
    let name = HSTRING::from(stored.name.as_str());
    let value = HSTRING::from(stored.value.as_str());
    let domain = HSTRING::from(stored.domain.as_str());
    let path = HSTRING::from(stored.path.as_str());
    let cookie = manager
        .CreateCookie(&name, &value, &domain, &path)
        .map_err(|e| format!("CreateCookie {}: {e}", stored.name))?;

    // SetExpires marks the cookie persistent so it survives WebView2 process death.
    // Session cookies from Apple are given a TTL in cookies.json before restore.
    cookie
        .SetExpires(stored.expiration_date as f64)
        .map_err(|e| format!("SetExpires {}: {e}", stored.name))?;
    cookie
        .SetIsHttpOnly(stored.http_only)
        .map_err(|e| format!("SetIsHttpOnly {}: {e}", stored.name))?;
    cookie
        .SetIsSecure(stored.secure)
        .map_err(|e| format!("SetIsSecure {}: {e}", stored.name))?;

    let same_site = match stored.same_site.as_str() {
        "no_restriction" | "none" => COREWEBVIEW2_COOKIE_SAME_SITE_KIND_NONE,
        "strict" => COREWEBVIEW2_COOKIE_SAME_SITE_KIND_STRICT,
        _ => COREWEBVIEW2_COOKIE_SAME_SITE_KIND_LAX,
    };
    cookie
        .SetSameSite(same_site)
        .map_err(|e| format!("SetSameSite {}: {e}", stored.name))?;

    manager
        .AddOrUpdateCookie(&cookie)
        .map_err(|e| format!("AddOrUpdateCookie {}: {e}", stored.name))?;
    Ok(())
}
