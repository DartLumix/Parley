//! Phase 7 bridge crate: parley-core surfaced to Flutter via
//! flutter_rust_bridge. API surface lives in `api/`; this file stays
//! minimal (the generator may append its wiring here — verify with cat
//! after generate, before building).

mod api;
mod frb_generated;

/// Android's stdout/stderr is not visible in logcat from the Flutter
/// embedding, which makes native stream failures invisible on a phone.
/// liblog is already part of the NDK, so use it directly rather than
/// adding a logging crate just to diagnose the voice path.
#[cfg(target_os = "android")]
pub(crate) mod android_log {
    use std::ffi::CString;
    use std::os::raw::{c_char, c_int};

    #[link(name = "log")]
    unsafe extern "C" {
        fn __android_log_write(prio: c_int, tag: *const c_char, text: *const c_char) -> c_int;
    }

    fn write(priority: c_int, message: &str) {
        let Ok(tag) = CString::new("Parley") else {
            return;
        };
        let Ok(text) = CString::new(message) else {
            return;
        };
        // SAFETY: both C strings are NUL-terminated and outlive the call.
        unsafe {
            __android_log_write(priority, tag.as_ptr(), text.as_ptr());
        }
    }

    pub(crate) fn info(message: &str) {
        write(4, message); // ANDROID_LOG_INFO
    }

    pub(crate) fn error(message: &str) {
        write(6, message); // ANDROID_LOG_ERROR
    }

    /// Route Rust panics to logcat once. A panic on the cpal device
    /// thread otherwise just closes its readiness channel and surfaces
    /// as "device thread died", with the actual cause lost on stderr.
    pub(crate) fn install_panic_hook() {
        use std::sync::Once;
        static ONCE: Once = Once::new();
        ONCE.call_once(|| {
            let previous = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |panic| {
                error(&format!("panic: {panic}"));
                previous(panic);
            }));
        });
    }
}

#[cfg(not(target_os = "android"))]
#[allow(dead_code)] // The same surface as the Android logger; some calls only exist on phone builds.
pub(crate) mod android_log {
    pub(crate) fn info(_message: &str) {}
    pub(crate) fn error(_message: &str) {}
    pub(crate) fn install_panic_hook() {}
}

/// Oboe talks to Android's AudioManager through `ndk-context`, which
/// expects an application context. Flutter's Dart VM loads native
/// libraries without running JNI initialization, so MainActivity passes
/// its context down once. The GlobalRef is kept for the process lifetime
/// because `ndk_context` stores a raw pointer to it.
#[cfg(target_os = "android")]
mod android_context {
    use jni::objects::{GlobalRef, JObject};
    use jni::JNIEnv;
    use std::ffi::c_void;
    use std::sync::OnceLock;

    static CONTEXT: OnceLock<GlobalRef> = OnceLock::new();

    #[no_mangle]
    pub extern "system" fn Java_dev_parley_app_MainActivity_initAndroidContext(
        env: JNIEnv,
        _activity: JObject,
        context: JObject,
    ) {
        if CONTEXT.get().is_some() {
            return;
        }
        let global = match env.new_global_ref(context) {
            Ok(global) => global,
            Err(e) => {
                crate::android_log::error(&format!("voice: could not retain Android context: {e}"));
                return;
            }
        };
        let vm = match env.get_java_vm() {
            Ok(vm) => vm,
            Err(e) => {
                crate::android_log::error(&format!("voice: could not get Java VM for audio: {e}"));
                return;
            }
        };
        let vm = vm.get_java_vm_pointer() as *mut c_void;
        let context = global.as_obj().as_raw() as *mut c_void;
        // SAFETY: the VM pointer and GlobalRef both live for the process
        // (the GlobalRef is moved into CONTEXT immediately below).
        unsafe {
            ndk_context::initialize_android_context(vm, context);
        }
        if CONTEXT.set(global).is_ok() {
            crate::android_log::info("voice: Android context initialized");
        }
    }
}

// The desktop client ships echo cancellation: voice on open speakers
// without it is a feedback loop. A Linux build that forgot the feature
// compiles cleanly otherwise, so fail loudly here instead of shipping
// a quieter regression. Gated on `screen-capture` — the marker of the
// shipping desktop feature set — so the reduced voice-only shape can
// still be built to check it. (Windows takes the same guard through the
// release workflow: `aec-rust`, the pure-Rust filter; see aec.rs.)
#[cfg(all(
    target_os = "linux",
    feature = "screen-capture",
    not(any(feature = "aec", feature = "aec-rust"))
))]
compile_error!("the Linux desktop build must enable the `aec` feature");
