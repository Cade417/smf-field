//! Small persistent key-value storage for settings and caches: files in the app's config and
//! cache folders on the desktop, the browser's `localStorage` on the web. Values are text
//! (JSON). Failures are silent on purpose: a settings file that cannot be written must never
//! stop the app.

#[cfg(test)]
thread_local! {
    static TEST_ROOT: std::cell::RefCell<Option<std::path::PathBuf>> = const { std::cell::RefCell::new(None) };
}

/// Point this thread's storage at a fresh temporary folder, so tests never touch real settings.
#[cfg(test)]
pub fn use_temp_root(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("smf-store-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    TEST_ROOT.with(|r| *r.borrow_mut() = Some(d.clone()));
    d
}

#[cfg(not(target_arch = "wasm32"))]
mod imp {
    use directories::ProjectDirs;
    use std::path::PathBuf;

    fn dir(cache: bool) -> PathBuf {
        #[cfg(test)]
        if let Some(r) = super::TEST_ROOT.with(|r| r.borrow().clone()) {
            return r.join(if cache { "cache" } else { "config" });
        }
        match ProjectDirs::from("org", "ShowMeFire", "Field") {
            Some(d) if cache => d.cache_dir().join("data"),
            Some(d) => d.config_dir().to_path_buf(),
            None => std::env::temp_dir().join("smf-field-store"),
        }
    }

    pub fn read(cache: bool, name: &str) -> Option<String> {
        std::fs::read_to_string(dir(cache).join(name)).ok()
    }

    pub fn write(cache: bool, name: &str, text: &str) {
        let d = dir(cache);
        let _ = std::fs::create_dir_all(&d);
        let tmp = d.join(format!("{name}.tmp"));
        // Write then rename so a crash never leaves half a file.
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, d.join(name));
        }
    }

    pub fn remove(cache: bool, name: &str) {
        let _ = std::fs::remove_file(dir(cache).join(name));
    }
}

#[cfg(target_arch = "wasm32")]
mod imp {
    fn storage() -> Option<web_sys::Storage> {
        web_sys::window()?.local_storage().ok()?
    }

    fn key(cache: bool, name: &str) -> String {
        format!(
            "smf-field/{}/{name}",
            if cache { "cache" } else { "config" }
        )
    }

    pub fn read(cache: bool, name: &str) -> Option<String> {
        storage()?.get_item(&key(cache, name)).ok()?
    }

    pub fn write(cache: bool, name: &str, text: &str) {
        if let Some(s) = storage() {
            // Quota errors (private windows, full storage) are ignored.
            let _ = s.set_item(&key(cache, name), text);
        }
    }

    pub fn remove(cache: bool, name: &str) {
        if let Some(s) = storage() {
            let _ = s.remove_item(&key(cache, name));
        }
    }
}

/// A setting stored under the config folder (or `localStorage`).
pub fn read(name: &str) -> Option<String> {
    imp::read(false, name)
}

pub fn write(name: &str, text: &str) {
    imp::write(false, name, text);
}

/// Something that can be rebuilt, such as a downloaded catalog.
pub fn read_cache(name: &str) -> Option<String> {
    imp::read(true, name)
}

pub fn write_cache(name: &str, text: &str) {
    imp::write(true, name, text);
}

#[allow(dead_code)]
pub fn remove(name: &str) {
    imp::remove(false, name);
}

/// Parse a stored JSON value, returning the default when it is missing or damaged.
pub fn load_json<T: serde::de::DeserializeOwned + Default>(name: &str) -> T {
    read(name)
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_json<T: serde::Serialize>(name: &str, value: &T) {
    if let Ok(s) = serde_json::to_string_pretty(value) {
        write(name, &s);
    }
}
