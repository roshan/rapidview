//! Rapid View — native macOS JSON viewer.
//!
//! Multi-window: each tab is a separate `NSWindow` sharing a common
//! `tabbingIdentifier` so AppKit merges them automatically. Per-tab
//! state lives in `app_state::WINDOWS`, keyed by the raw `NSWindow`
//! pointer cast to `usize`. Action handlers look up the state by the
//! sender button's window (or, for menu-triggered actions, the current
//! key window). Worker messages carry the same `WindowId` so a parse
//! that finishes after the user has switched tabs still lands in the
//! right view.

#![deny(unsafe_op_in_unsafe_fn)]

mod doc;
mod json_view;
mod parser;
mod pretty;
mod worker;

use json_view::{JsonView, ViewMode};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSApplicationDelegate, NSBackingStoreType,
    NSBorderType, NSButton, NSColor, NSEventModifierFlags, NSFont, NSLayoutConstraint,
    NSLayoutConstraintOrientation, NSLineBreakMode, NSMenu, NSMenuItem, NSModalResponse,
    NSOpenPanel, NSPasteboard, NSPasteboardTypeString, NSScrollView, NSStackView,
    NSStackViewDistribution, NSTextField, NSUserInterfaceLayoutOrientation, NSView,
    NSViewBoundsDidChangeNotification, NSWindow, NSWindowDelegate, NSWindowStyleMask,
    NSWindowTabbingMode,
};
use objc2_foundation::{
    NSArray, NSEdgeInsets, NSNotification, NSNotificationCenter, NSObject, NSObjectProtocol,
    NSPoint, NSRect, NSSize, NSString, NSTimer, NSURL,
};
use worker::{WindowId, WorkerChannel, WorkerMsg};

mod app_state {
    //! Process-wide state. Single-threaded access from the main thread only;
    //! the worker thread talks back via mpsc, so we don't need a Mutex here.
    use crate::doc::Document;
    use crate::json_view::JsonView;
    use crate::worker::{WindowId, WorkerChannel};
    use objc2::rc::Retained;
    use objc2_app_kit::{NSButton, NSTextField, NSWindow};
    use objc2_foundation::NSTimer;
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicI32};

    /// Per-tab state. Stays in the `WINDOWS` map for the lifetime of
    /// the window and is dropped by the `windowWillClose:` hook.
    pub struct WindowState {
        pub window: Retained<NSWindow>,
        pub json_view: Retained<JsonView>,
        pub mode_button: Retained<NSButton>,
        pub prettify_button: Retained<NSButton>,
        /// Kept alive so the view's weak reference to the breadcrumb
        /// label stays valid for the window's lifetime.
        #[allow(dead_code)]
        pub breadcrumb: Retained<NSTextField>,
        pub current_path: Option<String>,
        pub original_doc: Option<Arc<Document>>,
        pub pretty_doc: Option<Arc<Document>>,
        pub is_pretty: bool,
        pub pretty_pending: bool,
    }

    thread_local! {
        pub static WINDOWS: RefCell<HashMap<WindowId, WindowState>> =
            RefCell::new(HashMap::new());
        pub static WORKER: RefCell<Option<WorkerChannel>> = const { RefCell::new(None) };
        pub static POLL_TIMER: RefCell<Option<Retained<NSTimer>>> = const { RefCell::new(None) };
    }

    /// Number of outstanding worker jobs across all tabs. Incremented
    /// on spawn and decremented when a terminal message is processed;
    /// the polling timer tears itself down as soon as this hits zero.
    pub static WORK_PENDING: AtomicI32 = AtomicI32::new(0);

    pub static SELFTEST_LAUNCH: AtomicBool = AtomicBool::new(false);
}

/// Raw pointer identity for an `NSWindow`. Safe because we only use
/// the number as a map key and never dereference it.
fn window_id_of(w: &NSWindow) -> WindowId {
    (w as *const NSWindow) as WindowId
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
            // Always create one window up front. Argv files load into
            // it (first) or new tabs (subsequent).
            new_window(mtm, self);
            for arg in std::env::args().skip(1) {
                if arg.starts_with('-') {
                    continue;
                }
                let target = find_or_create_tab_for_load(mtm, self);
                load_file_into_window(target, &arg);
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
            let mtm = self.mtm();
            for url in urls.iter() {
                if let Some(path) = url.path() {
                    // Each incoming URL becomes a fresh tab unless we
                    // still have a blank initial window we can reuse.
                    let target = find_or_create_tab_for_load(mtm, self);
                    load_file_into_window(target, &path.to_string());
                }
            }
        }
    }

    unsafe impl NSWindowDelegate for AppDelegate {
        #[unsafe(method(windowWillClose:))]
        fn window_will_close(&self, notif: &NSNotification) {
            let Some(obj) = notif.object() else { return };
            // The notification's object is always the closing NSWindow.
            let id = Retained::as_ptr(&obj) as WindowId;
            app_state::WINDOWS.with(|m| {
                m.borrow_mut().remove(&id);
            });
        }
    }

    // Custom action methods live in their own impl block so define_class!
    // doesn't try to verify them against any Cocoa protocol.
    impl AppDelegate {
        #[unsafe(method(rvNewWindow:))]
        fn rv_new_window(&self, _sender: &AnyObject) {
            let mtm = self.mtm();
            new_window(mtm, self);
        }

        #[unsafe(method(rvOpenDocument:))]
        fn rv_open_document(&self, _sender: &AnyObject) {
            let mtm = self.mtm();
            show_open_panel(mtm, self);
        }

        #[unsafe(method(rvToggleMode:))]
        fn rv_toggle_mode(&self, sender: &NSButton) {
            if let Some(id) = window_id_from_button(sender) {
                toggle_mode(id);
            }
        }

        #[unsafe(method(rvTogglePrettify:))]
        fn rv_toggle_prettify(&self, sender: &NSButton) {
            if let Some(id) = window_id_from_button(sender) {
                toggle_prettify(id);
            }
        }

        #[unsafe(method(rvPasteJson:))]
        fn rv_paste_json(&self, sender: &NSButton) {
            if let Some(id) = window_id_from_button(sender) {
                paste_from_clipboard(id);
            }
        }

        #[unsafe(method(rvClearDocument:))]
        fn rv_clear_document(&self, sender: &NSButton) {
            if let Some(id) = window_id_from_button(sender) {
                clear_view(id);
            }
        }

        #[unsafe(method(rvCopyJq:))]
        fn rv_copy_jq(&self, sender: &NSButton) {
            if let Some(id) = window_id_from_button(sender) {
                copy_jq(id);
            }
        }

        #[unsafe(method(rvWorkerTick:))]
        fn rv_worker_tick(&self, _timer: &NSTimer) {
            drain_worker();
        }

        #[unsafe(method(rvClipDidScroll:))]
        fn rv_clip_did_scroll(&self, notif: &NSNotification) {
            // The notification's object is the clip view whose bounds
            // changed. Its window() tells us which tab to refresh.
            let Some(obj) = notif.object() else { return };
            let clip_ptr = Retained::as_ptr(&obj) as *const objc2_app_kit::NSClipView;
            let window_ptr = unsafe {
                let clip: &objc2_app_kit::NSClipView = &*clip_ptr;
                clip.window()
            };
            let Some(window) = window_ptr else { return };
            let id = window_id_of(&window);
            app_state::WINDOWS.with(|m| {
                if let Some(state) = m.borrow().get(&id) {
                    if state.json_view.view_mode() == ViewMode::ScrollLock {
                        state.json_view.refresh_path_display();
                    }
                }
            });
        }
    }
);

impl AppDelegate {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(());
        unsafe { msg_send![super(this), init] }
    }
}

// -- menu bar ---------------------------------------------------------

fn install_menu_bar(mtm: MainThreadMarker) {
    let menubar = NSMenu::new(mtm);

    // App menu (Quit).
    let app_menu_item = NSMenuItem::new(mtm);
    menubar.addItem(&app_menu_item);
    let app_menu = NSMenu::new(mtm);
    add_menu_item(
        mtm,
        &app_menu,
        "Quit Rapid View",
        objc2::sel!(terminate:),
        "q",
        NSEventModifierFlags::Command,
    );
    app_menu_item.setSubmenu(Some(&app_menu));

    // File menu: New Window (Cmd+N), Open… (Cmd+O), Close (Cmd+W).
    let file_menu_item = NSMenuItem::new(mtm);
    menubar.addItem(&file_menu_item);
    let file_menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str("File"));
    add_menu_item(
        mtm,
        &file_menu,
        "New Window",
        objc2::sel!(rvNewWindow:),
        "n",
        NSEventModifierFlags::Command,
    );
    add_menu_item(
        mtm,
        &file_menu,
        "Open…",
        objc2::sel!(rvOpenDocument:),
        "o",
        NSEventModifierFlags::Command,
    );
    add_menu_item(
        mtm,
        &file_menu,
        "Close",
        objc2::sel!(performClose:),
        "w",
        NSEventModifierFlags::Command,
    );
    file_menu_item.setSubmenu(Some(&file_menu));

    let app = NSApplication::sharedApplication(mtm);
    app.setMainMenu(Some(&menubar));
}

fn add_menu_item(
    mtm: MainThreadMarker,
    menu: &NSMenu,
    title: &str,
    action: objc2::runtime::Sel,
    key: &str,
    modifiers: NSEventModifierFlags,
) {
    let item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str(title),
            Some(action),
            &NSString::from_str(key),
        )
    };
    item.setKeyEquivalentModifierMask(modifiers);
    menu.addItem(&item);
}

// -- window builder ---------------------------------------------------

fn new_window(mtm: MainThreadMarker, delegate: &AppDelegate) -> WindowId {
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

    // Tabbing: every window shares an identifier so AppKit auto-tabs
    // them. Preferred mode overrides the system-wide setting.
    window.setTabbingIdentifier(&NSString::from_str("RapidView"));
    window.setTabbingMode(NSWindowTabbingMode::Preferred);

    // Window delegate tracks windowWillClose: so we can drop state.
    let delegate_proto: &ProtocolObject<dyn NSWindowDelegate> =
        ProtocolObject::from_ref(delegate);
    window.setDelegate(Some(delegate_proto));

    // Observer for scroll-lock — registered per-window so we can tear
    // it down with the window.
    let content_view = window.contentView().expect("window has content view");
    let content_bounds = content_view.bounds();

    let scroll = NSScrollView::initWithFrame(NSScrollView::alloc(mtm), content_bounds);
    scroll.setHasVerticalScroller(true);
    scroll.setHasHorizontalScroller(true);
    scroll.setAutohidesScrollers(true);
    scroll.setBorderType(NSBorderType::NoBorder);

    let json_view = JsonView::new(mtm, json_view::initial_frame());
    scroll.setDocumentView(Some(&json_view));

    let delegate_obj = delegate as &AnyObject;
    let (header, breadcrumb_label, mode_button, prettify_button) =
        build_header_bar(mtm, delegate_obj);
    json_view.set_breadcrumb(breadcrumb_label.clone());

    let stack_views: Retained<NSArray<NSView>> =
        NSArray::from_slice(&[&*header as &NSView, &*scroll as &NSView]);
    let stack = NSStackView::stackViewWithViews(&stack_views, mtm);
    stack.setOrientation(NSUserInterfaceLayoutOrientation::Vertical);
    stack.setSpacing(0.0);
    stack.setDistribution(NSStackViewDistribution::Fill);
    stack.setTranslatesAutoresizingMaskIntoConstraints(false);

    content_view.addSubview(&stack);

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

    // Scroll-lock observer tied to this window's clip view.
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

    let id = window_id_of(&window);
    let state = app_state::WindowState {
        window,
        json_view,
        mode_button,
        prettify_button,
        breadcrumb: breadcrumb_label,
        current_path: None,
        original_doc: None,
        pretty_doc: None,
        is_pretty: false,
        pretty_pending: false,
    };
    app_state::WINDOWS.with(|m| {
        m.borrow_mut().insert(id, state);
    });
    update_mode_button_title(id);
    id
}

fn build_header_bar(
    mtm: MainThreadMarker,
    target: &AnyObject,
) -> (
    Retained<NSStackView>,
    Retained<NSTextField>,
    Retained<NSButton>,
    Retained<NSButton>,
) {
    let cmd = NSEventModifierFlags::Command;
    let clipboard_button = make_button(mtm, "Clipboard", target, objc2::sel!(rvPasteJson:));
    set_key(&clipboard_button, "v", cmd);
    let clear_button = make_button(mtm, "Clear", target, objc2::sel!(rvClearDocument:));
    set_key(&clear_button, "k", cmd);
    let mode_button = make_button(mtm, "Cursor", target, objc2::sel!(rvToggleMode:));
    set_key(&mode_button, "l", cmd);
    let prettify_button = make_button(mtm, "Prettify", target, objc2::sel!(rvTogglePrettify:));
    set_key(&prettify_button, "p", cmd);
    let copy_button = make_button(mtm, "Copy jq", target, objc2::sel!(rvCopyJq:));
    set_key(&copy_button, "c", cmd);

    let label = {
        let s = NSString::from_str(".");
        let tf = NSTextField::labelWithString(&s, mtm);
        tf.setFont(Some(&NSFont::userFixedPitchFontOfSize(12.0).unwrap()));
        tf.setTextColor(Some(&NSColor::secondaryLabelColor()));
        tf.setLineBreakMode(NSLineBreakMode::ByTruncatingMiddle);
        tf.setSelectable(true);
        tf
    };

    let header_views: Retained<NSArray<NSView>> = NSArray::from_slice(&[
        &*clipboard_button as &NSView,
        &*clear_button as &NSView,
        &*mode_button as &NSView,
        &*prettify_button as &NSView,
        &*label as &NSView,
        &*copy_button as &NSView,
    ]);
    let header = NSStackView::stackViewWithViews(&header_views, mtm);
    header.setOrientation(NSUserInterfaceLayoutOrientation::Horizontal);
    header.setSpacing(8.0);
    header.setEdgeInsets(NSEdgeInsets {
        top: 6.0,
        left: 10.0,
        bottom: 6.0,
        right: 10.0,
    });

    header.setHuggingPriority_forOrientation(249.0, NSLayoutConstraintOrientation::Horizontal);
    label.setContentHuggingPriority_forOrientation(10.0, NSLayoutConstraintOrientation::Horizontal);
    label.setContentCompressionResistancePriority_forOrientation(
        100.0,
        NSLayoutConstraintOrientation::Horizontal,
    );

    (header, label, mode_button, prettify_button)
}

fn make_button(
    mtm: MainThreadMarker,
    title: &str,
    target: &AnyObject,
    action: objc2::runtime::Sel,
) -> Retained<NSButton> {
    let ns_title = NSString::from_str(title);
    let btn = unsafe {
        NSButton::buttonWithTitle_target_action(&ns_title, Some(target), Some(action), mtm)
    };
    btn.setBezelStyle(objc2_app_kit::NSBezelStyle::Automatic);
    btn
}

fn set_key(btn: &NSButton, key: &str, modifiers: NSEventModifierFlags) {
    btn.setKeyEquivalent(&NSString::from_str(key));
    btn.setKeyEquivalentModifierMask(modifiers);
}

// -- window-state helpers --------------------------------------------

fn window_id_from_button(btn: &NSButton) -> Option<WindowId> {
    let win = btn.window()?;
    Some(window_id_of(&win))
}

/// Reuse the current key window if it's empty; otherwise create a new
/// tab. Matches the "first-file-into-the-blank-tab" launch experience.
fn find_or_create_tab_for_load(mtm: MainThreadMarker, delegate: &AppDelegate) -> WindowId {
    let key_window_id = {
        let app = NSApplication::sharedApplication(mtm);
        app.keyWindow().map(|w| window_id_of(&w))
    };
    if let Some(id) = key_window_id {
        let is_blank = app_state::WINDOWS.with(|m| {
            m.borrow()
                .get(&id)
                .map(|s| s.original_doc.is_none() && s.current_path.is_none())
                .unwrap_or(false)
        });
        if is_blank {
            return id;
        }
    }
    new_window(mtm, delegate)
}

fn show_open_panel(mtm: MainThreadMarker, delegate: &AppDelegate) {
    let panel = NSOpenPanel::openPanel(mtm);
    panel.setCanChooseFiles(true);
    panel.setCanChooseDirectories(false);
    panel.setAllowsMultipleSelection(true);
    let response = panel.runModal();
    // NSModalResponseOK is 1; anything else (Cancel, etc.) aborts.
    const NS_MODAL_RESPONSE_OK: NSModalResponse = 1;
    if response != NS_MODAL_RESPONSE_OK {
        return;
    }
    let urls = panel.URLs();
    for url in urls.iter() {
        if let Some(path) = url.path() {
            let target = find_or_create_tab_for_load(mtm, delegate);
            load_file_into_window(target, &path.to_string());
        }
    }
}

// -- per-window actions ----------------------------------------------

fn clear_view(id: WindowId) {
    app_state::WINDOWS.with(|m| {
        if let Some(state) = m.borrow_mut().get_mut(&id) {
            state.original_doc = None;
            state.pretty_doc = None;
            state.current_path = None;
            state.is_pretty = false;
            state.pretty_pending = false;
            state.prettify_button.setTitle(&NSString::from_str("Prettify"));
            state.window.setTitle(&NSString::from_str("Rapid View"));
            state.json_view.clear_document();
        }
    });
}

fn paste_from_clipboard(id: WindowId) {
    let pb = NSPasteboard::generalPasteboard();
    let maybe_text = unsafe {
        let type_str: &NSString = NSPasteboardTypeString;
        pb.stringForType(type_str)
    };
    let Some(ns_text) = maybe_text else {
        eprintln!("rapid-view: clipboard has no text");
        return;
    };
    let text = ns_text.to_string();
    if text.trim().is_empty() {
        eprintln!("rapid-view: clipboard text is empty");
        return;
    }

    let label = "<clipboard>".to_string();
    app_state::WINDOWS.with(|m| {
        if let Some(state) = m.borrow_mut().get_mut(&id) {
            state.is_pretty = false;
            state.pretty_pending = false;
            state.pretty_doc = None;
            state.original_doc = None;
            state.current_path = Some(label.clone());
            state
                .window
                .setTitle(&NSString::from_str("Rapid View — parsing clipboard…"));
            state.prettify_button.setTitle(&NSString::from_str("Prettify"));
        }
    });

    let tx = ensure_worker_channel();
    app_state::WORK_PENDING.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    worker::spawn_parse_bytes(id, text.into_bytes(), label, tx);
    ensure_poll_timer();
}

fn load_file_into_window(id: WindowId, path: &str) {
    let name = basename(path);
    app_state::WINDOWS.with(|m| {
        if let Some(state) = m.borrow_mut().get_mut(&id) {
            state.is_pretty = false;
            state.pretty_pending = false;
            state.pretty_doc = None;
            state.original_doc = None;
            state.current_path = Some(path.to_string());
            state
                .window
                .setTitle(&NSString::from_str(&format!("Rapid View — loading {}…", name)));
            state.prettify_button.setTitle(&NSString::from_str("Prettify"));
        }
    });

    let tx = ensure_worker_channel();
    app_state::WORK_PENDING.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    worker::spawn_load(id, path.to_string(), tx);
    ensure_poll_timer();
}

fn toggle_mode(id: WindowId) {
    app_state::WINDOWS.with(|m| {
        let borrow = m.borrow();
        let Some(state) = borrow.get(&id) else { return };
        let next = match state.json_view.view_mode() {
            ViewMode::Cursor => ViewMode::ScrollLock,
            ViewMode::ScrollLock => ViewMode::Cursor,
        };
        state.json_view.set_view_mode(next);
        state.json_view.refresh_path_display();
    });
    update_mode_button_title(id);
}

fn update_mode_button_title(id: WindowId) {
    app_state::WINDOWS.with(|m| {
        if let Some(state) = m.borrow().get(&id) {
            let title = match state.json_view.view_mode() {
                ViewMode::Cursor => "Cursor",
                ViewMode::ScrollLock => "Scroll-lock",
            };
            state.mode_button.setTitle(&NSString::from_str(title));
        }
    });
}

fn copy_jq(id: WindowId) {
    let expr = app_state::WINDOWS.with(|m| {
        m.borrow()
            .get(&id)
            .map(|s| s.json_view.current_jq_expression())
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

fn toggle_prettify(id: WindowId) {
    // Decide what to do under a short borrow; anything that calls into
    // the view or spawns workers happens after the borrow drops.
    enum Action {
        SwapToOriginal(std::sync::Arc<doc::Document>),
        SwapToCachedPretty(std::sync::Arc<doc::Document>),
        SpawnPretty(doc::ByteSource),
        Nothing,
    }

    let action = app_state::WINDOWS.with(|m| {
        let borrow = m.borrow();
        let Some(state) = borrow.get(&id) else {
            return Action::Nothing;
        };
        if state.is_pretty {
            return state
                .original_doc
                .clone()
                .map(Action::SwapToOriginal)
                .unwrap_or(Action::Nothing);
        }
        if let Some(pretty) = state.pretty_doc.clone() {
            return Action::SwapToCachedPretty(pretty);
        }
        state
            .original_doc
            .as_ref()
            .map(|d| Action::SpawnPretty(d.bytes.clone()))
            .unwrap_or(Action::Nothing)
    });

    match action {
        Action::Nothing => {}
        Action::SwapToOriginal(doc) => {
            app_state::WINDOWS.with(|m| {
                if let Some(state) = m.borrow_mut().get_mut(&id) {
                    state.is_pretty = false;
                    state.json_view.set_document(doc);
                    state.prettify_button.setTitle(&NSString::from_str("Prettify"));
                    refresh_title(state);
                }
            });
        }
        Action::SwapToCachedPretty(doc) => {
            app_state::WINDOWS.with(|m| {
                if let Some(state) = m.borrow_mut().get_mut(&id) {
                    state.is_pretty = true;
                    state.json_view.set_document(doc);
                    state.prettify_button.setTitle(&NSString::from_str("Original"));
                    refresh_title(state);
                }
            });
        }
        Action::SpawnPretty(source) => {
            app_state::WINDOWS.with(|m| {
                if let Some(state) = m.borrow_mut().get_mut(&id) {
                    state.is_pretty = true;
                    state.pretty_pending = true;
                    state.prettify_button.setTitle(&NSString::from_str("Original"));
                    state
                        .window
                        .setTitle(&NSString::from_str("Rapid View — prettifying…"));
                }
            });
            let tx = ensure_worker_channel();
            app_state::WORK_PENDING.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            worker::spawn_prettify(id, source, tx);
            ensure_poll_timer();
        }
    }
}

fn refresh_title(state: &app_state::WindowState) {
    let Some(path) = state.current_path.as_ref() else {
        state.window.setTitle(&NSString::from_str("Rapid View"));
        return;
    };
    let suffix = if state.is_pretty { " · pretty" } else { "" };
    let title = format!("Rapid View — {}{}", basename(path), suffix);
    state.window.setTitle(&NSString::from_str(&title));
}

// -- worker dispatch --------------------------------------------------

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
            WorkerMsg::DocumentReady {
                window_id,
                doc,
                path,
            } => {
                on_document_ready(window_id, doc, &path);
                app_state::WORK_PENDING.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
            }
            WorkerMsg::PrettyReady { window_id, doc } => {
                on_pretty_ready(window_id, doc);
                app_state::WORK_PENDING.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
            }
            WorkerMsg::Error { window_id, message } => {
                eprintln!("rapid-view: {}", message);
                app_state::WINDOWS.with(|m| {
                    if let Some(state) = m.borrow_mut().get_mut(&window_id) {
                        state.window.setTitle(&NSString::from_str("Rapid View"));
                    }
                });
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

fn on_document_ready(id: WindowId, doc: std::sync::Arc<doc::Document>, path: &str) {
    let size = doc.bytes.len();
    let lines = doc.line_count();
    eprintln!("loaded {} ({} bytes, {} lines)", path, size, lines);
    app_state::WINDOWS.with(|m| {
        if let Some(state) = m.borrow_mut().get_mut(&id) {
            state.original_doc = Some(doc.clone());
            state.pretty_doc = None;
            state.is_pretty = false;
            state.current_path = Some(path.to_string());
            state.json_view.set_document(doc);
            state.prettify_button.setTitle(&NSString::from_str("Prettify"));
            refresh_title(state);
        }
    });
}

fn on_pretty_ready(id: WindowId, doc: std::sync::Arc<doc::Document>) {
    app_state::WINDOWS.with(|m| {
        if let Some(state) = m.borrow_mut().get_mut(&id) {
            state.pretty_doc = Some(doc.clone());
            state.pretty_pending = false;
            // If the user is still asking for pretty (set optimistically
            // at click time), install it; otherwise keep the cache for
            // the next toggle.
            if state.is_pretty {
                state.json_view.set_document(doc);
                state.prettify_button.setTitle(&NSString::from_str("Original"));
            }
            refresh_title(state);
        }
    });
}

// -- misc helpers ----------------------------------------------------

fn basename(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string())
}

/// Borrow the process-wide `AppDelegate` as an `AnyObject` so we can
/// pass it as a selector target without caring about the protocol type.
fn with_app_delegate<R>(f: impl FnOnce(&AnyObject) -> R) -> R {
    let mtm = MainThreadMarker::new().expect("main thread");
    let app = NSApplication::sharedApplication(mtm);
    let delegate = app
        .delegate()
        .expect("AppDelegate installed before worker ticks");
    let delegate_ptr: *const ProtocolObject<dyn NSApplicationDelegate> = &*delegate;
    let delegate_obj: &AnyObject = unsafe { &*(delegate_ptr as *const AnyObject) };
    f(delegate_obj)
}

fn main() {
    if std::env::args().any(|a| a == "--selftest-launch") {
        app_state::SELFTEST_LAUNCH.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    let mtm = MainThreadMarker::new().expect("must run on main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);

    // Let AppKit auto-tab windows with the same tabbingIdentifier
    // regardless of the user's global "Prefer tabs" preference.
    NSWindow::setAllowsAutomaticWindowTabbing(true, mtm);

    let delegate = AppDelegate::new(mtm);
    let proto: &ProtocolObject<dyn NSApplicationDelegate> = ProtocolObject::from_ref(&*delegate);
    app.setDelegate(Some(proto));

    app.activate();
    app.run();
}
