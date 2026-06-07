//! Kernel heap: a hand-rolled first-fit linked-list allocator (milestone 10).
//!
//! Backs the global allocator (so `alloc` — `Box`/`Vec`/collections — works in
//! the kernel) over a region carved from the physical frame allocator. Free
//! regions form an intrusive singly-linked list stored in the free memory
//! itself; `alloc` first-fits and splits, `dealloc` pushes the region back. A
//! spinlock guards the list so `GlobalAlloc`'s `&self` methods are sound.
//!
//! Known limitation (documented follow-up): freed regions are not coalesced and
//! alignment padding ahead of an allocation is not reclaimed. Fine for the
//! kernel's allocation pattern now; a coalescing pass comes later.

use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::mem;
use core::ptr;
use core::sync::atomic::{AtomicBool, Ordering};

/// A free region of the heap, stored in-place at the start of that free memory.
struct FreeRegion {
    size: usize,
    next: Option<&'static mut FreeRegion>,
}

impl FreeRegion {
    fn start_addr(&self) -> usize {
        ptr::from_ref(self) as usize
    }
    fn end_addr(&self) -> usize {
        self.start_addr() + self.size
    }
}

/// First-fit linked-list heap. `head` is a zero-size sentinel.
struct Heap {
    head: FreeRegion,
}

impl Heap {
    const fn new() -> Self {
        Self {
            head: FreeRegion { size: 0, next: None },
        }
    }

    /// Adds `[addr, addr+size)` to the free list. The region must be aligned to
    /// and at least as large as a `FreeRegion`.
    ///
    /// # Safety
    /// `addr` must be a unique, writable, identity-mapped region of `size` bytes
    /// not aliased by any live allocation.
    unsafe fn add_free_region(&mut self, addr: usize, size: usize) {
        debug_assert_eq!(align_up(addr, mem::align_of::<FreeRegion>()), addr);
        debug_assert!(size >= mem::size_of::<FreeRegion>());
        let node_ptr = addr as *mut FreeRegion;
        // SAFETY: caller guarantees `addr`/`size` describe free, unaliased,
        // writable memory large enough for a FreeRegion.
        unsafe {
            node_ptr.write(FreeRegion {
                size,
                next: self.head.next.take(),
            });
            self.head.next = Some(&mut *node_ptr);
        }
    }

    /// Removes and returns the first region that fits `size`/`align`, along with
    /// the aligned allocation start within it.
    fn find_region(&mut self, size: usize, align: usize) -> Option<(&'static mut FreeRegion, usize)> {
        let mut current = &mut self.head;
        while let Some(ref mut region) = current.next {
            if let Ok(start) = Self::fits(region, size, align) {
                let next = region.next.take();
                let region = current.next.take().unwrap();
                current.next = next;
                return Some((region, start));
            }
            current = current.next.as_mut().unwrap();
        }
        None
    }

    /// Returns the aligned start if an allocation of `size`/`align` fits in
    /// `region` while leaving either no remainder or a remainder large enough to
    /// be its own free region.
    fn fits(region: &FreeRegion, size: usize, align: usize) -> Result<usize, ()> {
        let start = align_up(region.start_addr(), align);
        let end = start.checked_add(size).ok_or(())?;
        if end > region.end_addr() {
            return Err(());
        }
        let remainder = region.end_addr() - end;
        if remainder > 0 && remainder < mem::size_of::<FreeRegion>() {
            return Err(());
        }
        Ok(start)
    }

    /// Rounds a layout up to the allocator's minimum size/alignment.
    fn size_align(layout: Layout) -> (usize, usize) {
        let layout = layout
            .align_to(mem::align_of::<FreeRegion>())
            .expect("alignment overflow")
            .pad_to_align();
        (layout.size().max(mem::size_of::<FreeRegion>()), layout.align())
    }
}

/// Rounds `x` up to a multiple of `align` (a power of two).
const fn align_up(x: usize, align: usize) -> usize {
    (x + align - 1) & !(align - 1)
}

/// Spinlock-guarded heap exposed as the global allocator.
pub struct LockedHeap {
    locked: AtomicBool,
    heap: UnsafeCell<Heap>,
}

// SAFETY: all access to the inner Heap goes through the spinlock in `lock()`.
unsafe impl Sync for LockedHeap {}

impl LockedHeap {
    const fn new() -> Self {
        Self {
            locked: AtomicBool::new(false),
            heap: UnsafeCell::new(Heap::new()),
        }
    }

    fn lock(&self) -> HeapGuard<'_> {
        while self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
        }
        HeapGuard { owner: self }
    }
}

struct HeapGuard<'a> {
    owner: &'a LockedHeap,
}

impl core::ops::Deref for HeapGuard<'_> {
    type Target = Heap;
    fn deref(&self) -> &Heap {
        // SAFETY: the spinlock grants exclusive access for the guard's lifetime.
        unsafe { &*self.owner.heap.get() }
    }
}
impl core::ops::DerefMut for HeapGuard<'_> {
    fn deref_mut(&mut self) -> &mut Heap {
        // SAFETY: as above.
        unsafe { &mut *self.owner.heap.get() }
    }
}
impl Drop for HeapGuard<'_> {
    fn drop(&mut self) {
        self.owner.locked.store(false, Ordering::Release);
    }
}

unsafe impl GlobalAlloc for LockedHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let (size, align) = Heap::size_align(layout);
        let mut heap = self.lock();
        match heap.find_region(size, align) {
            Some((region, start)) => {
                let end = start + size;
                let remainder = region.end_addr() - end;
                if remainder > 0 {
                    // SAFETY: `[end, end+remainder)` is the unused tail of the
                    // region we just removed; it is free and unaliased.
                    unsafe { heap.add_free_region(end, remainder) };
                }
                start as *mut u8
            }
            None => ptr::null_mut(),
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let (size, _) = Heap::size_align(layout);
        // SAFETY: `ptr`/`size` came from a prior `alloc` with this layout, so
        // they describe a now-free, unaliased region of the heap.
        unsafe { self.lock().add_free_region(ptr as usize, size) };
    }
}

#[global_allocator]
static ALLOCATOR: LockedHeap = LockedHeap::new();

/// Initializes the kernel heap over `[start, start+size)`.
///
/// # Safety
/// The region must be unique, writable, identity-mapped, and never handed out
/// elsewhere.
pub unsafe fn init(start: usize, size: usize) {
    // SAFETY: upheld by the caller (a fresh frame-allocator region).
    unsafe { ALLOCATOR.lock().add_free_region(start, size) };
}
