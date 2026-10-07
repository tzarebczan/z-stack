//! Thread-caching allocator for the multicore wasm build.
//!
//! std's wasm32 allocator is dlmalloc behind one global spin lock. Compact-block
//! decoding and trial decryption make several small allocations per output on
//! every Rayon worker, so past 8 workers the renderer burned 16 cores and scanned
//! no faster than with 8. Here each thread recycles small blocks through its own
//! free lists and takes the shared lock only to trade batches.
//!
//! Small blocks never return to the backing allocator: wasm memory cannot shrink,
//! and a freed block is reused by whichever thread frees it. Each thread keeps at
//! most `LOCAL_MAX` blocks per class and hands half back when it passes that.

#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout};
use std::cell::{Cell, UnsafeCell};
use std::ptr::{self, null_mut};
use std::sync::atomic::{AtomicBool, Ordering};

const ALIGN: usize = 16;
const CLASSES: [usize; 12] = [16, 32, 48, 64, 96, 128, 192, 256, 384, 512, 768, 1024];
const N: usize = CLASSES.len();
/// Blocks carved from the backing allocator when a class runs dry.
const CARVE: usize = 128;
const LOCAL_MAX: usize = 256;

/// A free block. The first block of a batch on the shared list also links to
/// the next batch, so the lock is held only to push or pop one pointer. The
/// smallest class (16 bytes) fits both pointers on every target.
#[repr(C)]
struct Block {
    next: *mut Block,
    next_batch: *mut Block,
}

fn class_of(layout: Layout) -> Option<usize> {
    if layout.align() > ALIGN {
        return None;
    }
    let size = layout.size().max(1);
    CLASSES.iter().position(|&c| c >= size)
}

fn class_layout(class: usize) -> Layout {
    // SAFETY: every class is a nonzero multiple of ALIGN, which is a power of two.
    unsafe { Layout::from_size_align_unchecked(CLASSES[class], ALIGN) }
}

struct Local {
    head: [Cell<*mut Block>; N],
    len: [Cell<usize>; N],
}

thread_local! {
    static LOCAL: Local = const {
        Local {
            head: [const { Cell::new(null_mut()) }; N],
            len: [const { Cell::new(0) }; N],
        }
    };
}

/// Batches handed back by threads over their limit, per class.
struct Shared {
    locked: AtomicBool,
    head: UnsafeCell<[*mut Block; N]>,
}

// SAFETY: `head` is only touched while `locked` is held.
unsafe impl Sync for Shared {}

impl Shared {
    fn with<R>(&self, f: impl FnOnce(&mut [*mut Block; N]) -> R) -> R {
        while self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            std::hint::spin_loop();
        }
        // SAFETY: the lock above gives exclusive access until the store below.
        let r = f(unsafe { &mut *self.head.get() });
        self.locked.store(false, Ordering::Release);
        r
    }
}

pub struct ThreadCache<A> {
    inner: A,
    shared: Shared,
}

impl<A> ThreadCache<A> {
    pub const fn new(inner: A) -> Self {
        Self {
            inner,
            shared: Shared {
                locked: AtomicBool::new(false),
                head: UnsafeCell::new([null_mut(); N]),
            },
        }
    }
}

impl<A: GlobalAlloc> ThreadCache<A> {
    /// Take the shared list for `class`, or carve fresh blocks. Returns one
    /// block and leaves the rest on this thread's list.
    unsafe fn refill(&self, local: &Local, class: usize) -> *mut u8 {
        let mut head = self.shared.with(|heads| {
            let batch = heads[class];
            if !batch.is_null() {
                heads[class] = (*batch).next_batch;
            }
            batch
        });
        let mut len = 0;
        let mut cur = head;
        while !cur.is_null() {
            len += 1;
            cur = (*cur).next;
        }
        if head.is_null() {
            let size = CLASSES[class];
            let chunk = self
                .inner
                .alloc(Layout::from_size_align_unchecked(size * CARVE, ALIGN));
            if chunk.is_null() {
                return null_mut();
            }
            for i in (0..CARVE).rev() {
                let block = chunk.add(i * size).cast::<Block>();
                (*block).next = head;
                head = block;
            }
            len = CARVE;
        }
        local.head[class].set((*head).next);
        local.len[class].set(len - 1);
        head.cast()
    }

    /// Hand the older half of this thread's list back to the shared list.
    unsafe fn spill(&self, local: &Local, class: usize) {
        let keep = LOCAL_MAX / 2;
        let mut tail = local.head[class].get();
        for _ in 1..keep {
            tail = (*tail).next;
        }
        let spilled = (*tail).next;
        (*tail).next = null_mut();
        local.len[class].set(keep);
        self.push_batch(class, spilled);
    }

    unsafe fn push_batch(&self, class: usize, batch: *mut Block) {
        self.shared.with(|heads| {
            (*batch).next_batch = heads[class];
            heads[class] = batch;
        });
    }
}

// SAFETY: small blocks are at least their class size and 16-aligned, and are only
// ever handed out once until freed; larger requests go straight to `inner`.
unsafe impl<A: GlobalAlloc> GlobalAlloc for ThreadCache<A> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let Some(class) = class_of(layout) else {
            return self.inner.alloc(layout);
        };
        LOCAL
            .try_with(|local| {
                let head = local.head[class].get();
                if head.is_null() {
                    return self.refill(local, class);
                }
                local.head[class].set((*head).next);
                local.len[class].set(local.len[class].get() - 1);
                head.cast()
            })
            // Class-sized, so it can join a free list later.
            .unwrap_or_else(|_| self.inner.alloc(class_layout(class)))
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let Some(class) = class_of(layout) else {
            return self.inner.dealloc(ptr, layout);
        };
        let pushed = LOCAL.try_with(|local| {
            let block = ptr.cast::<Block>();
            (*block).next = local.head[class].get();
            local.head[class].set(block);
            let len = local.len[class].get() + 1;
            local.len[class].set(len);
            if len > LOCAL_MAX {
                self.spill(local, class);
            }
        });
        if pushed.is_err() {
            let block = ptr.cast::<Block>();
            (*block).next = null_mut();
            self.push_batch(class, block);
        }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new_layout = Layout::from_size_align_unchecked(new_size, layout.align());
        match (class_of(layout), class_of(new_layout)) {
            (None, None) => self.inner.realloc(ptr, layout, new_size),
            (Some(a), Some(b)) if a == b => ptr,
            _ => {
                let new = self.alloc(new_layout);
                if !new.is_null() {
                    ptr::copy_nonoverlapping(ptr, new, layout.size().min(new_size));
                    self.dealloc(ptr, layout);
                }
                new
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::alloc::System;
    use std::sync::mpsc;

    static CACHE: ThreadCache<System> = ThreadCache::new(System);

    struct Live {
        ptr: usize,
        layout: Layout,
        tag: u8,
    }

    fn fill(l: &Live) {
        unsafe { ptr::write_bytes(l.ptr as *mut u8, l.tag, l.layout.size()) };
    }

    fn check(l: &Live) {
        let bytes = unsafe { std::slice::from_raw_parts(l.ptr as *const u8, l.layout.size()) };
        assert!(
            bytes.iter().all(|&b| b == l.tag),
            "block {:#x} was overwritten",
            l.ptr
        );
        assert_eq!(l.ptr % l.layout.align(), 0);
    }

    /// Deterministic xorshift so failures reproduce.
    fn rng(seed: u64) -> impl FnMut() -> u64 {
        let mut x = seed | 1;
        move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        }
    }

    #[test]
    fn blocks_never_overlap_across_threads() {
        let threads = 8;
        let (senders, receivers): (Vec<_>, Vec<_>) =
            (0..threads).map(|_| mpsc::channel::<Live>()).unzip();
        let mut handles = Vec::new();
        for (t, rx) in receivers.into_iter().enumerate() {
            let peers = senders.clone();
            handles.push(std::thread::spawn(move || {
                let mut next = rng(t as u64 + 7);
                let mut live: Vec<Live> = Vec::new();
                for i in 0..60_000u32 {
                    // Free blocks another thread allocated.
                    while let Ok(l) = rx.try_recv() {
                        check(&l);
                        unsafe { CACHE.dealloc(l.ptr as *mut u8, l.layout) };
                    }
                    let r = next();
                    if live.len() > 400 || (r % 3 == 0 && !live.is_empty()) {
                        let l = live.swap_remove((r as usize / 3) % live.len());
                        check(&l);
                        if r % 5 == 0 {
                            let _ = peers[(r as usize) % threads].send(l);
                        } else if r % 7 == 0 {
                            let new_size = 1 + (next() % 1500) as usize;
                            let p = unsafe { CACHE.realloc(l.ptr as *mut u8, l.layout, new_size) };
                            let kept = l.layout.size().min(new_size);
                            let head = unsafe { std::slice::from_raw_parts(p, kept) };
                            assert!(head.iter().all(|&b| b == l.tag), "realloc lost contents");
                            let moved = Live {
                                ptr: p as usize,
                                layout: Layout::from_size_align(new_size, l.layout.align())
                                    .unwrap(),
                                tag: l.tag,
                            };
                            fill(&moved);
                            live.push(moved);
                        } else {
                            unsafe { CACHE.dealloc(l.ptr as *mut u8, l.layout) };
                        }
                        continue;
                    }
                    let size = 1 + (r % 1400) as usize;
                    let align = [1, 4, 8, 16, 32][(r >> 20) as usize % 5];
                    let layout = Layout::from_size_align(size, align).unwrap();
                    let ptr = unsafe { CACHE.alloc(layout) };
                    assert!(!ptr.is_null());
                    let l = Live {
                        ptr: ptr as usize,
                        layout,
                        tag: (i % 251) as u8 + 1,
                    };
                    fill(&l);
                    live.push(l);
                }
                for l in live {
                    check(&l);
                    unsafe { CACHE.dealloc(l.ptr as *mut u8, l.layout) };
                }
                drop(peers);
                rx
            }));
        }
        drop(senders);
        for h in handles {
            let rx = h.join().unwrap();
            for l in rx.try_iter() {
                check(&l);
                unsafe { CACHE.dealloc(l.ptr as *mut u8, l.layout) };
            }
        }
    }

    #[test]
    fn classes_cover_every_small_size() {
        for size in 1..=1024 {
            let class = class_of(Layout::from_size_align(size, 8).unwrap()).unwrap();
            assert!(CLASSES[class] >= size);
            assert!(class == 0 || CLASSES[class - 1] < size);
        }
        assert_eq!(class_of(Layout::from_size_align(1025, 8).unwrap()), None);
        assert_eq!(class_of(Layout::from_size_align(64, 32).unwrap()), None);
    }
}
