//! Shared Tauri shell for iCloud / Apple web wrappers.

mod cookies;
#[cfg(windows)]
mod cookies_webview2;

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;
use tauri::webview::{DownloadEvent, NewWindowResponse, PageLoadEvent};
use tauri::{
    AppHandle, Context, Listener, Manager, RunEvent, WebviewUrl, WebviewWindow,
    WebviewWindowBuilder,
};
use tauri_plugin_notification::NotificationExt;
use tauri_plugin_opener::OpenerExt;
use tauri_tray_base::{
    apply_opacity, apply_window_settings, enable_frameless_chrome, hide_main, install_state,
    on_window_event, save_settings, set_on_before_quit, setup_tray, show_main, with_common_plugins,
    Quitting, TrayBaseOptions, TrayBaseState, TrayClickAction, TrayExtraItem, TraySetupOptions,
    MAIN_WINDOW_LABEL,
};
use url::Url;

const SAFARI_UA: &str =
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/26.0 Safari/605.1.15";

pub(crate) const ICLOUD_HOST: &str = "www.icloud.com";
pub(crate) const ICLOUD_CN_HOST: &str = "www.icloud.com.cn";
const ICLOUD_HOST_SETTING: &str = "icloudHost";

/// Safari UA makes iCloud request Apple fonts. Remap to Segoe UI on Windows WebView2.
const FONT_FIX_SCRIPT: &str = r#"
(function () {
  var css = [
    '@font-face{font-family:"-apple-system";src:local("Segoe UI"),local("Arial");}',
    '@font-face{font-family:"BlinkMacSystemFont";src:local("Segoe UI"),local("Arial");}',
    '@font-face{font-family:"SF Pro Text";src:local("Segoe UI"),local("Arial");}',
    '@font-face{font-family:"SF Pro Display";src:local("Segoe UI"),local("Arial");}',
    '@font-face{font-family:"SF Pro";src:local("Segoe UI"),local("Arial");}',
    '@font-face{font-family:"SFNS Text";src:local("Segoe UI"),local("Arial");}',
    '@font-face{font-family:"SFNSDisplay";src:local("Segoe UI"),local("Arial");}',
    '@font-face{font-family:".SF NS Text";src:local("Segoe UI"),local("Arial");}',
    '@font-face{font-family:".SF NS Display";src:local("Segoe UI"),local("Arial");}',
    '@font-face{font-family:"Helvetica Neue";src:local("Segoe UI"),local("Arial");}',
    '@font-face{font-family:"Apple Color Emoji";src:local("Segoe UI Emoji"),local("Segoe UI Symbol");}',
    'html,body,p,span,div,li,td,th,input,textarea,[contenteditable],.ck-content,.ProseMirror{',
    'font-family:"Segoe UI","Helvetica Neue",Arial,sans-serif !important;',
    '}'
  ].join('');
  function inject() {
    var s = document.getElementById('icloud-win-font-fix');
    if (!s) {
      s = document.createElement('style');
      s.id = 'icloud-win-font-fix';
      (document.head || document.documentElement).appendChild(s);
    }
    s.textContent = css;
  }
  inject();
  if (!window.__icloudWinFontFix) {
    window.__icloudWinFontFix = true;
    document.addEventListener('DOMContentLoaded', inject);
  }
})();
"#;

/// Hide Apple's toolbar without removing it. iCloud measures the toolbar's
/// width to decide which buttons fit; deleting it (or using display:none) makes
/// iCloud recreate it, which shows up as a partially visible bar.
const TOOLBAR_HIDE_SCRIPT: &str = r#"
(function () {
  var STYLE_ID = 'icloud-win-toolbar-hide';
  var css = [
    '.cloudos-toolbar-view,header.cloudos-toolbar.app,header.cloudos-toolbar,',
    '.cw-toolbar,header.cloudos-toolbar .toolbar-view,.cloudos-toolbar-view .toolbar-view,[class*="cloudos-toolbar"]{',
    'position:absolute!important;top:0!important;left:0!important;width:100%!important;',
    'height:0!important;min-height:0!important;max-height:0!important;',
    'margin:0!important;padding:0!important;border:0!important;',
    'opacity:0!important;visibility:hidden!important;pointer-events:none!important;',
    'z-index:-1!important;',
    '}'
  ].join('');
  function inject() {
    var s = document.getElementById(STYLE_ID);
    if (!s) {
      s = document.createElement('style');
      s.id = STYLE_ID;
      (document.head || document.documentElement).appendChild(s);
    }
    s.textContent = css;
  }
  inject();
  if (!window.__icloudBaseToolbarObserver) {
    window.__icloudBaseToolbarObserver = new MutationObserver(inject);
    window.__icloudBaseToolbarObserver.observe(document.documentElement, {
      childList: true,
      subtree: true
    });
  }
})();
"#;

#[cfg(windows)]
const WINDOW_CHROME_SCRIPT: &str = r#"
(function () {
  // WebView2 always runs init scripts in subframes; never paint window chrome
  // inside Apple's auth iframe (shows min/max/close inside the sign-in sheet).
  if (window !== window.top) return;
  if (window.__icloudWinChrome) return;
  window.__icloudWinChrome = true;

  var STYLE_ID = 'icloud-win-chrome-style';
  var ROOT_ID = 'icloud-win-chrome';
  var css = [
    /* Keep Apple's toolbar measurable but invisible — iCloud uses its width to
       lay out toolbar buttons, and removing it makes it recreate half a bar. */
    '.cloudos-toolbar-view,header.cloudos-toolbar.app,header.cloudos-toolbar,',
    '.cw-toolbar,header.cloudos-toolbar .toolbar-view,.cloudos-toolbar-view .toolbar-view,[class*="cloudos-toolbar"]{',
    'position:absolute!important;top:0!important;left:0!important;width:100%!important;',
    'height:0!important;min-height:0!important;max-height:0!important;',
    'margin:0!important;padding:0!important;border:0!important;',
    'opacity:0!important;visibility:hidden!important;pointer-events:none!important;',
    'z-index:-1!important;',
    '}',
    'html{--icloud-win-chrome-h:28px;height:100%!important;overflow:hidden!important;}',
    /* Reserve the bar with a border instead of padding. The transform makes
       iCloud's position:fixed panes start below the bar instead of being
       clipped by 28px at the bottom of the page. */
    'body{',
    'height:100%!important;max-height:100%!important;margin:0!important;',
    'border-top:var(--icloud-win-chrome-h) solid transparent!important;',
    'box-sizing:border-box!important;transform:translateZ(0)!important;',
    'overflow:hidden!important;',
    '}',
    '#' + ROOT_ID + '{',
    'position:fixed;top:0;left:0;right:0;height:var(--icloud-win-chrome-h);z-index:2147483646;',
    'display:flex;align-items:stretch;justify-content:flex-end;',
    'box-sizing:border-box;pointer-events:none;',
    'background:#121212;color:#b3b3b3;',
    'border-bottom:1px solid rgba(255,255,255,0.08);',
    'font-family:"Segoe UI",system-ui,sans-serif;',
    '}',
    /* Drag only on the filler — nesting no-drag buttons under drag is flaky on WebView2 */
    '#' + ROOT_ID + ' .icloud-win-chrome-drag{',
    'flex:1 1 auto;min-width:0;pointer-events:auto;',
    '-webkit-app-region:drag;app-region:drag;',
    '}',
    '#' + ROOT_ID + ' .icloud-win-chrome-controls{',
    'display:flex;align-items:stretch;flex:0 0 auto;height:100%;',
    'pointer-events:auto;-webkit-app-region:no-drag;app-region:no-drag;',
    '}',
    '@media (prefers-color-scheme: light){',
    '#' + ROOT_ID + '{background:#f5f5f7;color:#6e6e73;',
    'border-bottom-color:rgba(0,0,0,0.08);}',
    '}',
    '#' + ROOT_ID + ' .icloud-win-chrome-btn{',
    'width:46px;border:0;background:transparent;color:inherit;cursor:default;',
    'display:inline-flex;align-items:center;justify-content:center;padding:0;',
    '-webkit-app-region:no-drag;app-region:no-drag;',
    '}',
    '#' + ROOT_ID + ' .icloud-win-chrome-btn--refresh svg{',
    'width:12px;height:12px;',
    '}',
    '#' + ROOT_ID + ' .icloud-win-chrome-btn:hover{',
    'background:rgba(255,255,255,0.08);color:#fff;',
    '}',
    '@media (prefers-color-scheme: light){',
    '#' + ROOT_ID + ' .icloud-win-chrome-btn:hover{',
    'background:rgba(0,0,0,0.06);color:#1d1d1f;}',
    '}',
    '#' + ROOT_ID + ' .icloud-win-chrome-btn--close:hover{',
    'background:#e5484d;color:#fff;',
    '}',
    '#' + ROOT_ID + ' svg{display:block;pointer-events:none;width:10px;height:10px;}'
  ].join('');

  function ensureStyle() {
    var s = document.getElementById(STYLE_ID);
    if (!s) {
      s = document.createElement('style');
      s.id = STYLE_ID;
      (document.head || document.documentElement).appendChild(s);
    }
    s.textContent = css;
  }

  // Same-origin path — WebView2 always delivers NavigationStarting; we cancel it.
  // (Custom schemes are dropped; fake downloads triggered "download failed" toasts.)
  var CHROME_PATH = '/__icloud_win_chrome__/';
  var lastActionAt = 0;
  function action(name) {
    var now = Date.now();
    if (now - lastActionAt < 250) return;
    lastActionAt = now;
    try {
      var u = new URL(window.location.href);
      u.pathname = CHROME_PATH + name;
      u.search = '';
      u.hash = '';
      window.location.assign(u.href);
    } catch (_) {}
  }

  function ensureChrome() {
    ensureStyle();
    var root = document.getElementById(ROOT_ID);
    if (root) return;
    root = document.createElement('div');
    root.id = ROOT_ID;
    root.setAttribute('role', 'banner');
    root.setAttribute('aria-label', 'Window');
    root.innerHTML = [
      '<div class="icloud-win-chrome-drag" aria-hidden="true"></div>',
      '<div class="icloud-win-chrome-controls">',
      '<button type="button" class="icloud-win-chrome-btn icloud-win-chrome-btn--refresh" data-action="refresh" title="Refresh" aria-label="Refresh">',
      '<svg viewBox="0 0 24 24"><path fill="currentColor" d="M17.65 6.35C16.2 4.9 14.21 4 12 4c-4.42 0-7.99 3.58-7.99 8s3.57 8 7.99 8c3.73 0 6.84-2.55 7.73-6h-2.08c-.82 2.33-3.04 4-5.65 4-3.31 0-6-2.69-6-6s2.69-6 6-6c1.66 0 3.14.69 4.22 1.78L13 11h7V4l-2.35 2.35z"/></svg></button>',
      '<button type="button" class="icloud-win-chrome-btn" data-action="minimize" title="Minimize" aria-label="Minimize">',
      '<svg viewBox="0 0 10 10"><path fill="currentColor" d="M1 5h8v1H1z"/></svg></button>',
      '<button type="button" class="icloud-win-chrome-btn" data-action="maximize" title="Maximize" aria-label="Maximize">',
      '<svg viewBox="0 0 10 10"><path fill="currentColor" d="M1.5 1.5h7v7h-7zm1 1v5h5v-5z"/></svg></button>',
      '<button type="button" class="icloud-win-chrome-btn icloud-win-chrome-btn--close" data-action="close" title="Close" aria-label="Close">',
      '<svg viewBox="0 0 10 10"><path fill="currentColor" d="M1.2 2.1 2.1 1.2 5 4.1 7.9 1.2 8.8 2.1 5.9 5l2.9 2.9-.9.9L5 5.9 2.1 8.8l-.9-.9L4.1 5z"/></svg></button>',
      '</div>'
    ].join('');
    // pointerdown fires before WebView2 drag-region hit-testing can swallow click
    function onControl(e) {
      if (e.button != null && e.button !== 0) return;
      var btn = e.target && e.target.closest ? e.target.closest('[data-action]') : null;
      if (!btn || !root.contains(btn)) return;
      e.preventDefault();
      e.stopPropagation();
      action(btn.getAttribute('data-action'));
    }
    root.addEventListener('pointerdown', onControl, true);
    root.addEventListener('click', onControl, true);
    // body is transformed to reserve the bar, which would make a fixed child
    // relative to the content area. Keep the chrome outside that containing block.
    (document.documentElement || document.body).appendChild(root);
  }

  function boot() {
    ensureChrome();
    if (!window.__icloudWinChromeObserver) {
      var scheduled = false;
      window.__icloudWinChromeObserver = new MutationObserver(function () {
        if (scheduled) return;
        scheduled = true;
        requestAnimationFrame(function () {
          scheduled = false;
          ensureChrome();
        });
      });
      window.__icloudWinChromeObserver.observe(document.documentElement, {
        childList: true,
        subtree: true
      });
    }
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', boot);
  } else {
    boot();
  }
})();
"#;

const CHROME_PATH_MARKER: &str = "/__icloud_win_chrome__/";

fn chrome_action_name(name: &str) -> Option<&str> {
    match name {
        "minimize" | "maximize" | "close" | "refresh" => Some(name),
        _ => None,
    }
}

fn chrome_action(url: &Url) -> Option<&str> {
    if let Some(idx) = url.path().find(CHROME_PATH_MARKER) {
        let rest = &url.path()[idx + CHROME_PATH_MARKER.len()..];
        let action = rest.split('/').next().unwrap_or("").trim_matches('/');
        return chrome_action_name(action);
    }
    None
}

fn handle_chrome_action(app: &AppHandle, action: &str, url: &Url) {
    if action == "close" {
        hide_main(app);
        return;
    }
    if action == "refresh" {
        // Reuse the tray's refresh path: navigate back to the app URL so iCloud
        // reloads its data instead of reloading a possibly stale sub-view.
        let handle = app.clone();
        let target = url.clone();
        std::thread::spawn(move || refresh_page(&handle, target));
        return;
    }
    let handle = app.clone();
    let action = action.to_string();
    let _ = app.run_on_main_thread(move || {
        let Some(window) = handle.get_webview_window(MAIN_WINDOW_LABEL) else {
            return;
        };
        match action.as_str() {
            "minimize" => {
                let _ = window.minimize();
            }
            "maximize" => {
                let maximized = window.is_maximized().unwrap_or(false);
                if maximized {
                    let _ = window.unmaximize();
                } else {
                    let _ = window.maximize();
                }
            }
            _ => {}
        }
    });
}

fn force_full_opacity(app: &AppHandle) {
    if let Some(state) = app.try_state::<TrayBaseState>() {
        let mut settings = state.settings.lock();
        if (settings.opacity - 1.0).abs() > f64::EPSILON {
            settings.opacity = 1.0;
            let _ = save_settings(&state.settings_path, &settings);
        }
    }
    apply_opacity(app, 1.0);
}

/// Product config for an iCloud / Apple web wrapper.
#[derive(Debug, Clone, Copy)]
pub struct IcloudAppConfig {
    pub app_name: &'static str,
    pub url: &'static str,
    /// Unused — Apple's cloudos toolbar is always stripped.
    pub remove_toolbar: bool,
}

fn parse_app_url(url: &str) -> Url {
    Url::parse(url)
        .unwrap_or_else(|_| Url::parse(&format!("https://{ICLOUD_HOST}")).expect("fallback"))
}

/// The iCloud host the user actually signed in to. Mainland China accounts are
/// routed to icloud.com.cn by Apple; restarting on www.icloud.com would look
/// logged out and repeat the redirect/login loop.
fn canonical_app_host(host: &str) -> Option<&'static str> {
    match host.trim_start_matches('.').to_ascii_lowercase().as_str() {
        "icloud.com" | "www.icloud.com" => Some(ICLOUD_HOST),
        "icloud.com.cn" | "www.icloud.com.cn" => Some(ICLOUD_CN_HOST),
        _ => None,
    }
}

fn app_url_with_host(url: &Url, host: &str) -> Option<Url> {
    let mut next = url.clone();
    next.set_host(Some(host)).ok()?;
    Some(next)
}

fn saved_app_host(app: &AppHandle) -> Option<&'static str> {
    let state = app.try_state::<TrayBaseState>()?;
    let settings = state.settings.lock();
    let host = settings.extra.get(ICLOUD_HOST_SETTING)?.as_str()?;
    canonical_app_host(host)
}

fn remember_app_host(app: &AppHandle, host: &str) {
    let Some(canonical) = canonical_app_host(host) else {
        return;
    };
    let Some(state) = app.try_state::<TrayBaseState>() else {
        return;
    };
    let mut settings = state.settings.lock();
    if settings
        .extra
        .get(ICLOUD_HOST_SETTING)
        .and_then(|v| v.as_str())
        == Some(canonical)
    {
        return;
    }
    settings
        .extra
        .insert(ICLOUD_HOST_SETTING.into(), json!(canonical));
    if let Err(e) = save_settings(&state.settings_path, &settings) {
        eprintln!("[icloud] save host preference: {e}");
    }
}

/// Drop a remembered region when the user signs out, so switching Apple IDs
/// does not keep sending the app to a host that belongs to the old account.
fn forget_app_host(app: &AppHandle) {
    let Some(state) = app.try_state::<TrayBaseState>() else {
        return;
    };
    let mut settings = state.settings.lock();
    if settings.extra.remove(ICLOUD_HOST_SETTING).is_none() {
        return;
    }
    if let Err(e) = save_settings(&state.settings_path, &settings) {
        eprintln!("[icloud] clear host preference: {e}");
    }
}

/// Resolve the configured app URL against the remembered (or inferred) region.
fn resolve_app_url(app: &AppHandle, configured: &str) -> Url {
    let base = parse_app_url(configured);
    // Auth cookies are stronger evidence than a preference written before Apple
    // redirected the user to their regional host.
    let Some(host) = cookies::persisted_icloud_host(app).or_else(|| saved_app_host(app)) else {
        return base;
    };
    if let Some(next) = app_url_with_host(&base, host) {
        // Persist the inferred host so later launches skip the cookie scan.
        remember_app_host(app, host);
        return next;
    }
    base
}

type SharedAppUrl = Arc<Mutex<Url>>;

fn current_app_url(shared: &SharedAppUrl) -> Url {
    shared.lock().unwrap().clone()
}

/// Switch the in-memory target when Apple redirects the user to their regional
/// iCloud host, so Refresh and post-sign-in restores stay on that host.
fn set_app_url(app: &AppHandle, shared: &SharedAppUrl, host: &str) {
    let Some(canonical) = canonical_app_host(host) else {
        return;
    };
    remember_app_host(app, canonical);
    let mut current = shared.lock().unwrap();
    if current.host_str() == Some(canonical) {
        return;
    }
    if let Some(next) = app_url_with_host(&current, canonical) {
        *current = next;
    }
}

fn unique_download_path(path: PathBuf) -> PathBuf {
    if !path.exists() {
        return path;
    }
    let parent = path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("download");
    let ext = path.extension().and_then(|s| s.to_str());
    for i in 1..10_000 {
        let name = match ext {
            Some(e) => format!("{stem} ({i}).{e}"),
            None => format!("{stem} ({i})"),
        };
        let candidate = parent.join(name);
        if !candidate.exists() {
            return candidate;
        }
    }
    path
}

fn downloads_destination(app: &AppHandle, suggested: &Path, url: &Url) -> PathBuf {
    let dir = app.path().download_dir().unwrap_or_else(|_| {
        std::env::var_os("USERPROFILE")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
            .join("Downloads")
    });
    let _ = std::fs::create_dir_all(&dir);

    let name = suggested
        .file_name()
        .map(|n| n.to_os_string())
        .filter(|n| !n.is_empty())
        .or_else(|| {
            url.path_segments()
                .and_then(|mut parts| parts.next_back())
                .filter(|s| !s.is_empty() && *s != "/")
                .map(OsString::from)
        })
        .unwrap_or_else(|| OsString::from("download"));

    unique_download_path(dir.join(name))
}

fn notify_download(app: &AppHandle, app_name: &str, path: Option<&Path>, success: bool) {
    let file_name = path
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("file");
    let (title, body) = if success {
        (
            format!("{app_name} download complete"),
            format!("{file_name} saved to Downloads"),
        )
    } else {
        (
            format!("{app_name} download failed"),
            format!("Could not download {file_name}"),
        )
    };
    let _ = app.notification().builder().title(title).body(body).show();
}

fn is_icloud_home_redirect(url: &Url, app_url: &Url) -> bool {
    let Some(app_host) = app_url.host_str() else {
        return false;
    };
    if canonical_app_host(app_host).is_none() {
        return false;
    }
    let app_path = app_url.path().trim_end_matches('/');
    if app_path.is_empty() || app_path == "/" {
        return false;
    }

    let Some(host) = url.host_str() else {
        return false;
    };
    if canonical_app_host(host).is_none() {
        return false;
    }
    let path = url.path().trim_end_matches('/');
    path.is_empty() || path == "/"
}

fn persist_cookies(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(MAIN_WINDOW_LABEL) {
        if let Err(e) = cookies::save_persisted_cookies(app, &window) {
            eprintln!("[cookies] persist: {e}");
        }
    }
}

const RESTORE_RETRY_DELAYS_MS: [u64; 10] = [50, 100, 200, 400, 800, 1600, 3200, 5000, 8000, 12000];

fn is_apple_family_url(url: &Url) -> bool {
    url.host_str()
        .map(cookies::is_apple_family_domain)
        .unwrap_or(false)
}

fn is_apple_sign_in_url(url: &Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    let host = host.trim_start_matches('.').to_ascii_lowercase();
    matches!(
        host.as_str(),
        "idmsa.apple.com"
            | "appleid.apple.com"
            | "signin.apple.com"
            | "idmsa.apple.com.cn"
            | "appleid.apple.com.cn"
            | "signin.apple.com.cn"
    )
}

fn restore_session_then_navigate(app: AppHandle, window: WebviewWindow, url: Url) {
    std::thread::spawn(move || {
        let mut restored = false;
        for delay_ms in RESTORE_RETRY_DELAYS_MS {
            std::thread::sleep(Duration::from_millis(delay_ms));
            match cookies::load_persisted_cookies(&app, &window) {
                Ok(true) => {
                    eprintln!("[cookies] restored persisted Apple cookies");
                    restored = true;
                    break;
                }
                Ok(false) => {
                    eprintln!("[cookies] no persisted cookies");
                    break;
                }
                Err(e) => eprintln!("[cookies] restore retry: {e}"),
            }
        }
        if restored {
            // Give CookieManager a beat before the first real navigation so
            // www.icloud.com receives the restored jar.
            std::thread::sleep(Duration::from_millis(300));
        }
        if let Err(e) = window.navigate(url) {
            eprintln!("[cookies] navigate after restore: {e}");
        }
        cookies::start_cookie_poller(app, window);
    });
}

fn refresh_page(app: &AppHandle, url: Url) {
    if let Some(window) = app.get_webview_window(MAIN_WINDOW_LABEL) {
        let _ = window.navigate(url);
    }
}

fn sign_out(app: AppHandle, url: Url) {
    std::thread::spawn(move || {
        let Some(window) = app.get_webview_window(MAIN_WINDOW_LABEL) else {
            return;
        };
        if let Err(e) = cookies::clear_session(&app, &window) {
            eprintln!("[cookies] sign out: {e}");
        }
        forget_app_host(&app);
        std::thread::sleep(Duration::from_millis(50));
        if let Err(e) = window.navigate(url) {
            eprintln!("[cookies] navigate after sign out: {e}");
        }
        show_main(&app);
    });
}

fn build_main_window(
    app: &AppHandle,
    config: IcloudAppConfig,
    target: &SharedAppUrl,
) -> tauri::Result<WebviewWindow> {
    let app_data = app
        .path()
        .app_data_dir()
        .unwrap_or_else(|_| PathBuf::from("."));
    let webview_data = app_data.join("webview");
    let _ = std::fs::create_dir_all(&webview_data);

    let blank_url = Url::parse("about:blank").expect("about:blank");
    let app_for_nav = app.clone();
    let app_for_chrome = app.clone();
    let app_for_chrome_nw = app.clone();
    let chrome_nav_target = target.clone();
    let app_for_open = app.clone();
    let app_for_download = app.clone();
    let app_for_cookies = app.clone();
    let app_for_auth_window = app.clone();
    let page_load_target = target.clone();
    let nav_target = target.clone();
    let app_name = config.app_name;

    let mut builder =
        WebviewWindowBuilder::new(app, MAIN_WINDOW_LABEL, WebviewUrl::External(blank_url))
            .title(config.app_name)
            .inner_size(1280.0, 800.0)
            .resizable(true)
            .visible(false)
            .user_agent(SAFARI_UA)
            .data_directory(webview_data)
            .initialization_script_for_all_frames(FONT_FIX_SCRIPT)
            .initialization_script_for_all_frames(TOOLBAR_HIDE_SCRIPT);

    // Frameless before create — set_decorations(false) after build can lose to the
    // initial Win32 frame, which stacks native caption buttons under our chrome.
    #[cfg(windows)]
    {
        builder = builder
            .decorations(false)
            .shadow(true)
            .initialization_script(WINDOW_CHROME_SCRIPT);
    }

    let window = builder
        .on_page_load(move |window, payload| {
            let _ = window.eval(FONT_FIX_SCRIPT);
            let _ = window.eval(TOOLBAR_HIDE_SCRIPT);
            #[cfg(windows)]
            {
                let _ = window.eval(WINDOW_CHROME_SCRIPT);
            }
            if payload.event() == PageLoadEvent::Finished && is_apple_family_url(payload.url()) {
                if let Some(host) = payload.url().host_str() {
                    set_app_url(&app_for_cookies, &page_load_target, host);
                }
                let app = app_for_cookies.clone();
                let win = window.clone();
                std::thread::spawn(move || {
                    if let Err(e) = cookies::save_persisted_cookies_if_authed(&app, &win) {
                        eprintln!("[cookies] save on auth: {e}");
                    }
                });
            }
        })
        .on_navigation(move |url| {
            if let Some(action) = chrome_action(url) {
                handle_chrome_action(&app_for_chrome, action, &current_app_url(&nav_target));
                return false;
            }
            if let Some(host) = url.host_str() {
                set_app_url(&app_for_nav, &nav_target, host);
            }
            let target = current_app_url(&nav_target);
            if is_icloud_home_redirect(url, &target) {
                let handle = app_for_nav.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_millis(200));
                    let Some(w) = handle.get_webview_window(MAIN_WINDOW_LABEL) else {
                        return;
                    };
                    if cookies::webview_has_apple_auth(&w).unwrap_or(false) {
                        let _ = w.navigate(target);
                    }
                });
                // Never cancel `/` — Apple ID sign-in returns here. Cancelling it
                // loops the user back to the Sign In button.
                return true;
            }
            true
        })
        .on_download(move |_webview, event| match event {
            DownloadEvent::Requested { url, destination } => {
                *destination = downloads_destination(&app_for_download, destination, &url);
                true
            }
            DownloadEvent::Finished { path, success, .. } => {
                notify_download(&app_for_download, app_name, path.as_deref(), success);
                true
            }
            _ => true,
        })
        .on_new_window(move |url, _features| {
            if let Some(action) = chrome_action(&url) {
                handle_chrome_action(
                    &app_for_chrome_nw,
                    action,
                    &current_app_url(&chrome_nav_target),
                );
                return NewWindowResponse::Deny;
            }
            if let Some(host) = url.host_str() {
                set_app_url(&app_for_auth_window, &chrome_nav_target, host);
            }
            // Apple ID sign-in often window.open()s idmsa. Keep it in this webview
            // instead of the system browser (which leaves the app stuck on Sign In).
            if is_apple_family_url(&url) || is_apple_sign_in_url(&url) {
                let handle = app_for_auth_window.clone();
                let target = url.clone();
                std::thread::spawn(move || {
                    if let Some(w) = handle.get_webview_window(MAIN_WINDOW_LABEL) {
                        if let Err(e) = w.navigate(target) {
                            eprintln!("[cookies] navigate auth window: {e}");
                        }
                    }
                });
                return NewWindowResponse::Deny;
            }
            let href = url.as_str().to_string();
            if href != "about:blank#blocked" && !href.starts_with("about:blank") {
                let _ = app_for_open.opener().open_url(href, None::<&str>);
            }
            NewWindowResponse::Deny
        })
        .build()?;

    #[cfg(windows)]
    {
        // Belt-and-suspenders: opacity / FRAMECHANGED can revive the native frame.
        let _ = window.set_decorations(false);
        let _ = window.set_shadow(true);
        // Frameless WebView2 can show the desktop through page chrome unless the
        // controller has an opaque default background.
        let _ = window.set_background_color(Some(tauri::window::Color::from((
            255u8, 255u8, 255u8, 255u8,
        ))));
    }

    Ok(window)
}

/// Run a tray-first iCloud/Apple web wrapper.
///
/// `context` must come from the **app** crate via `tauri::generate_context!()`.
pub fn run(config: IcloudAppConfig, context: Context) {
    let builder =
        with_common_plugins(tauri::Builder::default()).plugin(tauri_plugin_notification::init());

    builder
        .setup(move |app| {
            let handle = app.handle().clone();

            install_state(
                &handle,
                TrayBaseOptions {
                    app_name: config.app_name.into(),
                    show_hide: false,
                    show_always_on_top: false,
                    tray_on_click: TrayClickAction::Toggle,
                    defaults: HashMap::from([("opacity".into(), json!(1.0))]),
                    extra_tray_items: vec![
                        TrayExtraItem {
                            id: "refresh".into(),
                            label: "Refresh".into(),
                        },
                        TrayExtraItem {
                            id: "sign-out".into(),
                            label: "Sign Out".into(),
                        },
                    ],
                    ..Default::default()
                },
            )?;

            let app_url = resolve_app_url(&handle, config.url);
            let target = Arc::new(Mutex::new(app_url.clone()));
            let window = build_main_window(&handle, config, &target)?;

            setup_tray(
                &handle,
                TraySetupOptions {
                    tooltip: Some(config.app_name.into()),
                },
            )?;

            apply_window_settings(&handle);
            // Web wrappers must stay fully opaque. tray-base defaults to 0.94, and
            // layered opacity + frameless makes Apple UIs look transparent.
            force_full_opacity(&handle);
            // Re-assert after opacity (SWP_FRAMECHANGED can restore native captions).
            enable_frameless_chrome(&handle);

            let quit_app = handle.clone();
            set_on_before_quit(&handle, move || {
                persist_cookies(&quit_app);
            });

            restore_session_then_navigate(handle.clone(), window, app_url.clone());

            let action_handle = handle.clone();
            let action_target = target.clone();
            handle.listen("tray:action", move |event| {
                match event.payload().trim_matches('"') {
                    "refresh" => refresh_page(&action_handle, current_app_url(&action_target)),
                    "sign-out" => sign_out(action_handle.clone(), current_app_url(&action_target)),
                    _ => {}
                }
            });

            Ok(())
        })
        .on_window_event(|window, event| on_window_event(window, &event))
        .build(context)
        .expect("error while building tauri application")
        .run(|app_handle, event| {
            // Windows restart/shutdown sends ExitRequested without the tray quit
            // path. Prevent exit, flush cookies.json, then exit once.
            if let RunEvent::ExitRequested { api, .. } = &event {
                let already_quitting = app_handle
                    .try_state::<Quitting>()
                    .map(|q| q.get())
                    .unwrap_or(false);
                if already_quitting {
                    return;
                }
                static EXIT_FLUSH_STARTED: AtomicBool = AtomicBool::new(false);
                if EXIT_FLUSH_STARTED.swap(true, Ordering::SeqCst) {
                    return;
                }
                api.prevent_exit();
                let app = app_handle.clone();
                let _ = std::thread::Builder::new()
                    .name("exit-persist".into())
                    .spawn(move || {
                        if let Some(q) = app.try_state::<Quitting>() {
                            q.set(true);
                        }
                        persist_cookies(&app);
                        app.exit(0);
                    });
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_host_normalizes_china_and_standard() {
        assert_eq!(canonical_app_host("www.icloud.com"), Some(ICLOUD_HOST));
        assert_eq!(canonical_app_host("icloud.com"), Some(ICLOUD_HOST));
        assert_eq!(
            canonical_app_host("www.icloud.com.cn"),
            Some(ICLOUD_CN_HOST)
        );
        assert_eq!(canonical_app_host(".icloud.com.cn"), Some(ICLOUD_CN_HOST));
        assert_eq!(canonical_app_host("www.example.com"), None);
    }

    #[test]
    fn app_url_with_host_keeps_the_app_path() {
        let url = parse_app_url("https://www.icloud.com/reminders");
        let china = app_url_with_host(&url, ICLOUD_CN_HOST).expect("china url");
        assert_eq!(china.as_str(), "https://www.icloud.com.cn/reminders");
    }

    #[test]
    fn home_redirect_is_detected_on_china_domain() {
        let target = parse_app_url("https://www.icloud.com.cn/reminders");
        let root = parse_app_url("https://www.icloud.com.cn/");
        assert!(is_icloud_home_redirect(&root, &target));

        let app_page = parse_app_url("https://www.icloud.com.cn/reminders");
        assert!(!is_icloud_home_redirect(&app_page, &target));
    }

    #[test]
    fn china_apple_sign_in_hosts_are_recognized() {
        let sign_in = parse_app_url("https://idmsa.apple.com.cn/appleauth/auth/signin");
        assert!(is_apple_sign_in_url(&sign_in));
        let icloud_cn = parse_app_url("https://www.icloud.com.cn/");
        assert!(!is_apple_sign_in_url(&icloud_cn));
    }
}
