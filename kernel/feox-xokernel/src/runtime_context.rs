//! Retained bootstrap runtime context shared across early kernel paths.

use feox_asi::CoreId;

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
const BOOTSTRAP_EVENT_CAPACITY: usize = 8;
static mut BOOTSTRAP_EVENTS: [Option<&'static str>; BOOTSTRAP_EVENT_CAPACITY] =
    [None; BOOTSTRAP_EVENT_CAPACITY];
static mut BOOTSTRAP_EVENT_COUNT: usize = 0;

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

/// Stores the current retained bootstrap runtime snapshot.
pub fn store(snapshot: RuntimeSnapshot) {
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
    unsafe {
        BOOTSTRAP_CORE_CONTEXT = Some(core);
    }
}

/// Returns the retained bootstrap core context, if one has been recorded.
#[must_use]
pub fn core() -> Option<BootstrapCoreContext> {
    unsafe { BOOTSTRAP_CORE_CONTEXT }
}

/// Stores one retained bootstrap event in a fixed-size rolling buffer.
pub fn push_event(event: &'static str) {
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
