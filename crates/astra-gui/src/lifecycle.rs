//! Keep process teardown under Rust's control: AppKit's `terminate:` calls exit.
use gpui::App;

pub fn finish(cx: &mut App) {
    #[cfg(all(target_os = "macos", not(test)))]
    {
        if macos::installed() {
            cx.defer(|cx| {
                cx.shutdown();
                macos::stop();
            });
        } else {
            // Integration tests use GPUI's test platform without AppKit.
            cx.quit();
        }
    }
    #[cfg(any(not(target_os = "macos"), test))]
    cx.quit();
}

pub fn install_quit_hook(commands: async_channel::Sender<()>) {
    #[cfg(all(target_os = "macos", not(test)))]
    macos::install(Some(commands));
    #[cfg(any(not(target_os = "macos"), test))]
    let _ = commands;
}

#[cfg(all(target_os = "macos", not(test)))]
mod macos {
    use objc2::{
        msg_send,
        runtime::{AnyClass, AnyObject, Sel},
        sel, MainThreadMarker,
    };
    use objc2_app_kit::{NSApplication, NSEvent, NSEventModifierFlags, NSEventType};
    use objc2_foundation::NSPoint;
    use std::{
        cell::{Cell, RefCell},
        ffi::{c_char, c_void},
    };

    thread_local! {
        static INSTALLED: Cell<bool> = const { Cell::new(false) };
        static QUIT: RefCell<Option<async_channel::Sender<()>>> = const { RefCell::new(None) };
    }
    #[link(name = "objc")]
    unsafe extern "C" {
        fn object_getClass(object: *const AnyObject) -> *mut AnyClass;
        fn class_addMethod(
            class: *mut AnyClass,
            selector: Sel,
            implementation: *const c_void,
            types: *const c_char,
        ) -> bool;
    }
    extern "C" fn should_terminate(_: *mut AnyObject, _: Sel, _: *mut AnyObject) -> usize {
        QUIT.with(|sender| {
            if let Some(sender) = sender.borrow().as_ref() {
                if sender.try_send(()).is_err() {
                    stop();
                }
            } else {
                stop();
            }
        });
        // NSTerminateCancel: the shared model first waits for durable storage.
        0
    }
    pub fn installed() -> bool {
        INSTALLED.with(Cell::get)
    }
    pub fn install(commands: Option<async_channel::Sender<()>>) {
        let marker = MainThreadMarker::new().expect("GPUI main thread");
        let app = NSApplication::sharedApplication(marker);
        // The delegate belongs to GPUI. Add only the missing approval callback;
        // all window/lifecycle callbacks continue to be handled by GPUI itself.
        let delegate: *mut AnyObject = unsafe { msg_send![&*app, delegate] };
        assert!(!delegate.is_null(), "GPUI application delegate must exist");
        QUIT.with(|sender| *sender.borrow_mut() = commands);
        INSTALLED.with(|installed| installed.set(true));
        let added = unsafe {
            class_addMethod(
                object_getClass(delegate),
                sel!(applicationShouldTerminate:),
                should_terminate as *const () as *const c_void,
                c"Q@:@".as_ptr(),
            )
        };
        assert!(
            added,
            "GPUI delegate already defines applicationShouldTerminate"
        );
    }
    pub fn stop() {
        let marker = MainThreadMarker::new().expect("GPUI main thread");
        let app = NSApplication::sharedApplication(marker);
        app.stop(None);
        // stop does not wake a blocked nextEventMatchingMask; a harmless
        // application-defined event allows NSApplication.run to return.
        if let Some(event) = NSEvent::otherEventWithType_location_modifierFlags_timestamp_windowNumber_context_subtype_data1_data2(
            NSEventType::ApplicationDefined, NSPoint::new(0., 0.), NSEventModifierFlags::empty(), 0., 0, None, 0, 0, 0,
        ) {
            app.postEvent_atStart(&event, true);
        }
    }
}
