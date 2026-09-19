#!/usr/bin/env python3
"""icloud_status.py — Nautilus sidebar status for icloud-linux.

With crawl_mode: lazy the mount comes up almost immediately and folders are
enumerated as they are opened, so there is nothing long-running to wait for at
startup. What is useful instead is seeing, in the file manager itself, which
folder is being enumerated and which file is being downloaded right now.

This extension:

  - tails ~/.local/state/icloud-linux/icloud.log, reading only the bytes
    appended since the previous poll;
  - recognizes the events driver.py emits through _log_sync():
        sync list-directory-start path='...'       -> folder being listed
        sync list-directory-complete path='...'    -> listing finished
        sync hydrate-start path='...' size=...     -> file downloading
        sync hydrate-complete path='...' ...       -> download finished
    plus the crawl_mode: full pattern "Timed out enumerating ... skipping";
  - relabels the "iCloud" bookmark in the sidebar accordingly, e.g.
    "iCloud (listing: Documents)" or "iCloud (downloading: invoice.pdf)",
    falling back to plain "iCloud" once nothing has happened for
    ACTIVITY_WINDOW_SECONDS;
  - adds an "iCloud sync status…" context-menu item showing the last known
    activity as a desktop notification.

Install with './icloudctl nautilus-install', which copies this file into
~/.local/share/nautilus-python/extensions/ and restarts Nautilus. It needs the
python3-nautilus (Debian/Ubuntu) or nautilus-python (Fedora) package.

Note: the bookmark whose label carries the status is created if it does not
exist yet, which is also what puts iCloud in the Nautilus sidebar.
"""

import gi

try:
    gi.require_version("Nautilus", "4.0")
except ValueError:
    # Nautilus may already have loaded another version of its GI namespace
    # into this process; using whatever is loaded is better than failing.
    pass

from gi.repository import Nautilus, GObject, GLib, Gio

import os
import re
import time

CONFIG_DIR = os.path.expanduser("~/.config/icloud-linux")
ENV_FILE = os.path.join(CONFIG_DIR, "icloud.env")
LOG_PATH = os.environ.get(
    "ICLOUD_LOG_PATH", os.path.expanduser("~/.local/state/icloud-linux/icloud.log")
)
BOOKMARKS_FILE = os.path.expanduser("~/.config/gtk-3.0/bookmarks")
MOUNT_DIR_DEFAULT = os.path.expanduser("~/iCloud")
POLL_SECONDS = 5
ACTIVITY_WINDOW_SECONDS = 20  # how long a silence before the label resets

RE_LIST_START = re.compile(r"sync list-directory-start path='([^']*)'")
RE_LIST_DONE = re.compile(r"sync list-directory-complete path='([^']*)'\s*entries=(\d+)")
RE_HYDRATE_START = re.compile(r"sync hydrate-start path='([^']*)'")
RE_HYDRATE_DONE = re.compile(r"sync hydrate-complete path='([^']*)'")
RE_TIMEOUT_SKIP = re.compile(r"Timed out enumerating (\S+) after \d+s")


def mount_dir():
    """Return the configured mount point, as icloudctl recorded it."""
    try:
        with open(ENV_FILE) as handle:
            for line in handle:
                line = line.strip()
                if line.startswith("ICLOUD_MOUNT="):
                    value = line.split("=", 1)[1].strip().strip("'\"")
                    if value:
                        return os.path.expanduser(value)
    except OSError:
        pass
    return MOUNT_DIR_DEFAULT


def _display_name(icloud_path):
    """Shorten an iCloud path to its last component for a compact label."""
    if not icloud_path or icloud_path == "/":
        return "iCloud"
    return os.path.basename(icloud_path.rstrip("/")) or icloud_path


class ICloudStatusMonitor:
    """Holds the current status, refreshed every POLL_SECONDS from the log."""

    _instance = None

    def __init__(self):
        self._log_pos = 0
        self._last_event_desc = None   # human-readable text for the last event
        self._last_event_at = 0.0      # when that event was seen
        self._init_log_position()

    @classmethod
    def instance(cls):
        if cls._instance is None:
            cls._instance = cls()
        return cls._instance

    def _init_log_position(self):
        # Start from the end of the existing log so a Nautilus restart does not
        # replay the whole history as if it had just happened.
        try:
            self._log_pos = os.path.getsize(LOG_PATH)
        except OSError:
            self._log_pos = 0

    def poll(self):
        """Read new log lines. Returns True when the label needs updating."""
        try:
            size = os.path.getsize(LOG_PATH)
        except OSError:
            return False  # log not created yet: the service has never run

        if size < self._log_pos:
            # Rotated or truncated: start over.
            self._log_pos = 0

        changed = False
        if size > self._log_pos:
            with open(LOG_PATH, "r", errors="replace") as handle:
                handle.seek(self._log_pos)
                new_lines = handle.read()
                self._log_pos = handle.tell()
            changed = self._process_lines(new_lines) or changed

        # Even without new lines the label has to expire on its own.
        if self._last_event_desc and (time.time() - self._last_event_at) > ACTIVITY_WINDOW_SECONDS:
            self._last_event_desc = None
            changed = True

        return changed

    def _process_lines(self, text):
        changed = False
        now = time.time()
        for line in text.splitlines():
            match = RE_LIST_START.search(line)
            if match:
                self._record(f"listing: {_display_name(match.group(1))}", now)
                changed = True
                continue

            match = RE_LIST_DONE.search(line)
            if match:
                path, count = match.group(1), match.group(2)
                self._record(f"{_display_name(path)}: {count} items", now)
                changed = True
                continue

            match = RE_HYDRATE_START.search(line)
            if match:
                self._record(f"downloading: {_display_name(match.group(1))}", now)
                changed = True
                continue

            match = RE_HYDRATE_DONE.search(line)
            if match:
                self._record(f"{_display_name(match.group(1))} downloaded", now)
                changed = True
                continue

            match = RE_TIMEOUT_SKIP.search(line)
            if match:
                # Only reachable under crawl_mode: full, kept so the message is
                # still explained rather than silently ignored.
                self._record(f"slow folder, skipped for now: {_display_name(match.group(1))}", now)
                changed = True
                continue

        return changed

    def _record(self, description, when):
        self._last_event_desc = description
        self._last_event_at = when

    @property
    def status_label_suffix(self):
        """Suffix to append to the "iCloud" bookmark, empty when idle."""
        if not self._last_event_desc:
            return ""
        return f" ({self._last_event_desc})"

    @property
    def status_menu_text(self):
        if not self._last_event_desc:
            return "iCloud: no recent activity"
        return f"iCloud: {self._last_event_desc}"


def _rewrite_bookmark_label(suffix):
    """Update only the iCloud line in ~/.config/gtk-3.0/bookmarks, leaving any
    other custom label in the file untouched."""
    mount_uri = "file://" + GLib.uri_escape_string(mount_dir(), "/", False)
    try:
        with open(BOOKMARKS_FILE, "r") as handle:
            lines = handle.readlines()
    except FileNotFoundError:
        lines = []

    new_lines = []
    found = False
    for line in lines:
        if line.rstrip("\n").startswith(mount_uri):
            found = True
            new_lines.append(f"{mount_uri} iCloud{suffix}\n")
        else:
            new_lines.append(line)

    if not found:
        new_lines.append(f"{mount_uri} iCloud{suffix}\n")

    os.makedirs(os.path.dirname(BOOKMARKS_FILE), exist_ok=True)
    with open(BOOKMARKS_FILE, "w") as handle:
        handle.writelines(new_lines)


class ICloudStatusExtension(GObject.GObject, Nautilus.MenuProvider, Nautilus.InfoProvider):
    # Nautilus instantiates providers more than once; one poll timer is enough.
    _timer_started = False

    def __init__(self):
        super().__init__()
        self._monitor = ICloudStatusMonitor.instance()
        if not ICloudStatusExtension._timer_started:
            ICloudStatusExtension._timer_started = True
            GLib.timeout_add_seconds(POLL_SECONDS, self._on_tick)

    def _on_tick(self):
        try:
            if self._monitor.poll():
                _rewrite_bookmark_label(self._monitor.status_label_suffix)
        except Exception:
            # Never let a transient error kill the timer: it would stop all
            # status updates until Nautilus is restarted.
            pass
        return True  # keep repeating

    # --- Nautilus.MenuProvider ---------------------------------------
    def get_file_items(self, files):
        return []

    def get_background_items(self, current_folder):
        item = Nautilus.MenuItem(
            name="ICloudStatusExtension::status",
            label="iCloud sync status…",
            tip="Show the last known icloud-linux activity",
        )
        item.connect("activate", self._show_status, current_folder)
        return [item]

    def _show_status(self, menu_item, current_folder):
        # A desktop notification rather than a dedicated GTK dialog: enough to
        # answer "what is it doing right now" without building a UI for it.
        try:
            notification = Gio.Notification.new("iCloud")
            notification.set_body(self._monitor.status_menu_text)
            app = Gio.Application.get_default()
            if app is not None:
                app.send_notification("icloud-status", notification)
        except Exception:
            pass  # without an active GApplication there is nothing to notify

    # --- Nautilus.InfoProvider ----------------------------------------
    def update_file_info(self, file):
        return Nautilus.OperationResult.COMPLETE
