#!/usr/bin/env python3
"""katerm — LodaxOS Kernel Access Terminal Client

PyQt5 GUI terminal emulator that connects to the kernel's COM2 debug serial
port over TCP (QEMU -serial tcp::4444).

Usage:
    python katerm_client.py
"""

import socket
import sys
import re
from collections import deque
from threading import Thread, Lock, Event

from PyQt5.QtCore import (
    Qt, QTimer, pyqtSignal, QObject, QRect
)
from PyQt5.QtWidgets import (
    QApplication, QMainWindow, QWidget, QVBoxLayout, QHBoxLayout,
    QTextEdit, QLineEdit, QPushButton, QStatusBar, QLabel,
    QMessageBox, QMenuBar, QAction, QFileDialog
)
from PyQt5.QtGui import QFont, QTextCursor, QIcon, QKeyEvent

# ── Configuration ───────────────────────────────────────────────────
DEFAULT_HOST = "127.0.0.1"
DEFAULT_PORT = 4444
BUFFER_SIZE = 4096
MAX_HISTORY = 200
RECONNECT_DELAY_MS = 3000

# ── Katerm commands for autocomplete ────────────────────────────────
KATERM_COMMANDS = [
    "help(", "clear()", "echo(",
    # Symbols
    "symbols(", "lookup(", "disasm(",
    # Memory
    "dump(", "peek(", "poke(", "meminfo()", "translate(", "pte(", "vmas()",
    # Tasks
    "ps()", "trace(", "vcpus()",
    # Scheduler
    "loadavg()", "rq(",
    # Drivers
    "drivers()", "services()", "drv_call(",
    # Hardware
    "cpuinfo()", "lapic()", "ioapic_dump(", "irq()", "ticks()", "dumpcpu(",
    # I/O
    "read(", "write(",
    # System
    "reboot()",
]


# ══════════════════════════════════════════════════════════════════════
# VT100 → HTML parser
# ══════════════════════════════════════════════════════════════════════

_VT100_SGR_RE = re.compile(r"\x1b\[([\d;]*)m")
_VT100_CSI_RE = re.compile(r"\x1b\[[0-9;]*[A-HJ-KRSTdfhlnsu]")
_VT100_OSC_RE = re.compile(r"\x1b\].*?(\x1b\\|\x07)")
_VT100_OTHER_RE = re.compile(r"\x1b[\[\]()][^a-zA-Z]*[a-zA-Z]|\x1b.")

_STYLES = {
    0: ("font-weight:normal;color:inherit;background:inherit", False),
    1: ("font-weight:bold;", False),
    4: ("text-decoration:underline;", False),
    7: ("color:#1e1e1e;background:#d4d4d4;", True),  # reverse
}

_FG_COLORS = {
    30: "color:#1e1e1e;", 31: "color:#cd3131;", 32: "color:#0dbc79;",
    33: "color:#e5e510;", 34: "color:#2472c8;", 35: "color:#bc3fbc;",
    36: "color:#11a8cd;", 37: "color:#e5e5e5;",
    90: "color:#666666;", 91: "color:#f14c4c;", 92: "color:#23d18b;",
    93: "color:#f5f543;", 94: "color:#3b8eea;", 95: "color:#d670d6;",
    96: "color:#29b8db;", 97: "color:#ffffff;",
}

_BG_COLORS = {
    40: "background:#1e1e1e;", 41: "background:#cd3131;",
    42: "background:#0dbc79;", 43: "background:#e5e510;",
    44: "background:#2472c8;", 45: "background:#bc3fbc;",
    46: "background:#11a8cd;", 47: "background:#e5e5e5;",
    100: "background:#666666;", 101: "background:#f14c4c;",
    102: "background:#23d18b;", 103: "background:#f5f543;",
    104: "background:#3b8eea;", 105: "background:#d670d6;",
    106: "background:#29b8db;", 107: "background:#ffffff;",
}


def strip_vt100(text: str) -> str:
    """Remove all VT100 escape sequences, keeping visible text."""
    t = _VT100_OSC_RE.sub("", text)
    t = _VT100_CSI_RE.sub("", t)
    t = _VT100_OTHER_RE.sub("", t)
    return t


def vt100_to_html(text: str) -> str:
    """Convert VT100-encoded text to Qt rich-text HTML."""
    css = ["color:#d4d4d4;", "background:#1e1e1e;"]
    reverse = False
    parts = []
    pos = 0

    for m in _VT100_SGR_RE.finditer(text):
        start, end = m.start(), m.end()
        if start > pos:
            raw = text[pos:start]
            raw = raw.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")
            raw = raw.replace("\n", "<br>").replace(" ", "&nbsp;").replace("\t", "&nbsp;&nbsp;&nbsp;&nbsp;")
            parts.append(raw)
        pos = end

        params = m.group(1)
        if not params:
            css = ["color:#d4d4d4;", "background:#1e1e1e;"]
            reverse = False
        else:
            for p in params.split(";"):
                try:
                    code = int(p)
                except ValueError:
                    continue
                if code == 0:
                    css = ["color:#d4d4d4;", "background:#1e1e1e;"]
                    reverse = False
                elif code in _STYLES:
                    style, rev = _STYLES[code]
                    if rev:
                        reverse = not reverse
                    else:
                        if "font-weight" in style:
                            css = [s for s in css if "font-weight" not in s] + [style]
                        elif "text-decoration" in style:
                            css = [s for s in css if "text-decoration" not in s] + [style]
                elif code in _FG_COLORS:
                    css = [s for s in css if "color:" not in s] + [_FG_COLORS[code]]
                elif code in _BG_COLORS:
                    css = [s for s in css if "background:" not in s] + [_BG_COLORS[code]]
                elif code >= 90 and code <= 97:
                    css = [s for s in css if "color:" not in s] + [_FG_COLORS.get(code, "")]
                elif code >= 100 and code <= 107:
                    css = [s for s in css if "background:" not in s] + [_BG_COLORS.get(code, "")]

    if pos < len(text):
        raw = text[pos:]
        raw = raw.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")
        raw = raw.replace("\n", "<br>").replace(" ", "&nbsp;").replace("\t", "&nbsp;&nbsp;&nbsp;&nbsp;")
        parts.append(raw)

    if reverse:
        css = [c for c in css if c not in ("color:#1e1e1e;", "background:#d4d4d4;")] + \
              ["color:#1e1e1e;", "background:#d4d4d4;"]

    inline = "".join(css)
    return f"<span style='{inline}'>{''.join(parts)}</span>"


# ══════════════════════════════════════════════════════════════════════
# TCP worker (runs in background thread)
# ══════════════════════════════════════════════════════════════════════

class RxSignal(QObject):
    data_received = pyqtSignal(bytes)
    disconnected = pyqtSignal()


class SerialClient:
    """Manages a non-blocking TCP connection to QEMU's COM2 port."""

    def __init__(self, host: str, port: int):
        self.host = host
        self.port = port
        self.sock: socket.socket | None = None
        self._rx_thread: Thread | None = None
        self._rx_signal = RxSignal()
        self._stop = Event()
        self._lock = Lock()
        self._connected = False

    # ── Signals ─────────────────────────────────────────────────────
    @property
    def data_received(self) -> pyqtSignal:
        return self._rx_signal.data_received

    @property
    def disconnected(self) -> pyqtSignal:
        return self._rx_signal.disconnected

    @property
    def is_connected(self) -> bool:
        return self._connected

    # ── Connection management ───────────────────────────────────────

    def connect(self) -> str | None:
        """Connect to the serial TCP port. Returns error string or None."""
        with self._lock:
            if self._connected:
                return None
            try:
                self.sock = socket.create_connection(
                    (self.host, self.port), timeout=5
                )
                self.sock.settimeout(None)
                self._connected = True
                self._stop.clear()
                self._rx_thread = Thread(target=self._rx_loop, daemon=True)
                self._rx_thread.start()
                return None
            except (socket.timeout, ConnectionRefusedError, OSError) as e:
                self._connected = False
                self.sock = None
                return str(e)

    def disconnect(self):
        with self._lock:
            self._stop.set()
            if self.sock:
                try:
                    self.sock.close()
                except OSError:
                    pass
                self.sock = None
            self._connected = False

    def send(self, data: bytes):
        """Send bytes on the socket (thread-safe)."""
        with self._lock:
            if self.sock and self._connected:
                try:
                    self.sock.sendall(data)
                except OSError:
                    self._connected = False
                    self.disconnected.emit()

    # ── Background receive loop ─────────────────────────────────────

    def _rx_loop(self):
        while not self._stop.is_set():
            try:
                data = self.sock.recv(BUFFER_SIZE)
                if not data:
                    break
                self._rx_signal.data_received.emit(data)
            except OSError:
                break
        with self._lock:
            self._connected = False
        self.disconnected.emit()


# ══════════════════════════════════════════════════════════════════════
# Command history
# ══════════════════════════════════════════════════════════════════════

class CommandHistory:
    """Stores and navigates previously-entered commands."""

    def __init__(self, maxlen: int = MAX_HISTORY):
        self._history: deque[str] = deque(maxlen=maxlen)
        self._index = -1
        self._saved = ""

    def push(self, cmd: str):
        cmd = cmd.strip()
        if cmd and (not self._history or self._history[-1] != cmd):
            self._history.append(cmd)
        self._index = len(self._history)

    def up(self, current: str = "") -> str | None:
        if not self._history:
            return None
        if self._index == len(self._history):
            self._saved = current
        if self._index > 0:
            self._index -= 1
            return self._history[self._index]
        return None

    def down(self, current: str = "") -> str | None:
        if self._index == len(self._history):
            return None
        self._index += 1
        if self._index >= len(self._history):
            self._index = len(self._history)
            return self._saved
        return self._history[self._index]


# ══════════════════════════════════════════════════════════════════════
# Terminal widget
# ══════════════════════════════════════════════════════════════════════

class TerminalDisplay(QTextEdit):
    """Read-only rich-text terminal output area."""

    def __init__(self, parent=None):
        super().__init__(parent)
        self.setReadOnly(True)
        self.setFont(QFont("Consolas", 10))
        self.setStyleSheet("""
            QTextEdit {
                background-color: #1e1e1e;
                color: #d4d4d4;
                border: none;
                padding: 4px;
                selection-background-color: #264f78;
            }
        """)
        self.setMinimumHeight(200)
        self._buffer = ""

    def append_html(self, html: str):
        cursor = self.textCursor()
        cursor.movePosition(QTextCursor.End)
        cursor.insertHtml(html)
        self.setTextCursor(cursor)
        self.ensureCursorVisible()

    def append_text(self, text: str):
        """Append raw text (VT100 sequences stripped for safety)."""
        cleaned = strip_vt100(text)
        cleaned = cleaned.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")
        cleaned = cleaned.replace("\n", "<br>").replace(" ", "&nbsp;")
        self.append_html(f"<span style='color:#d4d4d4;'>{cleaned}</span>")

    def append_vt100(self, text: str):
        """Append text with VT100 escape sequences rendered as HTML."""
        html = vt100_to_html(text)
        self.append_html(html)

    def clear_terminal(self):
        self.clear()

    def save_to_file(self, path: str):
        with open(path, "w", encoding="utf-8") as f:
            f.write(self.toPlainText())


# ══════════════════════════════════════════════════════════════════════
# Command input
# ══════════════════════════════════════════════════════════════════════

class CommandInput(QLineEdit):
    """QLineEdit with history navigation and autocomplete."""

    def __init__(self, parent=None):
        super().__init__(parent)
        self.setFont(QFont("Consolas", 10))
        self.setStyleSheet("""
            QLineEdit {
                background-color: #252526;
                color: #d4d4d4;
                border: 1px solid #3c3c3c;
                padding: 4px;
                selection-background-color: #264f78;
            }
            QLineEdit:focus {
                border: 1px solid #007acc;
            }
        """)
        self._history = CommandHistory()
        self._completion_index = -1
        self._completion_base = ""
        self.returnPressed.connect(self._on_return)

    def keyPressEvent(self, event: QKeyEvent):
        if event.key() == Qt.Key_Up:
            result = self._history.up(self.text())
            if result is not None:
                self.setText(result)
        elif event.key() == Qt.Key_Down:
            result = self._history.down(self.text())
            if result is not None:
                self.setText(result)
        elif event.key() == Qt.Key_Tab:
            self._autocomplete()
        else:
            self._completion_index = -1
            super().keyPressEvent(event)

    def _on_return(self):
        cmd = self.text().strip()
        if cmd:
            self._history.push(cmd)
        self.window().send_command(cmd)
        self.clear()

    def _autocomplete(self):
        text = self.text().strip()
        if not text:
            return
        if self._completion_index < 0:
            self._completion_base = text
            self._completion_index = 0

        matches = [c for c in KATERM_COMMANDS if c.startswith(self._completion_base)]
        if not matches:
            self._completion_index = -1
            return

        idx = self._completion_index % len(matches)
        self.setText(matches[idx])
        self._completion_index = idx + 1


# ══════════════════════════════════════════════════════════════════════
# Main window
# ══════════════════════════════════════════════════════════════════════

class KatermWindow(QMainWindow):
    def __init__(self):
        super().__init__()
        self._client = SerialClient(DEFAULT_HOST, DEFAULT_PORT)
        self._reconnect_timer = QTimer(self)
        self._reconnect_timer.setSingleShot(True)
        self._reconnect_timer.timeout.connect(self._try_reconnect)

        self._build_ui()
        self._connect_signals()

        self.setWindowTitle("katerm — LodaxOS Kernel Access Terminal")
        self.resize(900, 600)

        # Attempt initial connection
        self._toggle_connection()

    # ── UI construction ─────────────────────────────────────────────

    def _build_ui(self):
        central = QWidget()
        self.setCentralWidget(central)
        layout = QVBoxLayout(central)
        layout.setContentsMargins(0, 0, 0, 0)
        layout.setSpacing(0)

        # Terminal output
        self._terminal = TerminalDisplay()
        layout.addWidget(self._terminal, 1)

        # Input bar
        input_bar = QHBoxLayout()
        input_bar.setContentsMargins(4, 4, 4, 4)
        self._input = CommandInput(self)
        self._input.setPlaceholderText("Type a command…")
        self._send_btn = QPushButton("Send")
        self._send_btn.setStyleSheet("""
            QPushButton {
                background-color: #0e639c;
                color: white;
                border: none;
                padding: 6px 12px;
                min-width: 60px;
            }
            QPushButton:hover { background-color: #1177bb; }
            QPushButton:pressed { background-color: #094771; }
        """)
        self._conn_btn = QPushButton("Connect")
        self._conn_btn.setStyleSheet("""
            QPushButton {
                background-color: #1e1e1e;
                color: #d4d4d4;
                border: 1px solid #3c3c3c;
                padding: 6px 12px;
                min-width: 80px;
            }
            QPushButton:hover { background-color: #2a2a2a; }
        """)
        input_bar.addWidget(self._input, 1)
        input_bar.addWidget(self._send_btn)
        input_bar.addWidget(self._conn_btn)
        layout.addLayout(input_bar)

        # Status bar
        self._status = QStatusBar()
        self._status_label = QLabel("Disconnected")
        self._status_label.setStyleSheet("color:#888888;")
        self._status.addPermanentWidget(self._status_label)
        self.setStatusBar(self._status)

        # Menu bar
        menubar = self.menuBar()
        file_menu = menubar.addMenu("&File")
        save_action = QAction("&Save output…", self)
        save_action.setShortcut("Ctrl+S")
        save_action.triggered.connect(self._save_output)
        file_menu.addAction(save_action)

        clear_action = QAction("&Clear terminal", self)
        clear_action.setShortcut("Ctrl+L")
        clear_action.triggered.connect(self._terminal.clear_terminal)
        file_menu.addAction(clear_action)

        self._terminal.append_html(
            "<span style='color:#666666;'>"
            "katerm — connect to QEMU at "
            f"{DEFAULT_HOST}:{DEFAULT_PORT}, then type "
            "<span style='color:#569cd6;font-weight:bold;'>bootkaterm</span>"
            "</span><br>"
        )

    # ── Signal wiring ───────────────────────────────────────────────

    def _connect_signals(self):
        self._send_btn.clicked.connect(lambda: self._input._on_return())
        self._conn_btn.clicked.connect(self._toggle_connection)
        self._client.data_received.connect(self._on_data)
        self._client.disconnected.connect(self._on_disconnected)

    # ── Connection ──────────────────────────────────────────────────

    def _toggle_connection(self):
        if self._client.is_connected:
            self._client.disconnect()
            self._on_disconnected()
        else:
            self._connect()

    def _connect(self):
        err = self._client.connect()
        if err:
            self._terminal.append_html(
                f"<span style='color:#f14c4c;'>Connection failed: "
                f"{err}</span><br>"
            )
            self._status_label.setText(f"Error: {err}")
            self._status_label.setStyleSheet("color:#f14c4c;")
            self._reconnect_timer.start(RECONNECT_DELAY_MS)
        else:
            self._conn_btn.setText("Disconnect")
            self._status_label.setText(f"Connected to {DEFAULT_HOST}:{DEFAULT_PORT}")
            self._status_label.setStyleSheet("color:#0dbc79;")

    def _on_disconnected(self):
        self._conn_btn.setText("Reconnect")
        self._status_label.setText("Disconnected")
        self._status_label.setStyleSheet("color:#888888;")
        self._terminal.append_html(
            "<span style='color:#666666;'>Disconnected</span><br>"
        )
        if not self._reconnect_timer.isActive():
            self._reconnect_timer.start(RECONNECT_DELAY_MS)

    def _try_reconnect(self):
        if not self._client.is_connected:
            self._connect()

    # ── Data received from serial ───────────────────────────────────

    def _on_data(self, data: bytes):
        try:
            text = data.decode("utf-8", errors="replace")
        except Exception:
            text = data.decode("latin-1", errors="replace")
        self._terminal.append_vt100(text)

    # ── Send command ────────────────────────────────────────────────

    def send_command(self, cmd: str):
        if not cmd:
            return
        if not self._client.is_connected:
            self._terminal.append_html(
                "<span style='color:#f14c4c;'>Not connected</span><br>"
            )
            return
        data = (cmd + "\n").encode("utf-8")
        self._client.send(data)

    # ── File menu ───────────────────────────────────────────────────

    def _save_output(self):
        path, _ = QFileDialog.getSaveFileName(
            self, "Save terminal output", "katerm_output.txt",
            "Text files (*.txt);;All files (*)"
        )
        if path:
            try:
                self._terminal.save_to_file(path)
            except OSError as e:
                QMessageBox.warning(self, "Save error", str(e))

    # ── Window close ────────────────────────────────────────────────

    def closeEvent(self, event):
        self._client.disconnect()
        event.accept()


# ══════════════════════════════════════════════════════════════════════
# Entry point
# ══════════════════════════════════════════════════════════════════════

def main():
    app = QApplication(sys.argv)
    app.setStyle("Fusion")
    app.setStyleSheet("""
        QMainWindow, QWidget {
            background-color: #1e1e1e;
            color: #d4d4d4;
        }
        QMenuBar {
            background-color: #2d2d2d;
            color: #d4d4d4;
        }
        QMenuBar::item:selected {
            background-color: #094771;
        }
        QMenu {
            background-color: #2d2d2d;
            color: #d4d4d4;
            border: 1px solid #3c3c3c;
        }
        QMenu::item:selected {
            background-color: #094771;
        }
        QStatusBar {
            background-color: #007acc;
            color: white;
        }
    """)
    window = KatermWindow()
    window.show()
    sys.exit(app.exec_())


if __name__ == "__main__":
    main()
