# tauri-icloud-base

Shared Tauri 2 shell for iCloud / Apple web wrappers — Safari UA, Windows font fix, Apple cookie persistence, tray, cloudos toolbar strip, and on Windows a Spotify-minimal frameless titlebar (min / max / close + drag).

Depends on [`tauri-tray-base`](https://github.com/taylorivanoff/tauri-tray-base).

## Usage

```rust
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri_icloud_base::run(
        tauri_icloud_base::IcloudAppConfig {
            app_name: "iCloud Notes",
            url: "https://www.icloud.com/notes",
            remove_toolbar: true, // ignored; toolbar is always removed
        },
        tauri::generate_context!(),
    );
}
```

## Toolbar

Apple's `cloudos-toolbar` is always removed so it does not leave a gap under the custom window chrome.

## Windows chrome

On Windows the main window is frameless. A thin overlay titlebar is injected into the page (`icloud-chrome://` actions, no remote Tauri IPC). macOS keeps the native titlebar / traffic lights.
