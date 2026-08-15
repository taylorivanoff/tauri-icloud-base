//! Shared Tauri shell for iCloud / Apple web wrappers.

mod cookies;

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::json;
use tauri::webview::{DownloadEvent, NewWindowResponse};
use tauri::{
    AppHandle, Context, Listener, Manager, RunEvent, WebviewUrl, WebviewWindow, WebviewWindowBuilder,
};
use tauri_plugin_notification::NotificationExt;
use tauri_plugin_opener::OpenerExt;
use tauri_tray_base::{
    apply_opacity, apply_window_settings, enable_frameless_chrome, hide_main, install_state,
    on_window_event, save_settings, set_on_before_quit, setup_tray, show_main, with_common_plugins,
    TrayBaseOptions, TrayBaseState, TrayClickAction, TrayExtraItem, TraySetupOptions,
    MAIN_WINDOW_LABEL,
};
use url::Url;

const SAFARI_UA: &str =
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/26.0 Safari/605.1.15";

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

const TOOLBAR_REMOVE_SCRIPT: &str = r#"
(function () {
  const SELECTORS = [
    '.cloudos-toolbar-view',
    'header.cloudos-toolbar.app',
    'header.cloudos-toolbar'
  ];
  function removeToolbar(root) {
    root = root || document;
    for (const selector of SELECTORS) {
      root.querySelectorAll?.(selector)?.forEach((el) => el.remove());
    }
    for (const frame of root.querySelectorAll?.('iframe') || []) {
      try {
        const doc = frame.contentDocument;
        if (doc) removeToolbar(doc);
      } catch (_) {}
    }
  }
  removeToolbar();
  if (!window.__icloudBaseToolbarObserver) {
    let scheduled = false;
    window.__icloudBaseToolbarObserver = new MutationObserver(() => {
      if (scheduled) return;
      scheduled = true;
      requestAnimationFrame(() => {
        scheduled = false;
        removeToolbar();
      });
    });
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
    /* Collapse Apple's own top chrome — it leaves a dead gap under our bar */
    '.cloudos-toolbar-view,header.cloudos-toolbar.app,header.cloudos-toolbar,',
    '.cw-toolbar,.toolbar-view,[class*="cloudos-toolbar"]{',
    'display:none!important;height:0!important;min-height:0!important;',
    'margin:0!important;padding:0!important;overflow:hidden!important;border:0!important;',
    '}',
    'html{--icloud-win-chrome-h:28px;height:100%!important;overflow:hidden!important;}',
    'body{',
    'height:100%!important;max-height:100%!important;margin:0!important;',
    'padding-top:var(--icloud-win-chrome-h)!important;box-sizing:border-box!important;',
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

  function stripAppleToolbar(root) {
    root = root || document;
    var selectors = [
      '.cloudos-toolbar-view',
      'header.cloudos-toolbar.app',
      'header.cloudos-toolbar',
      '.cw-toolbar',
      '.toolbar-view'
    ];
    for (var i = 0; i < selectors.length; i++) {
      root.querySelectorAll?.(selectors[i])?.forEach(function (el) {
        el.remove();
      });
    }
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
    stripAppleToolbar();
    var root = document.getElementById(ROOT_ID);
    if (root) return;
    root = document.createElement('div');
    root.id = ROOT_ID;
    root.setAttribute('role', 'banner');
    root.setAttribute('aria-label', 'Window');
    root.innerHTML = [
      '<div class="icloud-win-chrome-drag" aria-hidden="true"></div>',
      '<div class="icloud-win-chrome-controls">',
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
    (document.body || document.documentElement).appendChild(root);
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
        "minimize" | "maximize" | "close" => Some(name),
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

fn handle_chrome_action(app: &AppHandle, action: &str) {
    if action == "close" {
        hide_main(app);
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
    Url::parse(url).unwrap_or_else(|_| Url::parse("https://www.icloud.com").expect("fallback"))
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
    let _ = app
        .notification()
        .builder()
        .title(title)
        .body(body)
        .show();
}

fn is_icloud_home_redirect(url: &Url, app_url: &Url) -> bool {
    let Some(app_host) = app_url.host_str() else {
        return false;
    };
    if !(app_host == "www.icloud.com" || app_host == "icloud.com") {
        return false;
    }
    let app_path = app_url.path().trim_end_matches('/');
    if app_path.is_empty() || app_path == "/" {
        return false;
    }

    let Some(host) = url.host_str() else {
        return false;
    };
    if !(host == "www.icloud.com" || host == "icloud.com") {
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

fn restore_session_then_navigate(app: AppHandle, window: WebviewWindow, url: Url) {
    std::thread::spawn(move || {
        let mut restored = false;
        for delay_ms in [50_u64, 100, 200, 400] {
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
            std::thread::sleep(Duration::from_millis(50));
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
        std::thread::sleep(Duration::from_millis(50));
        if let Err(e) = window.navigate(url) {
            eprintln!("[cookies] navigate after sign out: {e}");
        }
        show_main(&app);
    });
}

fn build_main_window(app: &AppHandle, config: IcloudAppConfig) -> tauri::Result<WebviewWindow> {
    let app_data = app
        .path()
        .app_data_dir()
        .unwrap_or_else(|_| PathBuf::from("."));
    let webview_data = app_data.join("webview");
    let _ = std::fs::create_dir_all(&webview_data);

    let app_url = parse_app_url(config.url);
    let blank_url = Url::parse("about:blank").expect("about:blank");
    let app_for_nav = app.clone();
    let app_for_chrome = app.clone();
    let app_for_chrome_nw = app.clone();
    let app_for_open = app.clone();
    let app_for_download = app.clone();
    let nav_target = app_url.clone();
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
            .initialization_script_for_all_frames(TOOLBAR_REMOVE_SCRIPT);

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
        .on_page_load(move |window, _payload| {
            let _ = window.eval(FONT_FIX_SCRIPT);
            let _ = window.eval(TOOLBAR_REMOVE_SCRIPT);
            #[cfg(windows)]
            {
                let _ = window.eval(WINDOW_CHROME_SCRIPT);
            }
        })
        .on_navigation(move |url| {
            if let Some(action) = chrome_action(url) {
                handle_chrome_action(&app_for_chrome, action);
                return false;
            }
            if is_icloud_home_redirect(url, &nav_target) {
                let handle = app_for_nav.clone();
                let target = nav_target.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    if let Some(w) = handle.get_webview_window(MAIN_WINDOW_LABEL) {
                        let _ = w.navigate(target);
                    }
                });
                return false;
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
                handle_chrome_action(&app_for_chrome_nw, action);
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
        let _ = window.set_background_color(Some(
            tauri::window::Color::from((255u8, 255u8, 255u8, 255u8)),
        ));
    }

    Ok(window)
}

/// Run a tray-first iCloud/Apple web wrapper.
///
/// `context` must come from the **app** crate via `tauri::generate_context!()`.
pub fn run(config: IcloudAppConfig, context: Context) {
    let app_url = parse_app_url(config.url);
    let builder = with_common_plugins(tauri::Builder::default())
        .plugin(tauri_plugin_notification::init());

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

            let window = build_main_window(&handle, config)?;

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
            let action_url = app_url.clone();
            handle.listen("tray:action", move |event| {
                match event.payload().trim_matches('"') {
                    "refresh" => refresh_page(&action_handle, action_url.clone()),
                    "sign-out" => sign_out(action_handle.clone(), action_url.clone()),
                    _ => {}
                }
            });

            Ok(())
        })
        .on_window_event(|window, event| on_window_event(window, &event))
        .build(context)
        .expect("error while building tauri application")
        .run(|_app_handle, event| {
            if let RunEvent::ExitRequested { .. } = event {
                // Cookies persist via before-quit; do not block the UI thread here.
            }
        });
}
