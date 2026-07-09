pub mod tasks;
pub mod cpus;
pub mod memory;
pub mod irqs;

use super::Panel;
use crate::katerm::tui::screen::{Screen, Rect};
use crate::katerm::tui::input::KeyEvent;

pub enum InspectorPanel {
    Tasks(tasks::TasksPanel),
    Cpus(cpus::CpusPanel),
    Memory(memory::MemoryPanel),
    Irqs(irqs::IrqsPanel),
}

impl Panel for InspectorPanel {
    fn area(&self) -> Rect {
        match self {
            Self::Tasks(p) => p.area(),
            Self::Cpus(p) => p.area(),
            Self::Memory(p) => p.area(),
            Self::Irqs(p) => p.area(),
        }
    }

    fn render(&self, screen: &mut Screen) {
        match self {
            Self::Tasks(p) => p.render(screen),
            Self::Cpus(p) => p.render(screen),
            Self::Memory(p) => p.render(screen),
            Self::Irqs(p) => p.render(screen),
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> bool {
        match self {
            Self::Tasks(p) => p.handle_key(key),
            Self::Cpus(p) => p.handle_key(key),
            Self::Memory(p) => p.handle_key(key),
            Self::Irqs(p) => p.handle_key(key),
        }
    }

    fn update(&mut self) {
        match self {
            Self::Tasks(p) => p.update(),
            Self::Cpus(p) => p.update(),
            Self::Memory(p) => p.update(),
            Self::Irqs(p) => p.update(),
        }
    }

    fn title(&self) -> &str {
        match self {
            Self::Tasks(p) => p.title(),
            Self::Cpus(p) => p.title(),
            Self::Memory(p) => p.title(),
            Self::Irqs(p) => p.title(),
        }
    }
}

pub struct InspectorContainer {
    panels: [Option<InspectorPanel>; 8],
    count: usize,
    active: usize,
    title: &'static str,
}

impl InspectorContainer {
    pub fn new(title: &'static str) -> Self {
        Self {
            panels: [None, None, None, None, None, None, None, None],
            count: 0,
            active: 0,
            title,
        }
    }

    pub fn push(&mut self, panel: InspectorPanel) {
        if self.count < 8 {
            self.panels[self.count] = Some(panel);
            self.count += 1;
        }
    }

    pub fn set_active(&mut self, idx: usize) {
        if idx < self.count {
            self.active = idx;
        }
    }

    pub fn active(&self) -> usize {
        self.active
    }

    pub fn count(&self) -> usize {
        self.count
    }

    pub fn active_title(&self) -> &str {
        if let Some(panel) = &self.panels[self.active] {
            panel.title()
        } else {
            ""
        }
    }

    pub fn render(&self, screen: &mut Screen) {
        if let Some(panel) = &self.panels[self.active] {
            panel.render(screen);
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> bool {
        if let Some(panel) = &mut self.panels[self.active] {
            panel.handle_key(key)
        } else {
            false
        }
    }

    pub fn update(&mut self) {
        for i in 0..self.count {
            if let Some(panel) = &mut self.panels[i] {
                panel.update();
            }
        }
    }
}
