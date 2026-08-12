# tauri-icloud-base

Shared Tauri 2 shell for iCloud / Apple web wrappers — Safari UA, Windows font fix, Apple cookie persistence, tray, optional toolbar strip.

Depends on [`tauri-tray-base`](https://github.com/taylorivanoff/tauri-tray-base).

## Usage

```rust
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri_icloud_base::run(
        tauri_icloud_base::IcloudAppConfig {
            app_name: "iCloud Notes",
            url: "https://www.icloud.com/notes",
            remove_toolbar: false,
        },
        tauri::generate_context!(),
    );
}
```
