pub mod input;
pub mod screen;
pub mod panels;

use crate::sync::SyncUnsafeCell;
use input::{InputParser, KeyEvent};
use screen::{Screen, Rect};
use panels::Panel;
use panels::tab_bar::TabBar;
use panels::status_bar::StatusBar;
use panels::output::OutputPane;
use panels::input_line::InputLine;
use panels::inspector::{InspectorContainer, InspectorPanel, tasks::TasksPanel, cpus::CpusPanel, memory::MemoryPanel, irqs::IrqsPanel};

const REFRESH_RATE: usize = 10;
const TAB_NAMES: &[&str] = &["Tasks", "CPUs", "Memory", "IRQs"];

pub static TUI_ACTIVE: SyncUnsafeCell<bool> = SyncUnsafeCell::new(false);

struct TuiState {
    screen: Screen,
    parser: InputParser,
    tick_counter: usize,
    active_panel: usize,
    inspector_tab: usize,
    inspector: InspectorContainer,
    output: OutputPane,
    input: InputLine,
    status: StatusBar,
    tabs: TabBar,
    initialized: bool,
}

static TUI_STATE: SyncUnsafeCell<Option<TuiState>> = SyncUnsafeCell::new(None);

fn get_state() -> Option<&'static mut TuiState> {
    unsafe { &mut *TUI_STATE.get() }.as_mut()
}

pub fn init() {
    unsafe {
        *TUI_ACTIVE.get() = false;
        *TUI_STATE.get() = None;
    }
}

pub fn enter_tui() {
    if unsafe { *TUI_ACTIVE.get() } {
        return;
    }

    unsafe {
        *TUI_ACTIVE.get() = true;
        *TUI_STATE.get() = Some(create_tui_state());
    }

    if let Some(state) = get_state() {
        state.screen.clear();
        state.screen.force_redraw();
        render_frame();
    }
}

fn create_tui_state() -> TuiState {
    let output = OutputPane::new(Rect::new(0, 2, 80, 19));
    let input = InputLine::new(21, "> ", submit_command);

    let mut inspector = InspectorContainer::new("Inspectors");
    inspector.push(InspectorPanel::Tasks(TasksPanel::new(Rect::new(0, 2, 80, 19))));
    inspector.push(InspectorPanel::Cpus(CpusPanel::new(Rect::new(0, 2, 80, 19))));
    inspector.push(InspectorPanel::Memory(MemoryPanel::new(Rect::new(0, 2, 80, 19))));
    inspector.push(InspectorPanel::Irqs(IrqsPanel::new(Rect::new(0, 2, 80, 19))));

    let tabs = TabBar::new(0, TAB_NAMES);
    let mut status = StatusBar::new(22);
    status.set_left("TUI | output");
    status.set_right("Tab:switch  Esc:exit");

    TuiState {
        screen: Screen::new(),
        parser: InputParser::new(),
        tick_counter: 0,
        active_panel: 0,
        inspector_tab: 0,
        inspector,
        output,
        input,
        status,
        tabs,
        initialized: false,
    }
}

pub fn exit_tui() {
    if !unsafe { *TUI_ACTIVE.get() } {
        return;
    }

    if let Some(_state) = get_state() {
        let conn = match crate::katerm::connector::get_active() {
            Some(c) => c,
            None => {
                unsafe { *TUI_ACTIVE.get() = false; *TUI_STATE.get() = None; }
                return;
            }
        };
        conn.write_str("\x1b[2J\x1b[H");
        conn.write_str(super::termcmds::prompt_for_mode(super::termcmds::current_mode()));
    }

    unsafe {
        *TUI_ACTIVE.get() = false;
        *TUI_STATE.get() = None;
    }
}

pub fn process_input_tui() {
    let state = match get_state() {
        Some(s) => s,
        None => return,
    };

    let conn = match crate::katerm::connector::get_active() {
        Some(c) => c,
        None => return,
    };

    while let Some(b) = conn.read_byte() {
        if let Some(key) = state.parser.feed(b) {
            handle_key_event(key, state);
        }
    }

    state.tick_counter += 1;
    if state.tick_counter >= REFRESH_RATE {
        state.tick_counter = 0;
        state.inspector.update();
        render_frame();
    }
}

fn handle_key_event(key: KeyEvent, state: &mut TuiState) {
    match key {
        KeyEvent::Escape => {
            if state.active_panel == 1 {
                state.active_panel = 0;
                state.status.set_left("TUI | output");
            } else {
                state.active_panel = 1;
                state.inspector_tab = 0;
                state.inspector.set_active(0);
                state.status.set_left("TUI | inspector");
            }
            render_frame();
        }
        KeyEvent::Tab if state.active_panel == 1 => {
            state.inspector_tab = (state.inspector_tab + 1) % state.inspector.count();
            state.inspector.set_active(state.inspector_tab);
            state.tabs.set_active(state.inspector_tab);
            let name = state.inspector.active_title();
            let mut buf = [0u8; 40];
            let prefix = b"TUI | ";
            buf[..prefix.len()].copy_from_slice(prefix);
            let name_bytes = name.as_bytes();
            let end = (prefix.len() + name_bytes.len()).min(40);
            buf[prefix.len()..end].copy_from_slice(&name_bytes[..end - prefix.len()]);
            state.status.set_left(core::str::from_utf8(&buf[..end]).unwrap_or("TUI"));
            render_frame();
        }
        KeyEvent::ShiftTab if state.active_panel == 1 => {
            if state.inspector_tab > 0 {
                state.inspector_tab -= 1;
            } else {
                state.inspector_tab = state.inspector.count() - 1;
            }
            state.inspector.set_active(state.inspector_tab);
            state.tabs.set_active(state.inspector_tab);
            render_frame();
        }
        _ => {
            if state.active_panel == 1 {
                state.inspector.handle_key(key);
            } else {
                state.input.handle_key(key);
            }
            render_frame();
        }
    }
}

fn submit_command(cmd: &str) {
    if cmd.is_empty() {
        return;
    }

    if let Some(state) = get_state() {
        let prompt = crate::katerm::termcmds::prompt_for_mode(crate::katerm::termcmds::current_mode());
        let mut line_buf = [0u8; 280];
        let mut pos = 0;
        for &b in prompt.as_bytes() {
            if pos < line_buf.len() { line_buf[pos] = b; pos += 1; }
        }
        for &b in cmd.as_bytes() {
            if pos < line_buf.len() { line_buf[pos] = b; pos += 1; }
        }
        let line_str = core::str::from_utf8(&line_buf[..pos]).unwrap_or(cmd);
        state.output.push_line(line_str, screen::Style::colored(screen::Color::GREEN, screen::Color::BLACK));
    }

    super::termexec::execute(cmd);

    if let Some(state) = get_state() {
        state.output.push_line("", screen::Style::plain());
    }
}

fn render_frame() {
    let state = match get_state() {
        Some(s) => s,
        None => return,
    };

    let conn = match crate::katerm::connector::get_active() {
        Some(c) => c,
        None => return,
    };

    conn.write_str("\x1b[?25l");

    if !state.initialized {
        conn.write_str("\x1b[2J\x1b[H");
        state.initialized = true;
    }

    state.tabs.render(&mut state.screen);

    if state.active_panel == 1 {
        state.inspector.render(&mut state.screen);
    } else {
        state.output.render(&mut state.screen);
    }

    state.input.render(&mut state.screen);
    state.status.render(&mut state.screen);

    state.screen.render_diff(conn);

    conn.write_str("\x1b[?25h");
    let _ = core::fmt::write(
        &mut super::vtparser::ConnectorWriter { conn },
        format_args!("\x1b[{};{}H", 22, 3 + state.input.cursor() as u16),
    );
}

pub fn is_active() -> bool {
    unsafe { *TUI_ACTIVE.get() }
}
