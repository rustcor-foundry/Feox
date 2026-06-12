//! Threads + preemptive round-robin scheduler (milestone 14).
//!
//! A fixed table of kernel-managed U-mode threads, context-switched at trap
//! level: every thread's full register state lives in a [`TrapFrame`], and a
//! switch is just `save *frame -> old TCB; *frame = new TCB` inside the trap
//! dispatcher — the trap stub's restore path (which reloads `sepc`, `sstatus`,
//! and `x2` from the frame) does the actual resumption. Preemption comes from
//! the M9 supervisor timer; the entry/exit longjmp is the M12 `enter_user` /
//! `exit_to_kernel` primitive, exactly as planned.
//!
//! Scope: threads share the kernel address space (each gets its own U-mapped
//! code/stack window in the unused 4 GiB gigapage region). Per-process
//! isolated address spaces arrive with the ELF loader milestone. Single boot
//! hart only — all scheduler state is hart-local by the same invariant as
//! `frame.rs`.

use super::trap::{REG_A0, REG_A1, REG_SP, SSTATUS_SPP, TrapFrame};
use super::{frame, paging, time, umode};

/// `sstatus.SIE` (bit 1): S-mode global interrupt enable.
const SSTATUS_SIE: usize = 1 << 1;
/// `sstatus.SPIE` (bit 5): interrupts become enabled when `sret` drops to U.
const SSTATUS_SPIE: usize = 1 << 5;
/// `sstatus.SUM` (bit 18): S-mode may access U-accessible memory.
const SSTATUS_SUM: usize = 1 << 18;

/// Scheduler tick rate while threads run (10 ms slices).
const SCHED_TICK_HZ: u64 = 100;
/// Ticks after which a run is force-stopped, bounding the demo's wall-clock
/// time independently of host speed (~120 ms at 100 Hz).
const TICK_BUDGET: u64 = 12;

const MAX_THREADS: usize = 4;

/// Each thread's user window: code page at the base, stack page at +0x4000
/// (stack top +0x5000), spaced 0x10000 apart, above the single-excursion
/// window used by `umode::run_user_program`.
const USER_WINDOW_BASE: usize = 0x1_0001_0000;
const USER_WINDOW_STRIDE: usize = 0x10000;
const STACK_OFFSET: usize = 0x4000;

#[derive(Clone, Copy, Eq, PartialEq)]
enum State {
    Free,
    Ready,
    Exited,
}

struct Thread {
    state: State,
    /// Complete saved register state; what the trap stub resumes.
    frame: TrapFrame,
    /// `satp` this thread runs under (the kernel root for raw threads, a
    /// per-process root for ELF processes). Written on switch when it differs.
    satp: usize,
    code_frame: usize,
    stack_frame: usize,
    exit_value: usize,
    preemptions: u64,
    yields: u64,
}

const EMPTY_FRAME: TrapFrame = TrapFrame {
    regs: [0; 31],
    sepc: 0,
    sstatus: 0,
    scause: 0,
    stval: 0,
};

impl Thread {
    const fn free() -> Self {
        Self {
            state: State::Free,
            frame: EMPTY_FRAME,
            satp: 0,
            code_frame: 0,
            stack_frame: 0,
            exit_value: 0,
            preemptions: 0,
            yields: 0,
        }
    }
}

struct Scheduler {
    threads: [Thread; MAX_THREADS],
    current: usize,
    active: bool,
    ticks_used: u64,
}

/// Global scheduler state.
///
/// Invariant: touched only by the boot hart — from `riscv_main` while no
/// thread runs, and from the trap path (interrupts masked) while one does.
static mut SCHEDULER: Scheduler = Scheduler {
    threads: [const { Thread::free() }; MAX_THREADS],
    current: 0,
    active: false,
    ticks_used: 0,
};

/// Returns an exclusive reference to the scheduler.
#[allow(static_mut_refs)]
fn sched() -> &'static mut Scheduler {
    // SAFETY: single boot hart (see the invariant above); the main flow and
    // the trap path never run concurrently because interrupts are masked
    // during trap handling and the main flow is suspended while threads run.
    unsafe { &mut SCHEDULER }
}

/// Whether a scheduling run is in progress (threads own the hart).
#[must_use]
pub fn active() -> bool {
    sched().active
}

/// Reads `sstatus` (for building fresh thread frames).
fn read_sstatus() -> usize {
    let value: usize;
    // SAFETY: csrr from sstatus has no side effects.
    unsafe { core::arch::asm!("csrr {0}, sstatus", out(reg) value, options(nomem, nostack)) };
    value
}

/// Creates a thread from raw instruction words: allocates code/stack frames,
/// maps them U-accessible in the thread's window, and builds the initial
/// [`TrapFrame`] (entry pc, stack top, U-mode `sstatus` with SPIE + SUM).
/// Returns the slot index, or `None` when the table or frame pool is full.
pub fn spawn(words: &[u32]) -> Option<usize> {
    debug_assert!(words.len() * 4 <= frame::FRAME_SIZE);
    let slot = sched().threads.iter().position(|t| t.state == State::Free)?;
    let code = frame::alloc()?;
    let Some(stack) = frame::alloc() else {
        frame::free(code);
        return None;
    };

    // SAFETY: `code` is a fresh, identity-mapped, writable frame and the
    // program fits in it (asserted above).
    unsafe {
        let p = code as *mut u32;
        for (i, word) in words.iter().enumerate() {
            p.add(i).write_volatile(*word);
        }
        core::arch::asm!("fence.i", options(nostack));
    }

    let base = USER_WINDOW_BASE + slot * USER_WINDOW_STRIDE;
    let mut space = paging::AddressSpace::from_active();
    space.map(base, code, 4096, paging::PTE_U | paging::PTE_R | paging::PTE_X);
    space.map(
        base + STACK_OFFSET,
        stack,
        4096,
        paging::PTE_U | paging::PTE_R | paging::PTE_W,
    );
    paging::flush_tlb_all();

    init_slot(slot, base, base + STACK_OFFSET + frame::FRAME_SIZE, paging::read_satp());
    let thread = &mut sched().threads[slot];
    thread.code_frame = code;
    thread.stack_frame = stack;
    Some(slot)
}

/// Creates a thread whose code/stack the caller has already mapped (e.g. an
/// ELF process loaded into its own address space). The caller owns the
/// backing frames and the space; release the slot with [`clear_slot`] after
/// the run. Returns the slot index, or `None` when the table is full.
pub fn spawn_at(entry: usize, stack_top: usize, satp: usize) -> Option<usize> {
    let slot = sched().threads.iter().position(|t| t.state == State::Free)?;
    init_slot(slot, entry, stack_top, satp);
    Some(slot)
}

/// Initializes `slot` as Ready with a crafted U-mode entry frame.
fn init_slot(slot: usize, entry: usize, stack_top: usize, satp: usize) {
    let thread = &mut sched().threads[slot];
    *thread = Thread::free();
    thread.state = State::Ready;
    thread.satp = satp;
    thread.frame.sepc = entry;
    thread.frame.regs[REG_SP] = stack_top;
    // sret target: U-mode (SPP=0), interrupts on once there (SPIE), and SUM
    // so syscall argument access keeps working. SIE must be 0, like every
    // hardware-saved frame, or the restore stub's `csrw sstatus` would
    // re-enable interrupts before its sret.
    thread.frame.sstatus =
        (read_sstatus() & !(SSTATUS_SPP | SSTATUS_SIE)) | SSTATUS_SPIE | SSTATUS_SUM;
}

/// Per-thread stats: `(exited, exit_value, preemptions, yields)`. `None` for
/// a Free slot.
#[must_use]
pub fn stats(slot: usize) -> Option<(bool, usize, u64, u64)> {
    let t = sched().threads.get(slot)?;
    if t.state == State::Free {
        return None;
    }
    Some((t.state == State::Exited, t.exit_value, t.preemptions, t.yields))
}

/// Releases a slot whose backing resources the caller owns (`spawn_at`
/// threads). Must not be called during a run.
pub fn clear_slot(slot: usize) {
    debug_assert!(!sched().active);
    sched().threads[slot] = Thread::free();
}

/// Next Ready slot after (and wrapping past) `from`, including `from` itself.
fn next_ready(s: &Scheduler, from: usize) -> Option<usize> {
    (1..=MAX_THREADS)
        .map(|offset| (from + offset) % MAX_THREADS)
        .find(|&slot| s.threads[slot].state == State::Ready)
}

/// Switches the live trap frame to `slot` (saving it to the current thread
/// first when `save` is set), and the address space when it differs. The
/// trap handler keeps working across the satp write because every thread's
/// root maps the kernel (raw threads use the kernel root; process roots clone
/// its top-level entries).
fn switch_to(s: &mut Scheduler, frame: &mut TrapFrame, slot: usize, save: bool) {
    if save {
        s.threads[s.current].frame = *frame;
    }
    s.current = slot;
    *frame = s.threads[slot].frame;
    let target = s.threads[slot].satp;
    if paging::read_satp() != target {
        // SAFETY: see above — kernel mappings are present in every thread's
        // root, and the user frame about to be resumed belongs to `target`.
        unsafe { paging::write_satp(target) };
    }
}

/// Ends the run from trap context: capture the interrupted thread's state,
/// mask the timer, and longjmp back to [`run`]'s `enter_user` call.
fn stop(s: &mut Scheduler, frame: &TrapFrame, save_current: bool) -> ! {
    if save_current {
        s.threads[s.current].frame = *frame;
    }
    s.active = false;
    time::disable();
    // SAFETY: a scheduling run is in progress, so run()'s enter_user is on
    // the stack; this longjmps back to it.
    unsafe { umode::exit_to_kernel(0) }
}

/// Timer hook (called from the trap dispatcher after the tick is re-armed).
/// Rotates to the next ready thread when the tick preempted U-mode code.
pub fn on_tick(frame: &mut TrapFrame) {
    let s = sched();
    if !s.active {
        return;
    }
    if frame.sstatus & SSTATUS_SPP != 0 {
        // The tick landed in the brief S-mode window inside run() before the
        // first thread was entered; nothing to preempt.
        return;
    }
    s.ticks_used += 1;
    if s.ticks_used >= TICK_BUDGET {
        stop(s, frame, true);
    }
    s.threads[s.current].preemptions += 1;
    if let Some(next) = next_ready(s, s.current) {
        switch_to(s, frame, next, true);
    }
}

/// `ProcYield` from a running thread: success result, advance past the
/// `ecall`, rotate to the next ready thread.
pub fn yield_current(frame: &mut TrapFrame) {
    let s = sched();
    frame.regs[REG_A0] = 0;
    frame.regs[REG_A1] = 0;
    frame.sepc += 4;
    s.threads[s.current].yields += 1;
    if let Some(next) = next_ready(s, s.current) {
        switch_to(s, frame, next, true);
    }
}

/// `ProcExit` from a running thread: record the exit value and either resume
/// the next ready thread or end the run.
pub fn exit_current(frame: &mut TrapFrame, value: usize) {
    let s = sched();
    let current = s.current;
    s.threads[current].state = State::Exited;
    s.threads[current].exit_value = value;
    s.threads[current].frame = *frame;
    crate::kprintln!("[feox] sched: thread {} exit({})", current, value);
    match next_ready(s, current) {
        Some(next) => switch_to(s, frame, next, false),
        None => stop(s, frame, false),
    }
}

/// Runs all Ready threads until they exit or the tick budget lapses. Entered
/// via `enter_user` into the first ready thread; ends when stop() longjmps
/// back here. The caller's address space is restored before returning, so
/// teardown (e.g. `destroy_user`) always compares against the kernel root.
pub fn run(timebase_hz: u64) {
    let s = sched();
    let Some(first) = next_ready(s, MAX_THREADS - 1) else {
        crate::kprintln!("[feox] sched: no ready threads");
        return;
    };
    s.current = first;
    s.ticks_used = 0;
    s.active = true;

    let entry = s.threads[first].frame.sepc;
    let stack_top = s.threads[first].frame.regs[REG_SP];
    let home_satp = paging::read_satp();
    if s.threads[first].satp != home_satp {
        // SAFETY: process roots clone the kernel top-level entries, so the
        // code here (and the trap path) stays mapped across the switch.
        unsafe { paging::write_satp(s.threads[first].satp) };
    }
    time::enable(timebase_hz, SCHED_TICK_HZ);
    // SAFETY: thread `first`'s code/stack windows are mapped U-accessible in
    // its space; the run ends with a longjmp back here from stop().
    unsafe { umode::enter_user(entry, stack_top) };
    // (time::disable() already ran in stop(); s.active is false again.)
    if paging::read_satp() != home_satp {
        // SAFETY: returning to the space that was live when run() was called.
        unsafe { paging::write_satp(home_satp) };
    }
}

/// Milestone 14 demo: two yielding threads that exit with distinct values and
/// one spinner that never exits, preempted by the timer until the tick budget
/// ends the run.
pub fn demo(timebase_hz: u64) {
    // Yielder: three ProcYields, then ProcExit(100 + n). Sets every register
    // it reads, so it may also be the slot entered via enter_user (whose
    // non-frame registers are undefined).
    let yielder = |exit_value: u32| {
        [
            0x3020_0893,                      // li a7, 0x302 (ProcYield)
            0x0000_0073,                      // ecall
            0x0000_0073,                      // ecall
            0x0000_0073,                      // ecall
            0x0000_0513 | (exit_value << 20), // li a0, exit_value
            0x3010_0893,                      // li a7, 0x301 (ProcExit)
            0x0000_0073,                      // ecall
            0x0000_006f,                      // 1: j 1b (unreached)
        ]
    };
    // Spinner: counts in a0 forever; only the timer takes the hart back. Must
    // not be the first-entered slot (it relies on a0 starting at 0 from its
    // zeroed TrapFrame).
    let spinner = [
        0x0015_0513u32, // 1: addi a0, a0, 1
        0xffdf_f06f,    // j 1b
    ];

    let t0 = spawn(&yielder(100));
    let t1 = spawn(&yielder(101));
    let t2 = spawn(&spinner);
    if t0.is_none() || t1.is_none() || t2.is_none() {
        crate::kprintln!("[feox] sched: out of frames/slots for the demo");
        return;
    }
    crate::kprintln!(
        "[feox] sched: 3 threads spawned (2 yielders + 1 spinner); preempting at {} Hz...",
        SCHED_TICK_HZ
    );

    run(timebase_hz);

    // Collect stats and tear down.
    let s = sched();
    let mut exits_ok = true;
    let mut preemptions = 0u64;
    let mut yields = 0u64;
    let mut spinner_count = 0usize;
    for (slot, expected) in [(t0.unwrap(), Some(100)), (t1.unwrap(), Some(101)), (t2.unwrap(), None)] {
        let t = &s.threads[slot];
        preemptions += t.preemptions;
        yields += t.yields;
        match expected {
            Some(value) => exits_ok &= t.state == State::Exited && t.exit_value == value,
            None => spinner_count = t.frame.regs[REG_A0],
        }
    }
    let mut space = paging::AddressSpace::from_active();
    for slot in 0..MAX_THREADS {
        let t = &mut s.threads[slot];
        // Only reap raw-spawned threads (they own kernel-space windows and
        // frames); spawn_at slots belong to their caller.
        if t.state == State::Free || t.code_frame == 0 {
            continue;
        }
        let base = USER_WINDOW_BASE + slot * USER_WINDOW_STRIDE;
        space.unmap(base, 4096);
        space.unmap(base + STACK_OFFSET, 4096);
        frame::free(t.code_frame);
        frame::free(t.stack_frame);
        *t = Thread::free();
    }
    paging::flush_tlb_all();

    let ok = exits_ok && spinner_count > 0 && preemptions > 0;
    crate::kprintln!(
        "[feox] milestone 14: preemptive scheduler (exits 100/101={}, spinner spun {}, yields={}, preemptions={}, ok={}).",
        exits_ok,
        spinner_count,
        yields,
        preemptions,
        ok
    );
}
