//! JNI entry point for Android's memory callbacks (task 2b.4).
//!
//! `MainActivity.onTrimMemory`, `onLowMemory` and `onStop` call `nativeTrim(kind)`. This sets the
//! engine's trim flag (an atomic in `greenline_engine::guard`) and wakes every running engine. A light
//! trim only empties MuPDF's store; a hard one also drops the display list and aborts a list read in
//! flight (that page is then read again; task 2bf.2). Nothing goes through the WebView, nothing here
//! calls MuPDF, blocks, allocates on the Java heap or logs.
//!
//! `kind`: 1 = onTrimMemory at real pressure, 2 = onLowMemory (both hard); 3 = onStop (backgrounded or
//! covered by the SAF picker), 4 = a light onTrimMemory level such as UI_HIDDEN (both light). Unknown
//! values count as 1.

use std::ffi::c_void;

use greenline_engine::guard::{request_trim, TrimKind};

/// `private external fun nativeTrim(kind: Int)` in `com.greenline.android.MainActivity`.
/// JNI signature `(I)V`: (JNIEnv*, jobject this, jint kind). The env and object are not used.
#[no_mangle]
pub extern "system" fn Java_com_greenline_android_MainActivity_nativeTrim(_env: *mut c_void, _this: *mut c_void, kind: i32) {
    // A panic must never unwind into the JVM.
    let _ = std::panic::catch_unwind(|| request_trim(TrimKind::from_i32(kind)));
}
