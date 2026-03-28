//! Retained bootstrap runtime context shared across early kernel paths.
#![allow(clippy::undocumented_unsafe_blocks)]

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

/// Stores the retained runtime service state.
pub fn store_service(service: RuntimeServiceState) {
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

#[cfg(test)]
fn reset_for_tests() {
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
    }
}

#[cfg(test)]
mod tests {
    use super::{
        RuntimeReadinessState, RuntimeReadySummary, RuntimeServiceCommand, RuntimeServiceHeartbeat,
        RuntimeServiceReport, RuntimeServiceState, dequeue_command, enqueue_command, events,
        push_event, ready_summary, reset_for_tests, runtime_readiness, service, service_heartbeat,
        service_report, store_ready_summary, store_runtime_readiness, store_service,
        store_service_heartbeat, store_service_report,
    };
    use feox_asi::CoreId;

    #[test]
    fn runtime_service_state_round_trips() {
        reset_for_tests();

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
        reset_for_tests();

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
        reset_for_tests();

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
        reset_for_tests();

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
        reset_for_tests();

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
}
