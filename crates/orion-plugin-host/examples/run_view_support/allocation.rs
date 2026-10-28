//! Allocation requests made during one measured engine run. This is a benchmark
//! allocator only; the library and server keep their normal allocator.
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

pub struct Meter;
static ENABLED: AtomicBool = AtomicBool::new(false);
static CALLS: AtomicU64 = AtomicU64::new(0);
static BYTES: AtomicU64 = AtomicU64::new(0);

fn record(size: usize, ptr: *mut u8) {
    if !ptr.is_null() && ENABLED.load(Ordering::Relaxed) {
        CALLS.fetch_add(1, Ordering::Relaxed);
        BYTES.fetch_add(size as u64, Ordering::Relaxed);
    }
}

// SAFETY: all layouts, pointers and sizes are forwarded unchanged to System.
// The counters never allocate or touch the memory returned by System.
unsafe impl GlobalAlloc for Meter {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        record(layout.size(), ptr);
        ptr
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc_zeroed(layout) };
        record(layout.size(), ptr);
        ptr
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let ptr = unsafe { System.realloc(ptr, layout, size) };
        record(size, ptr);
        ptr
    }
}

pub struct Window;
impl Window {
    pub fn start() -> Self {
        CALLS.store(0, Ordering::Relaxed);
        BYTES.store(0, Ordering::Relaxed);
        ENABLED.store(true, Ordering::SeqCst);
        Self
    }
    pub fn finish(self) -> (u64, u64) {
        drop(self);
        (CALLS.load(Ordering::Relaxed), BYTES.load(Ordering::Relaxed))
    }
}
impl Drop for Window {
    fn drop(&mut self) {
        ENABLED.store(false, Ordering::SeqCst);
    }
}
