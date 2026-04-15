//! Rapid View — native macOS JSON viewer.
//!
//! Turn 1: minimal scaffold. Opens a titled window, installs a menu bar,
//! and routes Finder "open with" / drag-drop via an NSApplicationDelegate.
//! Subsequent turns add the parser, CoreText rendering, and breadcrumbs.

#![deny(unsafe_op_in_unsafe_fn)]

mod doc;
mod json_view;
mod parser;
mod pretty;
mod worker;

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
    NSSize, NSString, NSTimer, NSURL,
};
use worker::{WorkerChannel, WorkerMsg};

mod app_state {
    //! Process-wide state. Single-threaded access from the main thread only;
    //! the worker thread talks back via mpsc, so we don't need a Mutex here.
    use crate::doc::Document;
    use crate::json_view::JsonView;
    use crate::worker::WorkerChannel;
    use objc2::rc::Retained;
    use objc2_app_kit::{NSButton, NSWindow};
    use objc2_foundation::NSTimer;
    use std::cell::{Cell, RefCell};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicI32};

    thread_local! {
        pub static MAIN_WINDOW: RefCell<Option<Retained<NSWindow>>> = const { RefCell::new(None) };
        pub static JSON_VIEW: RefCell<Option<Retained<JsonView>>> = const { RefCell::new(None) };
        pub static MODE_BUTTON: RefCell<Option<Retained<NSButton>>> = const { RefCell::new(None) };
        pub static PRETTIFY_BUTTON: RefCell<Option<Retained<NSButton>>> = const { RefCell::new(None) };

        pub static WORKER: RefCell<Option<WorkerChannel>> = const { RefCell::new(None) };
        pub static POLL_TIMER: RefCell<Option<Retained<NSTimer>>> = const { RefCell::new(None) };

        pub static CURRENT_PATH: RefCell<Option<String>> = const { RefCell::new(None) };
        pub static ORIGINAL_DOC: RefCell<Option<Arc<Document>>> = const { RefCell::new(None) };
        pub static PRETTY_DOC: RefCell<Option<Arc<Document>>> = const { RefCell::new(None) };
        pub static IS_PRETTY: Cell<bool> = const { Cell::new(false) };
        pub static PRETTY_PENDING: Cell<bool> = const { Cell::new(false) };
    }

    /// Number of outstanding worker jobs. Incremented when a job is
    /// spawned, decremented when a terminal message is processed. The
    /// polling timer stops as soon as this hits zero so we aren't
    /// burning a 60Hz tick for nothing.
    pub static WORK_PENDING: AtomicI32 = AtomicI32::new(0);

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
            toggle_prettify();
        }

        #[unsafe(method(rvWorkerTick:))]
        fn rv_worker_tick(&self, _timer: &NSTimer) {
            drain_worker();
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
    let (header, breadcrumb_label, mode_button, prettify_button) =
        build_header_bar(mtm, delegate_obj);
    json_view.set_breadcrumb(breadcrumb_label);
    app_state::MODE_BUTTON.with(|slot| *slot.borrow_mut() = Some(mode_button));
    app_state::PRETTIFY_BUTTON.with(|slot| *slot.borrow_mut() = Some(prettify_button));

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
) -> (
    Retained<NSStackView>,
    Retained<NSTextField>,
    Retained<NSButton>,
    Retained<NSButton>,
) {
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

    (header, label, mode_button, prettify_button)
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
    // Show a loading placeholder immediately so the UI never looks
    // frozen, then hand off to the worker.
    set_window_title(&format!("Rapid View — loading {}…", basename(path)));
    app_state::IS_PRETTY.with(|c| c.set(false));
    app_state::PRETTY_PENDING.with(|c| c.set(false));
    app_state::PRETTY_DOC.with(|slot| *slot.borrow_mut() = None);
    app_state::ORIGINAL_DOC.with(|slot| *slot.borrow_mut() = None);
    app_state::CURRENT_PATH.with(|slot| *slot.borrow_mut() = Some(path.to_string()));
    update_prettify_button_title();

    let tx = ensure_worker_channel();
    app_state::WORK_PENDING.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    worker::spawn_load(path.to_string(), tx);
    ensure_poll_timer();
}

fn ensure_worker_channel() -> std::sync::mpsc::Sender<WorkerMsg> {
    app_state::WORKER.with(|slot| {
        let mut b = slot.borrow_mut();
        if b.is_none() {
            *b = Some(WorkerChannel::new());
        }
        b.as_ref().unwrap().tx.clone()
    })
}

fn ensure_poll_timer() {
    app_state::POLL_TIMER.with(|slot| {
        if slot.borrow().is_some() {
            return;
        }
        // Every 16 ms — roughly one display frame. The tick handler
        // drops the timer as soon as WORK_PENDING falls to zero, so
        // this only burns cycles while something is actually parsing.
        let timer = with_app_delegate(|delegate| unsafe {
            NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(
                0.016,
                delegate,
                objc2::sel!(rvWorkerTick:),
                None,
                true,
            )
        });
        *slot.borrow_mut() = Some(timer);
    });
}

fn drain_worker() {
    // Copy messages out under a short borrow so downstream helpers can
    // re-enter app_state without tripping RefCell rules.
    let msgs: Vec<WorkerMsg> = app_state::WORKER.with(|slot| {
        let borrow = slot.borrow();
        let Some(chan) = borrow.as_ref() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        while let Ok(m) = chan.rx.try_recv() {
            out.push(m);
        }
        out
    });

    for msg in msgs {
        match msg {
            WorkerMsg::DocumentReady { doc, path } => {
                on_document_ready(doc, &path);
                app_state::WORK_PENDING.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
            }
            WorkerMsg::PrettyReady(doc) => {
                on_pretty_ready(doc);
                app_state::WORK_PENDING.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
            }
            WorkerMsg::Error(e) => {
                eprintln!("rapid-view: {}", e);
                set_window_title("Rapid View");
                app_state::WORK_PENDING.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }

    if app_state::WORK_PENDING.load(std::sync::atomic::Ordering::Relaxed) <= 0 {
        app_state::POLL_TIMER.with(|slot| {
            if let Some(t) = slot.borrow().as_ref() {
                t.invalidate();
            }
            *slot.borrow_mut() = None;
        });
    }
}

fn on_document_ready(doc: std::sync::Arc<doc::Document>, path: &str) {
    let size = doc.bytes.len();
    let lines = doc.line_count();
    eprintln!("loaded {} ({} bytes, {} lines)", path, size, lines);
    app_state::ORIGINAL_DOC.with(|slot| *slot.borrow_mut() = Some(doc.clone()));
    app_state::IS_PRETTY.with(|c| c.set(false));
    app_state::JSON_VIEW.with(|slot| {
        if let Some(view) = slot.borrow().as_ref() {
            view.set_document(doc);
        }
    });
    set_window_title(&format!("Rapid View — {}", basename(path)));
    update_prettify_button_title();
}

fn on_pretty_ready(doc: std::sync::Arc<doc::Document>) {
    app_state::PRETTY_DOC.with(|slot| *slot.borrow_mut() = Some(doc.clone()));
    app_state::PRETTY_PENDING.with(|c| c.set(false));
    // If the user is still asking to see pretty (IS_PRETTY was set
    // optimistically when they clicked), swap it in now.
    if app_state::IS_PRETTY.with(|c| c.get()) {
        app_state::JSON_VIEW.with(|slot| {
            if let Some(view) = slot.borrow().as_ref() {
                view.set_document(doc);
            }
        });
    }
    update_prettify_button_title();
    refresh_title_for_current_mode();
}

fn toggle_prettify() {
    let is_pretty_now = app_state::IS_PRETTY.with(|c| c.get());
    if is_pretty_now {
        // Swap back to the original document.
        let original = app_state::ORIGINAL_DOC.with(|slot| slot.borrow().clone());
        let Some(original) = original else {
            return;
        };
        app_state::IS_PRETTY.with(|c| c.set(false));
        app_state::JSON_VIEW.with(|slot| {
            if let Some(view) = slot.borrow().as_ref() {
                view.set_document(original);
            }
        });
        update_prettify_button_title();
        refresh_title_for_current_mode();
        return;
    }

    // Flip to pretty. If it's already cached, swap instantly. Otherwise
    // spawn the worker and flip optimistically — the tick handler will
    // actually install the doc when it arrives.
    if let Some(pretty) = app_state::PRETTY_DOC.with(|slot| slot.borrow().clone()) {
        app_state::IS_PRETTY.with(|c| c.set(true));
        app_state::JSON_VIEW.with(|slot| {
            if let Some(view) = slot.borrow().as_ref() {
                view.set_document(pretty);
            }
        });
        update_prettify_button_title();
        refresh_title_for_current_mode();
        return;
    }

    let Some(original) = app_state::ORIGINAL_DOC.with(|slot| slot.borrow().clone()) else {
        return;
    };
    let source = original.bytes.clone();
    app_state::IS_PRETTY.with(|c| c.set(true));
    app_state::PRETTY_PENDING.with(|c| c.set(true));
    update_prettify_button_title();
    set_window_title("Rapid View — prettifying…");

    let tx = ensure_worker_channel();
    app_state::WORK_PENDING.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    worker::spawn_prettify(source, tx);
    ensure_poll_timer();
}

fn update_prettify_button_title() {
    let title = if app_state::IS_PRETTY.with(|c| c.get()) {
        "Original"
    } else {
        "Prettify"
    };
    app_state::PRETTIFY_BUTTON.with(|slot| {
        if let Some(btn) = slot.borrow().as_ref() {
            btn.setTitle(&NSString::from_str(title));
        }
    });
}

fn refresh_title_for_current_mode() {
    let Some(path) = app_state::CURRENT_PATH.with(|slot| slot.borrow().clone()) else {
        return;
    };
    let suffix = if app_state::IS_PRETTY.with(|c| c.get()) {
        " · pretty"
    } else {
        ""
    };
    set_window_title(&format!("Rapid View — {}{}", basename(&path), suffix));
}

fn set_window_title(title: &str) {
    let ns = NSString::from_str(title);
    app_state::MAIN_WINDOW.with(|slot| {
        if let Some(window) = slot.borrow().as_ref() {
            window.setTitle(&ns);
        }
    });
}

fn basename(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string())
}

/// Run `f` with a borrow of the `AppDelegate` stored in the process-wide
/// `NSApplication`. The delegate is retained elsewhere (by NSApplication)
/// so we just cast the delegate pointer.
fn with_app_delegate<R>(f: impl FnOnce(&objc2::runtime::AnyObject) -> R) -> R {
    let mtm = MainThreadMarker::new().expect("main thread");
    let app = NSApplication::sharedApplication(mtm);
    let delegate = app
        .delegate()
        .expect("AppDelegate installed before worker ticks");
    // ProtocolObject derefs to NSObject which is an AnyObject — go
    // through a pointer cast rather than a Deref we don't have.
    let delegate_ptr: *const objc2::runtime::ProtocolObject<
        dyn NSApplicationDelegate,
    > = &*delegate;
    let delegate_obj: &objc2::runtime::AnyObject =
        unsafe { &*(delegate_ptr as *const objc2::runtime::AnyObject) };
    f(delegate_obj)
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
