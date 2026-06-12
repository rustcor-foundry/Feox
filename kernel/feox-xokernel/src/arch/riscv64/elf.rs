//! Minimal ELF64 loader + per-process address spaces (milestone 15).
//!
//! A hand-rolled static-executable loader (in keeping with the rest of this
//! backend): validate the ELF64 header, walk the program headers, and copy
//! each `PT_LOAD` segment into fresh frames mapped U-accessible in a
//! per-process [`AddressSpace`] with the segment's R/W/X permissions
//! (`p_memsz > p_filesz` tails are zero-filled, so `.bss` works).
//!
//! Process spaces come from `AddressSpace::new_user()` (a root cloning the
//! kernel's top-level entries, so the trap path works under any process satp)
//! and process VAs live in a top-level slot the kernel never touches — two
//! processes can therefore map the *same* virtual addresses onto different
//! physical frames, which the demo proves directly.
//!
//! Scope: ET_EXEC only (no relocation/dynamic linking), segments assumed not
//! to share pages (our linker scripts page-align segments). The libOS/app
//! delivery milestone (M16) feeds toolchain-built executables through this
//! same path.

use alloc::vec::Vec;

use super::{frame, paging, sched};
use paging::AddressSpace;

// ELF constants (the subset a static riscv64 executable needs).
const ELF_MAGIC: [u8; 4] = [0x7f, b'E', b'L', b'F'];
const ELFCLASS64: u8 = 2;
const ELFDATA2LSB: u8 = 1;
const ET_EXEC: u16 = 2;
const EM_RISCV: u16 = 243;
const PT_LOAD: u32 = 1;
const PF_X: u32 = 1;
const PF_W: u32 = 2;
const PF_R: u32 = 4;

const EHDR_SIZE: usize = 64;
const PHDR_SIZE: usize = 56;

/// A loaded executable: its entry point and the leaf frames backing its
/// segments (the caller frees them after the process is reaped — the address
/// space's `destroy_user` only reclaims table frames).
pub struct LoadedImage {
    /// Program entry point (`e_entry`).
    pub entry: usize,
    /// Physical frames allocated for the segments.
    pub frames: Vec<usize>,
}

fn read_u16(image: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        image.get(offset..offset + 2)?.try_into().ok()?,
    ))
}

fn read_u32(image: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        image.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

fn read_u64(image: &[u8], offset: usize) -> Option<u64> {
    Some(u64::from_le_bytes(
        image.get(offset..offset + 8)?.try_into().ok()?,
    ))
}

/// Loads a static ELF64 riscv executable into `space`, mapping each PT_LOAD
/// segment U-accessible with its program-header permissions. On error the
/// frames allocated so far are freed and the partial mappings remain in the
/// space (the caller destroys the space on failure anyway).
pub fn load(space: &mut AddressSpace, image: &[u8]) -> Result<LoadedImage, &'static str> {
    if image.get(..4) != Some(&ELF_MAGIC[..]) {
        return Err("bad ELF magic");
    }
    if image.get(4) != Some(&ELFCLASS64) || image.get(5) != Some(&ELFDATA2LSB) {
        return Err("not a little-endian ELF64");
    }
    if read_u16(image, 16) != Some(ET_EXEC) {
        return Err("not an ET_EXEC static executable");
    }
    if read_u16(image, 18) != Some(EM_RISCV) {
        return Err("not a riscv ELF");
    }
    let entry = read_u64(image, 24).ok_or("truncated header")? as usize;
    let phoff = read_u64(image, 32).ok_or("truncated header")? as usize;
    let phentsize = read_u16(image, 54).ok_or("truncated header")? as usize;
    let phnum = read_u16(image, 56).ok_or("truncated header")? as usize;
    if phentsize != PHDR_SIZE {
        return Err("unexpected phentsize");
    }

    let mut frames = Vec::new();
    for i in 0..phnum {
        let ph = phoff + i * PHDR_SIZE;
        let p_type = read_u32(image, ph).ok_or("truncated phdr")?;
        if p_type != PT_LOAD {
            continue;
        }
        let p_flags = read_u32(image, ph + 4).ok_or("truncated phdr")?;
        let p_offset = read_u64(image, ph + 8).ok_or("truncated phdr")? as usize;
        let p_vaddr = read_u64(image, ph + 16).ok_or("truncated phdr")? as usize;
        let p_filesz = read_u64(image, ph + 32).ok_or("truncated phdr")? as usize;
        let p_memsz = read_u64(image, ph + 40).ok_or("truncated phdr")? as usize;
        if p_filesz > p_memsz || image.len() < p_offset + p_filesz {
            free_frames(&frames);
            return Err("segment exceeds image bounds");
        }

        let mut flags = paging::PTE_U;
        if p_flags & PF_R != 0 {
            flags |= paging::PTE_R;
        }
        if p_flags & PF_W != 0 {
            flags |= paging::PTE_W;
        }
        if p_flags & PF_X != 0 {
            flags |= paging::PTE_X;
        }

        let seg_start = align_down(p_vaddr);
        let seg_end = align_up(p_vaddr + p_memsz);
        let mut page = seg_start;
        while page < seg_end {
            let Some(pa) = frame::alloc() else {
                free_frames(&frames);
                return Err("out of frames for a segment");
            };
            frames.push(pa);
            // Zero the page, then copy the slice of the file that overlaps it
            // (bytes past p_filesz stay zero — bss semantics).
            // SAFETY: `pa` is a fresh, identity-mapped, writable frame.
            unsafe {
                core::ptr::write_bytes(pa as *mut u8, 0, frame::FRAME_SIZE);
                let copy_from = p_vaddr.max(page);
                let copy_to = (p_vaddr + p_filesz).min(page + frame::FRAME_SIZE);
                if copy_to > copy_from {
                    core::ptr::copy_nonoverlapping(
                        image.as_ptr().add(p_offset + (copy_from - p_vaddr)),
                        (pa + (copy_from - page)) as *mut u8,
                        copy_to - copy_from,
                    );
                }
            }
            space.map(page, pa, frame::FRAME_SIZE, flags);
            page += frame::FRAME_SIZE;
        }
    }
    if frames.is_empty() {
        return Err("no PT_LOAD segments");
    }
    // Make the copied code visible to instruction fetch.
    // SAFETY: fence.i orders the segment stores before any U-mode fetch.
    unsafe { core::arch::asm!("fence.i", options(nostack)) };
    Ok(LoadedImage { entry, frames })
}

fn free_frames(frames: &[usize]) {
    for &pa in frames {
        frame::free(pa);
    }
}

const fn align_down(x: usize) -> usize {
    x & !(frame::FRAME_SIZE - 1)
}

const fn align_up(x: usize) -> usize {
    (x + frame::FRAME_SIZE - 1) & !(frame::FRAME_SIZE - 1)
}

// ---------------------------------------------------------------------------
// Milestone 15 demo: two processes from one image, isolated address spaces.
// ---------------------------------------------------------------------------

/// Process VAs live in sv39 root slot 8 (VA 0x2_0000_0000+), a top-level slot
/// the kernel never maps — so each cloned root grows a private table tree
/// there and identical VAs in different processes back onto different frames.
const PROC_CODE_VA: usize = 0x2_0000_0000;
const PROC_DATA_VA: usize = 0x2_0000_2000;
/// Stack page VA, 1 MiB above the image base so toolchain-built apps have
/// headroom for their segments.
const PROC_STACK_VA: usize = 0x2_0010_0000;

/// Builds a minimal static ELF64 in memory: one R+X code segment (the raw
/// instruction words) at [`PROC_CODE_VA`] and one R+W data segment (a single
/// word) at [`PROC_DATA_VA`].
fn synth_elf(code: &[u32], data_word: u32) -> Vec<u8> {
    const CODE_FILE_OFF: usize = 0x200;
    const DATA_FILE_OFF: usize = 0x400;
    let code_bytes = code.len() * 4;

    let mut image = alloc::vec![0u8; DATA_FILE_OFF + 4];
    let put16 = |img: &mut [u8], off: usize, v: u16| img[off..off + 2].copy_from_slice(&v.to_le_bytes());
    let put32 = |img: &mut [u8], off: usize, v: u32| img[off..off + 4].copy_from_slice(&v.to_le_bytes());
    let put64 = |img: &mut [u8], off: usize, v: u64| img[off..off + 8].copy_from_slice(&v.to_le_bytes());

    // ELF header.
    image[..4].copy_from_slice(&ELF_MAGIC);
    image[4] = ELFCLASS64;
    image[5] = ELFDATA2LSB;
    image[6] = 1; // EV_CURRENT
    put16(&mut image, 16, ET_EXEC);
    put16(&mut image, 18, EM_RISCV);
    put32(&mut image, 20, 1); // e_version
    put64(&mut image, 24, PROC_CODE_VA as u64); // e_entry
    put64(&mut image, 32, EHDR_SIZE as u64); // e_phoff
    put16(&mut image, 52, EHDR_SIZE as u16); // e_ehsize
    put16(&mut image, 54, PHDR_SIZE as u16); // e_phentsize
    put16(&mut image, 56, 2); // e_phnum

    // Program headers: code (R+X) then data (R+W).
    for (index, (file_off, vaddr, size, flags)) in [
        (CODE_FILE_OFF, PROC_CODE_VA, code_bytes, PF_R | PF_X),
        (DATA_FILE_OFF, PROC_DATA_VA, 4usize, PF_R | PF_W),
    ]
    .into_iter()
    .enumerate()
    {
        let ph = EHDR_SIZE + index * PHDR_SIZE;
        put32(&mut image, ph, PT_LOAD);
        put32(&mut image, ph + 4, flags);
        put64(&mut image, ph + 8, file_off as u64); // p_offset
        put64(&mut image, ph + 16, vaddr as u64); // p_vaddr
        put64(&mut image, ph + 24, vaddr as u64); // p_paddr
        put64(&mut image, ph + 32, size as u64); // p_filesz
        put64(&mut image, ph + 40, size as u64); // p_memsz
        put64(&mut image, ph + 48, frame::FRAME_SIZE as u64); // p_align
    }

    for (i, word) in code.iter().enumerate() {
        let off = CODE_FILE_OFF + i * 4;
        image[off..off + 4].copy_from_slice(&word.to_le_bytes());
    }
    image[DATA_FILE_OFF..DATA_FILE_OFF + 4].copy_from_slice(&data_word.to_le_bytes());
    image
}

/// One demo process: its space, image frames, stack frame, and thread slot.
struct Process {
    space: AddressSpace,
    frames: Vec<usize>,
    stack_frame: usize,
    slot: usize,
    entry: usize,
}

impl Process {
    /// Loads `image` into a fresh per-process space, maps a stack page at
    /// [`PROC_STACK_VA`], and parks it in a Ready thread slot with `arg0`
    /// delivered in the process's `a0` at entry. On error all intermediate
    /// resources are released.
    fn launch(image: &[u8], arg0: usize) -> Result<Self, &'static str> {
        let mut space = AddressSpace::new_user().ok_or("out of frames for a space")?;
        let loaded = match load(&mut space, image) {
            Ok(loaded) => loaded,
            Err(error) => {
                space.destroy_user();
                return Err(error);
            }
        };
        let Some(stack) = frame::alloc() else {
            free_frames(&loaded.frames);
            space.destroy_user();
            return Err("out of frames for a stack");
        };
        space.map(
            PROC_STACK_VA,
            stack,
            frame::FRAME_SIZE,
            paging::PTE_U | paging::PTE_R | paging::PTE_W,
        );
        let Some(slot) = sched::spawn_at(
            loaded.entry,
            PROC_STACK_VA + frame::FRAME_SIZE,
            space.satp() as usize,
            arg0,
        ) else {
            frame::free(stack);
            free_frames(&loaded.frames);
            space.destroy_user();
            return Err("no free thread slot");
        };
        Ok(Self {
            space,
            frames: loaded.frames,
            stack_frame: stack,
            slot,
            entry: loaded.entry,
        })
    }
}

/// Milestone 15 demo: synthesize ONE static ELF, load it twice into two
/// fresh per-process address spaces, poke a different value into each
/// process's `.data` word (same VA, different physical page), and run both
/// under the scheduler. Each process yields twice, reads its `.data` word,
/// and exits with it — distinct exit values from identical images prove the
/// address spaces are isolated.
pub fn demo(timebase_hz: u64) {
    // User program (loaded at PROC_CODE_VA in each process):
    //   yield twice, a0 = *(PROC_DATA_VA), exit(a0).
    let program = [
        0x3020_0893u32, // li a7, 0x302 (ProcYield)
        0x0000_0073,    // ecall
        0x0000_0073,    // ecall
        0x0000_2517,    // auipc a0, 2     -> a0 = PROC_CODE_VA + 0xC + 0x2000
        0xff45_2503,    // lw a0, -12(a0)  -> a0 = *(PROC_DATA_VA)
        0x3010_0893,    // li a7, 0x301 (ProcExit)
        0x0000_0073,    // ecall
        0x0000_006f,    // 1: j 1b (unreached)
    ];
    let image = synth_elf(&program, 0);

    let mut processes: Vec<Process> = Vec::new();
    for pid in 0..2usize {
        let process = match Process::launch(&image, 0) {
            Ok(process) => process,
            Err(error) => {
                crate::kprintln!("[feox] elf: process {} launch failed: {}", pid, error);
                teardown(processes);
                return;
            }
        };
        // Identical images — differentiate via the data word, through the
        // frame's identity mapping (proving translate() against the space).
        let Some((data_pa, _)) = process.space.translate(PROC_DATA_VA) else {
            crate::kprintln!("[feox] elf: process {} has no data segment", pid);
            teardown(processes);
            return;
        };
        // SAFETY: data_pa is the fresh segment frame this process owns.
        unsafe { (data_pa as *mut u32).write_volatile(200 + pid as u32) };
        processes.push(process);
    }

    // Isolation check before running: the same code VA must translate to
    // different physical frames in the two spaces.
    let isolated = processes[0].space.translate(PROC_CODE_VA).map(|(pa, _)| pa)
        != processes[1].space.translate(PROC_CODE_VA).map(|(pa, _)| pa);
    crate::kprintln!(
        "[feox] elf: 1 image ({} bytes) -> 2 processes at the same VAs (isolated={}); running...",
        image.len(),
        isolated
    );

    sched::run(timebase_hz);

    let mut exits_ok = true;
    for (pid, process) in processes.iter().enumerate() {
        let (exited, value, _, _, _) = sched::stats(process.slot).unwrap_or((false, 0, 0, 0, 0));
        exits_ok &= exited && value == 200 + pid;
    }
    teardown(processes);

    crate::kprintln!(
        "[feox] milestone 15: ELF loader + per-process address spaces (exits 200/201={}, isolated={}, ok={}).",
        exits_ok,
        isolated,
        exits_ok && isolated
    );
}

/// Frees everything a demo process owns: thread slot, stack + segment frames,
/// and the space's private table tree. Runs with the kernel space active
/// (sched::run restores it).
fn teardown(processes: Vec<Process>) {
    for process in processes {
        sched::clear_slot(process.slot);
        frame::free(process.stack_frame);
        free_frames(&process.frames);
        process.space.destroy_user();
    }
}

// ---------------------------------------------------------------------------
// Milestone 16: libOS + app delivery — a toolchain-built user executable.
// ---------------------------------------------------------------------------

/// `apps/feox-hello`, a real no_std Rust binary linked against `feox-libos`,
/// cross-built by this crate's build.rs and embedded here. It runs through
/// exactly the same loader path as the synthesized M15 image.
static HELLO_ELF: &[u8] = include_bytes!(env!("FEOX_HELLO_ELF"));

/// Milestones 16 + 17 demo: load and run `feox-hello`. The app runs the full
/// exokernel memory workflow from user space — `cap_request` two physical
/// pages, `mem_map` them, fill/sum `i^2` across a reschedule, `mem_vtop` the
/// mapping, `mem_unmap`, `cap_release` — and exits with
/// `sum(i^2, i<1024) % 65521`, which the kernel predicts independently here
/// via the closed form. The capability ledger must balance: the table count
/// after the run equals the count before it. Any failed step exits `0xbNN`
/// (a step-naming code that cannot equal the checksum the kernel expects).
pub fn app_demo(timebase_hz: u64) {
    let process = match Process::launch(HELLO_ELF, 0) {
        Ok(process) => process,
        Err(error) => {
            crate::kprintln!("[feox] libos: feox-hello launch failed: {}", error);
            return;
        }
    };
    // sum of i^2 for i in 0..1024 (two pages of u64), closed form, mod 65521.
    let n: u64 = (2 * frame::FRAME_SIZE as u64 / 8) - 1;
    let expected = ((n * (n + 1) * (2 * n + 1) / 6) % 65521) as usize;
    let caps_before = crate::capability::active_count();
    crate::kprintln!(
        "[feox] libos: feox-hello ({} bytes) loaded, entry {:#x}; running...",
        HELLO_ELF.len(),
        process.entry
    );

    sched::run(timebase_hz);

    let (exited, value, _, yields, _) = sched::stats(process.slot).unwrap_or((false, 0, 0, 0, 0));
    let balanced = crate::capability::active_count() == caps_before;
    let delivered = exited && value == expected && yields > 0;
    teardown(alloc::vec![process]);

    crate::kprintln!(
        "[feox] milestone 16: libOS + app delivery (feox-hello exit {} expected {}, yields={}, ok={}).",
        value,
        expected,
        yields,
        delivered
    );
    crate::kprintln!(
        "[feox] milestone 17: first real U-mode app (cap_request -> mem_map -> compute -> vtop -> unmap -> release; caps balanced={}, ok={}).",
        balanced,
        delivered && balanced
    );
}

// ---------------------------------------------------------------------------
// Milestone 18: IPC events — ThreadPark/EventSlot ping-pong over shared memory.
// ---------------------------------------------------------------------------

/// `apps/feox-pingpong`: one image, two roles (producer/consumer selected by
/// the `a0` argv0 the kernel passes at entry).
static PINGPONG_ELF: &[u8] = include_bytes!(env!("FEOX_PINGPONG_ELF"));

/// Shared page VA: mapped R+W into BOTH processes (same VA, same frame — the
/// deliberate inverse of the M15 isolation proof). Holds two EventSlots and a
/// mailbox word; layout is a compile-time convention with the app.
const SHARED_VA: usize = 0x2_2000_0000;

/// Milestone 18 demo: spawn producer and consumer from one image with a
/// kernel-provided shared page, and let them ping-pong 4 payloads through
/// real ThreadPark block/wake cycles (each side parks while the other works;
/// with both parked the scheduler drops to its S-mode idle loop until a tick
/// wakes someone). Exits are payload-sum-derived and predicted here.
pub fn ipc_demo(timebase_hz: u64) {
    let mut producer = match Process::launch(PINGPONG_ELF, 0) {
        Ok(process) => process,
        Err(error) => {
            crate::kprintln!("[feox] ipc: producer launch failed: {}", error);
            return;
        }
    };
    let mut consumer = match Process::launch(PINGPONG_ELF, 1) {
        Ok(process) => process,
        Err(error) => {
            crate::kprintln!("[feox] ipc: consumer launch failed: {}", error);
            teardown(alloc::vec![producer]);
            return;
        }
    };

    let Some(shared) = frame::alloc() else {
        crate::kprintln!("[feox] ipc: out of frames for the shared page");
        teardown(alloc::vec![producer, consumer]);
        return;
    };
    // SAFETY: fresh identity-mapped frame; both EventSlots and the mailbox
    // must start at zero.
    unsafe { core::ptr::write_bytes(shared as *mut u8, 0, frame::FRAME_SIZE) };
    let flags = paging::PTE_U | paging::PTE_R | paging::PTE_W;
    producer.space.map(SHARED_VA, shared, frame::FRAME_SIZE, flags);
    consumer.space.map(SHARED_VA, shared, frame::FRAME_SIZE, flags);
    paging::flush_tlb_all();

    // Payloads are 1000 + i*i for i in 0..4; the producer exits with the sum
    // (mod 65521) and the consumer with sum + rounds (mod 65521).
    let rounds = 4u64;
    let sum: u64 = (0..rounds).map(|i| 1000 + i * i).sum();
    let expected_producer = (sum % 65521) as usize;
    let expected_consumer = ((sum + rounds) % 65521) as usize;
    crate::kprintln!(
        "[feox] ipc: producer + consumer sharing a page at {:#x}; ping-ponging {} payloads...",
        SHARED_VA,
        rounds
    );

    // Park/wake cycles resolve at tick granularity (~2 ticks per round plus
    // startup); give the run more headroom than the default budget.
    sched::run_with_budget(timebase_hz, 64);

    let (p_exited, p_value, _, _, p_parks) =
        sched::stats(producer.slot).unwrap_or((false, 0, 0, 0, 0));
    let (c_exited, c_value, _, _, c_parks) =
        sched::stats(consumer.slot).unwrap_or((false, 0, 0, 0, 0));
    let ok = p_exited
        && c_exited
        && p_value == expected_producer
        && c_value == expected_consumer
        && p_parks > 0
        && c_parks > 0;
    teardown(alloc::vec![producer, consumer]);
    frame::free(shared);

    crate::kprintln!(
        "[feox] milestone 18: IPC events (ThreadPark/EventSlot ping-pong: producer exit {} expected {}, consumer exit {} expected {}, parks={}+{}, ok={}).",
        p_value,
        expected_producer,
        c_value,
        expected_consumer,
        p_parks,
        c_parks,
        ok
    );
}
