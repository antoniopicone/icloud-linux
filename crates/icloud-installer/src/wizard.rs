//! The installer window: five pages over the logic in `icloud_core::installer`.
//!
//! Built from libadwaita's widgets and style classes only, with no colours or
//! fonts of its own, so it looks like the rest of the desktop: native on
//! GNOME, and elsewhere whatever the user's GTK theme or `gtk.css` makes it.
//!
//! Every page is a view. What the buttons *do* (validate, write files, sign
//! in, start the service) is done by a [`Backend`] on a background thread, and
//! the results come back through small polling timers, so the window never
//! freezes on `systemctl`, on the browser or on Apple's servers.

use std::{
    cell::RefCell,
    rc::{Rc, Weak},
    sync::{Arc, mpsc},
    time::Duration,
};

use adw::prelude::*;
use gtk::{gio, glib};
use gtk4 as gtk;
use icloud_api::AccountInfo;
use icloud_core::{
    Layout,
    config::CrawlMode,
    i18n::{tr, trf},
    installer::{Backend, InstallPlan, Prepared, StepOutcome, validate_mount_dir},
    setup::Severity,
};
use libadwaita as adw;

thread_local! {
    /// The callbacks hold only weak references (so nothing leaks through a
    /// reference cycle); this is the one strong reference that keeps the
    /// wizard alive for as long as its window is.
    static ALIVE: RefCell<Option<Rc<Wizard>>> = const { RefCell::new(None) };
}

const STEPS: [(&str, &str); 5] = [
    ("welcome", "Requirements"),
    ("folder", "Folder"),
    ("account", "Sign in"),
    ("install", "Setting up"),
    ("done", "Done"),
];

/// The window title.
fn app_title() -> &'static str {
    tr("iCloud for Linux")
}

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

/// A message line under a form: hidden until something needs saying.
fn message_label() -> gtk::Label {
    let message = label("", &[]);
    message.set_visible(false);
    message
}

/// A ✓ or ⚠ in front of a row, coloured by the theme.
fn status_icon(ok: bool) -> gtk::Image {
    let icon = gtk::Image::from_icon_name(if ok { "emblem-ok-symbolic" } else { "dialog-warning-symbolic" });
    icon.add_css_class(if ok { "success" } else { "error" });
    icon
}

/// A row of a boxed list. Titles may hold paths and Apple's messages, so they
/// are never parsed as markup.
fn row(title: &str, subtitle: &str) -> adw::ActionRow {
    let row = adw::ActionRow::builder().title(title).subtitle(subtitle).build();
    row.set_use_markup(false);
    row
}

fn boxed_list() -> gtk::ListBox {
    let list = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::None).build();
    list.add_css_class("boxed-list");
    list
}

fn clear(list: &gtk::ListBox) {
    while let Some(child) = list.first_child() {
        list.remove(&child);
    }
}

/// A page: its content, which takes the free height, above a row of buttons.
fn page(content: &impl IsA<gtk::Widget>, buttons: &[&gtk::Button]) -> gtk::Box {
    let page = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.set_vexpand(true);
    page.append(content);
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    row.set_halign(gtk::Align::End);
    row.set_margin_top(12);
    row.set_margin_bottom(18);
    row.set_margin_start(18);
    row.set_margin_end(18);
    for button in buttons {
        row.append(*button);
    }
    page.append(&row);
    page
}

/// Scrolling, width-limited content under a title, for the form-like pages.
fn form(title: &str, description: &str) -> (gtk::ScrolledWindow, gtk::Box) {
    let content = gtk::Box::new(gtk::Orientation::Vertical, 18);
    content.set_margin_top(24);
    content.set_margin_bottom(24);
    content.set_margin_start(12);
    content.set_margin_end(12);
    content.append(&label(title, &["title-1"]));
    if !description.is_empty() {
        content.append(&label(description, &["dim-label"]));
    }
    let clamp = adw::Clamp::builder().maximum_size(560).child(&content).build();
    let scrolled = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).child(&clamp).build();
    (scrolled, content)
}

/// Keep a status page's content as narrow as its description.
fn narrow(child: &impl IsA<gtk::Widget>) -> adw::Clamp {
    adw::Clamp::builder().maximum_size(480).child(child).build()
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
        let text = label("", &["dim-label"]);
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
    checks: gtk::ListBox,
    welcome_next: gtk::Button,
    // folder
    folder_entry: adw::EntryRow,
    lazy: gtk::CheckButton,
    indexer: gtk::Switch,
    sidebar: gtk::Switch,
    menu: gtk::Switch,
    folder_message: gtk::Label,
    folder_busy: Busy,
    folder_next: gtk::Button,
    // account
    account_message: gtk::Label,
    account_busy: Busy,
    sign_in: gtk::Button,
    // install / done
    steps: gtk::ListBox,
    install_busy: Busy,
    install_next: gtk::Button,
    done: adw::StatusPage,
    open_folder: gtk::Button,
}

struct Wizard {
    window: adw::ApplicationWindow,
    stack: gtk::Stack,
    title: adw::WindowTitle,
    backend: Arc<dyn Backend>,
    layout: Layout,
    plan: RefCell<InstallPlan>,
    prepared: RefCell<Option<Prepared>>,
    account: RefCell<Option<AccountInfo>>,
    /// The open folder chooser, kept alive until it responds.
    chooser: RefCell<Option<gtk::FileChooserNative>>,
    widgets: Widgets,
}

pub(crate) fn present(
    app: &adw::Application,
    backend: Arc<dyn Backend>,
    layout: Layout,
    demo: bool,
    start_page: Option<String>,
) {
    let stack =
        gtk::Stack::builder().transition_type(gtk::StackTransitionType::SlideLeft).transition_duration(180).build();
    let title = adw::WindowTitle::new(if demo { tr("iCloud for Linux — demo") } else { app_title() }, "");
    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&title));

    let plan = InstallPlan::recommended(&layout);
    let widgets = build_pages(&stack, &plan, demo);
    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.append(&header);
    root.append(&stack);
    stack.set_vexpand(true);

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title(app_title())
        .default_width(640)
        .default_height(640)
        .content(&root)
        .build();

    let wizard = Rc::new(Wizard {
        window: window.clone(),
        stack,
        title,
        backend,
        layout,
        plan: RefCell::new(plan),
        prepared: RefCell::new(None),
        account: RefCell::new(None),
        chooser: RefCell::new(None),
        widgets,
    });
    ALIVE.with(|alive| *alive.borrow_mut() = Some(Rc::clone(&wizard)));
    window.connect_destroy(|_| ALIVE.with(|alive| drop(alive.borrow_mut().take())));

    wire(&wizard);
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
    let welcome_content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    let checks = boxed_list();
    welcome_content.append(&checks);
    if demo {
        welcome_content.append(&label(
            tr("Demo mode: nothing on this computer is changed, and no browser opens: the sign-in is pretended."),
            &["caption", "dim-label"],
        ));
    }
    let welcome = adw::StatusPage::builder()
        .icon_name("weather-overcast-symbolic")
        .title(tr("Welcome to iCloud for Linux"))
        .description(tr(
            "This sets up iCloud Drive as a normal folder on this computer. Files download when you open them, \
             and changes you make are uploaded for you.",
        ))
        .child(&narrow(&welcome_content))
        .build();
    let welcome_quit = gtk::Button::with_label(tr("Quit"));
    let recheck = gtk::Button::with_label(tr("Check Again"));
    let welcome_next = primary(tr("Continue"));
    stack.add_named(&page(&welcome, &[&welcome_quit, &recheck, &welcome_next]), Some("welcome"));

    // folder
    let (folder_scroll, folder) =
        form(tr("Where should iCloud Drive appear?"), tr("Pick an empty folder. It will show your iCloud files."));
    let location = adw::PreferencesGroup::new();
    let folder_entry =
        adw::EntryRow::builder().title(tr("Folder")).text(plan.mount_dir.to_string_lossy().as_ref()).build();
    let browse = gtk::Button::from_icon_name("folder-open-symbolic");
    browse.set_tooltip_text(Some(tr("Choose a folder")));
    browse.set_valign(gtk::Align::Center);
    browse.add_css_class("flat");
    folder_entry.add_suffix(&browse);
    location.add(&folder_entry);
    folder.append(&location);

    let mode = adw::PreferencesGroup::builder().title(tr("How should it work?")).build();
    let lazy = gtk::CheckButton::new();
    let full = gtk::CheckButton::new();
    full.set_group(Some(&lazy));
    lazy.set_active(plan.crawl_mode == CrawlMode::Lazy);
    full.set_active(plan.crawl_mode == CrawlMode::Full);
    for (check, title, subtitle) in [
        (&lazy, tr("Open folders on demand"), tr("Recommended. Fast to start, uses little disk.")),
        (&full, tr("Index everything up front"), tr("Slower to start; lets you search the whole drive.")),
    ] {
        let option = row(title, subtitle);
        check.set_valign(gtk::Align::Center);
        option.add_prefix(check);
        option.set_activatable_widget(Some(check));
        mode.add(&option);
    }
    folder.append(&mode);

    let extras = adw::PreferencesGroup::builder().title(tr("Extras")).build();
    let switch = |active: bool| gtk::Switch::builder().active(active).valign(gtk::Align::Center).build();
    let indexer = switch(plan.keep_indexer_out);
    let sidebar = switch(plan.show_sidebar_status);
    let menu = switch(plan.add_context_menu);
    for (toggle, title, subtitle) in [
        (
            &indexer,
            tr("Keep the search indexer out"),
            tr("Stops desktop search from downloading the whole drive. Recommended."),
        ),
        (&sidebar, tr("Show activity in the Files sidebar"), tr("What iCloud is doing appears next to its name.")),
        (
            &menu,
            tr("Add “Download from iCloud” to Files"),
            tr("In the right-click menu, to keep files on this computer."),
        ),
    ] {
        let option = row(title, subtitle);
        option.add_suffix(toggle);
        option.set_activatable_widget(Some(toggle));
        extras.add(&option);
    }
    folder.append(&extras);
    let folder_message = message_label();
    folder.append(&folder_message);
    let folder_busy = Busy::new();
    folder.append(&folder_busy.row);
    let folder_back = gtk::Button::with_label(tr("Back"));
    let folder_next = primary(tr("Continue"));
    stack.add_named(&page(&folder_scroll, &[&folder_back, &folder_next]), Some("folder"));

    // account
    let account_content = gtk::Box::new(gtk::Orientation::Vertical, 18);
    let sign_in = primary(tr("Sign In on icloud.com"));
    sign_in.add_css_class("pill");
    sign_in.set_halign(gtk::Align::Center);
    account_content.append(&sign_in);
    let account_busy = Busy::new();
    account_busy.row.set_halign(gtk::Align::Center);
    account_content.append(&account_busy.row);
    let account_message = message_label();
    account_message.set_justify(gtk::Justification::Center);
    account_message.set_xalign(0.5);
    account_content.append(&account_message);
    let account_note = label(
        tr("When Apple asks whether to trust the browser, choose Trust: that keeps you signed in here. \
         No password is stored; when the session expires, you sign in the same way again."),
        &["caption", "dim-label"],
    );
    account_note.set_justify(gtk::Justification::Center);
    account_note.set_xalign(0.5);
    account_content.append(&account_note);
    let account = adw::StatusPage::builder()
        .icon_name("web-browser-symbolic")
        .title(tr("Sign in to iCloud"))
        .description(tr(
            "You sign in on Apple's own page, in a browser window that opens for this. Your Apple ID, password \
             and verification code are typed there and never reach this app.",
        ))
        .child(&narrow(&account_content))
        .build();
    let account_back = gtk::Button::with_label(tr("Back"));
    stack.add_named(&page(&account, &[&account_back]), Some("account"));

    // install
    let (install_scroll, install) = form(tr("Setting things up"), "");
    let steps = boxed_list();
    install.append(&steps);
    let install_busy = Busy::new();
    install.append(&install_busy.row);
    let install_next = primary(tr("Continue"));
    install_next.set_sensitive(false);
    stack.add_named(&page(&install_scroll, &[&install_next]), Some("install"));

    // done
    let done = adw::StatusPage::builder().icon_name("emblem-ok-symbolic").build();
    let open_folder = primary(tr("Open iCloud Drive"));
    let close = gtk::Button::with_label(tr("Close"));
    stack.add_named(&page(&done, &[&close, &open_folder]), Some("done"));

    // The wiring finds buttons by these names.
    for (button, name) in [
        (&welcome_quit, "welcome-quit"),
        (&recheck, "recheck"),
        (&welcome_next, "welcome-next"),
        (&browse, "browse"),
        (&folder_back, "folder-back"),
        (&folder_next, "folder-next"),
        (&account_back, "account-back"),
        (&sign_in, "sign-in"),
        (&install_next, "install-next"),
        (&open_folder, "open-folder"),
        (&close, "close"),
    ] {
        button.set_widget_name(name);
    }

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
        account_message,
        account_busy,
        sign_in,
        steps,
        install_busy,
        install_next,
        done,
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
        self.title.set_subtitle(&trf("Step {} of {} · {}", &[&(index + 1), &STEPS.len(), &tr(STEPS[index].1)]));
        self.stack.set_visible_child_name(page);
        if page == "install" {
            self.begin_install();
        }
    }

    /// In the demo, jumping straight to a page needs the state the earlier
    /// pages would have produced.
    fn prime_demo_page(self: &Rc<Self>, page: Option<&str>) {
        if page == Some("done") {
            let outcomes: Vec<StepOutcome> =
                [tr("Start the iCloud service"), tr("Mount iCloud Drive"), tr("Keep the search indexer out")]
                    .iter()
                    .map(|title| StepOutcome { title: (*title).to_owned(), result: Ok(tr("done").into()) })
                    .collect();
            self.finish_page(&outcomes);
        }
    }

    fn show_error(label: &gtk::Label, text: &str) {
        label.set_label(text);
        label.remove_css_class("dim-label");
        label.add_css_class("error");
        label.set_visible(!text.is_empty());
    }

    fn show_note(label: &gtk::Label, text: &str) {
        label.set_label(text);
        label.remove_css_class("error");
        label.add_css_class("dim-label");
        label.set_visible(!text.is_empty());
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
                let list = &wizard.widgets.checks;
                clear(list);
                let mut blocked = false;
                for check in &checks {
                    let ok = check.severity == Severity::Ok;
                    blocked |= check.severity == Severity::Missing;
                    let line = row(&check.message, check.fix.as_deref().unwrap_or_default());
                    line.add_prefix(&status_icon(ok));
                    list.append(&line);
                }
                list.set_visible(!checks.is_empty());
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
        w.folder_busy.show(tr("Preparing the service…"));

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
                                &trf(
                                    "That folder could not be used; {} will be used instead.",
                                    &[&prepared.mount_dir.display()],
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
        Self::show_note(&w.account_message, "");
        w.sign_in.set_sensitive(false);
        w.account_busy.show(tr("Waiting for you to sign in in the browser window…"));

        let backend = Arc::clone(&self.backend);
        let weak = Rc::downgrade(self);
        background(
            move || backend.sign_in_with_browser(),
            move |result| {
                let Some(wizard) = weak.upgrade() else { return };
                let w = &wizard.widgets;
                w.account_busy.hide();
                w.sign_in.set_sensitive(true);
                match result {
                    Ok(info) => {
                        *wizard.account.borrow_mut() = Some(info);
                        wizard.go("install");
                    }
                    Err(err) => Self::show_error(&w.account_message, &err.to_string()),
                }
            },
        );
    }

    // install
    fn add_step(&self, title: &str, outcome: &Result<String, String>) {
        let (Ok(detail) | Err(detail)) = outcome;
        let line = row(title, detail);
        line.add_prefix(&status_icon(outcome.is_ok()));
        self.widgets.steps.append(&line);
    }

    fn begin_install(self: &Rc<Self>) {
        let w = &self.widgets;
        clear(&w.steps);
        let mount = self.plan.borrow().mount_dir.display().to_string();
        self.add_step(tr("Prepare the folder and the service"), &Ok(mount));
        let who = self
            .account
            .borrow()
            .as_ref()
            .and_then(|a| a.apple_id.clone())
            .unwrap_or_else(|| tr("your account").into());
        self.add_step(tr("Sign in to iCloud"), &Ok(trf("{}, no password stored", &[&who])));
        w.install_busy.show(tr("Starting iCloud Drive…"));
        w.install_next.set_sensitive(false);

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
        let mount = self.plan.borrow().mount_dir.display().to_string();
        if outcomes.iter().all(StepOutcome::is_ok) {
            w.done.set_icon_name(Some("emblem-ok-symbolic"));
            w.done.set_title(tr("iCloud Drive Is Ready"));
            let mut body = trf(
                "Your files are in {}. Folders load as you open them; a file downloads when you open it or \
                 choose “Download from iCloud” in its right-click menu. Changes you make there are uploaded \
                 automatically.",
                &[&mount],
            );
            if self.plan.borrow().show_sidebar_status {
                body.push_str("\n\n");
                body.push_str(tr("The Files sidebar shows what iCloud is doing next to its name."));
            }
            if self.plan.borrow().add_context_menu {
                body.push_str("\n\n");
                body.push_str(tr(
                    "Right-click a file or folder in iCloud Drive, then Scripts, to keep it on this computer.",
                ));
            }
            // The description is markup; the folder name must not be read as such.
            w.done.set_description(Some(&glib::markup_escape_text(&body)));
            w.open_folder.set_sensitive(true);
        } else {
            w.done.set_icon_name(Some("dialog-warning-symbolic"));
            w.done.set_title(tr("Almost There"));
            w.done.set_description(Some(tr(
                "Something did not finish. Run <tt>icloudctl doctor</tt> to see what is wrong and how to fix it, \
                 or <tt>icloudctl logs</tt> for details. Your sign-in was saved, so you will not have to repeat it.",
            )));
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
            Some(tr("Choose a folder for iCloud Drive")),
            Some(&self.window),
            gtk::FileChooserAction::SelectFolder,
            Some(tr("Choose")),
            Some(tr("Cancel")),
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
    on("install-next", Box::new(|w| w.go("done")), weak(wizard));
    on("open-folder", Box::new(|w| w.open_folder()), weak(wizard));
    on("close", Box::new(|w| w.window.close()), weak(wizard));

    // Enter in the folder field continues.
    let w = weak(wizard);
    wizard.widgets.folder_entry.connect_entry_activated(move |_| {
        if let Some(w) = w.upgrade() {
            w.continue_from_folder();
        }
    });
}

/// Demo only: click through the whole wizard, so the flow can be watched (or
/// screenshotted) without a person.
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
        (4000, click("sign-in")),
        (12000, click("install-next")),
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
