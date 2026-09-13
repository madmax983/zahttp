// zahttp module: alloc (the proof) — zero deps, zero heap. See main.rs for the rules.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};

// ---- allocation counter: the proof -------------------------------------

pub(crate) struct Counting;

pub(crate) static ALLOC_COUNT: AtomicU64 = AtomicU64::new(0);
pub(crate) static REQUEST_COUNT: AtomicU64 = AtomicU64::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
pub(crate) static GLOBAL: Counting = Counting;
