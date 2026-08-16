//! macOS "Open With": receiving the files Finder hands us.
//!
//! Finder never passes a chosen document in `argv` — it launches (or activates) the app and sends
//! an Apple event, which AppKit delivers to the application delegate as `application:openURLs:`.
//! Without a delegate implementing it, the event goes unhandled and the file is silently dropped:
//! Aether opens on its usual startup view as if nothing had been chosen. The document types that
//! get Aether *offered* in the first place are declared in `packaging/Info.plist`.
//!
//! Two things make this safe to do under iced:
//!
//! - **winit guarantees it registers no application delegate** (`winit::platform::macos`, "Custom
//!   `NSApplicationDelegate`"), so the slot is ours and we are not fighting the toolkit for it.
//! - **Timing.** The delegate has to exist before AppKit dispatches the launch event, and
//!   `iced_winit::run` calls the program's boot closure after building the winit `EventLoop` but
//!   before `run_app` — exactly the window winit's docs require ("call `sharedApplication` after
//!   `EventLoop::new`"). [`install`] is therefore called from [`crate::app::run`]'s boot closure,
//!   and a cold "Open With" launch lands its file rather than racing past it.
//!
//! The same callback covers both cases the OS produces: the file that *caused* the launch, and a
//! file dropped on an already-running instance.

use std::path::PathBuf;
use std::sync::Mutex;

use iced::futures::channel::mpsc;
use iced::futures::{Stream, StreamExt};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{define_class, msg_send, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{NSApplication, NSApplicationDelegate};
use objc2_foundation::{NSArray, NSObject, NSObjectProtocol, NSURL};

/// The delegate's end of the hand-off. Filled by [`install`] before the first event can arrive, and
/// read on the main thread only — the callback is an AppKit delegate method.
static SENDER: Mutex<Option<mpsc::UnboundedSender<PathBuf>>> = Mutex::new(None);

/// The app's end, parked here between [`install`] and the first [`opened_files`] call. `Mutex`
/// rather than a `OnceLock` because the subscription *takes* it: a stream can only be consumed once.
static RECEIVER: Mutex<Option<mpsc::UnboundedReceiver<PathBuf>>> = Mutex::new(None);

define_class!(
    // SAFETY:
    // - `NSObject` has no subclassing requirements.
    // - `OpenFilesDelegate` holds no ivars and implements no `Drop`.
    #[unsafe(super(NSObject))]
    // AppKit only ever calls a delegate on the main thread, and `MainThreadOnly` is what lets the
    // class be built from a `MainThreadMarker` without an unchecked assertion.
    #[thread_kind = MainThreadOnly]
    // Named so it is identifiable in a crash report or `po [NSApp delegate]`.
    #[name = "AetherOpenFilesDelegate"]
    struct OpenFilesDelegate;

    unsafe impl NSObjectProtocol for OpenFilesDelegate {}

    unsafe impl NSApplicationDelegate for OpenFilesDelegate {
        /// Files chosen in Finder (or `open -a Aether file`, or a drop on the Dock icon).
        #[unsafe(method(application:openURLs:))]
        #[allow(non_snake_case)]
        fn application_openURLs(&self, _app: &NSApplication, urls: &NSArray<NSURL>) {
            let Ok(sender) = SENDER.lock() else {
                return;
            };
            let Some(sender) = sender.as_ref() else {
                // Can't happen — `install` fills this before AppKit can call us — but a dropped
                // file is a better outcome than a panic unwinding into an Objective-C frame.
                tracing::warn!("openURLs arrived before the channel was installed");
                return;
            };
            for url in urls {
                // `openURLs` also carries custom-scheme URLs for apps that register them. We
                // register none, so anything that isn't a file is not ours to open.
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
    }
);

/// Install the delegate. Call once, on the main thread, from the boot closure — see the module
/// docs for why that exact moment matters.
pub fn install() {
    let Some(mtm) = MainThreadMarker::new() else {
        tracing::error!("not on the main thread: files opened from Finder will be ignored");
        return;
    };
    let (tx, rx) = mpsc::unbounded();
    *SENDER.lock().expect("SENDER is never poisoned") = Some(tx);
    *RECEIVER.lock().expect("RECEIVER is never poisoned") = Some(rx);

    let this = OpenFilesDelegate::alloc(mtm).set_ivars(());
    let delegate: Retained<OpenFilesDelegate> = unsafe { msg_send![super(this), init] };
    let app = NSApplication::sharedApplication(mtm);
    app.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));

    // `NSApplication.delegate` is a *weak* property: it does not keep the delegate alive, and a
    // dropped delegate means openURLs quietly stops arriving. Nothing else has a reason to own
    // this object and there is no teardown path (it must outlive the app), so leak it on purpose.
    std::mem::forget(delegate);
}

/// The files the OS has asked us to open, as a stream — the app subscribes to this and turns each
/// path into an open. Yields nothing at all if the delegate never installed, and (because a stream
/// can only be consumed once) if something ever subscribes twice: the first subscription keeps the
/// files, rather than the second silently stealing them.
pub fn opened_files() -> impl Stream<Item = PathBuf> {
    match RECEIVER.lock().ok().and_then(|mut r| r.take()) {
        Some(rx) => rx.left_stream(),
        None => iced::futures::stream::pending().right_stream(),
    }
}
