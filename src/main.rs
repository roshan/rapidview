//! Rapid View — native macOS JSON viewer.
//!
//! Turn 1: minimal scaffold. Opens a titled window, installs a menu bar,
//! and routes Finder "open with" / drag-drop via an NSApplicationDelegate.
//! Subsequent turns add the parser, CoreText rendering, and breadcrumbs.

#![deny(unsafe_op_in_unsafe_fn)]

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSApplicationDelegate, NSBackingStoreType,
    NSMenu, NSMenuItem, NSWindow, NSWindowStyleMask,
};
use objc2_foundation::{
    NSArray, NSNotification, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString, NSURL,
};

mod app_state {
    //! Process-wide state. Single-threaded access from the main thread only;
    //! the worker thread talks back via mpsc, so we don't need a Mutex here.
    use objc2::rc::Retained;
    use objc2_app_kit::NSWindow;
    use std::cell::RefCell;
    use std::sync::atomic::AtomicBool;

    thread_local! {
        pub static MAIN_WINDOW: RefCell<Option<Retained<NSWindow>>> = const { RefCell::new(None) };
    }

    pub static SELFTEST_LAUNCH: AtomicBool = AtomicBool::new(false);
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "RVAppDelegate"]
    struct AppDelegate;

    unsafe impl NSObjectProtocol for AppDelegate {}

    unsafe impl NSApplicationDelegate for AppDelegate {
        #[unsafe(method(applicationDidFinishLaunching:))]
        fn did_finish_launching(&self, _notification: &NSNotification) {
            let mtm = self.mtm();
            install_menu_bar(mtm);
            open_main_window(mtm);
            if app_state::SELFTEST_LAUNCH.load(std::sync::atomic::Ordering::Relaxed) {
                std::process::exit(0);
            }
        }

        #[unsafe(method(applicationShouldTerminateAfterLastWindowClosed:))]
        fn should_terminate_after_last_window_closed(&self, _sender: &NSApplication) -> bool {
            true
        }

        #[unsafe(method(application:openURLs:))]
        fn open_urls(&self, _app: &NSApplication, urls: &NSArray<NSURL>) {
            // Turn 1 stub: just log. Turn 2+ routes this to the document loader.
            for url in urls.iter() {
                if let Some(path) = url.path() {
                    eprintln!("open: {}", path);
                }
            }
        }
    }
);

impl AppDelegate {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(());
        unsafe { msg_send![super(this), init] }
    }
}

fn install_menu_bar(mtm: MainThreadMarker) {
    // macOS convention: the first menu in the menu bar is the "app" menu
    // and its title is overridden to the process name. Turn 1 only needs
    // Quit to work; richer menus come in later turns.
    let menubar = NSMenu::new(mtm);
    let app_menu_item = NSMenuItem::new(mtm);
    menubar.addItem(&app_menu_item);

    let app_menu = NSMenu::new(mtm);
    let quit_title = NSString::from_str("Quit Rapid View");
    let quit_key = NSString::from_str("q");
    let quit_item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &quit_title,
            Some(objc2::sel!(terminate:)),
            &quit_key,
        )
    };
    app_menu.addItem(&quit_item);
    app_menu_item.setSubmenu(Some(&app_menu));

    let app = NSApplication::sharedApplication(mtm);
    app.setMainMenu(Some(&menubar));
}

fn open_main_window(mtm: MainThreadMarker) {
    let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(960.0, 720.0));
    let style = NSWindowStyleMask::Titled
        | NSWindowStyleMask::Closable
        | NSWindowStyleMask::Resizable
        | NSWindowStyleMask::Miniaturizable;
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            frame,
            style,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    window.setTitle(&NSString::from_str("Rapid View"));
    window.center();
    window.makeKeyAndOrderFront(None);

    app_state::MAIN_WINDOW.with(|slot| *slot.borrow_mut() = Some(window));
}

fn main() {
    if std::env::args().any(|a| a == "--selftest-launch") {
        app_state::SELFTEST_LAUNCH.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    let mtm = MainThreadMarker::new().expect("must run on main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);

    let delegate = AppDelegate::new(mtm);
    let proto: &ProtocolObject<dyn NSApplicationDelegate> = ProtocolObject::from_ref(&*delegate);
    app.setDelegate(Some(proto));

    app.activate();
    app.run();
}
