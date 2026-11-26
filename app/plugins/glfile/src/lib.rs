//! Greenline's local Tauri 2 mobile plugin (task 2b.5): pick a PDF through Android's Storage Access
//! Framework and get back a detached file descriptor.
//!
//! Kotlin side: `android/src/main/java/com/greenline/glfile/GlFilePlugin.kt`. `ACTION_OPEN_DOCUMENT`
//! (`application/pdf`) -> `ContentResolver.openFileDescriptor(uri, "r")` -> `detachFd()`.
//! Also `saveText` (task 2b.7): `ACTION_CREATE_DOCUMENT` -> write the given text to the new document
//! (the measurement export); only a byte count comes back.
//!
//! The plugin registers no JS commands: the app's own commands call [`GlFile::pick_pdf`] from Rust.
//!
//! Confidentiality: only the fd number, a timing and the URI *authority* (the provider, e.g.
//! `com.android.providers.downloads.documents`) cross to Rust. The display name, the full URI and
//! any path never leave Kotlin, and nothing on either side logs. Errors are fixed codes.

use std::marker::PhantomData;

use serde::Deserialize;
use tauri::{
    plugin::{Builder, TauriPlugin},
    Manager, Runtime,
};

/// What the Kotlin side hands back after a successful pick.
#[derive(Debug, Deserialize)]
pub struct Picked {
    /// Detached fd: the caller owns it and must close it.
    pub fd: i32,
    /// From the picker returning a URI to `openFileDescriptor` returning (excludes the user's time in
    /// the picker; includes any download the provider does before it hands over an fd).
    pub pick_to_fd_ms: f64,
    /// The URI authority only (the document provider), never the full URI.
    pub authority: String,
    /// Random id of the file's recent-list entry (task 2bf.12), when its permission could be kept.
    #[serde(default)]
    pub recent_id: Option<String>,
    /// The entry was created by this open (not an existing one): remove it if the open fails.
    #[serde(default)]
    pub recent_new: bool,
    /// The file was sent by another app (task 2bf.11).
    #[serde(default)]
    pub incoming: bool,
}

/// Why a pick produced no fd. Every variant maps to a fixed code string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickError {
    /// The user backed out of the picker.
    Cancelled,
    /// The picker returned no URI.
    NoUri,
    /// `openFileDescriptor` failed or returned null.
    OpenFailed,
    /// The plugin call itself failed (JNI, bad response, unknown rejection).
    PluginFailed,
    /// Not on Android.
    Unsupported,
    /// Nothing waiting (no incoming file) or no such recent entry (task 2bf.10).
    Nothing,
    /// The read permission for a recent file is gone (task 2bf.10).
    NoPermission,
}

impl PickError {
    pub fn code(self) -> &'static str {
        match self {
            PickError::Cancelled => "E_PICK_CANCELLED",
            PickError::NoUri => "E_PICK_NO_URI",
            PickError::OpenFailed => "E_PICK_OPEN_FAILED",
            PickError::PluginFailed => "E_PICK_PLUGIN_FAILED",
            PickError::Unsupported => "E_PICK_UNSUPPORTED",
            PickError::Nothing => "E_NONE",
            PickError::NoPermission => "E_NO_PERMISSION",
        }
    }

    /// The Kotlin side's fixed rejection code -> error (anything else is a plugin failure).
    #[cfg_attr(not(target_os = "android"), allow(dead_code))]
    fn from_kotlin(code: Option<&str>) -> PickError {
        match code {
            Some("CANCELLED") => PickError::Cancelled,
            Some("NO_URI") => PickError::NoUri,
            Some("OPEN_FAILED") => PickError::OpenFailed,
            Some("NONE") => PickError::Nothing,
            Some("NO_PERMISSION") => PickError::NoPermission,
            _ => PickError::PluginFailed,
        }
    }
}

/// What the Kotlin side hands back after `saveText` wrote the document.
#[derive(Debug, Deserialize)]
pub struct Saved {
    /// Bytes written (UTF-8).
    pub bytes: u64,
}

/// Why `saveText` wrote nothing. Every variant maps to a fixed code string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveError {
    /// The user backed out of the "save as" screen.
    Cancelled,
    /// The provider could not be written to.
    WriteFailed,
    PluginFailed,
    Unsupported,
}

impl SaveError {
    pub fn code(self) -> &'static str {
        match self {
            SaveError::Cancelled => "E_SAVE_CANCELLED",
            SaveError::WriteFailed => "E_SAVE_WRITE_FAILED",
            SaveError::PluginFailed => "E_SAVE_PLUGIN_FAILED",
            SaveError::Unsupported => "E_SAVE_UNSUPPORTED",
        }
    }
}

/// Access to the plugin, kept in Tauri state; get it with [`GlFileExt::glfile`].
pub struct GlFile<R: Runtime> {
    #[cfg(target_os = "android")]
    handle: tauri::plugin::PluginHandle<R>,
    _r: PhantomData<fn() -> R>,
}

impl<R: Runtime> GlFile<R> {
    /// Show the system picker for a PDF and return the detached fd. Resolves when the user picks or
    /// cancels; `openFileDescriptor` runs off the UI thread on the Kotlin side.
    pub async fn pick_pdf(&self) -> Result<Picked, PickError> {
        self.call("pickPdf", serde_json::json!({})).await
    }

    /// Take the file another app sent (task 2bf.11), if one is waiting: [`PickError::Nothing`] if not.
    pub async fn take_incoming(&self) -> Result<Picked, PickError> {
        self.call("takeIncoming", serde_json::json!({})).await
    }

    /// Reopen a recent file by its random id (task 2bf.12).
    pub async fn open_recent(&self, id: &str) -> Result<Picked, PickError> {
        self.call("openRecent", serde_json::json!({ "id": id })).await
    }

    /// The recent list for the start screen, passed through unread: `{ "items": [...] }`. It holds
    /// display names, so the caller hands it to the page and nowhere else (no logs, no exports).
    pub async fn list_recents(&self) -> Result<serde_json::Value, PickError> {
        self.call("listRecents", serde_json::json!({})).await
    }

    pub async fn remove_recent(&self, id: &str) -> Result<serde_json::Value, PickError> {
        self.call("removeRecent", serde_json::json!({ "id": id })).await
    }

    pub async fn set_last_page(&self, id: &str, page: i32, pages: i32) -> Result<serde_json::Value, PickError> {
        self.call("setLastPage", serde_json::json!({ "id": id, "page": page, "pages": pages })).await
    }

    /// `data`: a small `data:image/…` URL made by the page from the fit image.
    pub async fn save_thumb(&self, id: &str, data: &str) -> Result<serde_json::Value, PickError> {
        self.call("saveThumb", serde_json::json!({ "id": id, "data": data })).await
    }

    /// `{ "is_default": bool }`: whether Greenline is the phone's default PDF app.
    pub async fn is_default_pdf_app(&self) -> Result<serde_json::Value, PickError> {
        self.call("isDefaultPdfApp", serde_json::json!({})).await
    }

    /// Send the app to the background (Back after a file another app sent).
    pub async fn move_to_back(&self) -> Result<serde_json::Value, PickError> {
        self.call("moveToBack", serde_json::json!({})).await
    }

    async fn call<T: serde::de::DeserializeOwned>(&self, cmd: &str, args: serde_json::Value) -> Result<T, PickError> {
        #[cfg(target_os = "android")]
        {
            use tauri::plugin::mobile::PluginInvokeError;
            self.handle.run_mobile_plugin_async::<T>(cmd, args).await.map_err(|e| match e {
                // Map only our own fixed codes; never pass the message through.
                PluginInvokeError::InvokeRejected(r) => PickError::from_kotlin(r.code.as_deref()),
                _ => PickError::PluginFailed,
            })
        }
        #[cfg(not(target_os = "android"))]
        {
            let _ = (cmd, args);
            Err(PickError::Unsupported)
        }
    }

    /// Show Android's "save as" screen (`ACTION_CREATE_DOCUMENT`) with `name` suggested, and write `text`
    /// (UTF-8) to the document the user creates. Nothing about the chosen place (URI, name) comes back.
    /// The caller passes only numbers-only text (the measurement export).
    pub async fn save_text(&self, name: &str, mime: &str, text: &str) -> Result<Saved, SaveError> {
        #[cfg(target_os = "android")]
        {
            use tauri::plugin::mobile::PluginInvokeError;
            self.handle
                .run_mobile_plugin_async::<Saved>("saveText", serde_json::json!({ "name": name, "mime": mime, "text": text }))
                .await
                .map_err(|e| match e {
                    PluginInvokeError::InvokeRejected(r) => match r.code.as_deref() {
                        Some("CANCELLED") => SaveError::Cancelled,
                        Some("WRITE_FAILED") | Some("NO_URI") => SaveError::WriteFailed,
                        _ => SaveError::PluginFailed,
                    },
                    _ => SaveError::PluginFailed,
                })
        }
        #[cfg(not(target_os = "android"))]
        {
            let _ = (name, mime, text);
            Err(SaveError::Unsupported)
        }
    }
}

pub trait GlFileExt<R: Runtime> {
    fn glfile(&self) -> &GlFile<R>;
}

impl<R: Runtime, T: Manager<R>> GlFileExt<R> for T {
    fn glfile(&self) -> &GlFile<R> {
        self.state::<GlFile<R>>().inner()
    }
}

/// Initializes the plugin (registers `com.greenline.glfile.GlFilePlugin` on Android).
pub fn init<R: Runtime>() -> TauriPlugin<R> {
    Builder::new("glfile")
        .setup(|app, api| {
            #[cfg(target_os = "android")]
            let glfile = GlFile::<R> {
                handle: api.register_android_plugin("com.greenline.glfile", "GlFilePlugin")?,
                _r: PhantomData,
            };
            #[cfg(not(target_os = "android"))]
            let glfile = {
                let _ = api;
                GlFile::<R> { _r: PhantomData }
            };
            app.manage(glfile);
            Ok(())
        })
        .build()
}
