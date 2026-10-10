//! Small differences between the desktop app and the browser, in one place: where files live,
//! opening links, and (in the browser) saving a file by downloading it.

use std::path::PathBuf;

/// A per-user folder for rebuildable data. Browsers have no folders: an empty path means
/// "keep nothing" (the browser's own cache does the work).
pub fn cache_dir(sub: &str) -> PathBuf {
    #[cfg(not(target_arch = "wasm32"))]
    {
        directories::ProjectDirs::from("org", "ShowMeFire", "Field")
            .map(|d| d.cache_dir().join(sub))
            .unwrap_or_else(|| std::env::temp_dir().join(format!("smf-field-{sub}")))
    }
    #[cfg(target_arch = "wasm32")]
    {
        let _ = sub;
        PathBuf::new()
    }
}

/// A per-user folder for user data such as downloaded offline areas.
#[allow(dead_code)]
pub fn data_dir(sub: &str) -> PathBuf {
    #[cfg(not(target_arch = "wasm32"))]
    {
        directories::ProjectDirs::from("org", "ShowMeFire", "Field")
            .map(|d| d.data_local_dir().join(sub))
            .unwrap_or_else(|| std::env::temp_dir().join(format!("smf-field-{sub}")))
    }
    #[cfg(target_arch = "wasm32")]
    {
        let _ = sub;
        PathBuf::new()
    }
}

/// Open a link in the system browser (a new tab on the web).
pub fn open_url(url: &str) {
    // Only web links are ever opened: the text may come from server data.
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return;
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = open::that(url);
    }
    #[cfg(target_arch = "wasm32")]
    if let Some(w) = web_sys::window() {
        let _ = w.open_with_url_and_target(url, "_blank");
    }
}

/// Hand a file to the user. Browsers cannot write files, so it is downloaded instead.
#[cfg(target_arch = "wasm32")]
pub fn download_bytes(file_name: &str, mime: &str, bytes: &[u8]) -> Result<(), String> {
    use wasm_bindgen::JsCast;
    let err = |e: wasm_bindgen::JsValue| format!("{e:?}");
    let arr = js_sys::Uint8Array::from(bytes);
    let parts = js_sys::Array::new();
    parts.push(&arr);
    let opts = web_sys::BlobPropertyBag::new();
    opts.set_type(mime);
    let blob = web_sys::Blob::new_with_u8_array_sequence_and_options(&parts, &opts).map_err(err)?;
    let url = web_sys::Url::create_object_url_with_blob(&blob).map_err(err)?;
    let doc = web_sys::window()
        .and_then(|w| w.document())
        .ok_or("no document")?;
    let a: web_sys::HtmlAnchorElement = doc
        .create_element("a")
        .map_err(err)?
        .dyn_into()
        .map_err(|_| "not an anchor".to_string())?;
    a.set_href(&url);
    a.set_download(file_name);
    a.click();
    // Revoke later: some browsers cancel the download if the object URL disappears at once.
    let url2 = url.clone();
    let revoke = wasm_bindgen::closure::Closure::once_into_js(move || {
        let _ = web_sys::Url::revoke_object_url(&url2);
    });
    if let Some(w) = web_sys::window() {
        let _ =
            w.set_timeout_with_callback_and_timeout_and_arguments_0(revoke.unchecked_ref(), 60_000);
    }
    Ok(())
}

/// Something the browser handed us asynchronously (file pickers and drops cannot block).
#[cfg(target_arch = "wasm32")]
pub enum PlatformEvent {
    /// A map file chosen with Open.
    OpenMap {
        name: String,
        bytes: Vec<u8>,
    },
    /// A data file chosen with Import (or dropped on the window).
    Import {
        name: String,
        bytes: Vec<u8>,
    },
    Message(String),
}

#[cfg(target_arch = "wasm32")]
thread_local! {
    static INBOX: std::cell::RefCell<Vec<PlatformEvent>> = const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(target_arch = "wasm32")]
fn push_event(e: PlatformEvent, ctx: &eframe::egui::Context) {
    INBOX.with(|i| i.borrow_mut().push(e));
    ctx.request_repaint();
}

/// Events delivered since the last call (the desktop's dialogs block, so it has none).
#[cfg(target_arch = "wasm32")]
pub fn take_events() -> Vec<PlatformEvent> {
    INBOX.with(|i| std::mem::take(&mut *i.borrow_mut()))
}

/// Which handler should receive the chosen files.
#[cfg(target_arch = "wasm32")]
#[derive(Clone, Copy)]
pub enum PickKind {
    OpenMap,
    Import,
}

/// Show the browser's file chooser; the files arrive through `take_events`.
#[cfg(target_arch = "wasm32")]
pub fn pick_files(
    ctx: &eframe::egui::Context,
    kind: PickKind,
    filter: &'static str,
    exts: &'static [&'static str],
) {
    let ctx = ctx.clone();
    wasm_bindgen_futures::spawn_local(async move {
        let handles = rfd::AsyncFileDialog::new()
            .add_filter(filter, exts)
            .pick_files()
            .await
            .unwrap_or_default();
        for h in handles {
            let name = h.file_name();
            let bytes = h.read().await;
            let ev = match kind {
                PickKind::OpenMap => PlatformEvent::OpenMap { name, bytes },
                PickKind::Import => PlatformEvent::Import { name, bytes },
            };
            push_event(ev, &ctx);
        }
    });
}

/// Read files dropped on the page and deliver them as imports.
#[cfg(target_arch = "wasm32")]
pub fn read_dropped(ctx: &eframe::egui::Context, files: Vec<eframe::egui::DroppedFileHandle>) {
    for f in files {
        let ctx = ctx.clone();
        wasm_bindgen_futures::spawn_local(async move {
            let name = f
                .path()
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            match f.bytes_async().await {
                Ok(bytes) => push_event(PlatformEvent::Import { name, bytes }, &ctx),
                Err(e) => push_event(
                    PlatformEvent::Message(format!("Could not read {name}: {e}")),
                    &ctx,
                ),
            }
        });
    }
}

/// Ask a yes/no question (the browser's confirm box on the web).
pub fn confirm(title: &str, question: &str) -> bool {
    #[cfg(not(target_arch = "wasm32"))]
    {
        rfd::MessageDialog::new()
            .set_title(title)
            .set_description(question)
            .set_buttons(rfd::MessageButtons::YesNo)
            .show()
            == rfd::MessageDialogResult::Yes
    }
    #[cfg(target_arch = "wasm32")]
    {
        let _ = title;
        web_sys::window()
            .and_then(|w| w.confirm_with_message(question).ok())
            .unwrap_or(true)
    }
}

/// Show a message the user must dismiss.
pub fn message(title: &str, text: &str) {
    #[cfg(not(target_arch = "wasm32"))]
    {
        rfd::MessageDialog::new()
            .set_title(title)
            .set_description(text)
            .show();
    }
    #[cfg(target_arch = "wasm32")]
    if let Some(w) = web_sys::window() {
        let _ = w.alert_with_message(&format!("{title}\n\n{text}"));
    }
}
