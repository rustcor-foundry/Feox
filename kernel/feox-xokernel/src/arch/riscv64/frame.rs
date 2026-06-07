//! Physical frame allocator for riscv64 early boot.
//!
//! A 4 KiB frame allocator over the usable RAM window discovered from the
//! device tree. It bump-allocates fresh frames and recycles freed frames via
//! an intrusive LIFO free list (each free frame stores the next free frame's
//! address in its first word). This is the physical-memory source the sv39
//! page-table walker and the kernel heap build on in later passes.
//!
//! Single-hart boot only: the global is accessed without a lock for now. A
//! spinlock is added when SMP brings up secondary harts.

/// Frame / page size (sv39 base page).
pub const FRAME_SIZE: usize = 4096;

/// Bump + free-list physical frame allocator.
pub struct FrameAllocator {
    /// Next never-yet-allocated frame (bump pointer), 4 KiB-aligned.
    next: usize,
    /// One past the last usable frame.
    end: usize,
    /// Head of the intrusive free list (0 = empty).
    free_list: usize,
    /// Total frames in the managed window.
    total: usize,
    /// Frames currently handed out.
    in_use: usize,
    /// Frames sitting on the free list.
    free_count: usize,
}

impl FrameAllocator {
    /// An empty allocator (manages nothing until [`Self::init`]).
    pub const fn empty() -> Self {
        Self {
            next: 0,
            end: 0,
            free_list: 0,
            total: 0,
            in_use: 0,
            free_count: 0,
        }
    }

    /// Initializes the allocator over `[start, end)`, aligning `start` up and
    /// `end` down to frame boundaries.
    pub fn init(&mut self, start: usize, end: usize) {
        let start = align_up(start, FRAME_SIZE);
        let end = align_down(end, FRAME_SIZE);
        self.next = start;
        self.end = if end > start { end } else { start };
        self.free_list = 0;
        self.total = (self.end - self.next) / FRAME_SIZE;
        self.in_use = 0;
        self.free_count = 0;
    }

    /// Allocates a physical frame, preferring recycled frames over fresh ones.
    /// Returns the frame's physical base address, or `None` when exhausted.
    pub fn alloc(&mut self) -> Option<usize> {
        if self.free_list != 0 {
            let frame = self.free_list;
            // SAFETY: free frames are identity-mapped usable RAM; the first
            // word holds the next free frame's address (0 = end of list).
            self.free_list = unsafe { *(frame as *const usize) };
            self.free_count -= 1;
            self.in_use += 1;
            Some(frame)
        } else if self.next + FRAME_SIZE <= self.end {
            let frame = self.next;
            self.next += FRAME_SIZE;
            self.in_use += 1;
            Some(frame)
        } else {
            None
        }
    }

    /// Allocates `count` physically contiguous frames (bump only — the free
    /// list is not consulted, since recycled frames may not be adjacent).
    /// Returns the base physical address. Used for multi-page regions like
    /// per-hart stacks.
    pub fn alloc_contiguous(&mut self, count: usize) -> Option<usize> {
        let bytes = count * FRAME_SIZE;
        if count == 0 || self.next + bytes > self.end {
            return None;
        }
        let base = self.next;
        self.next += bytes;
        self.in_use += count;
        Some(base)
    }

    /// Returns a previously allocated frame to the free list.
    pub fn free(&mut self, frame: usize) {
        // SAFETY: `frame` is a frame this allocator handed out (identity-mapped
        // RAM); store the current list head in its first word.
        unsafe { *(frame as *mut usize) = self.free_list };
        self.free_list = frame;
        self.free_count += 1;
        self.in_use -= 1;
    }

    /// Total frames in the managed window.
    #[must_use]
    pub fn total(&self) -> usize {
        self.total
    }

    /// Frames currently available (un-bumped + recycled).
    #[must_use]
    pub fn available(&self) -> usize {
        (self.end - self.next) / FRAME_SIZE + self.free_count
    }
}

/// Global early-boot frame allocator (single-hart until SMP adds a lock).
static mut FRAME_ALLOCATOR: FrameAllocator = FrameAllocator::empty();

/// Returns an exclusive reference to the global allocator.
///
/// Safe at this stage because only the boot hart runs; SMP will replace this
/// with a locked accessor.
#[allow(static_mut_refs)]
fn allocator() -> &'static mut FrameAllocator {
    // SAFETY: single-threaded early boot; no aliasing references exist.
    unsafe { &mut FRAME_ALLOCATOR }
}

/// Initializes the global allocator over `[start, end)`.
pub fn init(start: usize, end: usize) {
    allocator().init(start, end);
}

/// Allocates a physical frame from the global allocator.
pub fn alloc() -> Option<usize> {
    allocator().alloc()
}

/// Allocates `count` physically contiguous frames from the global allocator.
pub fn alloc_contiguous(count: usize) -> Option<usize> {
    allocator().alloc_contiguous(count)
}

/// Frees a physical frame back to the global allocator.
pub fn free(frame: usize) {
    allocator().free(frame);
}

/// Total frames managed by the global allocator.
#[must_use]
pub fn total() -> usize {
    allocator().total()
}

/// Frames currently available from the global allocator.
#[must_use]
pub fn available() -> usize {
    allocator().available()
}

/// Rounds `x` up to a multiple of `align` (a power of two).
const fn align_up(x: usize, align: usize) -> usize {
    (x + align - 1) & !(align - 1)
}

/// Rounds `x` down to a multiple of `align` (a power of two).
const fn align_down(x: usize, align: usize) -> usize {
    x & !(align - 1)
}
