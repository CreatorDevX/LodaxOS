//! UEFI Multi-Processor Services enumeration.
//!
//! Reads the `MpServices` protocol to discover the BSP's LAPIC ID and
//! the LAPIC IDs of all enabled APs.  Does NOT start APs — the kernel
//! brings them up via LAPIC INIT-SIPI-SIPI using its own real-mode
//! trampoline.
//!
//! The computed `ap_count` and `ap_apic_ids` are used by the kernel's
//! `ap_start::smp_boot_aps` to allocate per-AP stacks and send SIPIs.

use lodaxos_system::{BootInfo, MAX_CPUS};
use uefi::proto::pi::mp::MpServices;

/// Enumerate APs via UEFI MP Services.
///
/// Records the LAPIC ID of each enabled AP in `boot_info.ap_apic_ids`
/// and sets `boot_info.ap_count`.  Does NOT start any AP.
pub fn enumerate_aps(boot_info: &mut BootInfo) -> uefi::Result<()> {
    let mp_handle = uefi::boot::get_handle_for_protocol::<MpServices>()?;
    let mp = uefi::boot::open_protocol_exclusive::<MpServices>(mp_handle)?;

    let count = mp.get_number_of_processors()?;
    log::info!(
        "MP Services: total={} enabled={}",
        count.total, count.enabled
    );

    if count.enabled > MAX_CPUS {
        log::error!(
            "MP Services: {} enabled CPUs exceeds MAX_CPUS={}, clamping to {}",
            count.enabled, MAX_CPUS, MAX_CPUS
        );
    }
    let to_record = count.enabled.min(MAX_CPUS);
    let ap_slots = MAX_CPUS - 1; // one slot reserved for BSP if needed

    let mut ap_index = 0usize;
    for proc_num in 0..count.total {
        if ap_index >= to_record.min(ap_slots) {
            break;
        }
        let info = mp.get_processor_info(proc_num)?;
        if !info.is_enabled() || !info.is_healthy() {
            log::debug!("MP Services: proc {} disabled/unhealthy, skipping", proc_num);
            continue;
        }
        if info.is_bsp() {
            boot_info.bsp_apic_id = info.processor_id as u32;
            log::debug!("MP Services: BSP lapic_id={}", info.processor_id);
            continue;
        }
        if ap_index >= ap_slots {
            log::error!("MP Services: ran out of AP slots (max {} APs)", ap_slots);
            break;
        }

        boot_info.ap_apic_ids[ap_index] = info.processor_id as u32;
        log::info!(
            "MP Services: AP[{}] proc_num={} lapic_id={}",
            ap_index, proc_num, info.processor_id
        );
        ap_index += 1;
    }

    boot_info.ap_count = ap_index as u32;
    log::info!(
        "MP Services: {} AP(s) enumerated, BSP lapic_id={}",
        ap_index, boot_info.bsp_apic_id
    );

    Ok(())
}
