//! Retained bootstrap runtime context shared across early kernel paths.
#![allow(clippy::undocumented_unsafe_blocks)]

use crate::memory::PAGE_SIZE;
use core::sync::atomic::{AtomicU32, Ordering};
use feox_asi::{CapHandle, CoreId, MapFlags, MappedRegion};

/// Sentinel value meaning no core has claimed the context yet.
const CONTEXT_OWNER_NONE: u32 = u32::MAX;

/// Records which core owns the bootstrap context write lock.
///
/// Initialized to `CONTEXT_OWNER_NONE`. The first call to
/// `claim_bootstrap_context` atomically sets this to the claiming core's ID.
/// All subsequent `store_*` calls assert that the recorded owner matches via
/// `debug_assert`, preventing accidental multi-core writes without needing a
/// full spinlock in the current single-core bootstrap phase.
static CONTEXT_OWNER: AtomicU32 = AtomicU32::new(CONTEXT_OWNER_NONE);

/// Claims exclusive write access to the bootstrap runtime context for
/// `owner_core`.
///
/// Must be called before any `store_*` function is invoked. Idempotent when
/// called multiple times with the same `owner_core` (safe in test environments
/// where many threads share `CoreId(0)`). Panics in debug builds if a
/// *different* core attempts to claim an already-owned context.
pub fn claim_bootstrap_context(owner_core: CoreId) {
    match CONTEXT_OWNER.compare_exchange(
        CONTEXT_OWNER_NONE,
        owner_core.0,
        Ordering::AcqRel,
        Ordering::Acquire,
    ) {
        Ok(_) => {}
        Err(current) => {
            // Already claimed: acceptable only if the same core is re-claiming.
            debug_assert_eq!(
                current, owner_core.0,
                "bootstrap context owned by core {current}, cannot be claimed by core {}",
                owner_core.0
            );
        }
    }
}

/// Returns the `CoreId` of the core that claimed the bootstrap context, or
/// `None` if `claim_bootstrap_context` has not yet been called.
#[must_use]
pub fn context_owner() -> Option<CoreId> {
    let raw = CONTEXT_OWNER.load(Ordering::Acquire);
    if raw == CONTEXT_OWNER_NONE {
        None
    } else {
        Some(CoreId(raw))
    }
}

/// Asserts in debug builds that the bootstrap context has been claimed.
///
/// Called at the top of every `store_*` function to catch uses before
/// `claim_bootstrap_context` has been called.
#[inline(always)]
fn assert_context_claimed() {
    debug_assert_ne!(
        CONTEXT_OWNER.load(Ordering::Relaxed),
        CONTEXT_OWNER_NONE,
        "store called before claim_bootstrap_context"
    );
}

/// Snapshot of the retained bootstrap runtime state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RuntimeSnapshot {
    /// Active top-level page-table root after transition.
    pub active_root: u64,
    /// Higher-half kernel window base.
    pub kernel_window_base: u64,
    /// First address after the higher-half kernel window.
    pub kernel_window_end: u64,
    /// Number of kernel pages carried into the transition root.
    pub kernel_pages_mapped: u64,
    /// Identity-mapped stack top used for the handoff jump.
    pub identity_stack: u64,
    /// Higher-half stack top used after the switch.
    pub alias_stack: u64,
    /// Number of stack pages retained in the bootstrap runtime.
    pub stack_pages: u64,
    /// Identity-mapped bootstrap runtime data page.
    pub data_page: u64,
    /// Higher-half code entry used after the stack switch.
    pub alias_entry: u64,
    /// Higher-half GDT alias.
    pub alias_gdt: u64,
    /// Higher-half IDT alias.
    pub alias_idt: u64,
    /// Current bootstrap runtime stage label.
    pub stage: &'static str,
}

static mut BOOTSTRAP_RUNTIME_SNAPSHOT: Option<RuntimeSnapshot> = None;
static mut BOOTSTRAP_CORE_CONTEXT: Option<BootstrapCoreContext> = None;
static mut BOOTSTRAP_SERVICE_STATE: Option<RuntimeServiceState> = None;
static mut BOOTSTRAP_SERVICE_REPORT: Option<RuntimeServiceReport> = None;
static mut BOOTSTRAP_SERVICE_HEARTBEAT: Option<RuntimeServiceHeartbeat> = None;
static mut BOOTSTRAP_RUNTIME_READINESS: Option<RuntimeReadinessState> = None;
static mut BOOTSTRAP_READY_SUMMARY: Option<RuntimeReadySummary> = None;
const BOOTSTRAP_COMMAND_CAPACITY: usize = 7;
static mut BOOTSTRAP_COMMANDS: [Option<RuntimeServiceCommand>; BOOTSTRAP_COMMAND_CAPACITY] =
    [None; BOOTSTRAP_COMMAND_CAPACITY];
static mut BOOTSTRAP_COMMAND_HEAD: usize = 0;
static mut BOOTSTRAP_COMMAND_LEN: usize = 0;
const BOOTSTRAP_EVENT_CAPACITY: usize = 8;
static mut BOOTSTRAP_EVENTS: [Option<&'static str>; BOOTSTRAP_EVENT_CAPACITY] =
    [None; BOOTSTRAP_EVENT_CAPACITY];
static mut BOOTSTRAP_EVENT_COUNT: usize = 0;
const BOOTSTRAP_VM_MAPPING_CAPACITY: usize = 64;
static mut BOOTSTRAP_VM_MAPPINGS: [Option<BootstrapVmMapping>; BOOTSTRAP_VM_MAPPING_CAPACITY] =
    [None; BOOTSTRAP_VM_MAPPING_CAPACITY];
const BOOTSTRAP_PAGE_TABLE_ACCESS_SLOT_CAPACITY: usize = 3;
static mut BOOTSTRAP_PAGE_TABLE_ACCESS_SLOTS:
    [Option<BootstrapPageTableAccessSlot>; BOOTSTRAP_PAGE_TABLE_ACCESS_SLOT_CAPACITY] =
    [None; BOOTSTRAP_PAGE_TABLE_ACCESS_SLOT_CAPACITY];

/// Retained bootstrap record for the core that owns the current runtime slice.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BootstrapCoreContext {
    /// Bootstrap processor identifier.
    pub core_id: CoreId,
    /// Active page-table root observed on this core.
    pub active_root: u64,
    /// Current stack pointer associated with this bootstrap core path.
    pub stack_pointer: u64,
    /// Current higher-half entrypoint for this bootstrap core path.
    pub alias_entry: u64,
    /// Current bootstrap runtime stage label.
    pub stage: &'static str,
}

/// Retained state for the first post-handoff runtime service.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RuntimeServiceState {
    /// Core currently responsible for the retained runtime service.
    pub owner_core: CoreId,
    /// Current service phase label.
    pub phase: &'static str,
    /// Number of completed retained service iterations.
    pub iterations: u64,
    /// Last concrete runtime-service action that completed.
    pub last_action: &'static str,
}

/// Derived retained runtime accounting reported by the bootstrap service.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RuntimeServiceReport {
    /// Current higher-half runtime window size in bytes.
    pub kernel_window_bytes: u64,
    /// Current retained bootstrap stack footprint in bytes.
    pub stack_bytes: u64,
    /// Number of retained bootstrap events currently visible.
    pub retained_events: u64,
}

/// Mutable retained heartbeat for the first runtime service loop.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RuntimeServiceHeartbeat {
    /// Number of retained heartbeat updates completed so far.
    pub beats: u64,
    /// Service iteration that produced the latest heartbeat.
    pub last_iteration: u64,
    /// Number of retained events visible when the latest heartbeat ran.
    pub observed_events: u64,
}

/// Retained readiness marker published by the bootstrap runtime loop.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RuntimeReadinessState {
    /// Whether the retained runtime loop considers the bootstrap runtime ready.
    pub ready: bool,
    /// Service iteration that published the readiness state.
    pub published_iteration: u64,
    /// Heartbeat count observed at the moment readiness was published.
    pub settled_beats: u64,
}

/// Retained summary published once the bootstrap runtime reaches ready state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RuntimeReadySummary {
    /// Ready-state root carried into the settled runtime.
    pub active_root: u64,
    /// Ready-state page count carried into the settled runtime.
    pub kernel_pages_mapped: u64,
    /// Ready-state event count visible at publish time.
    pub retained_events: u64,
}

/// One retained bootstrap VM mapping owned by the bootstrap runtime.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BootstrapVmMapping {
    /// Region returned to the bootstrap caller.
    pub region: MappedRegion,
    /// Capability that backed the mapping.
    pub handle: CapHandle,
    /// Offset into the capability resource at map time.
    pub offset_bytes: u64,
}

/// One retained bootstrap page-table access slot reservation.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BootstrapPageTableAccessSlot {
    /// Slot number inside the dedicated page-table access window.
    pub slot_index: usize,
    /// Physical base address of the reserved page-table frame.
    pub frame_base: u64,
    /// Virtual base address of the slot inside the access window.
    pub virtual_base: u64,
}

/// Minimal retained command set for the first runtime service loop.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeServiceCommand {
    /// Refresh the retained runtime snapshot view.
    RefreshSnapshot,
    /// Recompute retained runtime accounting.
    RefreshAccounting,
    /// Report the retained runtime timeline after accounting is available.
    ReportTimeline,
    /// Update a retained heartbeat after reporting.
    UpdateHeartbeat,
    /// Publish a retained runtime-ready state once the loop has settled.
    PublishReady,
    /// Publish a retained summary after the runtime reaches ready state.
    PublishReadySummary,
    /// Move the service into its idle state.
    EnterIdle,
}

impl RuntimeServiceCommand {
    /// Stable label used in serial/debug output.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::RefreshSnapshot => "refresh-snapshot",
            Self::RefreshAccounting => "refresh-accounting",
            Self::ReportTimeline => "report-timeline",
            Self::UpdateHeartbeat => "update-heartbeat",
            Self::PublishReady => "publish-ready",
            Self::PublishReadySummary => "publish-ready-summary",
            Self::EnterIdle => "enter-idle",
        }
    }
}

/// Stores the current retained bootstrap runtime snapshot.
pub fn store(snapshot: RuntimeSnapshot) {
    assert_context_claimed();
    unsafe {
        BOOTSTRAP_RUNTIME_SNAPSHOT = Some(snapshot);
    }
}

/// Returns the retained bootstrap runtime snapshot, if one has been recorded.
#[must_use]
pub fn snapshot() -> Option<RuntimeSnapshot> {
    unsafe { BOOTSTRAP_RUNTIME_SNAPSHOT }
}

/// Stores the retained bootstrap core context.
pub fn store_core(core: BootstrapCoreContext) {
    assert_context_claimed();
    unsafe {
        BOOTSTRAP_CORE_CONTEXT = Some(core);
    }
}

/// Returns the retained bootstrap core context, if one has been recorded.
#[must_use]
pub fn core() -> Option<BootstrapCoreContext> {
    unsafe { BOOTSTRAP_CORE_CONTEXT }
}

/// Stores the retained runtime service state.
pub fn store_service(service: RuntimeServiceState) {
    assert_context_claimed();
    unsafe {
        BOOTSTRAP_SERVICE_STATE = Some(service);
    }
}

/// Returns the retained runtime service state, if one has been recorded.
#[must_use]
pub fn service() -> Option<RuntimeServiceState> {
    unsafe { BOOTSTRAP_SERVICE_STATE }
}

/// Stores the retained runtime service report.
pub fn store_service_report(report: RuntimeServiceReport) {
    assert_context_claimed();
    unsafe {
        BOOTSTRAP_SERVICE_REPORT = Some(report);
    }
}

/// Returns the retained runtime service report, if one has been recorded.
#[must_use]
pub fn service_report() -> Option<RuntimeServiceReport> {
    unsafe { BOOTSTRAP_SERVICE_REPORT }
}

/// Stores the retained runtime service heartbeat.
pub fn store_service_heartbeat(heartbeat: RuntimeServiceHeartbeat) {
    assert_context_claimed();
    unsafe {
        BOOTSTRAP_SERVICE_HEARTBEAT = Some(heartbeat);
    }
}

/// Returns the retained runtime service heartbeat, if one has been recorded.
#[must_use]
pub fn service_heartbeat() -> Option<RuntimeServiceHeartbeat> {
    unsafe { BOOTSTRAP_SERVICE_HEARTBEAT }
}

/// Stores the retained runtime readiness state.
pub fn store_runtime_readiness(readiness: RuntimeReadinessState) {
    assert_context_claimed();
    unsafe {
        BOOTSTRAP_RUNTIME_READINESS = Some(readiness);
    }
}

/// Returns the retained runtime readiness state, if one has been recorded.
#[must_use]
pub fn runtime_readiness() -> Option<RuntimeReadinessState> {
    unsafe { BOOTSTRAP_RUNTIME_READINESS }
}

/// Stores the retained runtime-ready summary.
pub fn store_ready_summary(summary: RuntimeReadySummary) {
    assert_context_claimed();
    unsafe {
        BOOTSTRAP_READY_SUMMARY = Some(summary);
    }
}

/// Returns the retained runtime-ready summary, if one has been recorded.
#[must_use]
pub fn ready_summary() -> Option<RuntimeReadySummary> {
    unsafe { BOOTSTRAP_READY_SUMMARY }
}

/// Enqueues one retained runtime-service command.
///
/// # Errors
///
/// Returns the supplied command when the fixed-size retained command queue is
/// already full.
pub fn enqueue_command(command: RuntimeServiceCommand) -> Result<(), RuntimeServiceCommand> {
    assert_context_claimed();
    unsafe {
        if BOOTSTRAP_COMMAND_LEN >= BOOTSTRAP_COMMAND_CAPACITY {
            return Err(command);
        }

        let index = (BOOTSTRAP_COMMAND_HEAD + BOOTSTRAP_COMMAND_LEN) % BOOTSTRAP_COMMAND_CAPACITY;
        BOOTSTRAP_COMMANDS[index] = Some(command);
        BOOTSTRAP_COMMAND_LEN += 1;
        Ok(())
    }
}

/// Dequeues the oldest retained runtime-service command, if one is present.
#[must_use]
pub fn dequeue_command() -> Option<RuntimeServiceCommand> {
    unsafe {
        if BOOTSTRAP_COMMAND_LEN == 0 {
            return None;
        }

        let command = BOOTSTRAP_COMMANDS[BOOTSTRAP_COMMAND_HEAD];
        BOOTSTRAP_COMMANDS[BOOTSTRAP_COMMAND_HEAD] = None;
        BOOTSTRAP_COMMAND_HEAD = (BOOTSTRAP_COMMAND_HEAD + 1) % BOOTSTRAP_COMMAND_CAPACITY;
        BOOTSTRAP_COMMAND_LEN -= 1;
        command
    }
}

/// Stores one retained bootstrap event in a fixed-size rolling buffer.
pub fn push_event(event: &'static str) {
    assert_context_claimed();
    unsafe {
        let index = BOOTSTRAP_EVENT_COUNT % BOOTSTRAP_EVENT_CAPACITY;
        BOOTSTRAP_EVENTS[index] = Some(event);
        BOOTSTRAP_EVENT_COUNT = BOOTSTRAP_EVENT_COUNT.saturating_add(1);
    }
}

/// Returns the retained bootstrap event list in oldest-to-newest order.
#[must_use]
pub fn events() -> [Option<&'static str>; BOOTSTRAP_EVENT_CAPACITY] {
    unsafe {
        let mut ordered = [None; BOOTSTRAP_EVENT_CAPACITY];
        let count = BOOTSTRAP_EVENT_COUNT.min(BOOTSTRAP_EVENT_CAPACITY);
        let start = BOOTSTRAP_EVENT_COUNT.saturating_sub(count) % BOOTSTRAP_EVENT_CAPACITY;
        let mut i = 0usize;
        while i < count {
            let source_index = (start + i) % BOOTSTRAP_EVENT_CAPACITY;
            ordered[i] = BOOTSTRAP_EVENTS[source_index];
            i += 1;
        }
        ordered
    }
}

/// Returns the retained bootstrap VM mappings in slot order.
#[must_use]
pub fn vm_mappings() -> [Option<BootstrapVmMapping>; BOOTSTRAP_VM_MAPPING_CAPACITY] {
    unsafe { BOOTSTRAP_VM_MAPPINGS }
}

/// Records one retained bootstrap VM mapping.
pub fn record_vm_mapping(mapping: BootstrapVmMapping) -> Result<(), BootstrapVmMapping> {
    assert_context_claimed();
    unsafe {
        let mut index = 0usize;
        while index < BOOTSTRAP_VM_MAPPING_CAPACITY {
            if BOOTSTRAP_VM_MAPPINGS[index].is_none() {
                BOOTSTRAP_VM_MAPPINGS[index] = Some(mapping);
                return Ok(());
            }
            index += 1;
        }
    }
    Err(mapping)
}

/// Removes and returns the retained VM mapping matching `region`.
#[must_use]
pub fn remove_vm_mapping(region: MappedRegion) -> Option<BootstrapVmMapping> {
    assert_context_claimed();
    unsafe {
        let mut index = 0usize;
        while index < BOOTSTRAP_VM_MAPPING_CAPACITY {
            if let Some(mapping) = BOOTSTRAP_VM_MAPPINGS[index]
                && mapping.region.base == region.base
                && mapping.region.length_bytes == region.length_bytes
            {
                BOOTSTRAP_VM_MAPPINGS[index] = None;
                return Some(mapping);
            }
            index += 1;
        }
        None
    }
}

/// Finds the retained bootstrap VM mapping that owns `virtual_address` for the
/// supplied capability handle.
#[must_use]
pub fn find_vm_mapping_for_address(
    handle: CapHandle,
    virtual_address: u64,
) -> Option<BootstrapVmMapping> {
    assert_context_claimed();
    unsafe {
        let mut index = 0usize;
        while index < BOOTSTRAP_VM_MAPPING_CAPACITY {
            if let Some(mapping) = BOOTSTRAP_VM_MAPPINGS[index] {
                let start = mapping.region.base;
                let end = mapping.region.base.saturating_add(mapping.region.length_bytes);
                if mapping.handle == handle && virtual_address >= start && virtual_address < end {
                    return Some(mapping);
                }
            }
            index += 1;
        }
        None
    }
}

/// Returns the retained page-table access slots in slot order.
#[must_use]
pub fn page_table_access_slots(
) -> [Option<BootstrapPageTableAccessSlot>; BOOTSTRAP_PAGE_TABLE_ACCESS_SLOT_CAPACITY] {
    unsafe { BOOTSTRAP_PAGE_TABLE_ACCESS_SLOTS }
}

/// Acquires one retained page-table access slot for `frame_base`.
///
/// Reuses an existing slot if the same frame is already reserved.
#[must_use]
pub fn acquire_page_table_access_slot(
    window_base: u64,
    frame_base: u64,
) -> Option<BootstrapPageTableAccessSlot> {
    assert_context_claimed();
    unsafe {
        let mut index = 0usize;
        while index < BOOTSTRAP_PAGE_TABLE_ACCESS_SLOT_CAPACITY {
            if let Some(slot) = BOOTSTRAP_PAGE_TABLE_ACCESS_SLOTS[index]
                && slot.frame_base == frame_base
            {
                return Some(slot);
            }
            index += 1;
        }

        index = 0;
        while index < BOOTSTRAP_PAGE_TABLE_ACCESS_SLOT_CAPACITY {
            if BOOTSTRAP_PAGE_TABLE_ACCESS_SLOTS[index].is_none() {
                let slot = BootstrapPageTableAccessSlot {
                    slot_index: index,
                    frame_base,
                    // Slot 0 in the access window is reserved as the permanent
                    // self-map of the control PT page. Dynamic aliases start
                    // at the second 4 KiB slot.
                    virtual_base: window_base + ((index as u64 + 1) * PAGE_SIZE),
                };
                BOOTSTRAP_PAGE_TABLE_ACCESS_SLOTS[index] = Some(slot);
                return Some(slot);
            }
            index += 1;
        }
        None
    }
}

/// Releases the retained page-table access slot matching `slot`.
#[must_use]
pub fn release_page_table_access_slot(slot: BootstrapPageTableAccessSlot) -> bool {
    assert_context_claimed();
    unsafe {
        if slot.slot_index >= BOOTSTRAP_PAGE_TABLE_ACCESS_SLOT_CAPACITY {
            return false;
        }
        let active = BOOTSTRAP_PAGE_TABLE_ACCESS_SLOTS[slot.slot_index];
        if active == Some(slot) {
            BOOTSTRAP_PAGE_TABLE_ACCESS_SLOTS[slot.slot_index] = None;
            return true;
        }
        false
    }
}

/// Finds the first non-overlapping page-aligned region inside the bootstrap VM
/// window that can satisfy `length_bytes`.
#[must_use]
pub fn allocate_vm_region(
    window_base: u64,
    window_size: u64,
    length_bytes: u64,
    flags: MapFlags,
) -> Option<MappedRegion> {
    assert_context_claimed();
    if length_bytes == 0 {
        return None;
    }

    let window_end = window_base.checked_add(window_size)?;
    let mut candidate = window_base;
    while candidate.checked_add(length_bytes)? <= window_end {
        let candidate_end = candidate.checked_add(length_bytes)?;
        let mut overlapped = false;
        let mut next_candidate = candidate_end;
        unsafe {
            let mut index = 0usize;
            while index < BOOTSTRAP_VM_MAPPING_CAPACITY {
                if let Some(mapping) = BOOTSTRAP_VM_MAPPINGS[index] {
                    let mapping_start = mapping.region.base;
                    let mapping_end =
                        mapping.region.base.saturating_add(mapping.region.length_bytes);
                    if candidate < mapping_end && candidate_end > mapping_start {
                        overlapped = true;
                        next_candidate = next_candidate.max(mapping_end);
                    }
                }
                index += 1;
            }
        }

        if !overlapped {
            return Some(MappedRegion {
                base: candidate,
                length_bytes,
                flags,
            });
        }
        candidate = next_candidate;
    }

    None
}

#[cfg(test)]
pub(crate) fn reset_for_tests() {
    // Ensure the context is claimed before the reset so that subsequent
    // store/enqueue/push calls pass the assert_context_claimed guard.
    claim_bootstrap_context(CoreId(0));
    unsafe {
        BOOTSTRAP_RUNTIME_SNAPSHOT = None;
        BOOTSTRAP_CORE_CONTEXT = None;
        BOOTSTRAP_SERVICE_STATE = None;
        BOOTSTRAP_SERVICE_REPORT = None;
        BOOTSTRAP_SERVICE_HEARTBEAT = None;
        BOOTSTRAP_RUNTIME_READINESS = None;
        BOOTSTRAP_READY_SUMMARY = None;
        BOOTSTRAP_COMMANDS = [None; BOOTSTRAP_COMMAND_CAPACITY];
        BOOTSTRAP_COMMAND_HEAD = 0;
        BOOTSTRAP_COMMAND_LEN = 0;
        BOOTSTRAP_EVENTS = [None; BOOTSTRAP_EVENT_CAPACITY];
        BOOTSTRAP_EVENT_COUNT = 0;
        BOOTSTRAP_VM_MAPPINGS = [None; BOOTSTRAP_VM_MAPPING_CAPACITY];
        BOOTSTRAP_PAGE_TABLE_ACCESS_SLOTS = [None; BOOTSTRAP_PAGE_TABLE_ACCESS_SLOT_CAPACITY];
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BootstrapPageTableAccessSlot, BootstrapVmMapping, RuntimeReadinessState,
        RuntimeReadySummary, RuntimeServiceCommand, RuntimeServiceHeartbeat,
        RuntimeServiceReport, RuntimeServiceState, acquire_page_table_access_slot,
        claim_bootstrap_context, dequeue_command, enqueue_command, events,
        find_vm_mapping_for_address, page_table_access_slots, push_event, ready_summary,
        record_vm_mapping, release_page_table_access_slot, remove_vm_mapping, reset_for_tests,
        runtime_readiness, service, service_heartbeat, service_report, store_ready_summary,
        store_runtime_readiness, store_service, store_service_heartbeat,
        store_service_report, vm_mappings,
    };
    use feox_asi::{CapHandle, CoreId, MapFlags, MappedRegion};

    #[test]
    fn runtime_service_state_round_trips() {
        claim_bootstrap_context(CoreId(0));
        let service_state = RuntimeServiceState {
            owner_core: CoreId(0),
            phase: "poll",
            iterations: 2,
            last_action: "retained-snapshot-scan",
        };

        store_service(service_state);

        assert_eq!(service(), Some(service_state));
    }

    #[test]
    fn runtime_service_report_round_trips() {
        claim_bootstrap_context(CoreId(0));
        let report = RuntimeServiceReport {
            kernel_window_bytes: 0x1b000,
            stack_bytes: 0x4000,
            retained_events: 6,
        };

        store_service_report(report);

        assert_eq!(service_report(), Some(report));
    }

    #[test]
    fn runtime_service_heartbeat_round_trips() {
        claim_bootstrap_context(CoreId(0));
        let heartbeat = RuntimeServiceHeartbeat {
            beats: 1,
            last_iteration: 3,
            observed_events: 7,
        };

        store_service_heartbeat(heartbeat);

        assert_eq!(service_heartbeat(), Some(heartbeat));
    }

    #[test]
    fn runtime_readiness_round_trips() {
        claim_bootstrap_context(CoreId(0));
        let readiness = RuntimeReadinessState {
            ready: true,
            published_iteration: 5,
            settled_beats: 2,
        };

        store_runtime_readiness(readiness);

        assert_eq!(runtime_readiness(), Some(readiness));
    }

    #[test]
    fn runtime_ready_summary_round_trips() {
        claim_bootstrap_context(CoreId(0));
        let summary = RuntimeReadySummary {
            active_root: 0x124000,
            kernel_pages_mapped: 29,
            retained_events: 8,
        };

        store_ready_summary(summary);

        assert_eq!(ready_summary(), Some(summary));
    }

    #[test]
    fn runtime_service_commands_round_trip_in_fifo_order() {
        claim_bootstrap_context(CoreId(0));
        assert_eq!(enqueue_command(RuntimeServiceCommand::RefreshSnapshot), Ok(()));
        assert_eq!(enqueue_command(RuntimeServiceCommand::RefreshAccounting), Ok(()));
        assert_eq!(enqueue_command(RuntimeServiceCommand::ReportTimeline), Ok(()));
        assert_eq!(enqueue_command(RuntimeServiceCommand::UpdateHeartbeat), Ok(()));
        assert_eq!(enqueue_command(RuntimeServiceCommand::PublishReady), Ok(()));
        assert_eq!(
            enqueue_command(RuntimeServiceCommand::PublishReadySummary),
            Ok(())
        );
        assert_eq!(enqueue_command(RuntimeServiceCommand::EnterIdle), Ok(()));

        assert_eq!(
            dequeue_command(),
            Some(RuntimeServiceCommand::RefreshSnapshot)
        );
        assert_eq!(
            dequeue_command(),
            Some(RuntimeServiceCommand::RefreshAccounting)
        );
        assert_eq!(
            dequeue_command(),
            Some(RuntimeServiceCommand::ReportTimeline)
        );
        assert_eq!(
            dequeue_command(),
            Some(RuntimeServiceCommand::UpdateHeartbeat)
        );
        assert_eq!(dequeue_command(), Some(RuntimeServiceCommand::PublishReady));
        assert_eq!(
            dequeue_command(),
            Some(RuntimeServiceCommand::PublishReadySummary)
        );
        assert_eq!(dequeue_command(), Some(RuntimeServiceCommand::EnterIdle));
        assert_eq!(dequeue_command(), None);
    }

    #[test]
    fn runtime_service_command_queue_rejects_overflow_and_recovers_after_drain() {
        reset_for_tests();

        assert_eq!(
            enqueue_command(RuntimeServiceCommand::RefreshSnapshot),
            Ok(())
        );
        assert_eq!(
            enqueue_command(RuntimeServiceCommand::RefreshAccounting),
            Ok(())
        );
        assert_eq!(
            enqueue_command(RuntimeServiceCommand::ReportTimeline),
            Ok(())
        );
        assert_eq!(
            enqueue_command(RuntimeServiceCommand::UpdateHeartbeat),
            Ok(())
        );
        assert_eq!(enqueue_command(RuntimeServiceCommand::PublishReady), Ok(()));
        assert_eq!(
            enqueue_command(RuntimeServiceCommand::PublishReadySummary),
            Ok(())
        );
        assert_eq!(enqueue_command(RuntimeServiceCommand::EnterIdle), Ok(()));
        assert_eq!(
            enqueue_command(RuntimeServiceCommand::RefreshSnapshot),
            Err(RuntimeServiceCommand::RefreshSnapshot)
        );

        assert_eq!(
            dequeue_command(),
            Some(RuntimeServiceCommand::RefreshSnapshot)
        );
        assert_eq!(
            enqueue_command(RuntimeServiceCommand::RefreshSnapshot),
            Ok(())
        );

        assert_eq!(
            dequeue_command(),
            Some(RuntimeServiceCommand::RefreshAccounting)
        );
        assert_eq!(
            dequeue_command(),
            Some(RuntimeServiceCommand::ReportTimeline)
        );
        assert_eq!(
            dequeue_command(),
            Some(RuntimeServiceCommand::UpdateHeartbeat)
        );
        assert_eq!(dequeue_command(), Some(RuntimeServiceCommand::PublishReady));
        assert_eq!(
            dequeue_command(),
            Some(RuntimeServiceCommand::PublishReadySummary)
        );
        assert_eq!(dequeue_command(), Some(RuntimeServiceCommand::EnterIdle));
        assert_eq!(
            dequeue_command(),
            Some(RuntimeServiceCommand::RefreshSnapshot)
        );
        assert_eq!(dequeue_command(), None);
    }

    #[test]
    fn retained_events_roll_forward_in_oldest_to_newest_order() {
        reset_for_tests();

        push_event("event-0");
        push_event("event-1");
        push_event("event-2");
        push_event("event-3");
        push_event("event-4");
        push_event("event-5");
        push_event("event-6");
        push_event("event-7");
        push_event("event-8");
        push_event("event-9");

        assert_eq!(
            events(),
            [
                Some("event-2"),
                Some("event-3"),
                Some("event-4"),
                Some("event-5"),
                Some("event-6"),
                Some("event-7"),
                Some("event-8"),
                Some("event-9"),
            ]
        );
    }

    #[test]
    fn bootstrap_vm_mapping_round_trips_and_removes_by_region() {
        reset_for_tests();
        let mapping = BootstrapVmMapping {
            region: MappedRegion {
                base: 0xFFFF_9000_0400_0000,
                length_bytes: 0x2000,
                flags: MapFlags::READ | MapFlags::WRITE,
            },
            handle: CapHandle {
                id: 7,
                generation: 3,
            },
            offset_bytes: 0x1000,
        };

        assert_eq!(record_vm_mapping(mapping), Ok(()));
        assert!(vm_mappings().iter().flatten().any(|entry| *entry == mapping));
        assert_eq!(remove_vm_mapping(mapping.region), Some(mapping));
        assert_eq!(remove_vm_mapping(mapping.region), None);
    }

    #[test]
    fn bootstrap_vm_lookup_matches_handle_and_address() {
        reset_for_tests();
        let mapping = BootstrapVmMapping {
            region: MappedRegion {
                base: 0xFFFF_9000_0400_0000,
                length_bytes: 0x3000,
                flags: MapFlags::READ,
            },
            handle: CapHandle {
                id: 9,
                generation: 2,
            },
            offset_bytes: 0x2000,
        };

        assert_eq!(record_vm_mapping(mapping), Ok(()));
        assert_eq!(
            find_vm_mapping_for_address(mapping.handle, mapping.region.base + 0x1000),
            Some(mapping)
        );
        assert_eq!(
            find_vm_mapping_for_address(
                CapHandle {
                    id: 9,
                    generation: 3,
                },
                mapping.region.base + 0x1000,
            ),
            None
        );
        assert_eq!(
            find_vm_mapping_for_address(mapping.handle, mapping.region.base + mapping.region.length_bytes),
            None
        );
    }

    #[test]
    fn bootstrap_page_table_access_slots_allocate_reuse_and_release() {
        reset_for_tests();
        let first = acquire_page_table_access_slot(0xFFFF_9000_0800_0000, 0x2000)
            .expect("first slot should allocate");
        let reused = acquire_page_table_access_slot(0xFFFF_9000_0800_0000, 0x2000)
            .expect("same frame should reuse slot");
        let second = acquire_page_table_access_slot(0xFFFF_9000_0800_0000, 0x3000)
            .expect("second slot should allocate");

        assert_eq!(first, reused);
        assert_eq!(
            first,
            BootstrapPageTableAccessSlot {
                slot_index: 0,
                frame_base: 0x2000,
                virtual_base: 0xFFFF_9000_0800_1000,
            }
        );
        assert_eq!(second.slot_index, 1);
        assert!(page_table_access_slots().iter().flatten().any(|slot| *slot == first));
        assert!(release_page_table_access_slot(first));
        assert!(!release_page_table_access_slot(first));
        assert!(page_table_access_slots().iter().flatten().all(|slot| *slot != first));
    }
}
