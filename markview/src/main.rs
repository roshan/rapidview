//! Markview — native macOS markdown viewer with a rendered/source
//! toggle. Both views are non-editable `NSTextView`s, so selection,
//! copy, and the system Find bar work the way macOS users expect.
//!
//! Per-tab state is keyed by the raw `NSWindow` pointer cast to
//! `usize`, the same approach Rapid View uses. Multi-window with
//! AppKit auto-tabbing comes from sharing a single `tabbingIdentifier`.

#![deny(unsafe_op_in_unsafe_fn)]

mod doc;
mod rendered;
mod source;
mod worker;

use doc::Document;
use markdown_core::ProgressSink;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSApplicationDelegate,
    NSAutoresizingMaskOptions, NSBackingStoreType, NSBorderType, NSButton, NSColor,
    NSEventModifierFlags, NSImage, NSImageSymbolConfiguration, NSImageSymbolScale,
    NSLayoutConstraint, NSLayoutConstraintOrientation, NSLineBreakMode, NSMenu, NSMenuItem,
    NSModalResponse, NSOpenPanel, NSPasteboard, NSPasteboardTypeString, NSProgressIndicator,
    NSProgressIndicatorStyle, NSScrollView, NSStackView, NSStackViewDistribution,
    NSTextField, NSTextView, NSUserInterfaceLayoutOrientation, NSView, NSWindow,
    NSWindowDelegate, NSWindowStyleMask, NSWindowTabbingMode,
};
use objc2_foundation::{
    NSArray, NSAttributedString, NSEdgeInsets, NSNotification, NSObject, NSObjectProtocol,
    NSPoint, NSRect, NSSize, NSString, NSTimer, NSURL,
};
use std::sync::Arc;
use worker::{WindowId, WorkerChannel, WorkerMsg};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Rendered,
    Source,
}

mod app_state {
    use crate::Mode;
    use crate::doc::Document;
    use crate::worker::{WindowId, WorkerChannel};
    use markdown_core::ProgressSink;
    use objc2::rc::Retained;
    use objc2_app_kit::{NSButton, NSProgressIndicator, NSTextView, NSWindow};
    use objc2_foundation::{NSAttributedString, NSTimer};
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicI32};

    pub struct WindowState {
        pub window: Retained<NSWindow>,
        pub text_view: Retained<NSTextView>,
        pub toggle_button: Retained<NSButton>,
        pub progress_bar: Retained<NSProgressIndicator>,
        pub progress: Option<Arc<ProgressSink>>,
        pub current_path: Option<String>,
        pub doc: Option<Arc<Document>>,
        pub rendered: Option<Retained<NSAttributedString>>,
        pub source: Option<Retained<NSAttributedString>>,
        pub mode: Mode,
    }

    thread_local! {
        pub static WINDOWS: RefCell<HashMap<WindowId, WindowState>> =
            RefCell::new(HashMap::new());
        pub static WORKER: RefCell<Option<WorkerChannel>> = const { RefCell::new(None) };
        pub static POLL_TIMER: RefCell<Option<Retained<NSTimer>>> = const { RefCell::new(None) };
    }

    pub static WORK_PENDING: AtomicI32 = AtomicI32::new(0);
    pub static SELFTEST_LAUNCH: AtomicBool = AtomicBool::new(false);
}

fn window_id_of(w: &NSWindow) -> WindowId {
    (w as *const NSWindow) as WindowId
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
    #[name = "MVAppDelegate"]
    struct AppDelegate;

    unsafe impl NSObjectProtocol for AppDelegate {}

    unsafe impl NSApplicationDelegate for AppDelegate {
        #[unsafe(method(applicationDidFinishLaunching:))]
        fn did_finish_launching(&self, _notification: &NSNotification) {
            let mtm = self.mtm();
            install_menu_bar(mtm);
            let n = app_state::WINDOWS.with(|m| m.borrow().len());
            if n == 0 {
                new_window(mtm, self);
            }
            for arg in std::env::args().skip(1) {
                if arg.starts_with('-') {
                    continue;
                }
                let target = find_or_create_tab(mtm, self);
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
                    let target = find_or_create_tab(mtm, self);
                    load_file_into_window(target, &path.to_string());
                }
            }
        }

        #[unsafe(method(application:openFile:))]
        fn open_file(&self, _app: &NSApplication, filename: &NSString) -> bool {
            let mtm = self.mtm();
            let target = find_or_create_tab(mtm, self);
            load_file_into_window(target, &filename.to_string());
            true
        }
    }

    unsafe impl NSWindowDelegate for AppDelegate {
        #[unsafe(method(windowWillClose:))]
        fn window_will_close(&self, notif: &NSNotification) {
            let Some(obj) = notif.object() else { return };
            let id = Retained::as_ptr(&obj) as WindowId;
            app_state::WINDOWS.with(|m| {
                m.borrow_mut().remove(&id);
            });
        }
    }

    impl AppDelegate {
        #[unsafe(method(mvNewWindow:))]
        fn mv_new_window(&self, _sender: &AnyObject) {
            let mtm = self.mtm();
            new_window(mtm, self);
        }

        #[unsafe(method(mvOpenDocument:))]
        fn mv_open_document(&self, _sender: &AnyObject) {
            let mtm = self.mtm();
            show_open_panel(mtm, self);
        }

        #[unsafe(method(mvToggleMode:))]
        fn mv_toggle_mode(&self, sender: &AnyObject) {
            if let Some(id) = window_id_from_sender(sender) {
                toggle_mode(id);
            }
        }

        #[unsafe(method(mvPaste:))]
        fn mv_paste(&self, sender: &AnyObject) {
            if let Some(id) = window_id_from_sender(sender) {
                paste_from_clipboard(id);
            }
        }

        #[unsafe(method(mvClearDocument:))]
        fn mv_clear_document(&self, sender: &AnyObject) {
            if let Some(id) = window_id_from_sender(sender) {
                clear_view(id);
            }
        }

        #[unsafe(method(mvWorkerTick:))]
        fn mv_worker_tick(&self, _timer: &NSTimer) {
            drain_worker();
        }
    }
);

impl AppDelegate {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(());
        unsafe { msg_send![super(this), init] }
    }
}

// -- menu bar --------------------------------------------------------

fn install_menu_bar(mtm: MainThreadMarker) {
    let menubar = NSMenu::new(mtm);

    let app_menu_item = NSMenuItem::new(mtm);
    menubar.addItem(&app_menu_item);
    let app_menu = NSMenu::new(mtm);
    add_menu_item(
        mtm,
        &app_menu,
        "Quit Markview",
        objc2::sel!(terminate:),
        "q",
        NSEventModifierFlags::Command,
    );
    app_menu_item.setSubmenu(Some(&app_menu));

    let file_menu_item = NSMenuItem::new(mtm);
    menubar.addItem(&file_menu_item);
    let file_menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str("File"));
    add_menu_item(
        mtm,
        &file_menu,
        "New Window",
        objc2::sel!(mvNewWindow:),
        "n",
        NSEventModifierFlags::Command,
    );
    add_menu_item(
        mtm,
        &file_menu,
        "Open…",
        objc2::sel!(mvOpenDocument:),
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

    let edit_menu_item = NSMenuItem::new(mtm);
    menubar.addItem(&edit_menu_item);
    let edit_menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str("Edit"));
    // NSTextView provides Copy / Find / Select All for free via the
    // first responder chain when these menu items have nil action.
    add_unbound_menu_item(mtm, &edit_menu, "Copy", objc2::sel!(copy:), "c", NSEventModifierFlags::Command);
    add_unbound_menu_item(mtm, &edit_menu, "Select All", objc2::sel!(selectAll:), "a", NSEventModifierFlags::Command);
    add_unbound_menu_item(
        mtm,
        &edit_menu,
        "Find…",
        objc2::sel!(performFindPanelAction:),
        "f",
        NSEventModifierFlags::Command,
    );
    edit_menu_item.setSubmenu(Some(&edit_menu));

    let view_menu_item = NSMenuItem::new(mtm);
    menubar.addItem(&view_menu_item);
    let view_menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str("View"));
    add_menu_item(
        mtm,
        &view_menu,
        "Toggle Rendered / Source",
        objc2::sel!(mvToggleMode:),
        "r",
        NSEventModifierFlags::Command,
    );
    add_menu_item(
        mtm,
        &view_menu,
        "Paste from Clipboard",
        objc2::sel!(mvPaste:),
        "v",
        NSEventModifierFlags::Command.union(NSEventModifierFlags::Shift),
    );
    add_menu_item(
        mtm,
        &view_menu,
        "Clear Document",
        objc2::sel!(mvClearDocument:),
        "k",
        NSEventModifierFlags::Command,
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

/// Menu item whose action is left to the first responder chain (so
/// NSTextView picks it up). Same as add_menu_item but without a
/// target.
fn add_unbound_menu_item(
    mtm: MainThreadMarker,
    menu: &NSMenu,
    title: &str,
    action: objc2::runtime::Sel,
    key: &str,
    modifiers: NSEventModifierFlags,
) {
    add_menu_item(mtm, menu, title, action, key, modifiers);
}

// -- window builder --------------------------------------------------

fn new_window(mtm: MainThreadMarker, delegate: &AppDelegate) -> WindowId {
    let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(900.0, 720.0));
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
    window.setTitle(&NSString::from_str("Markview"));
    window.center();
    unsafe { window.setReleasedWhenClosed(false) };
    window.setTabbingIdentifier(&NSString::from_str("Markview"));
    window.setTabbingMode(NSWindowTabbingMode::Preferred);

    let delegate_proto: &ProtocolObject<dyn NSWindowDelegate> =
        ProtocolObject::from_ref(delegate);
    window.setDelegate(Some(delegate_proto));

    let content_view = window.contentView().expect("window has content view");
    let content_bounds = content_view.bounds();

    let scroll = NSScrollView::initWithFrame(NSScrollView::alloc(mtm), content_bounds);
    scroll.setHasVerticalScroller(true);
    scroll.setHasHorizontalScroller(false);
    scroll.setAutohidesScrollers(true);
    scroll.setBorderType(NSBorderType::NoBorder);

    let text_view = build_text_view(mtm, &scroll);
    scroll.setDocumentView(Some(&text_view));

    let delegate_obj = delegate as &AnyObject;
    let hb = build_header_bar(mtm, delegate_obj);

    let stack_views: Retained<NSArray<NSView>> = NSArray::from_slice(&[
        &*hb.stack as &NSView,
        &*scroll as &NSView,
    ]);
    let stack = NSStackView::stackViewWithViews(&stack_views, mtm);
    stack.setOrientation(NSUserInterfaceLayoutOrientation::Vertical);
    stack.setSpacing(0.0);
    stack.setDistribution(NSStackViewDistribution::Fill);
    stack.setTranslatesAutoresizingMaskIntoConstraints(false);

    content_view.addSubview(&stack);
    let constraints = NSArray::from_retained_slice(&[
        stack.leadingAnchor().constraintEqualToAnchor(&content_view.leadingAnchor()),
        stack.trailingAnchor().constraintEqualToAnchor(&content_view.trailingAnchor()),
        stack.topAnchor().constraintEqualToAnchor(&content_view.topAnchor()),
        stack.bottomAnchor().constraintEqualToAnchor(&content_view.bottomAnchor()),
    ]);
    NSLayoutConstraint::activateConstraints(&constraints);

    window.makeKeyAndOrderFront(None);

    let id = window_id_of(&window);
    let state = app_state::WindowState {
        window,
        text_view,
        toggle_button: hb.toggle_button,
        progress_bar: hb.progress_bar,
        progress: None,
        current_path: None,
        doc: None,
        rendered: None,
        source: None,
        mode: Mode::Rendered,
    };
    app_state::WINDOWS.with(|m| {
        m.borrow_mut().insert(id, state);
    });
    id
}

fn build_text_view(mtm: MainThreadMarker, scroll: &NSScrollView) -> Retained<NSTextView> {
    let content_size = scroll.contentSize();
    let frame = NSRect::new(NSPoint::new(0.0, 0.0), content_size);
    let tv = NSTextView::initWithFrame(NSTextView::alloc(mtm), frame);
    tv.setMinSize(NSSize::new(0.0, content_size.height));
    tv.setMaxSize(NSSize::new(f64::MAX, f64::MAX));
    tv.setVerticallyResizable(true);
    tv.setHorizontallyResizable(false);
    tv.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
    tv.setEditable(false);
    tv.setSelectable(true);
    tv.setRichText(true);
    tv.setAllowsUndo(false);
    tv.setUsesFindBar(true);
    tv.setIncrementalSearchingEnabled(true);
    tv.setAutomaticLinkDetectionEnabled(false);
    tv.setAutomaticDataDetectionEnabled(false);
    tv.setAutomaticQuoteSubstitutionEnabled(false);
    tv.setAutomaticDashSubstitutionEnabled(false);
    tv.setAutomaticTextReplacementEnabled(false);
    tv.setAutomaticSpellingCorrectionEnabled(false);
    tv.setBackgroundColor(&NSColor::textBackgroundColor());
    tv.setTextContainerInset(NSSize::new(16.0, 16.0));
    if let Some(container) = unsafe { tv.textContainer() } {
        container.setContainerSize(NSSize::new(content_size.width, f64::MAX));
        container.setWidthTracksTextView(true);
    }
    tv
}

struct HeaderBar {
    stack: Retained<NSStackView>,
    toggle_button: Retained<NSButton>,
    progress_bar: Retained<NSProgressIndicator>,
}

fn build_header_bar(mtm: MainThreadMarker, target: &AnyObject) -> HeaderBar {
    let cmd = NSEventModifierFlags::Command;

    let paste_button = make_icon_button(
        mtm,
        "doc.on.clipboard",
        "Paste from clipboard",
        target,
        objc2::sel!(mvPaste:),
    );
    set_key(&paste_button, "v", cmd.union(NSEventModifierFlags::Shift));
    paste_button.setToolTip(Some(&NSString::from_str("Paste from clipboard  ⇧⌘V")));

    let clear_button = make_icon_button(
        mtm,
        "xmark.circle",
        "Clear document",
        target,
        objc2::sel!(mvClearDocument:),
    );
    set_key(&clear_button, "k", cmd);
    clear_button.setToolTip(Some(&NSString::from_str("Clear document  ⌘K")));

    let toggle_button = make_button(mtm, "Source", target, objc2::sel!(mvToggleMode:));
    set_key(&toggle_button, "r", cmd);
    toggle_button.setToolTip(Some(&NSString::from_str(
        "Switch between rendered and source view (⌘R)",
    )));

    let progress_bar = NSProgressIndicator::new(mtm);
    progress_bar.setStyle(NSProgressIndicatorStyle::Bar);
    progress_bar.setIndeterminate(false);
    progress_bar.setMinValue(0.0);
    progress_bar.setMaxValue(1.0);
    progress_bar.setDoubleValue(0.0);
    progress_bar.setHidden(true);

    // A spacer label keeps the toggle pinned to the right while the
    // progress bar takes the flexible middle when visible.
    let spacer = NSTextField::labelWithString(&NSString::from_str(""), mtm);
    spacer.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
    spacer.setContentHuggingPriority_forOrientation(
        10.0,
        NSLayoutConstraintOrientation::Horizontal,
    );
    spacer.setContentCompressionResistancePriority_forOrientation(
        100.0,
        NSLayoutConstraintOrientation::Horizontal,
    );
    progress_bar.setContentHuggingPriority_forOrientation(
        10.0,
        NSLayoutConstraintOrientation::Horizontal,
    );
    progress_bar.setContentCompressionResistancePriority_forOrientation(
        100.0,
        NSLayoutConstraintOrientation::Horizontal,
    );

    let header_views: Retained<NSArray<NSView>> = NSArray::from_slice(&[
        &*paste_button as &NSView,
        &*clear_button as &NSView,
        &*spacer as &NSView,
        &*progress_bar as &NSView,
        &*toggle_button as &NSView,
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
    header
        .setHuggingPriority_forOrientation(249.0, NSLayoutConstraintOrientation::Horizontal);

    HeaderBar {
        stack: header,
        toggle_button,
        progress_bar,
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

fn set_key(btn: &NSButton, key: &str, modifiers: NSEventModifierFlags) {
    btn.setKeyEquivalent(&NSString::from_str(key));
    btn.setKeyEquivalentModifierMask(modifiers);
}

// -- helpers ---------------------------------------------------------

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
    let mtm = MainThreadMarker::new()?;
    let app = NSApplication::sharedApplication(mtm);
    app.keyWindow().map(|w| window_id_of(&w))
}

fn find_or_create_tab(mtm: MainThreadMarker, delegate: &AppDelegate) -> WindowId {
    let blank = app_state::WINDOWS.with(|m| {
        m.borrow()
            .iter()
            .find(|(_, s)| s.doc.is_none() && s.current_path.is_none())
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
    const NS_MODAL_RESPONSE_OK: NSModalResponse = 1;
    if response != NS_MODAL_RESPONSE_OK {
        return;
    }
    let urls = panel.URLs();
    for url in urls.iter() {
        if let Some(path) = url.path() {
            let target = find_or_create_tab(mtm, delegate);
            load_file_into_window(target, &path.to_string());
        }
    }
}

fn clear_view(id: WindowId) {
    with_window_mut(id, |state| {
        state.doc = None;
        state.rendered = None;
        state.source = None;
        state.current_path = None;
        state.mode = Mode::Rendered;
        let empty = NSAttributedString::new();
        if let Some(ts) = unsafe { state.text_view.textStorage() } {
            ts.setAttributedString(&empty);
        }
        state.window.setTitle(&NSString::from_str("Markview"));
        state.toggle_button.setTitle(&NSString::from_str("Source"));
    });
}

fn paste_from_clipboard(id: WindowId) {
    let pb = NSPasteboard::generalPasteboard();
    let maybe_text = unsafe {
        let type_str: &NSString = NSPasteboardTypeString;
        pb.stringForType(type_str)
    };
    let Some(ns_text) = maybe_text else {
        eprintln!("markview: clipboard has no text");
        return;
    };
    let text = ns_text.to_string();
    if text.trim().is_empty() {
        eprintln!("markview: clipboard text is empty");
        return;
    }
    let label = "<clipboard>".to_string();
    with_window_mut(id, |state| {
        state.doc = None;
        state.rendered = None;
        state.source = None;
        state.current_path = Some(label.clone());
        state
            .window
            .setTitle(&NSString::from_str("Markview — parsing clipboard…"));
    });
    let tx = ensure_worker_channel();
    app_state::WORK_PENDING.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    worker::spawn_parse_bytes(id, text.into_bytes(), label, tx);
    ensure_poll_timer();
}

fn load_file_into_window(id: WindowId, path: &str) {
    let name = basename(path);
    with_window_mut(id, |state| {
        state.doc = None;
        state.rendered = None;
        state.source = None;
        state.current_path = Some(path.to_string());
        state.window.setTitle(&NSString::from_str(&format!(
            "Markview — loading {}…",
            name
        )));
    });
    let tx = ensure_worker_channel();
    app_state::WORK_PENDING.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    worker::spawn_load(id, path.to_string(), tx);
    ensure_poll_timer();
}

fn toggle_mode(id: WindowId) {
    with_window_mut(id, |state| {
        if state.doc.is_none() {
            return;
        }
        state.mode = match state.mode {
            Mode::Rendered => Mode::Source,
            Mode::Source => Mode::Rendered,
        };
        install_active_mode(state);
    });
}

fn install_active_mode(state: &mut app_state::WindowState) {
    let attr: Option<&NSAttributedString> = match state.mode {
        Mode::Rendered => state.rendered.as_deref(),
        Mode::Source => state.source.as_deref(),
    };
    if let (Some(attr), Some(ts)) = (attr, unsafe { state.text_view.textStorage() }) {
        ts.setAttributedString(attr);
        // Scroll to top after switching modes.
        let zero = objc2_foundation::NSPoint::new(0.0, 0.0);
        if let Some(clip) = unsafe { state.text_view.superview() } {
            clip.setBoundsOrigin(zero);
        }
    }
    let title = match state.mode {
        Mode::Rendered => "Source",
        Mode::Source => "Rendered",
    };
    state.toggle_button.setTitle(&NSString::from_str(title));
}

// -- worker dispatch -------------------------------------------------

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
                objc2::sel!(mvWorkerTick:),
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
            WorkerMsg::ParseStarted { window_id, progress } => {
                on_parse_started(window_id, progress);
            }
            WorkerMsg::DocumentReady { window_id, doc, path } => {
                on_document_ready(window_id, doc, &path);
                app_state::WORK_PENDING.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
            }
            WorkerMsg::Error { window_id, message } => {
                eprintln!("markview: {}", message);
                with_window_mut(window_id, |state| {
                    state.window.setTitle(&NSString::from_str("Markview"));
                    hide_progress(state);
                });
                app_state::WORK_PENDING.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }

    tick_progress_bars();

    if app_state::WORK_PENDING.load(std::sync::atomic::Ordering::Relaxed) <= 0 {
        app_state::POLL_TIMER.with(|slot| {
            if let Some(t) = slot.borrow().as_ref() {
                t.invalidate();
            }
            *slot.borrow_mut() = None;
        });
    }
}

fn on_parse_started(id: WindowId, progress: Arc<ProgressSink>) {
    with_window_mut(id, |state| {
        state.progress = Some(progress);
        state.progress_bar.setDoubleValue(0.0);
        state.progress_bar.setHidden(false);
    });
}

fn hide_progress(state: &mut app_state::WindowState) {
    state.progress = None;
    state.progress_bar.setHidden(true);
    state.progress_bar.setDoubleValue(0.0);
}

fn tick_progress_bars() {
    app_state::WINDOWS.with(|m| {
        for state in m.borrow().values() {
            if let Some(p) = &state.progress {
                state.progress_bar.setDoubleValue(p.fraction());
            }
        }
    });
}

fn on_document_ready(id: WindowId, doc: Arc<Document>, path: &str) {
    let mtm = MainThreadMarker::new().expect("main thread");
    let bytes = doc.bytes.as_slice();
    let rendered = rendered::build(mtm, bytes, &doc.output);
    let source = source::build_with_parse(bytes, Some(&doc.output));
    with_window_mut(id, |state| {
        state.doc = Some(doc.clone());
        state.rendered = Some(rendered);
        state.source = Some(source);
        state.mode = Mode::Rendered;
        state.current_path = Some(path.to_string());
        let name = basename(path);
        state
            .window
            .setTitle(&NSString::from_str(&format!("Markview — {}", name)));
        install_active_mode(state);
        hide_progress(state);
    });
    let _ = id;
}

// -- misc ------------------------------------------------------------

fn basename(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string())
}

fn with_app_delegate<R>(f: impl FnOnce(&AnyObject) -> R) -> R {
    let mtm = MainThreadMarker::new().expect("main thread");
    let app = NSApplication::sharedApplication(mtm);
    let delegate = app.delegate().expect("AppDelegate installed before worker ticks");
    let delegate_ptr: *const ProtocolObject<dyn NSApplicationDelegate> = &*delegate;
    let delegate_obj: &AnyObject = unsafe { &*(delegate_ptr as *const AnyObject) };
    f(delegate_obj)
}

fn main() {
    std::panic::set_hook(Box::new(|info| {
        use std::io::Write;
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open("/tmp/markview-panic.log")
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

    NSWindow::setAllowsAutomaticWindowTabbing(true, mtm);

    let delegate = AppDelegate::new(mtm);
    let proto: &ProtocolObject<dyn NSApplicationDelegate> = ProtocolObject::from_ref(&*delegate);
    app.setDelegate(Some(proto));

    app.activate();
    app.run();
}
