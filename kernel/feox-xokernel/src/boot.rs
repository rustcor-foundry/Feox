//! Early kernel bootstrap flow.

#[cfg(target_os = "none")]
use core::arch::global_asm;

use crate::arch;
use crate::bootabi::BootHandoff;
use crate::memory;
use crate::memory::MemoryRegionKind;
use crate::paging;
use crate::runtime_context::{
    BootstrapCoreContext, RuntimeServiceCommand, RuntimeServiceHeartbeat, RuntimeServiceReport,
    RuntimeServiceState,
    RuntimeSnapshot,
};
use crate::{KernelConfig, PROJECT_NAME, PROJECT_STYLE};

const TRANSITION_STACK_PAGES: u64 = 4;

static mut TRANSITION_ALIAS_STACK_TOP: u64 = 0;
static mut TRANSITION_ALIAS_ENTRY: u64 = 0;
static mut TRANSITION_DATA_ALIAS: u64 = 0;
static mut TRANSITION_GDT_ALIAS: u64 = 0;
static mut TRANSITION_IDT_ALIAS: u64 = 0;
static mut TRANSITION_HANDLER_DELTA: u64 = 0;
static mut TRANSITION_BOOTSTRAP_CORE_ID: u16 = 0;

const TRANSITION_DATA_MAGIC: u64 = 0x4645_4F58_5452_4E31;

#[repr(u64)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BootstrapRuntimeStage {
    Prepared = 1,
    IdentityActive = 2,
    AliasActive = 3,
    ExceptionValidated = 4,
    RuntimeActive = 5,
}

impl BootstrapRuntimeStage {
    const fn as_u64(self) -> u64 {
        self as u64
    }

    const fn from_raw(raw: u64) -> Option<Self> {
        match raw {
            1 => Some(Self::Prepared),
            2 => Some(Self::IdentityActive),
            3 => Some(Self::AliasActive),
            4 => Some(Self::ExceptionValidated),
            5 => Some(Self::RuntimeActive),
            _ => None,
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Prepared => "prepared",
            Self::IdentityActive => "identity-active",
            Self::AliasActive => "alias-active",
            Self::ExceptionValidated => "exception-validated",
            Self::RuntimeActive => "runtime-active",
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct BootstrapRuntimeState {
    magic: u64,
    active_root: u64,
    kernel_window_base: u64,
    kernel_window_end: u64,
    kernel_pages_mapped: u64,
    identity_stack: u64,
    alias_stack: u64,
    stack_pages: u64,
    data_page: u64,
    alias_entry: u64,
    alias_gdt: u64,
    alias_idt: u64,
    stage: u64,
}

impl BootstrapRuntimeState {
    fn stage(self) -> Option<BootstrapRuntimeStage> {
        BootstrapRuntimeStage::from_raw(self.stage)
    }
}

impl RuntimeSnapshot {
    fn from_state(state: BootstrapRuntimeState) -> Option<Self> {
        if state.magic != TRANSITION_DATA_MAGIC {
            return None;
        }

        Some(Self {
            active_root: state.active_root,
            kernel_window_base: state.kernel_window_base,
            kernel_window_end: state.kernel_window_end,
            kernel_pages_mapped: state.kernel_pages_mapped,
            identity_stack: state.identity_stack,
            alias_stack: state.alias_stack,
            stack_pages: state.stack_pages,
            data_page: state.data_page,
            alias_entry: state.alias_entry,
            alias_gdt: state.alias_gdt,
            alias_idt: state.alias_idt,
            stage: state
                .stage()
                .map_or("unknown", BootstrapRuntimeStage::label),
        })
    }
}

#[cfg(target_os = "none")]
global_asm!(
    ".global feox_transition_entry",
    "feox_transition_entry:",
    "mov dx, 0x402",
    "mov al, 0x5b",
    "out dx, al",
    "mov al, 0x54",
    "out dx, al",
    "mov al, 0x5d",
    "out dx, al",
    "jmp {rust_entry}",
    rust_entry = sym transition_stage_entry_rust,
);

#[cfg(target_os = "none")]
unsafe extern "C" {
    fn feox_transition_entry() -> !;
}

#[cfg(not(target_os = "none"))]
extern "C" fn feox_transition_entry() -> ! {
    transition_stage_entry_rust()
}

extern "C" fn transition_stage_entry_rust() -> ! {
    let alias_data_address = unsafe { TRANSITION_DATA_ALIAS };
    crate::kprintln!(
        "paging: transition_handoff active_root={:#018x}",
        memory::active_page_table_root().start_address().as_u64()
    );
    crate::kprintln!(
        "paging: transition_handoff_stack={:#018x}",
        arch::current_stack_pointer()
    );
    crate::runtime_context::push_event("transition-handoff-entered");
    let alias_stack_top = unsafe { TRANSITION_ALIAS_STACK_TOP };
    let alias_entry = unsafe { TRANSITION_ALIAS_ENTRY };
    if alias_data_address != 0 {
        let transition_data = unsafe {
            // SAFETY: the transition data page is kernel-owned bootstrap memory
            // that was explicitly mapped into the active transition root before
            // the CR3 handoff.
            &mut *(alias_data_address as *mut BootstrapRuntimeState)
        };
        crate::kprintln!(
            "paging: runtime_state magic={:#018x} stage={} kernel_window={:#018x}-{:#018x} pages={}",
            transition_data.magic,
            transition_data
                .stage()
                .map_or("unknown", BootstrapRuntimeStage::label),
            transition_data.kernel_window_base,
            transition_data.kernel_window_end,
            transition_data.kernel_pages_mapped,
        );
        transition_data.stage = BootstrapRuntimeStage::IdentityActive.as_u64();
        crate::runtime_context::push_event("identity-handoff-active");
        crate::runtime_context::store_core(BootstrapCoreContext {
            core_id: feox_asi::CoreId(unsafe { TRANSITION_BOOTSTRAP_CORE_ID }),
            active_root: memory::active_page_table_root().start_address().as_u64(),
            stack_pointer: arch::current_stack_pointer(),
            alias_entry,
            stage: BootstrapRuntimeStage::IdentityActive.label(),
        });
    }
    if alias_stack_top != 0
        && alias_entry != 0
        && arch::current_stack_pointer() != alias_stack_top
    {
        crate::kprintln!(
            "stage: switching to transition alias stack={:#018x} entry={:#018x}",
            alias_stack_top,
            alias_entry
        );
        unsafe {
            // SAFETY: the alias stack top was prepared and mapped in the active
            // transition root before the CR3 handoff, and the alias entry is
            // a prepared high-half mapping of the same kernel image.
            arch::switch_stack_and_jump(
                alias_stack_top,
                alias_entry,
            )
        }
    }
    transition_high_stack_entry()
}

extern "C" fn transition_high_stack_entry() -> ! {
    let alias_data_address = unsafe { TRANSITION_DATA_ALIAS };
    let alias_gdt_base = unsafe { TRANSITION_GDT_ALIAS };
    let alias_idt_base = unsafe { TRANSITION_IDT_ALIAS };
    let alias_handler_delta = unsafe { TRANSITION_HANDLER_DELTA };
    crate::kprintln!(
        "paging: transition_alias_stack active_root={:#018x}",
        memory::active_page_table_root().start_address().as_u64()
    );
    crate::kprintln!(
        "paging: transition_alias_stack_pointer={:#018x}",
        arch::current_stack_pointer()
    );
    if alias_data_address != 0 {
        let transition_data = unsafe {
            // SAFETY: the transition data page remains mapped in the active
            // transition root and is owned by the bootstrap kernel path.
            &mut *(alias_data_address as *mut BootstrapRuntimeState)
        };
        crate::kprintln!(
            "paging: runtime_state_post_switch magic={:#018x} stage={} identity_stack={:#018x} alias_stack={:#018x}",
            transition_data.magic,
            transition_data
                .stage()
                .map_or("unknown", BootstrapRuntimeStage::label),
            transition_data.identity_stack,
            transition_data.alias_stack
        );
        transition_data.stage = BootstrapRuntimeStage::AliasActive.as_u64();
        crate::kprintln!(
            "paging: runtime_layout window={:#018x}-{:#018x} entry={:#018x} gdt={:#018x} idt={:#018x} stack_pages={} data_page={:#018x}",
            transition_data.kernel_window_base,
            transition_data.kernel_window_end,
            transition_data.alias_entry,
            transition_data.alias_gdt,
            transition_data.alias_idt,
            transition_data.stack_pages,
            transition_data.data_page,
        );
        crate::runtime_context::push_event("higher-half-alias-active");
        if let Some(snapshot) = RuntimeSnapshot::from_state(*transition_data) {
            crate::runtime_context::store(snapshot);
        }
        crate::runtime_context::store_core(BootstrapCoreContext {
            core_id: feox_asi::CoreId(unsafe { TRANSITION_BOOTSTRAP_CORE_ID }),
            active_root: memory::active_page_table_root().start_address().as_u64(),
            stack_pointer: arch::current_stack_pointer(),
            alias_entry: transition_data.alias_entry,
            stage: BootstrapRuntimeStage::AliasActive.label(),
        });
    }
    if alias_gdt_base != 0 && alias_idt_base != 0 {
        unsafe {
            // SAFETY: both descriptor tables live inside the retained kernel
            // image mapping, and the handler delta retargets the bootstrap
            // exception stubs into the same high-half image window.
            arch::reload_descriptor_tables(
                alias_gdt_base,
                alias_idt_base,
                alias_handler_delta,
            );
        }
        crate::kprintln!(
            "paging: transition_descriptors gdt={:#018x} idt={:#018x} handler_delta={:#018x}",
            alias_gdt_base,
            alias_idt_base,
            alias_handler_delta
        );
        crate::runtime_context::push_event("descriptor-tables-reloaded");
        crate::kprintln!("stage: validating higher-half exception path");
        if alias_data_address != 0 {
            let transition_data = unsafe {
                // SAFETY: the bootstrap runtime state remains mapped in the
                // active transition root while this validation runs.
                &mut *(alias_data_address as *mut BootstrapRuntimeState)
            };
            transition_data.stage = BootstrapRuntimeStage::ExceptionValidated.as_u64();
            if let Some(snapshot) = RuntimeSnapshot::from_state(*transition_data) {
                crate::runtime_context::store(snapshot);
            }
            crate::runtime_context::store_core(BootstrapCoreContext {
                core_id: feox_asi::CoreId(unsafe { TRANSITION_BOOTSTRAP_CORE_ID }),
                active_root: memory::active_page_table_root().start_address().as_u64(),
                stack_pointer: arch::current_stack_pointer(),
                alias_entry: transition_data.alias_entry,
                stage: BootstrapRuntimeStage::ExceptionValidated.label(),
            });
        }
        crate::runtime_context::push_event("higher-half-exception-validation");
        arch::trigger_breakpoint();
        if alias_data_address != 0 {
            let transition_data = unsafe {
                // SAFETY: the bootstrap runtime state remains mapped in the
                // active transition root after the breakpoint returns.
                &mut *(alias_data_address as *mut BootstrapRuntimeState)
            };
            transition_data.stage = BootstrapRuntimeStage::RuntimeActive.as_u64();
            if let Some(snapshot) = RuntimeSnapshot::from_state(*transition_data) {
                crate::runtime_context::store(snapshot);
            }
            crate::runtime_context::store_core(BootstrapCoreContext {
                core_id: feox_asi::CoreId(unsafe { TRANSITION_BOOTSTRAP_CORE_ID }),
                active_root: memory::active_page_table_root().start_address().as_u64(),
                stack_pointer: arch::current_stack_pointer(),
                alias_entry: transition_data.alias_entry,
                stage: BootstrapRuntimeStage::RuntimeActive.label(),
            });
        }
        crate::runtime_context::push_event("higher-half-runtime-active");
        crate::kprintln!("stage: higher-half runtime active");
        runtime_active_entry();
    } else {
        crate::kprintln!("paging: transition_descriptors unavailable");
    }
    crate::kprintln!("stage: transition handoff complete");
    arch::halt_loop()
}

fn runtime_active_entry() -> ! {
    crate::runtime_context::push_event("runtime-service-entered");
    crate::kprintln!("stage: runtime service entered");

    let owner_core = crate::runtime_context::core()
        .map_or(feox_asi::CoreId(0), |core| core.core_id);
    crate::runtime_context::store_service(RuntimeServiceState {
        owner_core,
        phase: "entered",
        iterations: 0,
        last_action: "service-entered",
    });

    if let Some(runtime) = crate::runtime_context::snapshot() {
        crate::kprintln!(
            "runtime: summary root={:#018x} window={:#018x}-{:#018x} pages={} data_page={:#018x}",
            runtime.active_root,
            runtime.kernel_window_base,
            runtime.kernel_window_end,
            runtime.kernel_pages_mapped,
            runtime.data_page
        );
    }
    if let Some(core) = crate::runtime_context::core() {
        crate::kprintln!(
            "runtime: owner core={} stage={} stack={:#018x} entry={:#018x}",
            core.core_id.0,
            core.stage,
            core.stack_pointer,
            core.alias_entry
        );
    }

    let _ = crate::runtime_context::enqueue_command(RuntimeServiceCommand::RefreshSnapshot);

    let events = crate::runtime_context::events();
    let mut event_index = 0usize;
    while event_index < events.len() {
        if let Some(event) = events[event_index] {
            crate::kprintln!("runtime: retained_event[{}]={}", event_index, event);
        }
        event_index += 1;
    }

    let mut service_iteration = 0_u64;
    while let Some(command) = crate::runtime_context::dequeue_command() {
        service_iteration += 1;
        crate::kprintln!(
            "runtime: command phase={} iteration={}",
            command.label(),
            service_iteration
        );
        match command {
            RuntimeServiceCommand::RefreshSnapshot => {
                crate::runtime_context::push_event("runtime-service-poll");
                crate::runtime_context::store_service(RuntimeServiceState {
                    owner_core,
                    phase: "poll",
                    iterations: service_iteration,
                    last_action: "retained-snapshot-scan",
                });
                if let Some(service) = crate::runtime_context::service() {
                    crate::kprintln!(
                        "runtime: service core={} phase={} iterations={} action={}",
                        service.owner_core.0,
                        service.phase,
                        service.iterations,
                        service.last_action
                    );
                }
                if crate::runtime_context::snapshot().is_some() {
                    let _ = crate::runtime_context::enqueue_command(
                        RuntimeServiceCommand::RefreshAccounting,
                    );
                } else {
                    let _ = crate::runtime_context::enqueue_command(RuntimeServiceCommand::EnterIdle);
                }
            }
            RuntimeServiceCommand::RefreshAccounting => {
                if let Some(runtime) = crate::runtime_context::snapshot() {
                    let mut retained_events = 0_u64;
                    let events = crate::runtime_context::events();
                    let mut event_index = 0usize;
                    while event_index < events.len() {
                        if events[event_index].is_some() {
                            retained_events += 1;
                        }
                        event_index += 1;
                    }
                    let kernel_window_bytes =
                        runtime.kernel_window_end.saturating_sub(runtime.kernel_window_base);
                    let stack_bytes = runtime.stack_pages.saturating_mul(memory::PAGE_SIZE);
                    crate::runtime_context::push_event("runtime-service-accounting");
                    crate::runtime_context::store_service(RuntimeServiceState {
                        owner_core,
                        phase: "accounting",
                        iterations: service_iteration,
                        last_action: "retained-runtime-accounting",
                    });
                    crate::runtime_context::store_service_report(RuntimeServiceReport {
                        kernel_window_bytes,
                        stack_bytes,
                        retained_events,
                    });
                    if let Some(service) = crate::runtime_context::service() {
                        crate::kprintln!(
                            "runtime: service_accounting core={} phase={} iterations={} action={}",
                            service.owner_core.0,
                            service.phase,
                            service.iterations,
                            service.last_action
                        );
                    }
                    if let Some(report) = crate::runtime_context::service_report() {
                        crate::kprintln!(
                            "runtime: accounting window_bytes={:#018x} stack_bytes={:#018x} retained_events={}",
                            report.kernel_window_bytes,
                            report.stack_bytes,
                            report.retained_events
                        );
                    }
                    if retained_events > 0 {
                        let _ = crate::runtime_context::enqueue_command(
                            RuntimeServiceCommand::ReportTimeline,
                        );
                    } else {
                        let _ = crate::runtime_context::enqueue_command(
                            RuntimeServiceCommand::EnterIdle,
                        );
                    }
                } else {
                    let _ = crate::runtime_context::enqueue_command(RuntimeServiceCommand::EnterIdle);
                }
            }
            RuntimeServiceCommand::ReportTimeline => {
                crate::runtime_context::push_event("runtime-service-timeline");
                crate::runtime_context::store_service(RuntimeServiceState {
                    owner_core,
                    phase: "timeline",
                    iterations: service_iteration,
                    last_action: "retained-event-report",
                });
                if let Some(service) = crate::runtime_context::service() {
                    crate::kprintln!(
                        "runtime: service_timeline core={} phase={} iterations={} action={}",
                        service.owner_core.0,
                        service.phase,
                        service.iterations,
                        service.last_action
                    );
                }
                let events = crate::runtime_context::events();
                let mut timeline_index = 0usize;
                while timeline_index < events.len() {
                    if let Some(event) = events[timeline_index] {
                        crate::kprintln!(
                            "runtime: timeline_event[{}]={}",
                            timeline_index,
                            event
                        );
                    }
                    timeline_index += 1;
                }
                if crate::runtime_context::service_report().is_some() {
                    let _ = crate::runtime_context::enqueue_command(
                        RuntimeServiceCommand::UpdateHeartbeat,
                    );
                } else {
                    let _ = crate::runtime_context::enqueue_command(RuntimeServiceCommand::EnterIdle);
                }
            }
            RuntimeServiceCommand::UpdateHeartbeat => {
                crate::runtime_context::push_event("runtime-service-heartbeat");
                crate::runtime_context::store_service(RuntimeServiceState {
                    owner_core,
                    phase: "heartbeat",
                    iterations: service_iteration,
                    last_action: "retained-heartbeat-update",
                });
                let previous_beats = crate::runtime_context::service_heartbeat()
                    .map_or(0, |heartbeat| heartbeat.beats);
                let observed_events = crate::runtime_context::service_report()
                    .map_or(0, |report| report.retained_events);
                crate::runtime_context::store_service_heartbeat(RuntimeServiceHeartbeat {
                    beats: previous_beats.saturating_add(1),
                    last_iteration: service_iteration,
                    observed_events,
                });
                if let Some(service) = crate::runtime_context::service() {
                    crate::kprintln!(
                        "runtime: service_heartbeat core={} phase={} iterations={} action={}",
                        service.owner_core.0,
                        service.phase,
                        service.iterations,
                        service.last_action
                    );
                }
                if let Some(heartbeat) = crate::runtime_context::service_heartbeat() {
                    crate::kprintln!(
                        "runtime: heartbeat beats={} last_iteration={} observed_events={}",
                        heartbeat.beats,
                        heartbeat.last_iteration,
                        heartbeat.observed_events
                    );
                    if heartbeat.beats < 2 {
                        let _ = crate::runtime_context::enqueue_command(
                            RuntimeServiceCommand::RefreshSnapshot,
                        );
                    } else {
                        let _ = crate::runtime_context::enqueue_command(
                            RuntimeServiceCommand::EnterIdle,
                        );
                    }
                } else {
                    let _ = crate::runtime_context::enqueue_command(
                        RuntimeServiceCommand::EnterIdle,
                    );
                }
            }
            RuntimeServiceCommand::EnterIdle => {
                crate::runtime_context::push_event("runtime-service-idle");
                crate::runtime_context::store_service(RuntimeServiceState {
                    owner_core,
                    phase: "idle",
                    iterations: service_iteration,
                    last_action: "idle-loop",
                });
                if let Some(service) = crate::runtime_context::service() {
                    crate::kprintln!(
                        "runtime: service_idle core={} phase={} iterations={} action={}",
                        service.owner_core.0,
                        service.phase,
                        service.iterations,
                        service.last_action
                    );
                }
            }
        }
    }
    crate::kprintln!("stage: runtime service idle");
    arch::halt_loop()
}

#[cfg(test)]
mod tests {
    use super::{BootstrapRuntimeStage, BootstrapRuntimeState, RuntimeSnapshot, TRANSITION_DATA_MAGIC};

    #[test]
    fn runtime_snapshot_reflects_retained_bootstrap_state() {
        let state = BootstrapRuntimeState {
            magic: TRANSITION_DATA_MAGIC,
            active_root: 0x11e000,
            kernel_window_base: 0xffff_9000_0000_0000,
            kernel_window_end: 0xffff_9000_0001_8031,
            kernel_pages_mapped: 25,
            identity_stack: 0x11d000,
            alias_stack: 0xffff_9000_0200_4000,
            stack_pages: 4,
            data_page: 0x11d000,
            alias_entry: 0xffff_9000_0000_0970,
            alias_gdt: 0xffff_9000_0000_f740,
            alias_idt: 0xffff_9000_0001_7031,
            stage: BootstrapRuntimeStage::ExceptionValidated.as_u64(),
        };

        let snapshot = RuntimeSnapshot::from_state(state).expect("valid runtime snapshot");

        assert_eq!(snapshot.active_root, 0x11e000);
        assert_eq!(snapshot.kernel_pages_mapped, 25);
        assert_eq!(snapshot.stack_pages, 4);
        assert_eq!(snapshot.stage, "exception-validated");
    }

    #[test]
    fn runtime_snapshot_rejects_bad_magic() {
        let state = BootstrapRuntimeState {
            magic: 0,
            active_root: 0,
            kernel_window_base: 0,
            kernel_window_end: 0,
            kernel_pages_mapped: 0,
            identity_stack: 0,
            alias_stack: 0,
            stack_pages: 0,
            data_page: 0,
            alias_entry: 0,
            alias_gdt: 0,
            alias_idt: 0,
            stage: 0,
        };

        assert_eq!(RuntimeSnapshot::from_state(state), None);
    }
}

/// Initializes the earliest architecture support and halts in a known-good
/// state so higher layers can be added incrementally.
pub fn bootstrap(config: KernelConfig, handoff: Option<BootHandoff<'_>>) -> ! {
    arch::early_init();
    let kernel_image = memory::kernel_image();
    let pml4 = memory::active_page_table_root();

    crate::kprintln!("{} bootstrap", PROJECT_NAME);
    crate::kprintln!("profile: {}", PROJECT_STYLE);
    crate::kprintln!("arch: {}", arch::CURRENT_ARCH);
    crate::kprintln!(
        "bootstrap_core={} max_cores={} nvme_queue_depth={}",
        config.bootstrap_core.0,
        config.max_cores,
        config.nvme_queue_depth
    );
    crate::kprintln!("stage: descriptor tables loaded");
    crate::kprintln!(
        "memory: kernel_image={:#018x}-{:#018x} size={} KiB",
        kernel_image.start.as_u64(),
        kernel_image.end.as_u64(),
        kernel_image.size_bytes() / 1024
    );
    crate::kprintln!(
        "memory: active_pml4={:#018x} page_size={} bytes",
        pml4.start_address().as_u64(),
        memory::PAGE_SIZE
    );
    match handoff {
        Some(handoff) => {
            let runtime_layout = memory::BootstrapRuntimeLayout::new();
            crate::kprintln!(
                "memory: boot_map regions={} usable={} MiB top={:#018x}",
                handoff.memory_map().len(),
                handoff.usable_bytes() / (1024 * 1024),
                handoff
                    .highest_physical_address()
                    .map_or(0, |address| address.as_u64())
            );

            let mut reservations =
                memory::EarlyKernelReservations::for_bootstrap(kernel_image, pml4);
            let boot_map = memory::BootMemoryMap::new(handoff.memory_map());
            let allocator =
                memory::FrameAllocator::with_reservations(boot_map, reservations.as_view());
            let next_frame = allocator
                .clone()
                .allocate()
                .map_or(0, |frame| frame.start_address().as_u64());
            let transition_virtual_address = runtime_layout.kernel_window_base();
            let transition_data_alias = runtime_layout.data_window_base();
            let transition_stage_entry_alias = runtime_layout.alias_for_kernel_address(
                kernel_image,
                memory::VirtualAddress::new(feox_transition_entry as *const () as usize as u64),
            );
            let identity_transition_entry =
                memory::VirtualAddress::new(feox_transition_entry as *const () as usize as u64);
            let identity_gdt = memory::VirtualAddress::new(arch::x86_64::gdt::table_base());
            let identity_idt = memory::VirtualAddress::new(arch::x86_64::idt::table_base());
            let transition_high_stack_entry_alias = runtime_layout.alias_for_kernel_address(
                kernel_image,
                memory::VirtualAddress::new(
                    transition_high_stack_entry as *const () as usize as u64,
                ),
            );
            let transition_gdt_alias =
                runtime_layout.alias_for_kernel_address(kernel_image, identity_gdt);
            let transition_idt_alias =
                runtime_layout.alias_for_kernel_address(kernel_image, identity_idt);
            let reserved_ranges = reservations.len();
            let kernel_image_reserved_start = reservations
                .region_for_kind(memory::ReservationKind::KernelImage)
                .map_or(0, |region| region.start().as_u64());
            let kernel_image_reserved_end = reservations
                .region_for_kind(memory::ReservationKind::KernelImage)
                .map_or(0, |region| region.end().as_u64());
            let active_pml4_reserved = reservations
                .region_for_kind(memory::ReservationKind::ActivePageTableRoot)
                .map_or(0, |region| region.start().as_u64());

            crate::kprintln!(
                "memory: reserved_ranges={} kernel_image_reserved={:#018x}-{:#018x}",
                reserved_ranges,
                kernel_image_reserved_start,
                kernel_image_reserved_end
            );
            crate::kprintln!(
                "memory: active_pml4_reserved={:#018x}",
                active_pml4_reserved
            );
            crate::kprintln!(
                "memory: handoff accepted, first_usable_frame={:#018x}",
                next_frame
            );
            let (
                bootstrap_page_table_frame,
                transition_root_ready,
                transition_pages_mapped,
                transition_stack_top,
                transition_stack_alias_top,
                transition_data_identity,
                transition_data_alias_mapped,
                transition_map_result,
                transition_translate_result,
                identity_entry_translate_result,
                identity_stack_translate_result,
                identity_data_translate_result,
            ) = {
                let mut transition_stack_frames = [None; TRANSITION_STACK_PAGES as usize];
                let mut stack_page = 0usize;
                while stack_page < transition_stack_frames.len() {
                    let mut allocator =
                        memory::FrameAllocator::with_reservations(boot_map, reservations.as_view());
                    let Some(frame) = allocator.allocate() else {
                        break;
                    };
                    reservations.reserve_frame(memory::ReservationKind::BootstrapPerCoreState, frame);
                    transition_stack_frames[stack_page] = Some(frame);
                    stack_page += 1;
                }
                let transition_stack_top = transition_stack_frames
                    .iter()
                    .flatten()
                    .next_back()
                    .map(|frame| frame.start_address().as_u64() + memory::PAGE_SIZE)
                    .unwrap_or(0);
                let transition_stack_alias_top = transition_stack_frames
                    .iter()
                    .flatten()
                    .next_back()
                    .map(|frame| {
                        runtime_layout.stack_window_base().as_u64()
                            + ((frame.start_address().as_u64()
                                - transition_stack_frames[0]
                                    .unwrap()
                                    .start_address()
                                    .as_u64())
                                + memory::PAGE_SIZE)
                    })
                    .unwrap_or(0);
                let mut allocator =
                    memory::FrameAllocator::with_reservations(boot_map, reservations.as_view());
                let transition_data_frame = allocator.allocate();
                if let Some(frame) = transition_data_frame {
                    reservations.reserve_frame(memory::ReservationKind::BootstrapPerCoreState, frame);
                }
                let transition_data_identity = transition_data_frame
                    .map(|frame| frame.start_address().as_u64())
                    .unwrap_or(0);
                let transition_data_alias_mapped = if transition_data_identity != 0 {
                    transition_data_alias.as_u64()
                } else {
                    0
                };
                let mut paging_allocator =
                    paging::BootstrapPagingAllocator::new(boot_map, &mut reservations);
                match paging_allocator.allocate_table_frame() {
                    Some(bootstrap_page_table_root) => {
                        let mut live_page_tables = paging::BootstrapIdentityMappedPageTables;
                        let page_root = paging::PageTableRoot::new(bootstrap_page_table_root);
                        let transition_root_ready = paging::PageTableFrameMutSource::table_mut(
                            &mut live_page_tables,
                            bootstrap_page_table_root,
                        )
                        .map(|table| table.fill(0))
                        .is_some();
                        let mut transition_pages_mapped = 0_u64;
                        let transition_map_result = if transition_root_ready {
                            let kernel_pages =
                                kernel_image.size_bytes().div_ceil(memory::PAGE_SIZE).max(1);
                            let kernel_page_flags = 1_u64 << 1;
                            let mut map_result = Ok(());
                            let mut page_index = 0;
                            while page_index < kernel_pages {
                                let page_offset = page_index * memory::PAGE_SIZE;
                                let identity_virtual = memory::VirtualAddress::new(
                                    kernel_image.start.as_u64() + page_offset,
                                );
                                let mapped_virtual = memory::VirtualAddress::new(
                                    transition_virtual_address.as_u64() + page_offset,
                                );
                                let mapped_physical = memory::PhysicalFrame::containing(
                                    memory::PhysicalAddress::new(
                                        kernel_image.start.as_u64() + page_offset,
                                    ),
                                );
                                if page_root
                                    .map_4k_with(
                                        &mut live_page_tables,
                                        &mut paging_allocator,
                                        identity_virtual,
                                        mapped_physical,
                                        kernel_page_flags,
                                    )
                                    .is_err()
                                {
                                    map_result = Err("identity_map_failed");
                                    break;
                                }
                                if page_root
                                    .map_4k_with(
                                        &mut live_page_tables,
                                        &mut paging_allocator,
                                        mapped_virtual,
                                        mapped_physical,
                                        kernel_page_flags,
                                    )
                                    .is_err()
                                {
                                    map_result = Err("transition_map_failed");
                                    break;
                                }
                                transition_pages_mapped += 1;
                                page_index += 1;
                            }
                            let stack_page_flags = 1_u64 << 1;
                            if let Some(first_stack_frame) = transition_stack_frames[0] {
                                let mut stack_index = 0usize;
                                while stack_index < transition_stack_frames.len() {
                                    let Some(stack_frame) = transition_stack_frames[stack_index] else {
                                        map_result = Err("transition_stack_incomplete");
                                        break;
                                    };
                                    let stack_offset =
                                        stack_frame.start_address().as_u64()
                                            - first_stack_frame.start_address().as_u64();
                                    let identity_stack_virtual = memory::VirtualAddress::new(
                                        stack_frame.start_address().as_u64(),
                                    );
                                    let alias_stack_virtual = memory::VirtualAddress::new(
                                        runtime_layout.stack_window_base().as_u64() + stack_offset,
                                    );
                                    if page_root
                                        .map_4k_with(
                                            &mut live_page_tables,
                                            &mut paging_allocator,
                                            identity_stack_virtual,
                                            stack_frame,
                                            stack_page_flags,
                                        )
                                        .is_err()
                                    {
                                        map_result = Err("identity_stack_map_failed");
                                        break;
                                    }
                                    if page_root
                                        .map_4k_with(
                                            &mut live_page_tables,
                                            &mut paging_allocator,
                                            alias_stack_virtual,
                                            stack_frame,
                                            stack_page_flags,
                                        )
                                        .is_err()
                                    {
                                        map_result = Err("transition_stack_map_failed");
                                        break;
                                    }
                                    stack_index += 1;
                                }
                            } else {
                                map_result = Err("transition_stack_unavailable");
                            }
                            if map_result.is_ok() {
                                if let Some(data_frame) = transition_data_frame {
                                    let data_page_flags = 1_u64 << 1;
                                    let identity_data_virtual = memory::VirtualAddress::new(
                                        data_frame.start_address().as_u64(),
                                    );
                                    if page_root
                                        .map_4k_with(
                                            &mut live_page_tables,
                                            &mut paging_allocator,
                                            identity_data_virtual,
                                            data_frame,
                                            data_page_flags,
                                        )
                                        .is_err()
                                    {
                                        map_result = Err("identity_data_map_failed");
                                    }
                                    if map_result.is_ok()
                                        && page_root
                                            .map_4k_with(
                                                &mut live_page_tables,
                                                &mut paging_allocator,
                                                transition_data_alias,
                                                data_frame,
                                                data_page_flags,
                                            )
                                            .is_err()
                                    {
                                        map_result = Err("transition_data_map_failed");
                                    }
                                } else {
                                    map_result = Err("transition_data_unavailable");
                                }
                            }
                            map_result
                        } else {
                            Err("transition_root_not_identity_mapped")
                        };
                        let transition_translate_result = if transition_map_result.is_ok() {
                            page_root.translate_with(&live_page_tables, transition_virtual_address)
                        } else {
                            Ok(None)
                        };
                        let identity_entry_translate_result = if transition_map_result.is_ok() {
                            page_root.translate_with(&live_page_tables, identity_transition_entry)
                        } else {
                            Ok(None)
                        };
                        let identity_stack_translate_result = if transition_map_result.is_ok() {
                            page_root.translate_with(
                                &live_page_tables,
                                memory::VirtualAddress::new(transition_stack_top.saturating_sub(8)),
                            )
                        } else {
                            Ok(None)
                        };
                        let identity_data_translate_result = if transition_map_result.is_ok()
                            && transition_data_identity != 0
                        {
                            page_root.translate_with(
                                &live_page_tables,
                                memory::VirtualAddress::new(transition_data_identity),
                            )
                        } else {
                            Ok(None)
                        };

                        (
                            bootstrap_page_table_root.start_address().as_u64(),
                            transition_root_ready,
                            transition_pages_mapped,
                            transition_stack_top,
                            transition_stack_alias_top,
                            transition_data_identity,
                            transition_data_alias_mapped,
                            transition_map_result,
                            transition_translate_result,
                            identity_entry_translate_result,
                            identity_stack_translate_result,
                            identity_data_translate_result,
                        )
                    }
                    None => (
                        0,
                        false,
                        0,
                        0,
                        0,
                        0,
                        0,
                        Err("out_of_table_frames"),
                        Ok(None),
                        Ok(None),
                        Ok(None),
                        Ok(None),
                    ),
                }
            };
            crate::kprintln!(
                "paging: bootstrap_table_frame={:#018x}",
                bootstrap_page_table_frame
            );
            if transition_root_ready {
                crate::kprintln!(
                    "paging: transition_root_ready={:#018x}",
                    bootstrap_page_table_frame
                );
                crate::kprintln!(
                    "paging: transition_pages_mapped={}",
                    transition_pages_mapped
                );
                if let Some(entry) = transition_stage_entry_alias {
                    crate::kprintln!(
                        "paging: transition_entry={:#018x}",
                        entry.as_u64()
                    );
                } else {
                    crate::kprintln!("paging: transition_entry=unavailable");
                }
                crate::kprintln!("paging: transition_stack={:#018x}", transition_stack_alias_top);
                crate::kprintln!(
                    "paging: identity_transition_stack={:#018x}",
                    transition_stack_top
                );
                crate::kprintln!(
                    "paging: transition_data_alias={:#018x}",
                    transition_data_alias_mapped
                );
                crate::kprintln!(
                    "paging: identity_transition_data={:#018x}",
                    transition_data_identity
                );
                if let Some(entry) = transition_high_stack_entry_alias {
                    crate::kprintln!(
                        "paging: transition_alias_entry={:#018x}",
                        entry.as_u64()
                    );
                } else {
                    crate::kprintln!("paging: transition_alias_entry=unavailable");
                }
            } else {
                crate::kprintln!("paging: transition_root_error=transition_root_not_identity_mapped");
            }
            match transition_map_result {
                Ok(()) => {
                    crate::kprintln!(
                        "paging: transition_map_base virtual={:#018x} physical={:#018x}",
                        transition_virtual_address.as_u64(),
                        kernel_image.start.as_u64()
                    );
                    match transition_translate_result {
                        Ok(Some(translation)) => crate::kprintln!(
                            "paging: transition_translate physical={:#018x}",
                            translation.physical_address.as_u64()
                        ),
                        Ok(None) => crate::kprintln!("paging: transition_translate missing"),
                        Err(error) => {
                            crate::kprintln!("paging: transition_translate_error={:?}", error)
                        }
                    }
                    match identity_entry_translate_result {
                        Ok(Some(translation)) => crate::kprintln!(
                            "paging: identity_entry_translate physical={:#018x}",
                            translation.physical_address.as_u64()
                        ),
                        Ok(None) => crate::kprintln!("paging: identity_entry_translate missing"),
                        Err(error) => crate::kprintln!(
                            "paging: identity_entry_translate_error={:?}",
                            error
                        ),
                    }
                    match identity_stack_translate_result {
                        Ok(Some(translation)) => crate::kprintln!(
                            "paging: identity_stack_translate physical={:#018x}",
                            translation.physical_address.as_u64()
                        ),
                        Ok(None) => crate::kprintln!("paging: identity_stack_translate missing"),
                        Err(error) => crate::kprintln!(
                            "paging: identity_stack_translate_error={:?}",
                            error
                        ),
                    }
                    match identity_data_translate_result {
                        Ok(Some(translation)) => crate::kprintln!(
                            "paging: identity_data_translate physical={:#018x}",
                            translation.physical_address.as_u64()
                        ),
                        Ok(None) => crate::kprintln!("paging: identity_data_translate missing"),
                        Err(error) => crate::kprintln!(
                            "paging: identity_data_translate_error={:?}",
                            error
                        ),
                    }
                    if let (Some(entry), Some(stack)) =
                        (
                            Some(identity_transition_entry),
                            Some(memory::VirtualAddress::new(transition_stack_top)),
                        )
                    {
                        if transition_data_identity != 0 {
                            let transition_data = unsafe {
                                // SAFETY: the identity data page is kernel-owned bootstrap
                                // memory and is mapped into the transition root before handoff.
                                &mut *(transition_data_identity as *mut BootstrapRuntimeState)
                            };
                            transition_data.magic = TRANSITION_DATA_MAGIC;
                            transition_data.active_root = bootstrap_page_table_frame;
                            transition_data.kernel_window_base = transition_virtual_address.as_u64();
                            transition_data.kernel_window_end =
                                runtime_layout.kernel_window_end(kernel_image).as_u64();
                            transition_data.kernel_pages_mapped = transition_pages_mapped;
                            transition_data.identity_stack = transition_stack_top;
                            transition_data.alias_stack = transition_stack_alias_top;
                            transition_data.stack_pages = TRANSITION_STACK_PAGES;
                            transition_data.data_page = transition_data_identity;
                            transition_data.alias_entry =
                                transition_high_stack_entry_alias.map_or(0, |entry| entry.as_u64());
                            transition_data.alias_gdt =
                                transition_gdt_alias.map_or(0, |base| base.as_u64());
                            transition_data.alias_idt =
                                transition_idt_alias.map_or(0, |base| base.as_u64());
                            transition_data.stage = BootstrapRuntimeStage::Prepared.as_u64();
                        }
                        unsafe {
                            TRANSITION_BOOTSTRAP_CORE_ID = config.bootstrap_core.0;
                            TRANSITION_ALIAS_STACK_TOP = transition_stack_alias_top;
                            TRANSITION_ALIAS_ENTRY =
                                transition_high_stack_entry_alias.map_or(0, |entry| entry.as_u64());
                            TRANSITION_DATA_ALIAS = transition_data_alias_mapped;
                            TRANSITION_GDT_ALIAS =
                                transition_gdt_alias.map_or(0, |base| base.as_u64());
                            TRANSITION_IDT_ALIAS =
                                transition_idt_alias.map_or(0, |base| base.as_u64());
                            TRANSITION_HANDLER_DELTA =
                                runtime_layout.handler_delta(kernel_image);
                        }
                        crate::kprintln!("stage: switching to transition root (identity handoff)");
                        unsafe {
                            // SAFETY: the transition root retains low identity mappings for
                            // the current kernel image, including the dedicated transition
                            // entry and current bootstrap stack.
                            arch::switch_page_table_root_and_jump(
                                bootstrap_page_table_frame,
                                stack.as_u64(),
                                entry.as_u64(),
                            )
                        }
                    } else {
                        crate::kprintln!("paging: transition_switch_error=missing_entry_or_stack");
                    }
                }
                Err(error) => crate::kprintln!("paging: transition_map_error={}", error),
            }
            log_memory_kind_summary(handoff);
            log_reservation_summary(&reservations);
        }
        None => crate::kprintln!("memory: no boot handoff present"),
    }
    crate::kprintln!("stage: early bootstrap complete");

    arch::halt_loop()
}

fn log_memory_kind_summary(handoff: BootHandoff<'_>) {
    let mut usable = 0usize;
    let mut kernel = 0usize;
    let mut reserved = 0usize;
    let mut mmio = 0usize;
    let mut reclaimable = 0usize;

    for region in handoff.memory_map() {
        match region.kind {
            MemoryRegionKind::Usable => usable += 1,
            MemoryRegionKind::Kernel => kernel += 1,
            MemoryRegionKind::Reserved => reserved += 1,
            MemoryRegionKind::Mmio => mmio += 1,
            MemoryRegionKind::BootloaderReclaimable => reclaimable += 1,
        }
    }

    crate::kprintln!(
        "memory: region_kinds usable={} kernel={} reserved={} mmio={} reclaimable={}",
        usable,
        kernel,
        reserved,
        mmio,
        reclaimable
    );
}

fn log_reservation_summary(reservations: &memory::EarlyKernelReservations) {
    crate::kprintln!(
        "memory: reservation_kinds legacy_low={} kernel_image={} active_root={} bootstrap_pt={} bootstrap_per_core={}",
        reservations.count_by_kind(memory::ReservationKind::LegacyLowMemory),
        reservations.count_by_kind(memory::ReservationKind::KernelImage),
        reservations.count_by_kind(memory::ReservationKind::ActivePageTableRoot),
        reservations.count_by_kind(memory::ReservationKind::BootstrapPageTables),
        reservations.count_by_kind(memory::ReservationKind::BootstrapPerCoreState),
    );
}
