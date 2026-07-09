#!/usr/bin/env python3
"""katerm — LodaxOS Kernel Access Terminal

Connects to QEMU's COM2 serial port over TCP and provides an interactive
CLI for the kernel's built-in terminal.  Built on prompt_toolkit for
proper input handling, history, tab completion, and a status bar.

Usage:
    python katerm_client.py [host] [port]
"""

import argparse
import sys
import threading
from enum import Enum

from prompt_toolkit import PromptSession
from prompt_toolkit.completion import WordCompleter
from prompt_toolkit.formatted_text import HTML
from prompt_toolkit.history import InMemoryHistory
from prompt_toolkit.key_binding import KeyBindings

DEFAULT_HOST = "127.0.0.1"
DEFAULT_PORT = 4444
RECONNECT_BASE = 1.0
RECONNECT_MAX = 30.0
BOOT_CMD = b"bootkaterm\n"

# Global reference to the active connection for key bindings.
_active_conn = None


# ---------------------------------------------------------------------------
# Command definitions (mirrors kernel termcmds/mod.rs)
# ---------------------------------------------------------------------------

COMMANDS = {
    "cm":           "(mode)        Switch command mode",
    "listmodes":    "()            List all available modes",
    "tui":          "()            Enter TUI interactive inspector",
    "cli":          "()            Exit TUI, return to CLI",
    "help":         "(topic)       Show detailed help (memory, tasks, dump, ...)",
    "clear":        "()            Clear terminal screen",
    "echo":         "(msg)         Echo a message",
    "symbols":      "(filter)      List kernel symbols",
    "lookup":       "(addr)        Resolve address to symbol + file:line",
    "disasm":       "(addr, n)     Disassemble x86-64 instructions",
    "dump":         "(addr, len)   Hex dump memory",
    "peek":         "(addr)        Read 64-bit from memory",
    "poke":         "(addr, val)   Write 64-bit to physical memory",
    "meminfo":      "()            Physical memory stats",
    "translate":    "(virt_addr)   Virtual → physical address translation",
    "pte":          "(virt_addr)   Show page table entry with flags",
    "vmas":         "()            List kernel virtual memory areas",
    "pagestat":     "()            Physical page allocator stats",
    "ps":           "()            List all gangs (processes)",
    "trace":        "(vcpu_id)     Full register dump of a vCPU",
    "vcpus":        "()            List all allocated vCPUs",
    "loadavg":      "()            Per-CPU task counts and load",
    "rq":           "(cpu)         Peek at a CPU's ready queue",
    "slabstat":     "()            Kernel heap slab allocator stats",
    "drivers":      "()            List registered GDF drivers",
    "services":     "()            List running services",
    "drv_call":     "(name,cmd)    Send command to a driver",
    "cpuinfo":      "()            List online CPUs",
    "lapic":        "()            Show LAPIC registers",
    "ioapic_dump":  "(index)       Dump IOAPIC redirection entries",
    "irq":          "()            Show IOAPIC interrupt routing table",
    "irqstat":      "()            Per-vector interrupt/exception counts",
    "ticks":        "()            Show timer tick counts",
    "dumpcpu":      "(cpu)         Dump CPU state",
    "dumpremote":   "(cpu)         Force register dump on remote CPU via IPI",
    "dumpall":      "()            Dump ALL online CPUs via IPI",
    "read":         "(port)        Read byte from I/O port",
    "write":        "(port, val)   Write byte to I/O port",
    "reboot":       "()            Reboot the system",
    "set":          "(vcpu,reg,val) Modify a saved vCPU register",
    "bt":           "(vcpu_id)     Backtrace from vCPU's saved frame",
    "stack":        "(vcpu_id, n)  Hex dump n quadwords from vCPU stack",
    "read16":       "(port)        Read 16-bit from I/O port",
    "read32":       "(port)        Read 32-bit from I/O port",
    "write16":      "(port, val)   Write 16-bit to I/O port",
    "write32":      "(port, val)   Write 32-bit to I/O port",
    "cli":          "()            Disable interrupts (RFLAGS.IF=0)",
    "sti":          "()            Enable interrupts (RFLAGS.IF=1)",
    "rdmsr":        "(msr)         Read Model-Specific Register",
    "wrmsr":        "(msr, val)    Write Model-Specific Register",
    "invlpg":       "(addr)        Flush TLB entry",
    "break":        "(addr)        Set software breakpoint",
    "del":          "(index)       Delete breakpoint by index",
    "bpl":          "()            List all breakpoints",
    "cont":         "()            Continue vCPU after breakpoint",
    "step":         "(vcpu_id)     Single-step vCPU one instruction",
    "watch":        "(addr)        Set hardware execution breakpoint",
    "poke_code":    "(addr, b0..)  Write raw machine code bytes",
    "load_code":    "(vcpu,dst,src,len) Copy code bytes in vCPU space",
    "exec_page":    "(vcpu, phys)  Map physical page as executable",
    "jump":         "(vcpu, addr)  Set vCPU RIP and resume",
    "force_next":   "(cpu)         Force CPU to reschedule",
    "recover":      "(cpu)         Recover CPU from hard fault",
    "map":          "(vcpu, addr)  Full page table walk for vCPU address",
    "hlt":          "(vcpu)        Immediately halt a vCPU",
}


# ---------------------------------------------------------------------------
# SerialConnection — thread-safe TCP client with auto-reconnect
# ---------------------------------------------------------------------------

class State(Enum):
    DISCONNECTED = "disconnected"
    CONNECTING = "connecting"
    CONNECTED = "connected"


class SerialConnection:
    """TCP client for QEMU's COM2 serial port.

    Runs a background thread that receives data and prints it to stdout.
    Reconnects automatically with exponential backoff on disconnection.
    """

    def __init__(self, host: str, port: int):
        self.host = host
        self.port = port
        self._sock = None
        self._lock = threading.Lock()
        self._state = State.DISCONNECTED
        self._stop = threading.Event()
        self._rx_thread = None
        self._attempt = 0
        self._state_lock = threading.Lock()
        self.on_connected = None  # callback: () -> None

    @property
    def state(self) -> State:
        with self._state_lock:
            return self._state

    def _set_state(self, s: State):
        with self._state_lock:
            self._state = s

    def connect(self):
        """Start the connection manager (non-blocking)."""
        self._stop.clear()
        self._rx_thread = threading.Thread(target=self._connection_loop, daemon=True)
        self._rx_thread.start()

    def disconnect(self):
        """Stop everything."""
        self._stop.set()
        with self._lock:
            if self._sock:
                try:
                    self._sock.close()
                except OSError:
                    pass
                self._sock = None
        self._set_state(State.DISCONNECTED)

    def send(self, data: bytes) -> bool:
        """Send bytes to the kernel.  Returns False on failure."""
        with self._lock:
            if self._sock and self._state == State.CONNECTED:
                try:
                    self._sock.sendall(data)
                    return True
                except OSError:
                    self._sock = None
        self._set_state(State.DISCONNECTED)
        return False

    # -- internal -----------------------------------------------------------

    def _connection_loop(self):
        """Background thread: connect, receive, reconnect."""
        while not self._stop.is_set():
            self._set_state(State.CONNECTING)
            self._attempt += 1

            try:
                import socket
                sock = socket.create_connection((self.host, self.port), timeout=5)
                # Keep connection alive and increase buffers for large kernel output.
                sock.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 1 << 16)
                sock.setsockopt(socket.SOL_SOCKET, socket.SO_SNDBUF, 1 << 16)
                sock.setsockopt(socket.SOL_SOCKET, socket.SO_KEEPALIVE, 1)
                sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
                sock.settimeout(None)  # blocking reads in receive loop
            except (socket.timeout, ConnectionRefusedError, OSError):
                delay = min(RECONNECT_BASE * (2 ** (self._attempt - 1)), RECONNECT_MAX)
                self._print_status(f"Connecting... (attempt {self._attempt}, retry in {delay:.0f}s)")
                self._stop.wait(delay)
                continue

            with self._lock:
                self._sock = sock
            self._set_state(State.CONNECTED)
            self._attempt = 0
            self._print_status("Connected")

            if self.on_connected:
                self.on_connected()

            self._receive_loop(sock)

            with self._lock:
                if self._sock is sock:
                    self._sock = None
            try:
                sock.close()
            except OSError:
                pass

    def _receive_loop(self, sock):
        """Read data from socket and print to stdout."""
        while not self._stop.is_set():
            try:
                data = sock.recv(65536)
                if not data:
                    break
                sys.stdout.buffer.write(data)
                sys.stdout.buffer.flush()
            except OSError:
                break
        self._set_state(State.DISCONNECTED)

    def _print_status(self, msg: str):
        """Print a status line (kernel output goes to stdout, so we use stderr
        for status to avoid interleaving).  Then write to stdout as well so
        the user sees it in the terminal flow."""
        sys.stdout.write(f"\n--- {msg} ---\n")
        sys.stdout.flush()


# ---------------------------------------------------------------------------
# Tab completer
# ---------------------------------------------------------------------------

_completer = WordCompleter(
    list(COMMANDS.keys()),
    ignore_case=True,
    meta_dict=COMMANDS,
)


# ---------------------------------------------------------------------------
# Key bindings
# ---------------------------------------------------------------------------

_kb = KeyBindings()


@_kb.add("c-c")
def _send_interrupt(event):
    """Send Ctrl+C (0x03) to the kernel instead of exiting."""
    if _active_conn:
        _active_conn.send(b"\x03")


@_kb.add("c-d")
def _exit(event):
    """Ctrl+D exits the client."""
    event.app.exit()


# ---------------------------------------------------------------------------
# Prompt message and toolbar
# ---------------------------------------------------------------------------

def _make_message(conn: SerialConnection):
    """Return a callable that produces the current prompt string."""
    def _message():
        if conn.state == State.CONNECTED:
            return HTML("<b><ansiblue>:</ansiblue></b> ")
        elif conn.state == State.CONNECTING:
            return HTML("<b><ansiyellow>Connecting...</ansiyellow></b> ")
        else:
            return HTML("<b><ansired>Disconnected</ansired></b> ")
    return _message


def _make_toolbar(conn: SerialConnection):
    """Return a callable that produces the status bar content."""
    def _toolbar():
        state = conn.state
        if state == State.CONNECTED:
            return HTML(
                f" <ansigreen>Connected</ansigreen> "
                f"| {conn.host}:{conn.port} "
                f"| Ctrl+C=interrupt  Ctrl+D=exit  Tab=complete"
            )
        elif state == State.CONNECTING:
            return HTML(
                f" <ansiyellow>Connecting</ansiyellow> "
                f"| attempt {conn._attempt} "
                f"| Ctrl+D to cancel"
            )
        else:
            return HTML(
                f" <ansired>Disconnected</ansired> "
                f"| Ctrl+D to exit"
            )
    return _toolbar


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def main():
    parser = argparse.ArgumentParser(
        description="katerm — LodaxOS Kernel Access Terminal"
    )
    parser.add_argument(
        "host", nargs="?", default=DEFAULT_HOST,
        help=f"QEMU serial host (default: {DEFAULT_HOST})",
    )
    parser.add_argument(
        "port", nargs="?", type=int, default=DEFAULT_PORT,
        help=f"QEMU serial port (default: {DEFAULT_PORT})",
    )
    args = parser.parse_args()

    global _active_conn
    conn = SerialConnection(args.host, args.port)
    _active_conn = conn

    # Auto-send bootkaterm once on first connection so the kernel activates katerm.
    # Don't resend on reconnect — katerm is already booted.
    _boot_sent = [False]
    def _on_first_connect():
        if not _boot_sent[0]:
            _boot_sent[0] = True
            conn.send(BOOT_CMD)
    conn.on_connected = _on_first_connect

    conn.connect()

    session = PromptSession(
        message=_make_message(conn),
        completer=_completer,
        complete_while_typing=True,
        history=InMemoryHistory(),
        key_bindings=_kb,
        bottom_toolbar=_make_toolbar(conn),
        enable_open_in_editor=False,
        mouse_support=False,
    )

    # Stash connection reference on session so key bindings can access it
    # (now uses module-level _active_conn instead)

    try:
        while True:
            try:
                user_input = session.prompt()
            except KeyboardInterrupt:
                # PromptSession raises this on unhandled Ctrl+C;
                # our binding sends \x03, so this shouldn't fire normally.
                continue
            except EOFError:
                # Ctrl+D
                break

            if user_input is None:
                # Ctrl+D or prompt cancelled
                break

            if not user_input.strip():
                continue

            if not conn.send((user_input + "\n").encode()):
                sys.stdout.write("\n--- Not connected ---\n")
                sys.stdout.flush()

    except KeyboardInterrupt:
        pass
    finally:
        conn.disconnect()


if __name__ == "__main__":
    main()
