use crate::sync::IrqSaveSpinLock;
use crate::vcpu::VcpuId;

const MAX_SERVICES: usize = 32;
const MAX_MMIO: usize = 16;
const MAX_IRQ: usize = 8;
const MAX_DMA: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ServiceState {
    Loaded,
    Running,
    Crashed,
    Restarting,
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RestartPolicy {
    Always,
    OnFailure(u32),
    Never,
}

#[derive(Debug, Clone, Copy)]
pub struct ServiceResources {
    pub mmio: [(u64, u64, u64); MAX_MMIO],
    pub mmio_count: usize,
    pub irq: [u8; MAX_IRQ],
    pub irq_count: usize,
    pub dma: [(u64, u64); MAX_DMA],
    pub dma_count: usize,
}

impl ServiceResources {
    const fn empty() -> Self {
        Self {
            mmio: [(0, 0, 0); MAX_MMIO],
            mmio_count: 0,
            irq: [0; MAX_IRQ],
            irq_count: 0,
            dma: [(0, 0); MAX_DMA],
            dma_count: 0,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Service {
    pub id: u32,
    pub name: [u8; 32],
    pub state: ServiceState,
    pub vcpu_id: VcpuId,
    pub pml4: u64,
    resources: ServiceResources,
    pub restart_policy: RestartPolicy,
    pub restart_count: u32,
}

impl Service {
    pub fn track_mmio(&mut self, phys: u64, virt: u64, pages: u64) -> bool {
        if self.resources.mmio_count >= MAX_MMIO {
            return false;
        }
        let idx = self.resources.mmio_count;
        self.resources.mmio[idx] = (phys, virt, pages);
        self.resources.mmio_count = idx + 1;
        true
    }

    pub fn track_irq(&mut self, vector: u8) -> bool {
        if self.resources.irq_count >= MAX_IRQ {
            return false;
        }
        let idx = self.resources.irq_count;
        self.resources.irq[idx] = vector;
        self.resources.irq_count = idx + 1;
        true
    }

    pub fn track_dma(&mut self, phys: u64, pages: u64) -> bool {
        if self.resources.dma_count >= MAX_DMA {
            return false;
        }
        let idx = self.resources.dma_count;
        self.resources.dma[idx] = (phys, pages);
        self.resources.dma_count = idx + 1;
        true
    }

    pub fn untrack_dma(&mut self, phys: u64) -> bool {
        for i in 0..self.resources.dma_count {
            if self.resources.dma[i].0 == phys {
                let last = self.resources.dma_count - 1;
                self.resources.dma[i] = self.resources.dma[last];
                self.resources.dma[last] = (0, 0);
                self.resources.dma_count = last;
                return true;
            }
        }
        false
    }

    pub fn resources(&self) -> &ServiceResources {
        &self.resources
    }

    pub fn clear_resources(&mut self) {
        self.resources = ServiceResources::empty();
    }
}

struct ServiceTable {
    entries: [Option<Service>; MAX_SERVICES],
    count: usize,
}

static SERVICE_TABLE: IrqSaveSpinLock<ServiceTable> = IrqSaveSpinLock::new(ServiceTable {
    entries: [const { None }; MAX_SERVICES],
    count: 0,
});

pub fn init() {
    let mut t = SERVICE_TABLE.lock();
    t.count = 0;
    log::info!("service: table ready, {} slots", MAX_SERVICES);
}

pub fn alloc(name: &[u8], pml4: u64) -> Option<u32> {
    let mut t = SERVICE_TABLE.lock();
    if t.count >= MAX_SERVICES {
        log::error!("service: table exhausted (max {})", MAX_SERVICES);
        return None;
    }
    for i in 0..MAX_SERVICES {
        if t.entries[i].is_none() {
            let id = i as u32;
            let mut entry_name = [0u8; 32];
            let len = name.len().min(31);
            entry_name[..len].copy_from_slice(&name[..len]);
            t.entries[i] = Some(Service {
                id,
                name: entry_name,
                state: ServiceState::Loaded,
                vcpu_id: 0,
                pml4,
                resources: ServiceResources::empty(),
                restart_policy: RestartPolicy::OnFailure(3),
                restart_count: 0,
            });
            t.count += 1;
            return Some(id);
        }
    }
    None
}

pub fn free(id: u32) {
    let mut t = SERVICE_TABLE.lock();
    let idx = id as usize;
    if idx < MAX_SERVICES && t.entries[idx].is_some() {
        t.entries[idx] = None;
        t.count -= 1;
    }
}

pub fn get(id: u32) -> Option<&'static Service> {
    let t = unsafe { SERVICE_TABLE.unsafe_get() };
    let idx = id as usize;
    if idx < MAX_SERVICES {
        t.entries[idx].as_ref()
    } else {
        None
    }
}

pub fn with_mut<R>(id: u32, f: impl FnOnce(Option<&mut Service>) -> R) -> R {
    let mut t = SERVICE_TABLE.lock();
    let idx = id as usize;
    if idx < MAX_SERVICES {
        f(t.entries[idx].as_mut())
    } else {
        f(None)
    }
}

pub fn find_by_vcpu(vcpu_id: VcpuId) -> Option<u32> {
    let t = unsafe { SERVICE_TABLE.unsafe_get() };
    for (i, slot) in t.entries.iter().enumerate() {
        if let Some(s) = slot {
            if s.vcpu_id == vcpu_id {
                return Some(i as u32);
            }
        }
    }
    None
}

pub fn find_by_name(name: &[u8]) -> Option<u32> {
    let t = unsafe { SERVICE_TABLE.unsafe_get() };
    let len = name.len().min(31);
    for (i, slot) in t.entries.iter().enumerate() {
        if let Some(s) = slot {
            if s.name[..len] == name[..len] && (len == 31 || s.name[len] == 0) {
                return Some(i as u32);
            }
        }
    }
    None
}

pub fn count() -> usize {
    unsafe { SERVICE_TABLE.unsafe_get().count }
}
