//! Rapid View — native macOS JSON viewer.
//!
//! Turn 1: minimal scaffold. Opens a titled window, installs a menu bar,
//! and routes Finder "open with" / drag-drop via an NSApplicationDelegate.
//! Subsequent turns add the parser, CoreText rendering, and breadcrumbs.

#![deny(unsafe_op_in_unsafe_fn)]

mod doc;
mod json_view;
mod parser;

use json_view::{JsonView, ViewMode};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSApplicationDelegate, NSBackingStoreType,
    NSButton, NSColor, NSFont, NSLayoutConstraint, NSLayoutConstraintOrientation, NSMenu,
    NSMenuItem, NSPasteboard, NSPasteboardTypeString, NSScrollView, NSStackView,
    NSStackViewDistribution, NSTextField, NSUserInterfaceLayoutOrientation, NSView,
    NSViewBoundsDidChangeNotification, NSWindow, NSWindowStyleMask,
};
use objc2_foundation::{
    NSArray, NSNotification, NSNotificationCenter, NSObject, NSObjectProtocol, NSPoint, NSRect,
    NSSize, NSString, NSURL,
};

mod app_state {
    //! Process-wide state. Single-threaded access from the main thread only;
    //! the worker thread talks back via mpsc, so we don't need a Mutex here.
    use crate::json_view::JsonView;
    use objc2::rc::Retained;
    use objc2_app_kit::{NSButton, NSWindow};
    use std::cell::RefCell;
    use std::sync::atomic::AtomicBool;

    thread_local! {
        pub static MAIN_WINDOW: RefCell<Option<Retained<NSWindow>>> = const { RefCell::new(None) };
        pub static JSON_VIEW: RefCell<Option<Retained<JsonView>>> = const { RefCell::new(None) };
        pub static MODE_BUTTON: RefCell<Option<Retained<NSButton>>> = const { RefCell::new(None) };
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
            open_main_window(mtm, self);
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

    // Custom action methods (not part of any Cocoa protocol) live in
    // their own `impl` block so define_class! doesn't try to verify
    // them against NSApplicationDelegate.
    impl AppDelegate {
        #[unsafe(method(rvToggleMode:))]
        fn rv_toggle_mode(&self, _sender: &NSButton) {
            let new_mode = match json_view::view_mode() {
                ViewMode::Cursor => ViewMode::ScrollLock,
                ViewMode::ScrollLock => ViewMode::Cursor,
            };
            json_view::set_view_mode(new_mode);
            update_mode_button_title();
            refresh_current_path();
        }

        #[unsafe(method(rvTogglePrettify:))]
        fn rv_toggle_prettify(&self, _sender: &NSButton) {
            // T5 owns the actual prettify toggle (re-parses the pretty
            // buffer on the worker). Stub for now so the button is alive.
            eprintln!("prettify toggle — wired in T5");
        }

        #[unsafe(method(rvCopyJq:))]
        fn rv_copy_jq(&self, _sender: &NSButton) {
            let expr = app_state::JSON_VIEW.with(|slot| {
                slot.borrow()
                    .as_ref()
                    .map(|v| v.current_jq_expression())
                    .unwrap_or_else(|| String::from("."))
            });
            let pb = NSPasteboard::generalPasteboard();
            pb.clearContents();
            let ns = NSString::from_str(&expr);
            unsafe {
                let type_str: &NSString = NSPasteboardTypeString;
                let types = NSArray::from_slice(&[type_str]);
                pb.declareTypes_owner(&types, None);
                pb.setString_forType(&ns, type_str);
            }
            eprintln!("copied jq: {}", expr);
        }

        #[unsafe(method(rvClipDidScroll:))]
        fn rv_clip_did_scroll(&self, _notif: &NSNotification) {
            if json_view::view_mode() == ViewMode::ScrollLock {
                refresh_current_path();
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

fn open_main_window(mtm: MainThreadMarker, delegate: &AppDelegate) {
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

    let content_view = window.contentView().expect("window has content view");
    let content_bounds = content_view.bounds();

    // Scroll view with the custom JsonView as document view.
    let scroll = NSScrollView::initWithFrame(NSScrollView::alloc(mtm), content_bounds);
    scroll.setHasVerticalScroller(true);
    scroll.setHasHorizontalScroller(true);
    scroll.setAutohidesScrollers(true);
    scroll.setBorderType(objc2_app_kit::NSBorderType::NoBorder);

    let json_view = JsonView::new(mtm, json_view::initial_frame());
    scroll.setDocumentView(Some(&json_view));

    // Header bar: [Cursor] [Prettify] <breadcrumb label> [Copy jq]
    let delegate_obj = delegate as &objc2::runtime::AnyObject;
    let (header, breadcrumb_label, mode_button) = build_header_bar(mtm, delegate_obj);
    json_view.set_breadcrumb(breadcrumb_label);
    app_state::MODE_BUTTON.with(|slot| *slot.borrow_mut() = Some(mode_button));

    // Stack header over scroll view, filling the content view.
    let stack_views: Retained<NSArray<NSView>> = NSArray::from_slice(&[
        &*header as &NSView,
        &*scroll as &NSView,
    ]);
    let stack = NSStackView::stackViewWithViews(&stack_views, mtm);
    stack.setOrientation(NSUserInterfaceLayoutOrientation::Vertical);
    stack.setSpacing(0.0);
    stack.setDistribution(NSStackViewDistribution::Fill);
    stack.setTranslatesAutoresizingMaskIntoConstraints(false);

    content_view.addSubview(&stack);

    // Pin the outer stack to the content view's edges.
    let constraints = NSArray::from_retained_slice(&[
        stack
            .leadingAnchor()
            .constraintEqualToAnchor(&content_view.leadingAnchor()),
        stack
            .trailingAnchor()
            .constraintEqualToAnchor(&content_view.trailingAnchor()),
        stack
            .topAnchor()
            .constraintEqualToAnchor(&content_view.topAnchor()),
        stack
            .bottomAnchor()
            .constraintEqualToAnchor(&content_view.bottomAnchor()),
    ]);
    NSLayoutConstraint::activateConstraints(&constraints);

    // Scroll-lock observer: wake whenever the clip view's bounds change.
    let clip_view = scroll.contentView();
    clip_view.setPostsBoundsChangedNotifications(true);
    let center = NSNotificationCenter::defaultCenter();
    unsafe {
        let name: &NSString = NSViewBoundsDidChangeNotification;
        center.addObserver_selector_name_object(
            delegate_obj,
            objc2::sel!(rvClipDidScroll:),
            Some(name),
            Some(&clip_view),
        );
    }

    window.makeKeyAndOrderFront(None);

    app_state::MAIN_WINDOW.with(|slot| *slot.borrow_mut() = Some(window));
    app_state::JSON_VIEW.with(|slot| *slot.borrow_mut() = Some(json_view));

    update_mode_button_title();
}

fn build_header_bar(
    mtm: MainThreadMarker,
    target: &objc2::runtime::AnyObject,
) -> (Retained<NSStackView>, Retained<NSTextField>, Retained<NSButton>) {
    let mode_button = make_button(mtm, "Cursor", target, objc2::sel!(rvToggleMode:));
    let prettify_button = make_button(mtm, "Prettify", target, objc2::sel!(rvTogglePrettify:));
    let copy_button = make_button(mtm, "Copy jq", target, objc2::sel!(rvCopyJq:));

    let label = {
        let s = NSString::from_str(".");
        let tf = NSTextField::labelWithString(&s, mtm);
        tf.setFont(Some(&NSFont::userFixedPitchFontOfSize(12.0).unwrap()));
        tf.setTextColor(Some(&NSColor::secondaryLabelColor()));
        tf.setLineBreakMode(objc2_app_kit::NSLineBreakMode::ByTruncatingMiddle);
        tf.setSelectable(true);
        tf
    };

    let header_views: Retained<NSArray<NSView>> = NSArray::from_slice(&[
        &*mode_button as &NSView,
        &*prettify_button as &NSView,
        &*label as &NSView,
        &*copy_button as &NSView,
    ]);
    let header = NSStackView::stackViewWithViews(&header_views, mtm);
    header.setOrientation(NSUserInterfaceLayoutOrientation::Horizontal);
    header.setSpacing(8.0);
    header.setEdgeInsets(objc2_foundation::NSEdgeInsets {
        top: 6.0,
        left: 10.0,
        bottom: 6.0,
        right: 10.0,
    });

    // Let the label eat all leftover horizontal space between the left
    // and right button groups.
    header.setHuggingPriority_forOrientation(
        249.0,
        NSLayoutConstraintOrientation::Horizontal,
    );
    label.setContentHuggingPriority_forOrientation(
        10.0,
        NSLayoutConstraintOrientation::Horizontal,
    );
    label.setContentCompressionResistancePriority_forOrientation(
        100.0,
        NSLayoutConstraintOrientation::Horizontal,
    );

    (header, label, mode_button)
}

fn make_button(
    mtm: MainThreadMarker,
    title: &str,
    target: &objc2::runtime::AnyObject,
    action: objc2::runtime::Sel,
) -> Retained<NSButton> {
    let ns_title = NSString::from_str(title);
    let btn = unsafe {
        NSButton::buttonWithTitle_target_action(&ns_title, Some(target), Some(action), mtm)
    };
    btn.setBezelStyle(objc2_app_kit::NSBezelStyle::Automatic);
    btn
}

fn update_mode_button_title() {
    let title = match json_view::view_mode() {
        ViewMode::Cursor => "Cursor",
        ViewMode::ScrollLock => "Scroll-lock",
    };
    app_state::MODE_BUTTON.with(|slot| {
        if let Some(btn) = slot.borrow().as_ref() {
            btn.setTitle(&NSString::from_str(title));
        }
    });
}

fn refresh_current_path() {
    app_state::JSON_VIEW.with(|slot| {
        if let Some(view) = slot.borrow().as_ref() {
            view.refresh_path_display();
        }
    });
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
