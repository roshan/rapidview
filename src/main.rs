//! Rapid View — native macOS JSON / XML viewer.
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
mod doc_view;
mod format;
mod worker;

use doc_view::DocView;
use format::Format;
use objc2_app_kit::{NSControlTextEditingDelegate, NSTextFieldDelegate};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSApplicationDelegate, NSBackingStoreType,
    NSBorderType, NSButton, NSColor, NSEventModifierFlags, NSFont, NSImage,
    NSImageSymbolConfiguration, NSImageSymbolScale, NSLayoutConstraint,
    NSLayoutConstraintOrientation, NSLineBreakMode, NSMenu, NSMenuItem, NSModalResponse,
    NSOpenPanel, NSPasteboard, NSPasteboardTypeString, NSScrollView, NSStackView,
    NSStackViewDistribution, NSTextField, NSUserInterfaceLayoutOrientation, NSView,
    NSWindow, NSWindowDelegate, NSWindowStyleMask,
    NSWindowTabbingMode,
};
use objc2_foundation::{
    NSArray, NSEdgeInsets, NSNotification, NSObject, NSObjectProtocol,
    NSPoint, NSRect, NSSize, NSString, NSTimer, NSURL,
};
use worker::{WindowId, WorkerChannel, WorkerMsg};

mod app_state {
    //! Process-wide state. Single-threaded access from the main thread only;
    //! the worker thread talks back via mpsc, so we don't need a Mutex here.
    use crate::doc::Document;
    use crate::doc_view::DocView;
    use crate::worker::{WindowId, WorkerChannel};
    use objc2::rc::Retained;
    use objc2_app_kit::{NSButton, NSStackView, NSTextField, NSWindow};
    use objc2_foundation::NSTimer;
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicI32};

    /// Per-tab state. Stays in the `WINDOWS` map for the lifetime of
    /// the window and is dropped by the `windowWillClose:` hook.
    /// Cursor + scroll position saved when switching between original
    /// and pretty views so each remembers where the user was.
    #[derive(Default, Clone)]
    pub struct SavedViewport {
        pub click_offset: Option<u32>,
        pub scroll_origin: (f64, f64),
    }

    pub struct WindowState {
        pub window: Retained<NSWindow>,
        pub doc_view: Retained<DocView>,
        pub prettify_button: Retained<NSButton>,
        /// "Copy jq" / "Copy XPath" — title updates when a doc loads.
        pub copy_path_button: Retained<NSButton>,
        /// "Copy JSON" / "Copy XML" — title updates when a doc loads.
        pub copy_subtree_button: Retained<NSButton>,
        /// Kept alive so the view's weak reference to the breadcrumb
        /// label stays valid for the window's lifetime.
        #[allow(dead_code)]
        pub breadcrumb: Retained<NSTextField>,
        pub search_field: Retained<NSTextField>,
        pub search_count_label: Retained<NSTextField>,
        pub search_bar: Retained<NSStackView>,
        pub current_path: Option<String>,
        pub original_doc: Option<Arc<Document>>,
        pub pretty_doc: Option<Arc<Document>>,
        pub is_pretty: bool,
        pub pretty_pending: bool,
        pub saved_original: SavedViewport,
        pub saved_pretty: SavedViewport,
    }

    impl WindowState {
        /// Reset document state back to blank. Used by clear, paste, and
        /// load-file to avoid repeating the same five field assignments.
        pub fn reset_doc_state(&mut self) {
            self.original_doc = None;
            self.pretty_doc = None;
            self.is_pretty = false;
            self.pretty_pending = false;
            self.saved_original = SavedViewport::default();
            self.saved_pretty = SavedViewport::default();
            self.doc_view.set_last_click_offset(None);
            self.doc_view.clear_search();
            self.prettify_button
                .setTitle(&objc2_foundation::NSString::from_str("Prettify"));
        }
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

fn with_window<R>(id: WindowId, f: impl FnOnce(&app_state::WindowState) -> R) -> Option<R> {
    app_state::WINDOWS.with(|m| m.borrow().get(&id).map(f))
}

fn with_window_mut<R>(
    id: WindowId,
    f: impl FnOnce(&mut app_state::WindowState) -> R,
) -> Option<R> {
    app_state::WINDOWS.with(|m| m.borrow_mut().get_mut(&id).map(f))
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "RVAppDelegate"]
    struct AppDelegate;

    unsafe impl NSObjectProtocol for AppDelegate {}
    unsafe impl NSControlTextEditingDelegate for AppDelegate {}
    unsafe impl NSTextFieldDelegate for AppDelegate {}

    unsafe impl NSApplicationDelegate for AppDelegate {
        #[unsafe(method(applicationDidFinishLaunching:))]
        fn did_finish_launching(&self, _notification: &NSNotification) {
            let mtm = self.mtm();
            install_menu_bar(mtm);
            let n = app_state::WINDOWS.with(|m| m.borrow().len());
            // If openURLs/openFile already fired (it can on some launch
            // paths), don't create a duplicate blank window.
            if n == 0 {
                new_window(mtm, self);
            }
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
                    let target = find_or_create_tab_for_load(mtm, self);
                    load_file_into_window(target, &path.to_string());
                }
            }
        }

        #[unsafe(method(application:openFile:))]
        fn open_file(&self, _app: &NSApplication, filename: &NSString) -> bool {
            let mtm = self.mtm();
            let path = filename.to_string();
            let target = find_or_create_tab_for_load(mtm, self);
            load_file_into_window(target, &path);
            true
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

        #[unsafe(method(rvTogglePrettify:))]
        fn rv_toggle_prettify(&self, sender: &AnyObject) {
            if let Some(id) = window_id_from_sender(sender) {
                toggle_prettify(id);
            }
        }

        #[unsafe(method(rvPaste:))]
        fn rv_paste(&self, sender: &AnyObject) {
            if let Some(id) = window_id_from_sender(sender) {
                paste_from_clipboard(id);
            }
        }

        #[unsafe(method(rvClearDocument:))]
        fn rv_clear_document(&self, sender: &AnyObject) {
            if let Some(id) = window_id_from_sender(sender) {
                clear_view(id);
            }
        }

        #[unsafe(method(rvCopyPath:))]
        fn rv_copy_path(&self, sender: &AnyObject) {
            if let Some(id) = window_id_from_sender(sender) {
                copy_path_expression(id);
            }
        }

        #[unsafe(method(rvCopySubtree:))]
        fn rv_copy_subtree(&self, sender: &AnyObject) {
            if let Some(id) = window_id_from_sender(sender) {
                copy_subtree(id);
            }
        }

        #[unsafe(method(rvWorkerTick:))]
        fn rv_worker_tick(&self, _timer: &NSTimer) {
            drain_worker();
        }

        #[unsafe(method(rvShowSearch:))]
        fn rv_show_search(&self, _sender: &AnyObject) {
            let mtm = self.mtm();
            let app = NSApplication::sharedApplication(mtm);
            if let Some(win) = app.keyWindow() {
                show_search(window_id_of(&win));
            }
        }

        #[unsafe(method(rvSearchNext:))]
        fn rv_search_next(&self, _sender: &AnyObject) {
            let mtm = self.mtm();
            let app = NSApplication::sharedApplication(mtm);
            if let Some(win) = app.keyWindow() {
                search_navigate(window_id_of(&win), true);
            }
        }

        #[unsafe(method(rvSearchPrev:))]
        fn rv_search_prev(&self, _sender: &AnyObject) {
            let mtm = self.mtm();
            let app = NSApplication::sharedApplication(mtm);
            if let Some(win) = app.keyWindow() {
                search_navigate(window_id_of(&win), false);
            }
        }

        #[unsafe(method(rvDismissSearch:))]
        fn rv_dismiss_search(&self, _sender: &AnyObject) {
            let mtm = self.mtm();
            let app = NSApplication::sharedApplication(mtm);
            if let Some(win) = app.keyWindow() {
                dismiss_search(window_id_of(&win));
            }
        }

        /// Fired when the user presses Enter in the search field.
        /// Shift+Enter → previous, Enter → next.
        #[unsafe(method(rvSearchFieldAction:))]
        fn rv_search_field_action(&self, sender: &NSTextField) {
            if let Some(win) = sender.window() {
                let id = window_id_of(&win);
                // Enter commits the search regardless of query length.
                force_search(id);
                let event = NSApplication::sharedApplication(self.mtm()).currentEvent();
                let shift = event
                    .map(|e| e.modifierFlags().contains(NSEventModifierFlags::Shift))
                    .unwrap_or(false);
                search_navigate(id, !shift);
            }
        }

        /// Intercept Escape in the search field's field editor.
        /// Returning true means "I handled this command, don't beep."
        #[unsafe(method(control:textView:doCommandBySelector:))]
        fn control_text_view_do_command(
            &self,
            control: &objc2_app_kit::NSControl,
            _text_view: &objc2_app_kit::NSTextView,
            sel: objc2::runtime::Sel,
        ) -> objc2::runtime::Bool {
            if sel == objc2::sel!(cancelOperation:) {
                if let Some(win) = control.window() {
                    dismiss_search(window_id_of(&win));
                }
                return objc2::runtime::Bool::YES;
            }
            objc2::runtime::Bool::NO
        }

        #[unsafe(method(rvSearchChanged:))]
        fn rv_search_changed(&self, notif: &NSNotification) {
            // controlTextDidChange: — the notification's object is the
            // NSTextField. Walk up to the window to find the tab.
            let Some(obj) = notif.object() else { return };
            let view_ptr = Retained::as_ptr(&obj) as *const NSView;
            let window = unsafe { (*view_ptr).window() };
            let Some(window) = window else { return };
            let id = window_id_of(&window);
            run_search(id);
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

    // Edit menu: Find (Cmd+F), Find Next (Cmd+G), Find Previous (Shift+Cmd+G).
    let edit_menu_item = NSMenuItem::new(mtm);
    menubar.addItem(&edit_menu_item);
    let edit_menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str("Edit"));
    add_menu_item(
        mtm,
        &edit_menu,
        "Find…",
        objc2::sel!(rvShowSearch:),
        "f",
        NSEventModifierFlags::Command,
    );
    add_menu_item(
        mtm,
        &edit_menu,
        "Find Next",
        objc2::sel!(rvSearchNext:),
        "g",
        NSEventModifierFlags::Command,
    );
    add_menu_item(
        mtm,
        &edit_menu,
        "Find Previous",
        objc2::sel!(rvSearchPrev:),
        "g",
        NSEventModifierFlags::Command.union(NSEventModifierFlags::Shift),
    );
    edit_menu_item.setSubmenu(Some(&edit_menu));

    // View menu: Prettify, Paste from Clipboard, Clear, Copy Path.
    let view_menu_item = NSMenuItem::new(mtm);
    menubar.addItem(&view_menu_item);
    let view_menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str("View"));
    add_menu_item(
        mtm,
        &view_menu,
        "Prettify / Original",
        objc2::sel!(rvTogglePrettify:),
        "p",
        NSEventModifierFlags::Command,
    );
    add_menu_item(
        mtm,
        &view_menu,
        "Paste from Clipboard",
        objc2::sel!(rvPaste:),
        "v",
        NSEventModifierFlags::Command,
    );
    add_menu_item(
        mtm,
        &view_menu,
        "Clear Document",
        objc2::sel!(rvClearDocument:),
        "k",
        NSEventModifierFlags::Command,
    );
    add_menu_item(
        mtm,
        &view_menu,
        "Copy Path",
        objc2::sel!(rvCopyPath:),
        "c",
        NSEventModifierFlags::Command,
    );
    add_menu_item(
        mtm,
        &view_menu,
        "Copy Sub-tree",
        objc2::sel!(rvCopySubtree:),
        "c",
        NSEventModifierFlags::Command.union(NSEventModifierFlags::Shift),
    );
    view_menu_item.setSubmenu(Some(&view_menu));

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

    // Rust's Retained<NSWindow> inside WindowState is the canonical
    // owner. The default of YES here makes AppKit send a second
    // release on user-close, which races with our Drop and double-
    // frees the window. Disable it; our windowWillClose: hook takes
    // the map entry out and the Retained drop decrefs cleanly.
    unsafe { window.setReleasedWhenClosed(false) };

    // Tabbing: every window shares an identifier so AppKit auto-tabs
    // them. Preferred mode overrides the system-wide setting.
    window.setTabbingIdentifier(&NSString::from_str("RapidView"));
    window.setTabbingMode(NSWindowTabbingMode::Preferred);

    // Window delegate tracks windowWillClose: so we can drop state.
    let delegate_proto: &ProtocolObject<dyn NSWindowDelegate> =
        ProtocolObject::from_ref(delegate);
    window.setDelegate(Some(delegate_proto));

    let content_view = window.contentView().expect("window has content view");
    let content_bounds = content_view.bounds();

    let scroll = NSScrollView::initWithFrame(NSScrollView::alloc(mtm), content_bounds);
    scroll.setHasVerticalScroller(true);
    scroll.setHasHorizontalScroller(true);
    scroll.setAutohidesScrollers(true);
    scroll.setBorderType(NSBorderType::NoBorder);

    let doc_view = DocView::new(mtm, doc_view::initial_frame());
    scroll.setDocumentView(Some(&doc_view));

    let delegate_obj = delegate as &AnyObject;
    let hb = build_header_bar(mtm, delegate_obj);
    doc_view.set_breadcrumb(hb.breadcrumb.clone());

    // Set the delegate on the search field so control:textView:doCommandBySelector:
    // fires for Escape handling, and register for live-as-you-type search.
    {
        let tf_delegate: &ProtocolObject<dyn objc2_app_kit::NSTextFieldDelegate> =
            ProtocolObject::from_ref(delegate);
        unsafe { hb.search_field.setDelegate(Some(tf_delegate)) };
    }
    let center = objc2_foundation::NSNotificationCenter::defaultCenter();
    unsafe {
        center.addObserver_selector_name_object(
            delegate_obj,
            objc2::sel!(rvSearchChanged:),
            Some(objc2_app_kit::NSControlTextDidChangeNotification),
            Some(&hb.search_field),
        );
    }

    let stack_views: Retained<NSArray<NSView>> = NSArray::from_slice(&[
        &*hb.stack as &NSView,
        &*hb.search_bar as &NSView,
        &*scroll as &NSView,
    ]);
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

    window.makeKeyAndOrderFront(None);

    let id = window_id_of(&window);
    let state = app_state::WindowState {
        window,
        doc_view,
        prettify_button: hb.prettify_button,
        copy_path_button: hb.copy_path_button,
        copy_subtree_button: hb.copy_subtree_button,
        breadcrumb: hb.breadcrumb,
        search_field: hb.search_field,
        search_count_label: hb.search_count_label,
        search_bar: hb.search_bar,
        current_path: None,
        original_doc: None,
        pretty_doc: None,
        is_pretty: false,
        pretty_pending: false,
        saved_original: app_state::SavedViewport::default(),
        saved_pretty: app_state::SavedViewport::default(),
    };
    app_state::WINDOWS.with(|m| {
        m.borrow_mut().insert(id, state);
    });
    id
}

struct HeaderBar {
    stack: Retained<NSStackView>,
    breadcrumb: Retained<NSTextField>,
    prettify_button: Retained<NSButton>,
    copy_path_button: Retained<NSButton>,
    copy_subtree_button: Retained<NSButton>,
    search_field: Retained<NSTextField>,
    search_count_label: Retained<NSTextField>,
    search_bar: Retained<NSStackView>,
}

fn build_header_bar(
    mtm: MainThreadMarker,
    target: &AnyObject,
) -> HeaderBar {
    let cmd = NSEventModifierFlags::Command;
    let clipboard_button =
        make_icon_button(mtm, "doc.on.clipboard", "Paste from clipboard", target, objc2::sel!(rvPaste:));
    set_key(&clipboard_button, "v", cmd);
    clipboard_button.setToolTip(Some(&NSString::from_str("Paste from clipboard  ⌘V")));
    let clear_button =
        make_icon_button(mtm, "xmark.circle", "Clear document", target, objc2::sel!(rvClearDocument:));
    set_key(&clear_button, "k", cmd);
    clear_button.setToolTip(Some(&NSString::from_str("Clear document  ⌘K")));
    let prettify_button = make_button_underlined(mtm, "Prettify", 'P', target, objc2::sel!(rvTogglePrettify:));
    set_key(&prettify_button, "p", cmd);
    prettify_button.setToolTip(Some(&NSString::from_str("Toggle pretty-print (⌘P)")));
    // Title is set to the JSON labels by default; refresh_format_chrome
    // updates both buttons when a document loads.
    let copy_subtree_button = make_button(mtm, "Copy JSON", target, objc2::sel!(rvCopySubtree:));
    set_key(&copy_subtree_button, "c", cmd.union(NSEventModifierFlags::Shift));
    copy_subtree_button.setToolTip(Some(&NSString::from_str(
        "Copy sub-tree at cursor — or entire document if nothing selected (⇧⌘C)",
    )));
    let copy_path_button = make_button_underlined(mtm, "Copy jq", 'C', target, objc2::sel!(rvCopyPath:));
    set_key(&copy_path_button, "c", cmd);
    copy_path_button.setToolTip(Some(&NSString::from_str("Copy path expression (⌘C)")));

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
        &*prettify_button as &NSView,
        &*label as &NSView,
        &*copy_subtree_button as &NSView,
        &*copy_path_button as &NSView,
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

    // Search bar — hidden until Cmd+F. Contains a text field and a
    // match-count label, arranged horizontally.
    let search_field = {
        let tf = NSTextField::textFieldWithString(&NSString::from_str(""), mtm);
        tf.setFont(Some(&NSFont::userFixedPitchFontOfSize(12.0).unwrap()));
        tf.setPlaceholderString(Some(&NSString::from_str("Search… (Enter=next, Esc=close)")));
        // Width constraint so it doesn't collapse.
        let wc = tf.widthAnchor().constraintGreaterThanOrEqualToConstant(240.0);
        wc.setActive(true);
        // Enter in the search field navigates to the next match.
        unsafe {
            tf.setAction(Some(objc2::sel!(rvSearchFieldAction:)));
            tf.setTarget(Some(target));
        }
        tf
    };
    let search_count = {
        let tf = NSTextField::labelWithString(&NSString::from_str(""), mtm);
        tf.setFont(Some(&NSFont::userFixedPitchFontOfSize(11.0).unwrap()));
        tf.setTextColor(Some(&NSColor::secondaryLabelColor()));
        tf
    };
    let search_views: Retained<NSArray<NSView>> = NSArray::from_slice(&[
        &*search_field as &NSView,
        &*search_count as &NSView,
    ]);
    let search_bar = NSStackView::stackViewWithViews(&search_views, mtm);
    search_bar.setOrientation(NSUserInterfaceLayoutOrientation::Horizontal);
    search_bar.setSpacing(6.0);
    search_bar.setHidden(true);

    HeaderBar {
        stack: header,
        breadcrumb: label,
        prettify_button,
        copy_path_button,
        copy_subtree_button,
        search_field,
        search_count_label: search_count,
        search_bar,
    }
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

/// Create a button with one character underlined to hint at the keyboard
/// shortcut. `underline_char` is matched case-insensitively in `title`.
fn make_button_underlined(
    mtm: MainThreadMarker,
    title: &str,
    underline_char: char,
    target: &AnyObject,
    action: objc2::runtime::Sel,
) -> Retained<NSButton> {
    let btn = make_button(mtm, title, target, action);
    set_underlined_title(&btn, title, underline_char);
    btn
}

fn make_icon_button(
    mtm: MainThreadMarker,
    symbol_name: &str,
    accessibility_label: &str,
    target: &AnyObject,
    action: objc2::runtime::Sel,
) -> Retained<NSButton> {
    let base_image = NSImage::imageWithSystemSymbolName_accessibilityDescription(
        &NSString::from_str(symbol_name),
        Some(&NSString::from_str(accessibility_label)),
    )
    .expect("SF Symbol should be available");
    let small_config =
        NSImageSymbolConfiguration::configurationWithScale(NSImageSymbolScale::Small);
    let image = base_image
        .imageWithSymbolConfiguration(&small_config)
        .unwrap_or(base_image);
    let btn = unsafe {
        NSButton::buttonWithImage_target_action(&image, Some(target), Some(action), mtm)
    };
    btn.setBezelStyle(objc2_app_kit::NSBezelStyle::Toolbar);
    btn
}

/// Set a button's title with one character underlined.
fn set_underlined_title(btn: &NSButton, title: &str, underline_char: char) {
    use objc2::AnyThread;
    let ns_title = NSString::from_str(title);
    let attr = objc2_foundation::NSMutableAttributedString::initWithString(
        objc2_foundation::NSMutableAttributedString::alloc(),
        &ns_title,
    );
    let uc_lower = underline_char.to_ascii_lowercase();
    if let Some(pos) = title.char_indices().position(|(_, c)| c.to_ascii_lowercase() == uc_lower) {
        let byte_pos = title.char_indices().nth(pos).unwrap().0;
        let char_len = title[byte_pos..].chars().next().unwrap().len_utf16();
        let utf16_pos: usize = title[..byte_pos].encode_utf16().count();
        let range = objc2_foundation::NSRange {
            location: utf16_pos,
            length: char_len,
        };
        unsafe {
            attr.addAttribute_value_range(
                objc2_app_kit::NSUnderlineStyleAttributeName,
                &*objc2_foundation::NSNumber::new_i64(1),
                range,
            );
        }
    }
    btn.setAttributedTitle(&attr);
}

fn set_key(btn: &NSButton, key: &str, modifiers: NSEventModifierFlags) {
    btn.setKeyEquivalent(&NSString::from_str(key));
    btn.setKeyEquivalentModifierMask(modifiers);
}

// -- window-state helpers --------------------------------------------

/// Get the window ID for an action sender. Works for NSButton (toolbar),
/// NSMenuItem (menu bar), or any NSView subclass. Falls back to the
/// application's key window.
///
/// `msg_send![sender, window]` on an NSMenuItem raises
/// `doesNotRecognizeSelector:` — an Objective-C exception that becomes a
/// nounwind panic in Rust. So we gate the call on `respondsToSelector:`.
fn window_id_from_sender(sender: &AnyObject) -> Option<WindowId> {
    let sel_window = objc2::sel!(window);
    let responds: objc2::runtime::Bool =
        unsafe { msg_send![sender, respondsToSelector: sel_window] };
    if responds.as_bool() {
        let win: Option<Retained<NSWindow>> = unsafe { msg_send![sender, window] };
        if let Some(w) = win {
            return Some(window_id_of(&w));
        }
    }
    // Fallback: key window (handles NSMenuItem which has no window).
    let mtm = MainThreadMarker::new()?;
    let app = NSApplication::sharedApplication(mtm);
    app.keyWindow().map(|w| window_id_of(&w))
}

/// Reuse any blank window (no document loaded); otherwise create a new
/// tab. Checks all windows, not just the key window, because
/// `application:openURLs:` can fire before the key window is set.
fn find_or_create_tab_for_load(mtm: MainThreadMarker, delegate: &AppDelegate) -> WindowId {
    let blank = app_state::WINDOWS.with(|m| {
        m.borrow()
            .iter()
            .find(|(_, s)| s.original_doc.is_none() && s.current_path.is_none())
            .map(|(&id, _)| id)
    });
    blank.unwrap_or_else(|| new_window(mtm, delegate))
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
    with_window_mut(id, |state| {
        state.reset_doc_state();
        state.current_path = None;
        state.window.setTitle(&NSString::from_str("Rapid View"));
        state.doc_view.clear_document();
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
    with_window_mut(id, |state| {
        state.reset_doc_state();
        state.current_path = Some(label.clone());
        state
            .window
            .setTitle(&NSString::from_str("Rapid View — parsing clipboard…"));
    });

    let tx = ensure_worker_channel();
    app_state::WORK_PENDING.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    worker::spawn_parse_bytes(id, text.into_bytes(), label, tx);
    ensure_poll_timer();
}

fn load_file_into_window(id: WindowId, path: &str) {
    let name = basename(path);
    with_window_mut(id, |state| {
        state.reset_doc_state();
        state.current_path = Some(path.to_string());
        state
            .window
            .setTitle(&NSString::from_str(&format!("Rapid View — loading {}…", name)));
    });

    let tx = ensure_worker_channel();
    app_state::WORK_PENDING.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    worker::spawn_load(id, path.to_string(), tx);
    ensure_poll_timer();
}

fn copy_path_expression(id: WindowId) {
    let expr = with_window(id, |s| s.doc_view.current_path_expression()).unwrap_or_else(|| String::from("."));
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

/// Copy the JSON sub-tree at the cursor — or the entire document if
/// nothing has been clicked yet — to the clipboard.
fn copy_subtree(id: WindowId) {
    let Some(text) = with_window(id, |s| s.doc_view.current_subtree()).flatten() else {
        eprintln!("rapid-view: nothing to copy (no document loaded)");
        return;
    };
    let pb = NSPasteboard::generalPasteboard();
    pb.clearContents();
    let ns = NSString::from_str(&text);
    unsafe {
        let type_str: &NSString = NSPasteboardTypeString;
        let types = NSArray::from_slice(&[type_str]);
        pb.declareTypes_owner(&types, None);
        pb.setString_forType(&ns, type_str);
    }
    eprintln!("copied JSON sub-tree ({} bytes)", text.len());
}

/// Capture the current scroll position + cursor offset from the view.
fn save_viewport(state: &app_state::WindowState) -> app_state::SavedViewport {
    let click = state.doc_view.last_click_offset();
    let scroll = state
        .doc_view
        .enclosingScrollView()
        .map(|sv| {
            let bounds = sv.contentView().bounds();
            (bounds.origin.x, bounds.origin.y)
        })
        .unwrap_or((0.0, 0.0));
    app_state::SavedViewport {
        click_offset: click,
        scroll_origin: scroll,
    }
}

/// Restore a previously saved scroll position + cursor offset.
fn restore_viewport(state: &app_state::WindowState, saved: &app_state::SavedViewport) {
    state.doc_view.set_last_click_offset(saved.click_offset);
    if let Some(sv) = state.doc_view.enclosingScrollView() {
        let clip = sv.contentView();
        // Clamp scroll position to the document bounds so we don't
        // end up staring at blank space (e.g. switching from a wide
        // minified view to a narrow pretty-printed one).
        let doc_frame = state.doc_view.frame();
        let clip_size = clip.bounds().size;
        let max_x = (doc_frame.size.width - clip_size.width).max(0.0);
        let max_y = (doc_frame.size.height - clip_size.height).max(0.0);
        let x = saved.scroll_origin.0.clamp(0.0, max_x);
        let y = saved.scroll_origin.1.clamp(0.0, max_y);
        clip.setBoundsOrigin(NSPoint::new(x, y));
    }
}

// -- search -----------------------------------------------------------

fn show_search(id: WindowId) {
    with_window(id, |state| {
        state.search_bar.setHidden(false);
        state.window.makeFirstResponder(Some(&state.search_field));
    });
}

fn dismiss_search(id: WindowId) {
    with_window(id, |state| {
        state.search_bar.setHidden(true);
        state.search_field.setStringValue(&NSString::from_str(""));
        state.search_count_label.setStringValue(&NSString::from_str(""));
        state.doc_view.clear_search();
        state.window.makeFirstResponder(Some(&state.doc_view));
    });
}

fn run_search(id: WindowId) {
    with_window(id, |state| {
        let query = state.search_field.stringValue().to_string();
        // Don't run incremental search until 4+ chars to avoid
        // scanning large files on every keystroke for short queries.
        if query.len() < 4 {
            state.doc_view.clear_search();
            let label = if query.is_empty() {
                String::new()
            } else {
                "Type 4+ chars…".to_string()
            };
            state.search_count_label.setStringValue(&NSString::from_str(&label));
            return;
        }
        let count = state.doc_view.search(&query);
        let label = if count == 0 {
            "No matches".to_string()
        } else {
            state.doc_view.scroll_to_current_match();
            if count >= 100_000 {
                "100,000+ matches".to_string()
            } else {
                format!("1 of {}", count)
            }
        };
        state.search_count_label.setStringValue(&NSString::from_str(&label));
    });
}

/// Re-run the active search against the current document (e.g. after
/// switching between original and pretty). No-op if the search bar is hidden.
fn rerun_search(id: WindowId) {
    let active = with_window(id, |s| !s.search_bar.isHidden()).unwrap_or(false);
    if active {
        run_search(id);
    }
}

/// Run search unconditionally (ignoring the 4-char threshold). Used
/// when the user presses Enter to commit a short query.
fn force_search(id: WindowId) {
    with_window(id, |state| {
        let query = state.search_field.stringValue().to_string();
        if query.is_empty() {
            return;
        }
        let count = state.doc_view.search(&query);
        let label = if count == 0 {
            "No matches".to_string()
        } else if count >= 100_000 {
            "100,000+ matches".to_string()
        } else {
            format!("1 of {}", count)
        };
        state.search_count_label.setStringValue(&NSString::from_str(&label));
    });
}

fn search_navigate(id: WindowId, forward: bool) {
    with_window(id, |state| {
        let result = if forward {
            state.doc_view.search_next()
        } else {
            state.doc_view.search_prev()
        };
        if let Some((idx, total)) = result {
            let label = format!("{} of {}", idx + 1, total);
            state.search_count_label.setStringValue(&NSString::from_str(&label));
        }
    });
}

// -- prettify ---------------------------------------------------------

fn toggle_prettify(id: WindowId) {
    // Decide what to do under a short borrow; anything that calls into
    // the view or spawns workers happens after the borrow drops.
    enum Action {
        SwapToOriginal(std::sync::Arc<doc::Document>),
        SwapToCachedPretty(std::sync::Arc<doc::Document>),
        SpawnPretty(Format, doc::ByteSource),
        Nothing,
    }

    let action = with_window(id, |state| {
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
            .map(|d| Action::SpawnPretty(d.format, d.bytes.clone()))
            .unwrap_or(Action::Nothing)
    })
    .unwrap_or(Action::Nothing);

    match action {
        Action::Nothing => {}
        Action::SwapToOriginal(doc) => {
            with_window_mut(id, |state| {
                state.saved_pretty = save_viewport(state);
                state.is_pretty = false;
                state.doc_view.set_document(doc);
                let saved = state.saved_original.clone();
                restore_viewport(state, &saved);
                set_underlined_title(&state.prettify_button, "Prettify", 'P');
                refresh_title(state);
            });
            rerun_search(id);
        }
        Action::SwapToCachedPretty(doc) => {
            with_window_mut(id, |state| {
                state.saved_original = save_viewport(state);
                state.is_pretty = true;
                state.doc_view.set_document(doc);
                let saved = state.saved_pretty.clone();
                restore_viewport(state, &saved);
                set_underlined_title(&state.prettify_button, "Original", 'O');
                refresh_title(state);
            });
            rerun_search(id);
        }
        Action::SpawnPretty(fmt, source) => {
            with_window_mut(id, |state| {
                state.saved_original = save_viewport(state);
                state.is_pretty = true;
                state.pretty_pending = true;
                set_underlined_title(&state.prettify_button, "Original", 'O');
                state
                    .window
                    .setTitle(&NSString::from_str("Rapid View — prettifying…"));
            });
            let tx = ensure_worker_channel();
            app_state::WORK_PENDING.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            worker::spawn_prettify(id, fmt, source, tx);
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
            }
            WorkerMsg::PrettyReady { window_id, doc } => {
                on_pretty_ready(window_id, doc);
            }
            WorkerMsg::Error { window_id, message } => {
                eprintln!("rapid-view: {}", message);
                with_window_mut(window_id, |state| {
                    state.window.setTitle(&NSString::from_str("Rapid View"));
                });
            }
        }
        app_state::WORK_PENDING.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
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
    let fmt = doc.format;
    eprintln!("loaded {} ({} bytes, {} lines, {:?})", path, size, lines, fmt);
    with_window_mut(id, |state| {
        state.reset_doc_state();
        state.original_doc = Some(doc.clone());
        state.current_path = Some(path.to_string());
        state.doc_view.set_document(doc);
        refresh_format_chrome(state, fmt);
        refresh_title(state);
    });
}

/// Update toolbar button titles to reflect the loaded document's
/// format ("Copy jq" / "Copy XPath", "Copy JSON" / "Copy XML").
fn refresh_format_chrome(state: &app_state::WindowState, fmt: Format) {
    let path_label = format::path_label(fmt);
    let content_label = format::content_label(fmt);
    set_underlined_title(
        &state.copy_path_button,
        &format!("Copy {}", path_label),
        'C',
    );
    state
        .copy_subtree_button
        .setTitle(&NSString::from_str(&format!("Copy {}", content_label)));
}

fn on_pretty_ready(id: WindowId, doc: std::sync::Arc<doc::Document>) {
    with_window_mut(id, |state| {
        state.pretty_doc = Some(doc.clone());
        state.pretty_pending = false;
        // If the user is still asking for pretty (set optimistically
        // at click time), install it; otherwise keep the cache for
        // the next toggle.
        if state.is_pretty {
            state.doc_view.set_document(doc);
            // First prettify — scroll to top since the layout changed
            // drastically (single minified line → many short lines).
            let default_vp = app_state::SavedViewport::default();
            restore_viewport(state, &default_vp);
            set_underlined_title(&state.prettify_button, "Original", 'O');
        }
        refresh_title(state);
    });
    rerun_search(id);
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
    // Apps launched from Finder have stderr connected to /dev/null, so Rust
    // panic messages are lost. Write panics to a fixed log file so we can
    // diagnose user-side crashes that don't reproduce locally.
    std::panic::set_hook(Box::new(|info| {
        use std::io::Write;
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open("/tmp/rapid-view-panic.log")
            .and_then(|mut f| {
                writeln!(
                    f,
                    "[{}] panic: {}",
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0),
                    info
                )
            });
    }));

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
