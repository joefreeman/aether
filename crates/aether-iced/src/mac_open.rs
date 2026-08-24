//! macOS "Open With": receiving the files Finder hands us.
//!
//! Finder never passes a chosen document in `argv` — it launches (or activates) the app and sends
//! an Apple event, which AppKit delivers to the application delegate as `application:openURLs:`.
//! With nothing handling that selector the event goes unanswered and the file is silently dropped:
//! Aether opens on its usual startup view as if nothing had been chosen. The document types that
//! get Aether *offered* in the first place are declared in `packaging/Info.plist`.
//!
//! # Why this doesn't install a delegate
//!
//! The obvious implementation is to register our own `NSApplicationDelegate`, which is what
//! `winit::platform::macos` tells you to do — "Winit guarantees that it will not register an
//! application delegate". That guarantee is stale, and following it aborts the process on launch:
//! winit 0.30 *does* register a `WinitApplicationDelegate` from `EventLoop::new`, and the
//! `sendEvent:` implementation it swizzles onto `NSApplication` fetches that delegate back out of
//! `NSApp` and panics if it is not winit's own object. Since `sendEvent:` is an `extern "C"` ObjC
//! method the panic cannot unwind, so it becomes an abort — on the first event after launch, for
//! every launch, not just an "Open With" one.
//!
//! So the delegate slot is not ours to take. Instead we add `application:openURLs:` to winit's
//! delegate *class* and leave winit's instance installed where it expects to find it. Objective-C
//! dispatches by selector at call time, so AppKit finds our implementation on the object it
//! already has. winit's delegate implements only `applicationDidFinishLaunching:` and
//! `applicationWillTerminate:`, so the selector is unclaimed and this adds rather than replaces.
//!
//! # Timing
//!
//! [`install`] has to run after winit has built its `EventLoop` — that is what registers the class
//! — and before AppKit dispatches the launch event. That is exactly the window iced's boot closure
//! sits in, which is why [`crate::app::run`] calls it from there. Unlike the delegate approach,
//! the ordering is self-enforcing rather than assumed: called any earlier the class does not exist
//! yet, and the lookup fails loudly instead of half-installing.
//!
//! The same callback covers both cases the OS produces: the file that *caused* the launch, and a
//! file dropped on an already-running instance.

use std::ffi::CStr;
use std::mem;
use std::path::PathBuf;
use std::sync::Mutex;

use iced::futures::channel::mpsc;
use iced::futures::{Stream, StreamExt};
use objc2::runtime::{AnyClass, AnyObject, Imp, Sel};
use objc2::{ffi, sel};
use objc2_app_kit::NSApplication;
use objc2_foundation::{NSArray, NSURL};

/// The delegate's end of the hand-off. Filled by [`install`] before the first event can arrive, and
/// read on the main thread only — the callback is an AppKit delegate method.
static SENDER: Mutex<Option<mpsc::UnboundedSender<PathBuf>>> = Mutex::new(None);

/// The app's end, parked here between [`install`] and the first [`opened_files`] call. `Mutex`
/// rather than a `OnceLock` because the subscription *takes* it: a stream can only be consumed once.
static RECEIVER: Mutex<Option<mpsc::UnboundedReceiver<PathBuf>>> = Mutex::new(None);

/// winit's application delegate, which owns `NSApp.delegate` for the process — see the module docs.
/// Private to winit, so this name is the one load-bearing assumption here; if it ever changes, the
/// lookup in [`install`] misses and "Open With" goes quiet rather than breaking anything.
const WINIT_DELEGATE: &CStr = c"WinitApplicationDelegate";

/// Type encoding of `-(void)application:(NSApplication *)app openURLs:(NSArray<NSURL *> *)urls`:
/// void return, then the two implicit arguments (`self`, `_cmd`) and the two declared ones.
const OPEN_URLS_TYPES: &CStr = c"v@:@@";

/// Files chosen in Finder (or `open -a Aether file`, or a drop on the Dock icon).
///
/// Deliberately plain `extern "C"` and not `extern "C-unwind"`: a panic here would be unwinding out
/// of an AppKit callback, and an abort at the boundary is a far better outcome than unwinding into
/// Objective-C frames. Nothing below panics, so this is a backstop rather than a plan.
extern "C" fn open_urls(_this: &AnyObject, _cmd: Sel, _app: &NSApplication, urls: &NSArray<NSURL>) {
    let Ok(sender) = SENDER.lock() else {
        return;
    };
    let Some(sender) = sender.as_ref() else {
        // Can't happen — `install` fills this before AppKit can call us — but a dropped file is a
        // better outcome than tearing the process down over it.
        tracing::warn!("openURLs arrived before the channel was installed");
        return;
    };
    for url in urls {
        // `openURLs` also carries custom-scheme URLs for apps that register them. We register
        // none, so anything that isn't a file is not ours to open.
        if !url.isFileURL() {
            continue;
        }
        match url.path() {
            Some(path) => {
                let _ = sender.unbounded_send(PathBuf::from(path.to_string()));
            }
            None => tracing::warn!("openURLs delivered a file URL with no path"),
        }
    }
}

/// Graft [`open_urls`] onto winit's delegate class. Call once, from the boot closure — see the
/// module docs for why that exact moment matters.
pub fn install() {
    let (tx, rx) = mpsc::unbounded();
    *SENDER.lock().expect("SENDER is never poisoned") = Some(tx);
    *RECEIVER.lock().expect("RECEIVER is never poisoned") = Some(rx);

    let Some(class) = AnyClass::get(WINIT_DELEGATE) else {
        tracing::error!(
            "{WINIT_DELEGATE:?} is not registered — either winit renamed it or `install` ran \
             before the event loop was built; files opened from Finder will be ignored"
        );
        return;
    };

    // SAFETY:
    // - `open_urls` takes exactly the arguments AppKit calls `application:openURLs:` with, and
    //   `OPEN_URLS_TYPES` is that signature's encoding.
    // - The transmute only re-labels the unwind ABI (`Imp` is `extern "C-unwind"`); the calling
    //   convention and argument layout are unchanged. See `open_urls` for why it is `extern "C"`.
    // - Adding a method to a registered class is supported by the runtime; unlike ivars, it does
    //   not require the class to still be under construction.
    let added = unsafe {
        let imp: Imp = mem::transmute(
            open_urls as extern "C" fn(&AnyObject, Sel, &NSApplication, &NSArray<NSURL>),
        );
        ffi::class_addMethod(
            (class as *const AnyClass).cast_mut(),
            sel!(application:openURLs:),
            imp,
            OPEN_URLS_TYPES.as_ptr(),
        )
    };

    // `class_addMethod` refuses rather than replaces when the class already implements the
    // selector, so this means a winit that grew its own `openURLs` handling — at which point the
    // files are going to it and this module wants revisiting.
    if !added.as_bool() {
        tracing::error!(
            "{WINIT_DELEGATE:?} already implements application:openURLs: — files opened from \
             Finder will be ignored"
        );
    }
}

/// The files the OS has asked us to open, as a stream — the app subscribes to this and turns each
/// path into an open. Yields nothing at all if [`install`] never found the class, and (because a
/// stream can only be consumed once) if something ever subscribes twice: the first subscription
/// keeps the files, rather than the second silently stealing them.
pub fn opened_files() -> impl Stream<Item = PathBuf> {
    match RECEIVER.lock().ok().and_then(|mut r| r.take()) {
        Some(rx) => rx.left_stream(),
        None => iced::futures::stream::pending().right_stream(),
    }
}
