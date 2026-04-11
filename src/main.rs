//! Rapid View — native macOS JSON viewer.
//!
//! Turn 1: minimal scaffold. Opens a titled window, installs a menu bar,
//! and routes Finder "open with" / drag-drop via an NSApplicationDelegate.
//! Subsequent turns add the parser, CoreText rendering, and breadcrumbs.

#![deny(unsafe_op_in_unsafe_fn)]

mod doc;
mod json_view;
mod parser;

use json_view::JsonView;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSApplicationDelegate, NSAutoresizingMaskOptions,
    NSBackingStoreType, NSMenu, NSMenuItem, NSScrollView, NSWindow, NSWindowStyleMask,
};
use objc2_foundation::{
    NSArray, NSNotification, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString, NSURL,
};

mod app_state {
    //! Process-wide state. Single-threaded access from the main thread only;
    //! the worker thread talks back via mpsc, so we don't need a Mutex here.
    use crate::json_view::JsonView;
    use objc2::rc::Retained;
    use objc2_app_kit::NSWindow;
    use std::cell::RefCell;
    use std::sync::atomic::AtomicBool;

    thread_local! {
        pub static MAIN_WINDOW: RefCell<Option<Retained<NSWindow>>> = const { RefCell::new(None) };
        pub static JSON_VIEW: RefCell<Option<Retained<JsonView>>> = const { RefCell::new(None) };
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
            // Dev convenience: first non-flag argv is treated as a file to open.
            for arg in std::env::args().skip(1) {
                if !arg.starts_with('-') {
                    load_file_from_path(&arg);
                    break;
                }
            }
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
            for url in urls.iter() {
                if let Some(path) = url.path() {
                    load_file_from_path(&path.to_string());
                    break;
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

    // Scroll view covering the full content area.
    let content_frame = window
        .contentView()
        .expect("window has content view")
        .frame();
    let scroll = NSScrollView::initWithFrame(NSScrollView::alloc(mtm), content_frame);
    scroll.setHasVerticalScroller(true);
    scroll.setHasHorizontalScroller(true);
    scroll.setAutohidesScrollers(true);
    scroll.setBorderType(objc2_app_kit::NSBorderType::NoBorder);
    scroll.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
    );

    let json_view = JsonView::new(mtm, json_view::initial_frame());
    scroll.setDocumentView(Some(&json_view));

    window.setContentView(Some(&scroll));
    window.makeKeyAndOrderFront(None);

    app_state::MAIN_WINDOW.with(|slot| *slot.borrow_mut() = Some(window));
    app_state::JSON_VIEW.with(|slot| *slot.borrow_mut() = Some(json_view));
}

fn load_file_from_path(path: &str) {
    let t0 = std::time::Instant::now();
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("rapid-view: failed to read {}: {}", path, e);
            return;
        }
    };
    let size = bytes.len();
    let document = doc::Document::from_bytes(bytes);
    let parse_ms = t0.elapsed().as_secs_f64() * 1000.0;
    eprintln!(
        "loaded {} ({} bytes, {} lines) in {:.1} ms",
        path,
        size,
        document.line_count(),
        parse_ms
    );
    app_state::JSON_VIEW.with(|slot| {
        if let Some(view) = slot.borrow().as_ref() {
            view.set_document(document);
        }
    });
    app_state::MAIN_WINDOW.with(|slot| {
        if let Some(window) = slot.borrow().as_ref() {
            let title = NSString::from_str(&format!(
                "Rapid View — {}",
                std::path::Path::new(path)
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.to_string())
            ));
            window.setTitle(&title);
        }
    });
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
