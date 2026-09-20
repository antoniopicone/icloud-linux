//! The installer window: six pages over the logic in `icloud_core::installer`.
//!
//! Every page is a view. What the buttons *do* (validate, write files, sign
//! in, start the service) is done by a [`Backend`] on a background thread, and
//! the results come back through small polling timers, so the window never
//! freezes on `systemctl` or on Apple's servers.

use std::{
    cell::RefCell,
    rc::{Rc, Weak},
    sync::{Arc, mpsc},
    time::Duration,
};

use gtk::{gio, glib, prelude::*};
use gtk4 as gtk;
use icloud_api::{AccountInfo, TwoFactorOptions};
use icloud_core::{
    Layout,
    config::CrawlMode,
    installer::{
        AuthEvent, AuthRequest, AuthWorker, Backend, Channel, InstallPlan, MAX_CODE_ATTEMPTS, Prepared, StepOutcome,
        code_is_complete, validate_account, validate_mount_dir,
    },
    setup::Severity,
};
use secrecy::SecretString;

thread_local! {
    /// The callbacks hold only weak references (so nothing leaks through a
    /// reference cycle); this is the one strong reference that keeps the
    /// wizard alive for as long as its window is.
    static ALIVE: RefCell<Option<Rc<Wizard>>> = const { RefCell::new(None) };
}

const STEPS: [(&str, &str); 6] = [
    ("welcome", "Requirements"),
    ("folder", "Folder"),
    ("account", "Apple ID"),
    ("verify", "Verification"),
    ("install", "Setting up"),
    ("done", "Done"),
];

const CSS: &str = "
.page-title { font-size: 22pt; font-weight: 800; }
.page-subtitle { opacity: 0.75; }
.step-indicator { opacity: 0.6; font-size: 9pt; letter-spacing: 1px; }
.check-ok { color: #2e9e5b; font-weight: 700; }
.check-bad { color: #c0392b; font-weight: 700; }
.error-text { color: #c0392b; }
.note-text { opacity: 0.7; font-size: 9pt; }
.big-code { font-size: 20pt; letter-spacing: 6px; }
.step-row { padding: 6px 0; }
";

/// Run `work` on a thread and hand its result to `done` on the main loop.
fn background<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static, done: impl FnOnce(T) + 'static) {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(work());
    });
    let done = RefCell::new(Some(done));
    glib::timeout_add_local(Duration::from_millis(40), move || match rx.try_recv() {
        Ok(value) => {
            if let Some(done) = done.borrow_mut().take() {
                done(value);
            }
            glib::ControlFlow::Break
        }
        Err(mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
        Err(mpsc::TryRecvError::Disconnected) => glib::ControlFlow::Break,
    });
}

fn label(text: &str, classes: &[&str]) -> gtk::Label {
    let label = gtk::Label::builder().label(text).wrap(true).xalign(0.0).hexpand(true).build();
    for class in classes {
        label.add_css_class(class);
    }
    label
}

/// The ✓ or ✗ in front of a line. It must not expand, or it would share the
/// row's width with the text beside it.
fn status_icon(ok: bool) -> gtk::Label {
    let icon = gtk::Label::builder().label(if ok { "✓" } else { "✗" }).xalign(0.5).valign(gtk::Align::Start).build();
    icon.add_css_class(if ok { "check-ok" } else { "check-bad" });
    icon
}

fn page(title: &str, subtitle: &str) -> gtk::Box {
    let page = gtk::Box::new(gtk::Orientation::Vertical, 14);
    page.set_margin_top(28);
    page.set_margin_bottom(24);
    page.set_margin_start(36);
    page.set_margin_end(36);
    page.append(&label(title, &["page-title"]));
    if !subtitle.is_empty() {
        page.append(&label(subtitle, &["page-subtitle"]));
    }
    page
}

fn button_row(buttons: &[&gtk::Button]) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    row.set_halign(gtk::Align::End);
    row.set_valign(gtk::Align::End);
    row.set_vexpand(true);
    for button in buttons {
        row.append(*button);
    }
    row
}

fn primary(text: &str) -> gtk::Button {
    let button = gtk::Button::with_label(text);
    button.add_css_class("suggested-action");
    button
}

/// A spinner with a message beside it, hidden until needed.
struct Busy {
    row: gtk::Box,
    spinner: gtk::Spinner,
    text: gtk::Label,
}

impl Busy {
    fn new() -> Self {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        let spinner = gtk::Spinner::new();
        let text = label("", &["page-subtitle"]);
        row.append(&spinner);
        row.append(&text);
        row.set_visible(false);
        Self { row, spinner, text }
    }

    fn show(&self, message: &str) {
        self.text.set_label(message);
        self.spinner.set_spinning(true);
        self.row.set_visible(true);
    }

    fn hide(&self) {
        self.spinner.set_spinning(false);
        self.row.set_visible(false);
    }
}

struct Widgets {
    // welcome
    checks: gtk::Box,
    welcome_next: gtk::Button,
    // folder
    folder_entry: gtk::Entry,
    lazy: gtk::CheckButton,
    indexer: gtk::CheckButton,
    sidebar: gtk::CheckButton,
    menu: gtk::CheckButton,
    folder_message: gtk::Label,
    folder_busy: Busy,
    folder_next: gtk::Button,
    // account
    username: gtk::Entry,
    password: gtk::PasswordEntry,
    remember: gtk::CheckButton,
    account_message: gtk::Label,
    account_busy: Busy,
    sign_in: gtk::Button,
    // verify
    device: gtk::CheckButton,
    sms: gtk::CheckButton,
    phones: gtk::DropDown,
    send_code: gtk::Button,
    code_entry: gtk::Entry,
    verify_message: gtk::Label,
    verify_busy: Busy,
    verify_button: gtk::Button,
    // install / done
    steps: gtk::Box,
    install_busy: Busy,
    install_next: gtk::Button,
    done_title: gtk::Label,
    done_body: gtk::Label,
    open_folder: gtk::Button,
}

struct Wizard {
    window: gtk::ApplicationWindow,
    stack: gtk::Stack,
    step: gtk::Label,
    backend: Arc<dyn Backend>,
    layout: Layout,
    plan: RefCell<InstallPlan>,
    prepared: RefCell<Option<Prepared>>,
    worker: RefCell<Option<AuthWorker>>,
    options: RefCell<TwoFactorOptions>,
    account: RefCell<Option<AccountInfo>>,
    /// The open folder chooser, kept alive until it responds.
    chooser: RefCell<Option<gtk::FileChooserNative>>,
    widgets: Widgets,
}

pub(crate) fn present(
    app: &gtk::Application,
    backend: Arc<dyn Backend>,
    layout: Layout,
    demo: bool,
    start_page: Option<String>,
) {
    if let Some(display) = gtk::gdk::Display::default() {
        let provider = gtk::CssProvider::new();
        provider.load_from_data(CSS);
        gtk::style_context_add_provider_for_display(&display, &provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
    }

    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title(if demo { "iCloud for Linux — demo" } else { "iCloud for Linux" })
        .default_width(640)
        .default_height(600)
        .build();
    let stack =
        gtk::Stack::builder().transition_type(gtk::StackTransitionType::SlideLeft).transition_duration(180).build();
    let step = label("", &["step-indicator"]);
    step.set_margin_top(14);
    step.set_margin_start(36);

    let plan = InstallPlan::recommended(&layout);
    let widgets = build_pages(&stack, &plan, demo);
    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.append(&step);
    root.append(&stack);
    stack.set_vexpand(true);
    window.set_child(Some(&root));

    let wizard = Rc::new(Wizard {
        window: window.clone(),
        stack,
        step,
        backend,
        layout,
        plan: RefCell::new(plan),
        prepared: RefCell::new(None),
        worker: RefCell::new(None),
        options: RefCell::new(TwoFactorOptions::default()),
        account: RefCell::new(None),
        chooser: RefCell::new(None),
        widgets,
    });
    ALIVE.with(|alive| *alive.borrow_mut() = Some(Rc::clone(&wizard)));
    window.connect_destroy(|_| ALIVE.with(|alive| drop(alive.borrow_mut().take())));

    wire(&wizard);
    start_event_pump(&wizard);
    wizard.refresh_requirements();
    if demo {
        wizard.prime_demo_page(start_page.as_deref());
    }
    wizard.go(start_page.as_deref().unwrap_or("welcome"));
    window.present();
    if demo && std::env::var_os("ICLOUD_INSTALLER_AUTOPLAY").is_some() {
        autoplay(&wizard);
    }
}

// ---- building the pages ------------------------------------------------------------

fn build_pages(stack: &gtk::Stack, plan: &InstallPlan, demo: bool) -> Widgets {
    // welcome
    let welcome = page(
        "Welcome to iCloud for Linux",
        "This sets up iCloud Drive as a normal folder on this computer. Files download when you open them, and changes you make are uploaded for you.",
    );
    if demo {
        welcome.append(&label("Demo mode: nothing on this computer is changed. Use the code 123456; the password “wrong” fails and “trusted” skips verification.", &["note-text"]));
    }
    let checks = gtk::Box::new(gtk::Orientation::Vertical, 4);
    welcome.append(&checks);
    let welcome_quit = gtk::Button::with_label("Quit");
    let recheck = gtk::Button::with_label("Check again");
    let welcome_next = primary("Continue");
    welcome.append(&button_row(&[&welcome_quit, &recheck, &welcome_next]));
    stack.add_named(&welcome, Some("welcome"));

    // folder
    let folder = page("Where should iCloud Drive appear?", "Pick an empty folder. It will show your iCloud files.");
    let folder_entry = gtk::Entry::builder().text(plan.mount_dir.to_string_lossy().as_ref()).hexpand(true).build();
    let browse = gtk::Button::with_label("Browse…");
    let folder_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    folder_row.append(&folder_entry);
    folder_row.append(&browse);
    folder.append(&folder_row);
    folder.append(&label("How should it work?", &["heading"]));
    let lazy = gtk::CheckButton::with_label("Open folders on demand (recommended) — fast to start, uses little disk");
    let full =
        gtk::CheckButton::with_label("Index everything up front — slower start, lets you search the whole drive");
    full.set_group(Some(&lazy));
    lazy.set_active(plan.crawl_mode == CrawlMode::Lazy);
    full.set_active(plan.crawl_mode == CrawlMode::Full);
    folder.append(&lazy);
    folder.append(&full);
    let indexer =
        gtk::CheckButton::with_label("Keep the desktop search indexer from downloading everything (recommended)");
    indexer.set_active(plan.keep_indexer_out);
    let sidebar = gtk::CheckButton::with_label("Show what iCloud is doing next to it in the Files sidebar");
    sidebar.set_active(plan.show_sidebar_status);
    let menu = gtk::CheckButton::with_label("Add \"Download from iCloud\" to the right-click menu of Files");
    menu.set_active(plan.add_context_menu);
    folder.append(&indexer);
    folder.append(&sidebar);
    folder.append(&menu);
    let folder_message = label("", &["error-text"]);
    folder.append(&folder_message);
    let folder_busy = Busy::new();
    folder.append(&folder_busy.row);
    let folder_back = gtk::Button::with_label("Back");
    let folder_next = primary("Continue");
    folder.append(&button_row(&[&folder_back, &folder_next]));
    stack.add_named(&folder, Some("folder"));

    // account
    let account =
        page("Sign in with your Apple ID", "Your password goes straight to Apple. It is not stored unless you ask.");
    let username = gtk::Entry::builder()
        .placeholder_text("Apple ID (email or phone number)")
        .hexpand(true)
        .activates_default(false)
        .build();
    let password =
        gtk::PasswordEntry::builder().placeholder_text("Password").show_peek_icon(true).hexpand(true).build();
    account.append(&username);
    account.append(&password);
    let remember = gtk::CheckButton::with_label("Remember my password on this computer");
    account.append(&remember);
    account.append(&label(
        "Off is safer. If you turn it on, the password is saved in a file only you can read, and the service can renew an expired session by itself. Otherwise you sign in again when it expires (every few weeks).",
        &["note-text"],
    ));
    let account_message = label("", &["error-text"]);
    account.append(&account_message);
    let account_busy = Busy::new();
    account.append(&account_busy.row);
    let account_back = gtk::Button::with_label("Back");
    let sign_in = primary("Sign in");
    account.append(&button_row(&[&account_back, &sign_in]));
    stack.add_named(&account, Some("account"));

    // verify
    let verify = page("Verify it is you", "Apple sent you a six-digit code. Choose how you want to receive it.");
    let device = gtk::CheckButton::with_label("Show the code on one of my Apple devices");
    let sms = gtk::CheckButton::with_label("Send the code by text message");
    sms.set_group(Some(&device));
    device.set_active(true);
    verify.append(&device);
    verify.append(&label("Approve the sign-in on your iPhone, iPad or Mac, then type the code it shows. Nothing appeared? Choose the text message.", &["note-text"]));
    verify.append(&sms);
    let phones = gtk::DropDown::from_strings(&[]);
    phones.set_visible(false);
    verify.append(&phones);
    let send_code = gtk::Button::with_label("Send code");
    send_code.set_halign(gtk::Align::Start);
    send_code.set_visible(false);
    verify.append(&send_code);
    let code_entry = gtk::Entry::builder()
        .placeholder_text("000000")
        .max_length(10)
        .input_purpose(gtk::InputPurpose::Digits)
        .hexpand(true)
        .build();
    code_entry.add_css_class("big-code");
    verify.append(&code_entry);
    let verify_message = label("", &["error-text"]);
    verify.append(&verify_message);
    let verify_busy = Busy::new();
    verify.append(&verify_busy.row);
    let verify_back = gtk::Button::with_label("Start over");
    let verify_button = primary("Verify");
    verify_button.set_sensitive(false);
    verify.append(&button_row(&[&verify_back, &verify_button]));
    stack.add_named(&verify, Some("verify"));

    // install
    let install = page("Setting things up", "");
    let steps = gtk::Box::new(gtk::Orientation::Vertical, 2);
    install.append(&steps);
    let install_busy = Busy::new();
    install.append(&install_busy.row);
    let install_next = primary("Continue");
    install_next.set_sensitive(false);
    install.append(&button_row(&[&install_next]));
    stack.add_named(&install, Some("install"));

    // done
    let done = page("", "");
    let done_title = label("", &["page-title"]);
    let done_body = label("", &["page-subtitle"]);
    // `page` already added an (empty) title; replace it.
    if let Some(first) = done.first_child() {
        done.remove(&first);
    }
    done.append(&done_title);
    done.append(&done_body);
    let open_folder = primary("Open iCloud Drive");
    let close = gtk::Button::with_label("Close");
    done.append(&button_row(&[&close, &open_folder]));
    stack.add_named(&done, Some("done"));

    // Buttons that only need the window are wired later; stash the ones the
    // wiring needs by name through widget names.
    for (button, name) in [
        (&welcome_quit, "welcome-quit"),
        (&recheck, "recheck"),
        (&browse, "browse"),
        (&folder_back, "folder-back"),
        (&account_back, "account-back"),
        (&verify_back, "verify-back"),
        (&close, "close"),
    ] {
        button.set_widget_name(name);
    }
    welcome_next.set_widget_name("welcome-next");
    folder_next.set_widget_name("folder-next");
    sign_in.set_widget_name("sign-in");
    send_code.set_widget_name("send-code");
    verify_button.set_widget_name("verify");
    install_next.set_widget_name("install-next");
    open_folder.set_widget_name("open-folder");

    Widgets {
        checks,
        welcome_next,
        folder_entry,
        lazy,
        indexer,
        sidebar,
        menu,
        folder_message,
        folder_busy,
        folder_next,
        username,
        password,
        remember,
        account_message,
        account_busy,
        sign_in,
        device,
        sms,
        phones,
        send_code,
        code_entry,
        verify_message,
        verify_busy,
        verify_button,
        steps,
        install_busy,
        install_next,
        done_title,
        done_body,
        open_folder,
    }
}

/// Find a button by the name given in `build_pages`.
fn find_button(root: &gtk::Stack, name: &str) -> Option<gtk::Button> {
    fn walk(widget: &gtk::Widget, name: &str) -> Option<gtk::Button> {
        if widget.widget_name() == name
            && let Ok(button) = widget.clone().downcast::<gtk::Button>()
        {
            return Some(button);
        }
        let mut child = widget.first_child();
        while let Some(current) = child {
            if let Some(found) = walk(&current, name) {
                return Some(found);
            }
            child = current.next_sibling();
        }
        None
    }
    walk(root.upcast_ref(), name)
}

// ---- behaviour ----------------------------------------------------------------------

impl Wizard {
    fn go(self: &Rc<Self>, page: &str) {
        let index = STEPS.iter().position(|(name, _)| *name == page).unwrap_or(0);
        self.step.set_label(&format!("STEP {} OF {}  ·  {}", index + 1, STEPS.len(), STEPS[index].1.to_uppercase()));
        self.stack.set_visible_child_name(page);
        match page {
            "verify" => self.prepare_verify_page(),
            "install" => self.begin_install(),
            _ => {}
        }
    }

    /// In the demo, jumping straight to a page needs the state the earlier
    /// pages would have produced.
    fn prime_demo_page(self: &Rc<Self>, page: Option<&str>) {
        match page {
            Some("verify") => {
                if let Ok(auth) = self.backend.authenticator() {
                    *self.options.borrow_mut() = auth.options();
                }
            }
            Some("done") => {
                let outcomes: Vec<StepOutcome> =
                    ["Start the iCloud service", "Mount iCloud Drive", "Keep the search indexer out"]
                        .iter()
                        .map(|title| StepOutcome { title: (*title).to_owned(), result: Ok("done".into()) })
                        .collect();
                self.finish_page(&outcomes);
            }
            _ => {}
        }
    }

    fn show_error(label: &gtk::Label, text: &str) {
        label.set_label(text);
        label.remove_css_class("note-text");
        label.add_css_class("error-text");
    }

    fn show_note(label: &gtk::Label, text: &str) {
        label.set_label(text);
        label.remove_css_class("error-text");
        label.add_css_class("note-text");
    }

    // welcome
    fn refresh_requirements(self: &Rc<Self>) {
        let backend = Arc::clone(&self.backend);
        let weak = Rc::downgrade(self);
        self.widgets.welcome_next.set_sensitive(false);
        background(
            move || backend.preflight(),
            move |checks| {
                let Some(wizard) = weak.upgrade() else { return };
                while let Some(child) = wizard.widgets.checks.first_child() {
                    wizard.widgets.checks.remove(&child);
                }
                let mut blocked = false;
                for check in &checks {
                    let ok = check.severity == Severity::Ok;
                    blocked |= check.severity == Severity::Missing;
                    let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
                    row.append(&status_icon(ok));
                    let text = match &check.fix {
                        Some(fix) => format!("{} — {fix}", check.message),
                        None => check.message.clone(),
                    };
                    row.append(&label(&text, &[]));
                    wizard.widgets.checks.append(&row);
                }
                wizard.widgets.welcome_next.set_sensitive(!blocked);
            },
        );
    }

    // folder
    fn continue_from_folder(self: &Rc<Self>) {
        let w = &self.widgets;
        let dir = std::path::PathBuf::from(w.folder_entry.text().trim());
        if let Err(problem) = validate_mount_dir(&dir, &self.layout) {
            Self::show_error(&w.folder_message, &problem);
            return;
        }
        {
            let mut plan = self.plan.borrow_mut();
            plan.mount_dir = dir;
            plan.crawl_mode = if w.lazy.is_active() { CrawlMode::Lazy } else { CrawlMode::Full };
            plan.keep_indexer_out = w.indexer.is_active();
            plan.show_sidebar_status = w.sidebar.is_active();
            plan.add_context_menu = w.menu.is_active();
        }
        Self::show_note(&w.folder_message, "");
        w.folder_next.set_sensitive(false);
        w.folder_busy.show("Preparing the service…");

        let (backend, plan) = (Arc::clone(&self.backend), self.plan.borrow().clone());
        let weak = Rc::downgrade(self);
        background(
            move || backend.prepare(&plan),
            move |result| {
                let Some(wizard) = weak.upgrade() else { return };
                let w = &wizard.widgets;
                w.folder_busy.hide();
                w.folder_next.set_sensitive(true);
                match result {
                    Ok(prepared) => {
                        if prepared.used_fallback {
                            Self::show_note(
                                &w.folder_message,
                                &format!(
                                    "That folder could not be used; {} will be used instead.",
                                    prepared.mount_dir.display()
                                ),
                            );
                        }
                        wizard.plan.borrow_mut().mount_dir = prepared.mount_dir.clone();
                        *wizard.prepared.borrow_mut() = Some(prepared);
                        wizard.go("account");
                    }
                    Err(err) => Self::show_error(&w.folder_message, &err.to_string()),
                }
            },
        );
    }

    // account
    fn sign_in(self: &Rc<Self>) {
        let w = &self.widgets;
        let (username, password) = (w.username.text().to_string(), w.password.text().to_string());
        if let Err(problem) = validate_account(&username, &password) {
            Self::show_error(&w.account_message, problem);
            return;
        }
        Self::show_note(&w.account_message, "");
        w.sign_in.set_sensitive(false);
        w.account_busy.show("Signing in…");
        let remember = w.remember.is_active();
        self.plan.borrow_mut().remember_password = remember;

        let backend = Arc::clone(&self.backend);
        let (user, pass) = (username.trim().to_owned(), password.clone());
        let weak = Rc::downgrade(self);
        background(
            move || {
                backend.save_account(&user, remember.then(|| SecretString::from(pass.clone())))?;
                backend.authenticator()
            },
            move |result| {
                let Some(wizard) = weak.upgrade() else { return };
                match result {
                    Ok(auth) => {
                        let worker = AuthWorker::spawn(auth);
                        worker.request(AuthRequest::Login(SecretString::from(
                            wizard.widgets.password.text().to_string(),
                        )));
                        *wizard.worker.borrow_mut() = Some(worker);
                    }
                    Err(err) => wizard.account_failed(&err.to_string()),
                }
            },
        );
    }

    fn account_failed(&self, message: &str) {
        let w = &self.widgets;
        w.account_busy.hide();
        w.sign_in.set_sensitive(true);
        Self::show_error(&w.account_message, message);
    }

    // verify
    fn prepare_verify_page(&self) {
        let w = &self.widgets;
        let options = self.options.borrow();
        let names: Vec<String> = options
            .phones
            .iter()
            .enumerate()
            .map(|(i, p)| p.display.clone().unwrap_or_else(|| format!("Phone number {}", i + 1)))
            .collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        w.phones.set_model(Some(&gtk::StringList::new(&refs)));
        w.sms.set_sensitive(options.can_use_sms());
        w.device.set_sensitive(options.device_code_available() || !options.can_use_sms());
        let prefer_sms = !options.device_code_available() && options.can_use_sms();
        w.sms.set_active(prefer_sms);
        w.device.set_active(!prefer_sms);
        w.code_entry.set_text("");
        w.verify_button.set_sensitive(false);
        w.verify_busy.hide();
        Self::show_note(
            &w.verify_message,
            if options.push_bridge && options.can_use_sms() {
                "For this account Apple does not show a code on your devices unless the app performs a push \
                 handshake, which is not supported yet. Have the code sent by text message instead."
            } else {
                ""
            },
        );
        drop(options);
        self.update_delivery_widgets();
    }

    fn update_delivery_widgets(&self) {
        let w = &self.widgets;
        let sms = w.sms.is_active();
        let many = self.options.borrow().phones.len() > 1;
        w.phones.set_visible(sms && many);
        w.send_code.set_visible(sms);
    }

    fn channel(&self) -> Channel {
        if self.widgets.sms.is_active() {
            Channel::Sms(self.widgets.phones.selected() as usize)
        } else {
            Channel::TrustedDevice
        }
    }

    fn send_sms(&self) {
        let w = &self.widgets;
        if let Some(worker) = self.worker.borrow().as_ref() {
            w.send_code.set_sensitive(false);
            w.verify_busy.show("Sending the text message…");
            worker.request(AuthRequest::SendSms { phone: w.phones.selected() as usize });
        }
    }

    fn verify(&self) {
        let w = &self.widgets;
        let code = w.code_entry.text().to_string();
        if !code_is_complete(&code) {
            return;
        }
        if let Some(worker) = self.worker.borrow().as_ref() {
            w.verify_button.set_sensitive(false);
            w.verify_busy.show("Checking the code…");
            worker.request(AuthRequest::Verify { channel: self.channel(), code });
        }
    }

    fn handle(self: &Rc<Self>, event: AuthEvent) {
        let w = &self.widgets;
        match event {
            AuthEvent::Authenticated(info) => {
                *self.account.borrow_mut() = Some(info);
                w.account_busy.hide();
                w.verify_busy.hide();
                self.go("install");
            }
            AuthEvent::CodeNeeded(options) => {
                *self.options.borrow_mut() = options;
                w.account_busy.hide();
                w.sign_in.set_sensitive(true);
                self.go("verify");
            }
            AuthEvent::SmsSent { to } => {
                w.verify_busy.hide();
                w.send_code.set_sensitive(true);
                Self::show_note(&w.verify_message, &format!("A code was sent to {to}. Type it below."));
            }
            AuthEvent::CodeRejected { attempts_left } => {
                w.verify_busy.hide();
                w.code_entry.set_text("");
                Self::show_error(
                    &w.verify_message,
                    &format!(
                        "That code was not accepted. {attempts_left} attempt{} left.",
                        if attempts_left == 1 { "" } else { "s" }
                    ),
                );
            }
            AuthEvent::OutOfAttempts => {
                w.verify_busy.hide();
                w.verify_button.set_sensitive(false);
                w.code_entry.set_sensitive(false);
                Self::show_error(
                    &w.verify_message,
                    &format!(
                        "{MAX_CODE_ATTEMPTS} codes were not accepted. To protect your account, wait a few minutes and start over."
                    ),
                );
            }
            AuthEvent::Failed(message) => {
                w.verify_busy.hide();
                w.send_code.set_sensitive(true);
                if self.stack.visible_child_name().as_deref() == Some("verify") {
                    Self::show_error(&w.verify_message, &message);
                } else {
                    self.account_failed(&message);
                }
            }
        }
    }

    fn start_over(self: &Rc<Self>) {
        *self.worker.borrow_mut() = None;
        let w = &self.widgets;
        w.code_entry.set_sensitive(true);
        w.sign_in.set_sensitive(true);
        w.account_busy.hide();
        Self::show_note(&w.account_message, "");
        self.go("account");
    }

    // install
    fn add_step(&self, title: &str, outcome: &Result<String, String>) {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        row.add_css_class("step-row");
        let ok = outcome.is_ok();
        row.append(&status_icon(ok));
        let text = gtk::Box::new(gtk::Orientation::Vertical, 0);
        text.append(&label(title, &["heading"]));
        let (Ok(detail) | Err(detail)) = outcome;
        let detail = detail.as_str();
        if !detail.is_empty() {
            text.append(&label(detail, &[if ok { "note-text" } else { "error-text" }]));
        }
        row.append(&text);
        self.widgets.steps.append(&row);
    }

    fn begin_install(self: &Rc<Self>) {
        let w = &self.widgets;
        while let Some(child) = w.steps.first_child() {
            w.steps.remove(&child);
        }
        let mount = self.plan.borrow().mount_dir.display().to_string();
        self.add_step("Prepare the folder and the service", &Ok(mount.clone()));
        self.add_step(
            "Save your Apple ID",
            &Ok(if self.plan.borrow().remember_password {
                "password remembered".into()
            } else {
                "password not stored".into()
            }),
        );
        let who =
            self.account.borrow().as_ref().and_then(|a| a.apple_id.clone()).unwrap_or_else(|| "your account".into());
        self.add_step("Sign in to iCloud", &Ok(who));
        w.install_busy.show("Starting iCloud Drive…");
        w.install_next.set_sensitive(false);

        // Sign-in is finished; free the worker (and Apple's session) now.
        *self.worker.borrow_mut() = None;

        let (backend, plan) = (Arc::clone(&self.backend), self.plan.borrow().clone());
        let mount_dir = plan.mount_dir.clone();
        let weak = Rc::downgrade(self);
        background(
            move || backend.finish(&plan, &mount_dir),
            move |outcomes: Vec<StepOutcome>| {
                let Some(wizard) = weak.upgrade() else { return };
                for outcome in &outcomes {
                    wizard.add_step(&outcome.title, &outcome.result);
                }
                wizard.widgets.install_busy.hide();
                wizard.widgets.install_next.set_sensitive(true);
                wizard.finish_page(&outcomes);
            },
        );
    }

    fn finish_page(&self, outcomes: &[StepOutcome]) {
        let w = &self.widgets;
        let all_ok = outcomes.iter().all(StepOutcome::is_ok);
        let mount = self.plan.borrow().mount_dir.display().to_string();
        if all_ok {
            w.done_title.set_label("iCloud Drive is ready");
            let mut body = format!(
                "Your files are in {mount}. Folders load as you open them; a file downloads when you open it or \
                 choose \"Download from iCloud\" in its right-click menu. Changes you make there are uploaded \
                 automatically."
            );
            if self.plan.borrow().show_sidebar_status {
                body.push_str("\n\nThe Files sidebar shows what iCloud is doing next to its name.");
            }
            if self.plan.borrow().add_context_menu {
                body.push_str(
                    "\n\nRight-click a file or folder in iCloud Drive, then Scripts, to keep it on this computer.",
                );
            }
            w.done_body.set_label(&body);
            w.open_folder.set_sensitive(true);
        } else {
            w.done_title.set_label("Almost there");
            w.done_body.set_label(
                "Something did not finish. Run `icloudctl doctor` to see what is wrong and how to fix it, or `icloudctl logs` for details. \
                 Your sign-in was saved, so you will not have to repeat it.",
            );
            w.open_folder.set_sensitive(false);
        }
    }

    fn open_folder(&self) {
        let uri = format!("file://{}", self.plan.borrow().mount_dir.display());
        if let Err(err) = gio::AppInfo::launch_default_for_uri(&uri, None::<&gio::AppLaunchContext>) {
            tracing::warn!("could not open {uri}: {err}");
        }
    }

    #[allow(deprecated)] // FileChooserNative is what GTK 4.8 (Debian 12, Ubuntu 22.04) offers.
    fn browse(self: &Rc<Self>) {
        let dialog = gtk::FileChooserNative::new(
            Some("Choose a folder for iCloud Drive"),
            Some(&self.window),
            gtk::FileChooserAction::SelectFolder,
            Some("Choose"),
            Some("Cancel"),
        );
        let weak = Rc::downgrade(self);
        dialog.connect_response(move |dialog, response| {
            if let Some(wizard) = weak.upgrade() {
                if response == gtk::ResponseType::Accept
                    && let Some(path) = dialog.file().and_then(|file| file.path())
                {
                    wizard.widgets.folder_entry.set_text(&path.to_string_lossy());
                }
                wizard.chooser.borrow_mut().take();
            }
        });
        dialog.show();
        // The dialog must outlive this call; the wizard holds it until it answers.
        *self.chooser.borrow_mut() = Some(dialog);
    }
}

fn wire(wizard: &Rc<Wizard>) {
    let stack = &wizard.stack;
    let weak = |w: &Rc<Wizard>| Rc::downgrade(w);
    type Action = Box<dyn Fn(&Rc<Wizard>)>;
    let on = |name: &str, action: Action, weak_wizard: Weak<Wizard>| {
        if let Some(button) = find_button(stack, name) {
            button.connect_clicked(move |_| {
                if let Some(wizard) = weak_wizard.upgrade() {
                    action(&wizard);
                }
            });
        }
    };

    on("welcome-quit", Box::new(|w| w.window.close()), weak(wizard));
    on("recheck", Box::new(Wizard::refresh_requirements), weak(wizard));
    on("welcome-next", Box::new(|w| w.go("folder")), weak(wizard));
    on("browse", Box::new(Wizard::browse), weak(wizard));
    on("folder-back", Box::new(|w| w.go("welcome")), weak(wizard));
    on("folder-next", Box::new(Wizard::continue_from_folder), weak(wizard));
    on("account-back", Box::new(|w| w.go("folder")), weak(wizard));
    on("sign-in", Box::new(Wizard::sign_in), weak(wizard));
    on("verify-back", Box::new(Wizard::start_over), weak(wizard));
    on("send-code", Box::new(|w| w.send_sms()), weak(wizard));
    on("verify", Box::new(|w| w.verify()), weak(wizard));
    on("install-next", Box::new(|w| w.go("done")), weak(wizard));
    on("open-folder", Box::new(|w| w.open_folder()), weak(wizard));
    on("close", Box::new(|w| w.window.close()), weak(wizard));

    // Enter submits the current page.
    let submit = |w: Weak<Wizard>, action: fn(&Rc<Wizard>)| {
        move |_: &gtk::Entry| {
            if let Some(w) = w.upgrade() {
                action(&w);
            }
        }
    };
    wizard.widgets.folder_entry.connect_activate(submit(weak(wizard), Wizard::continue_from_folder));
    wizard.widgets.username.connect_activate({
        let w = weak(wizard);
        move |_| {
            if let Some(w) = w.upgrade() {
                w.widgets.password.grab_focus();
            }
        }
    });
    wizard.widgets.password.connect_activate({
        let w = weak(wizard);
        move |_| {
            if let Some(w) = w.upgrade() {
                w.sign_in();
            }
        }
    });
    wizard.widgets.code_entry.connect_activate(submit(weak(wizard), |w| w.verify()));
    wizard.widgets.code_entry.connect_changed({
        let w = weak(wizard);
        move |entry| {
            if let Some(w) = w.upgrade() {
                let busy = w.widgets.verify_busy.row.is_visible();
                w.widgets.verify_button.set_sensitive(code_is_complete(&entry.text()) && !busy && entry.is_sensitive());
            }
        }
    });
    for radio in [&wizard.widgets.device, &wizard.widgets.sms] {
        let w = weak(wizard);
        radio.connect_toggled(move |_| {
            if let Some(w) = w.upgrade() {
                w.update_delivery_widgets();
            }
        });
    }
}

/// Poll the sign-in worker from the main loop.
fn start_event_pump(wizard: &Rc<Wizard>) {
    let weak = Rc::downgrade(wizard);
    glib::timeout_add_local(Duration::from_millis(80), move || {
        let Some(wizard) = weak.upgrade() else { return glib::ControlFlow::Break };
        loop {
            let event = wizard.worker.borrow().as_ref().and_then(AuthWorker::poll);
            match event {
                Some(event) => wizard.handle(event),
                None => break,
            }
        }
        glib::ControlFlow::Continue
    });
}

/// Demo only: click through the whole wizard, wrong code included, so the
/// flow can be watched (or screenshotted) without a person.
fn autoplay(wizard: &Rc<Wizard>) {
    type Step = Box<dyn Fn(&Rc<Wizard>)>;
    let click = |name: &'static str| -> Step {
        Box::new(move |w| {
            if let Some(button) = find_button(&w.stack, name) {
                button.emit_clicked();
            }
        })
    };
    let script: Vec<(u64, Step)> = vec![
        (1500, click("welcome-next")),
        (2500, Box::new(|w| w.widgets.folder_entry.set_text("/tmp/icloud-demo-mount"))),
        (2600, click("folder-next")),
        (
            4000,
            Box::new(|w| {
                w.widgets.username.set_text("demo@icloud.com");
                w.widgets.password.set_text("secret");
            }),
        ),
        (4100, click("sign-in")),
        (6500, Box::new(|w| w.widgets.sms.set_active(true))),
        (6700, click("send-code")),
        (8000, Box::new(|w| w.widgets.code_entry.set_text("000000"))),
        (8100, click("verify")),
        (9500, Box::new(|w| w.widgets.code_entry.set_text("123456"))),
        (9600, click("verify")),
        (14500, click("install-next")),
    ];
    for (delay, step) in script {
        let weak = Rc::downgrade(wizard);
        glib::timeout_add_local_once(Duration::from_millis(delay), move || {
            if let Some(w) = weak.upgrade() {
                step(&w);
            }
        });
    }
}
