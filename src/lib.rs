//! Shared Tauri shell for iCloud / Apple web wrappers.

mod cookies;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tauri::webview::{DownloadEvent, NewWindowResponse};
use tauri::{
    AppHandle, Context, Listener, Manager, RunEvent, WebviewUrl, WebviewWindow, WebviewWindowBuilder,
};
use tauri_plugin_notification::NotificationExt;
use tauri_plugin_opener::OpenerExt;
use tauri_tray_base::{
    apply_window_settings, install_state, on_window_event, set_on_before_quit, setup_tray,
    with_common_plugins, TrayBaseOptions, TrayClickAction, TrayExtraItem, TraySetupOptions,
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

/// Product config for an iCloud / Apple web wrapper.
#[derive(Debug, Clone, Copy)]
pub struct IcloudAppConfig {
    pub app_name: &'static str,
    pub url: &'static str,
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
    let app_for_open = app.clone();
    let app_for_download = app.clone();
    let nav_target = app_url.clone();
    let remove_toolbar = config.remove_toolbar;
    let app_name = config.app_name;

    let mut builder =
        WebviewWindowBuilder::new(app, MAIN_WINDOW_LABEL, WebviewUrl::External(blank_url))
            .title(config.app_name)
            .inner_size(1280.0, 800.0)
            .resizable(true)
            .visible(false)
            .user_agent(SAFARI_UA)
            .data_directory(webview_data)
            .initialization_script_for_all_frames(FONT_FIX_SCRIPT);

    if remove_toolbar {
        builder = builder.initialization_script_for_all_frames(TOOLBAR_REMOVE_SCRIPT);
    }

    builder
        .on_page_load(move |window, _payload| {
            let _ = window.eval(FONT_FIX_SCRIPT);
            if remove_toolbar {
                let _ = window.eval(TOOLBAR_REMOVE_SCRIPT);
            }
        })
        .on_navigation(move |url| {
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
            let href = url.as_str().to_string();
            if href != "about:blank#blocked" && !href.starts_with("about:blank") {
                let _ = app_for_open.opener().open_url(href, None::<&str>);
            }
            NewWindowResponse::Deny
        })
        .build()
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
                    extra_tray_items: vec![TrayExtraItem {
                        id: "refresh".into(),
                        label: "Refresh".into(),
                    }],
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

            let quit_app = handle.clone();
            set_on_before_quit(&handle, move || {
                persist_cookies(&quit_app);
            });

            restore_session_then_navigate(handle.clone(), window, app_url.clone());

            let refresh_handle = handle.clone();
            let refresh_url = app_url.clone();
            handle.listen("tray:action", move |event| {
                if event.payload().trim_matches('"') == "refresh" {
                    refresh_page(&refresh_handle, refresh_url.clone());
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
