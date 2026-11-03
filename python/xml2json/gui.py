"""GTK 3 front end. Run with: python3 -m xml2json.gui [file.xml]

Picking a file only counts its lines (a raw byte scan) and peeks at the first
few elements to suggest a record tag. The XML is never loaded or displayed,
so even very large files are ready to convert almost at once.
"""

from __future__ import annotations

import os
import sys
import threading
import time
import xml.etree.ElementTree as ET

import gi

gi.require_version("Gtk", "3.0")
from gi.repository import Gio, GLib, Gtk  # noqa: E402

from .core import Cancelled, convert, count_lines, guess_record_tag  # noqa: E402

UPDATE_INTERVAL = 0.1  # seconds between progress updates sent to the UI


def human_size(size: int) -> str:
    value = float(size)
    for unit in ("B", "KB", "MB", "GB"):
        if value < 1024 or unit == "GB":
            return f"{value:,.0f} {unit}" if unit == "B" else f"{value:,.1f} {unit}"
        value /= 1024
    return f"{size} B"


class Window(Gtk.ApplicationWindow):
    def __init__(self, app: Gtk.Application) -> None:
        super().__init__(application=app, title="XML to JSON")
        self.set_default_size(560, -1)
        self.set_border_width(16)

        self.input_path: str | None = None
        self.input_size = 0
        self.cancel = threading.Event()
        self.worker: threading.Thread | None = None
        self.scan_id = 0  # ignores results of a scan for a file no longer picked
        self.auto_tag = "part"  # last tag filled in by us, not typed by the user
        self.overwrite_ok: str | None = None  # output already confirmed in Save dialog

        grid = Gtk.Grid(column_spacing=8, row_spacing=10)
        self.add(grid)

        def label(text: str, row: int) -> None:
            widget = Gtk.Label(label=text, xalign=1)
            grid.attach(widget, 0, row, 1, 1)

        label("Input", 0)
        self.input_entry = Gtk.Entry(hexpand=True, editable=False)
        self.input_entry.set_placeholder_text("Choose an XML file")
        grid.attach(self.input_entry, 1, 0, 1, 1)
        self.input_button = Gtk.Button(label="Browse…")
        self.input_button.connect("clicked", self.on_browse_input)
        grid.attach(self.input_button, 2, 0, 1, 1)

        label("File", 1)
        self.info_label = Gtk.Label(label="–", xalign=0, selectable=True)
        grid.attach(self.info_label, 1, 1, 2, 1)

        label("Record tag", 2)
        self.tag_entry = Gtk.Entry(text="part")
        self.tag_entry.set_tooltip_text(
            "Each element with this name becomes one JSON record"
        )
        grid.attach(self.tag_entry, 1, 2, 2, 1)

        label("Output", 3)
        self.output_entry = Gtk.Entry(hexpand=True)
        grid.attach(self.output_entry, 1, 3, 1, 1)
        self.output_button = Gtk.Button(label="Browse…")
        self.output_button.connect("clicked", self.on_browse_output)
        grid.attach(self.output_button, 2, 3, 1, 1)

        self.ndjson_check = Gtk.CheckButton(
            label="NDJSON (one object per line instead of one array)"
        )
        self.ndjson_check.connect("toggled", self.on_ndjson_toggled)
        grid.attach(self.ndjson_check, 1, 4, 2, 1)

        self.progress = Gtk.ProgressBar(show_text=True, text="")
        grid.attach(self.progress, 0, 5, 3, 1)

        self.status_label = Gtk.Label(label="", xalign=0, wrap=True, selectable=True)
        grid.attach(self.status_label, 0, 6, 3, 1)

        buttons = Gtk.Box(spacing=8, halign=Gtk.Align.END)
        self.cancel_button = Gtk.Button(label="Cancel", sensitive=False)
        self.cancel_button.connect("clicked", lambda _b: self.cancel.set())
        self.convert_button = Gtk.Button(label="Convert", sensitive=False)
        self.convert_button.get_style_context().add_class("suggested-action")
        self.convert_button.connect("clicked", self.on_convert)
        buttons.pack_start(self.cancel_button, False, False, 0)
        buttons.pack_start(self.convert_button, False, False, 0)
        grid.attach(buttons, 0, 7, 3, 1)

        self.connect("delete-event", self.on_close)

    # ---- file selection -------------------------------------------------

    def on_browse_input(self, _button: Gtk.Button) -> None:
        dialog = Gtk.FileChooserNative.new(
            "Choose XML file", self, Gtk.FileChooserAction.OPEN, "_Open", "_Cancel"
        )
        xml_filter = Gtk.FileFilter()
        xml_filter.set_name("XML files")
        xml_filter.add_pattern("*.xml")
        xml_filter.add_pattern("*.XML")
        dialog.add_filter(xml_filter)
        all_filter = Gtk.FileFilter()
        all_filter.set_name("All files")
        all_filter.add_pattern("*")
        dialog.add_filter(all_filter)
        if dialog.run() == Gtk.ResponseType.ACCEPT:
            self.load_input(dialog.get_filename())
        dialog.destroy()

    def on_browse_output(self, _button: Gtk.Button) -> None:
        dialog = Gtk.FileChooserNative.new(
            "Save JSON as", self, Gtk.FileChooserAction.SAVE, "_Save", "_Cancel"
        )
        dialog.set_do_overwrite_confirmation(True)
        current = self.output_entry.get_text()
        if current:
            dialog.set_current_folder(os.path.dirname(current) or ".")
            dialog.set_current_name(os.path.basename(current))
        if dialog.run() == Gtk.ResponseType.ACCEPT:
            self.output_entry.set_text(dialog.get_filename())
            self.overwrite_ok = dialog.get_filename()
        dialog.destroy()

    def suggested_output(self) -> str:
        assert self.input_path is not None
        base, _ext = os.path.splitext(self.input_path)
        return base + (".ndjson" if self.ndjson_check.get_active() else ".json")

    def on_ndjson_toggled(self, _check: Gtk.CheckButton) -> None:
        if not self.input_path:
            return
        current = self.output_entry.get_text()
        base, ext = os.path.splitext(current)
        if ext in (".json", ".ndjson"):
            self.output_entry.set_text(
                base + (".ndjson" if self.ndjson_check.get_active() else ".json")
            )

    def load_input(self, path: str) -> None:
        try:
            size = os.path.getsize(path)
        except OSError as error:
            self.show_status(f"Cannot open file: {error}", error=True)
            return

        self.input_path = path
        self.input_size = size
        self.input_entry.set_text(path)
        self.output_entry.set_text(self.suggested_output())
        self.info_label.set_text(f"{human_size(size)} · counting lines…")
        self.progress.set_fraction(0)
        self.progress.set_text("")
        self.show_status("")
        self.convert_button.set_sensitive(True)

        self.scan_id += 1
        scan_id = self.scan_id
        threading.Thread(
            target=self.scan_worker, args=(path, size, scan_id), daemon=True
        ).start()

    def scan_worker(self, path: str, size: int, scan_id: int) -> None:
        started = time.monotonic()
        try:
            lines = count_lines(path)
        except OSError as error:
            GLib.idle_add(self.scan_failed, scan_id, str(error))
            return
        seconds = time.monotonic() - started
        tag = guess_record_tag(path)
        GLib.idle_add(self.scan_done, scan_id, size, lines, seconds, tag)

    def scan_done(
        self, scan_id: int, size: int, lines: int, seconds: float, tag: str | None
    ) -> bool:
        if scan_id != self.scan_id:
            return False
        self.info_label.set_text(
            f"{human_size(size)} · {lines:,} lines (counted in {seconds:.2f} s)"
        )
        current = self.tag_entry.get_text().strip()
        user_typed = current not in ("", self.auto_tag)
        if tag and tag != current and not user_typed and not self.is_running():
            self.auto_tag = tag
            self.tag_entry.set_text(tag)
            self.show_status(f"Record tag set to <{tag}>, the first repeating element in the file. Change it if needed.")
        return False

    def scan_failed(self, scan_id: int, message: str) -> bool:
        if scan_id == self.scan_id:
            self.info_label.set_text(f"{human_size(self.input_size)} · could not count lines")
            self.show_status(message, error=True)
        return False

    # ---- conversion -----------------------------------------------------

    def is_running(self) -> bool:
        return self.worker is not None and self.worker.is_alive()

    def on_convert(self, _button: Gtk.Button) -> None:
        if not self.input_path:
            return
        tag = self.tag_entry.get_text().strip()
        output = self.output_entry.get_text().strip()
        if not tag:
            self.show_status("Enter a record tag.", error=True)
            return
        if not output:
            self.show_status("Choose an output file.", error=True)
            return
        if os.path.abspath(output) == os.path.abspath(self.input_path):
            self.show_status("Output must be a different file from the input.", error=True)
            return
        already_confirmed = output == self.overwrite_ok
        self.overwrite_ok = None  # ask again next time, the file may be a good result
        if os.path.exists(output) and not already_confirmed:
            if not self.confirm_overwrite(output):
                return

        self.cancel.clear()
        self.set_running(True)
        self.progress.set_fraction(0)
        self.progress.set_text("0 %")
        self.show_status("Converting…")
        self.worker = threading.Thread(
            target=self.convert_worker,
            args=(self.input_path, output, tag, self.ndjson_check.get_active()),
            daemon=True,
        )
        self.worker.start()

    def confirm_overwrite(self, path: str) -> bool:
        dialog = Gtk.MessageDialog(
            transient_for=self,
            modal=True,
            message_type=Gtk.MessageType.QUESTION,
            buttons=Gtk.ButtonsType.OK_CANCEL,
            text="Replace existing file?",
        )
        dialog.format_secondary_text(f"{path} already exists.")
        response = dialog.run()
        dialog.destroy()
        return response == Gtk.ResponseType.OK

    def convert_worker(self, source: str, output: str, tag: str, ndjson: bool) -> None:
        size = max(self.input_size, 1)
        started = time.monotonic()
        last_update = 0.0

        def progress(bytes_read: int, records: int) -> None:
            nonlocal last_update
            now = time.monotonic()
            if now - last_update >= UPDATE_INTERVAL:
                last_update = now
                GLib.idle_add(self.update_progress, bytes_read / size, records)

        try:
            count = convert(source, output, tag, ndjson, progress, self.cancel)
        except Cancelled:
            GLib.idle_add(self.finish, "Cancelled. No output was written.", True, None)
        except ET.ParseError as error:
            GLib.idle_add(self.finish, f"XML error: {error}", True, None)
        except (OSError, ValueError) as error:
            GLib.idle_add(self.finish, f"Error: {error}", True, None)
        except Exception as error:  # keep the window usable whatever happens
            GLib.idle_add(self.finish, f"Unexpected error: {error!r}", True, None)
        else:
            seconds = time.monotonic() - started
            GLib.idle_add(
                self.finish,
                f"Wrote {count:,} records to {output} in {seconds:.1f} s.",
                False,
                count,
            )

    def update_progress(self, fraction: float, records: int) -> bool:
        if self.is_running():
            fraction = min(fraction, 1.0)
            self.progress.set_fraction(fraction)
            self.progress.set_text(f"{fraction:.0%} · {records:,} records")
        return False

    def finish(self, message: str, error: bool, count: int | None) -> bool:
        self.worker = None
        self.set_running(False)
        if count is not None:
            self.progress.set_fraction(1)
            self.progress.set_text(f"100% · {count:,} records")
        else:
            self.progress.set_text("")
        self.show_status(message, error=error)
        return False

    def set_running(self, running: bool) -> None:
        for widget in (
            self.input_button,
            self.output_button,
            self.output_entry,
            self.tag_entry,
            self.ndjson_check,
            self.convert_button,
        ):
            widget.set_sensitive(not running)
        self.cancel_button.set_sensitive(running)

    def show_status(self, message: str, error: bool = False) -> None:
        if error:
            self.status_label.set_markup(
                f'<span foreground="#d33">{GLib.markup_escape_text(message)}</span>'
            )
        else:
            self.status_label.set_text(message)

    def on_close(self, *_args: object) -> bool:
        # Stop the worker so it deletes its temporary file before we exit.
        if self.is_running():
            self.cancel.set()
            assert self.worker is not None
            self.worker.join(timeout=10)
        return False


class Application(Gtk.Application):
    def __init__(self) -> None:
        super().__init__(
            application_id="io.github.xml2json.Converter",
            # Every launch gets its own window, so a file passed on the command
            # line is never dropped by an already running instance.
            flags=Gio.ApplicationFlags.NON_UNIQUE,
        )
        self.window: Window | None = None
        self.initial_path = sys.argv[1] if len(sys.argv) > 1 else None

    def do_activate(self) -> None:
        if self.window is None:
            self.window = Window(self)
            if self.initial_path:
                self.window.load_input(os.path.abspath(self.initial_path))
        self.window.show_all()
        self.window.present()


def main() -> int:
    return Application().run([sys.argv[0]])


if __name__ == "__main__":
    sys.exit(main())
