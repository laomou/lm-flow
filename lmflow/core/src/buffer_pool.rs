use std::alloc::{alloc, dealloc, Layout};
use std::cell::RefCell;
use std::sync::{Arc, Mutex};

pub(crate) struct BufferAllocation {
    pub(crate) data: *mut u8,
    pub(crate) layout: Layout,
}

unsafe impl Send for BufferAllocation {}

pub(crate) struct BufferPool {
    max_bytes: usize,
    state: Mutex<(usize, Vec<BufferAllocation>)>,
}

impl BufferPool {
    pub(crate) fn new(max_bytes: usize) -> Self {
        Self {
            max_bytes,
            state: Mutex::new((0, Vec::new())),
        }
    }

    pub(crate) fn take(&self, layout: Layout) -> BufferAllocation {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(index) = state.1.iter().position(|entry| entry.layout == layout) {
            let entry = state.1.swap_remove(index);
            state.0 = state.0.saturating_sub(layout.size());
            return entry;
        }
        drop(state);
        BufferAllocation {
            data: unsafe { alloc(layout) },
            layout,
        }
    }

    pub(crate) fn release(&self, allocation: BufferAllocation) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if allocation.layout.size() <= self.max_bytes
            && state.0.saturating_add(allocation.layout.size()) <= self.max_bytes
        {
            state.0 = state.0.saturating_add(allocation.layout.size());
            state.1.push(allocation);
        } else if !allocation.data.is_null() {
            unsafe { dealloc(allocation.data, allocation.layout) };
        }
    }
}

impl Drop for BufferPool {
    fn drop(&mut self) {
        let state = self.state.get_mut().unwrap_or_else(|e| e.into_inner());
        for allocation in state.1.drain(..) {
            if !allocation.data.is_null() {
                unsafe { dealloc(allocation.data, allocation.layout) };
            }
        }
        state.0 = 0;
    }
}

thread_local! {
    static ACTIVE_BUFFER_POOL: RefCell<Option<Arc<BufferPool>>> = const { RefCell::new(None) };
}

pub(crate) struct BufferPoolGuard(Option<Arc<BufferPool>>);

pub(crate) fn enter(pool: Arc<BufferPool>) -> BufferPoolGuard {
    let previous = ACTIVE_BUFFER_POOL.with(|active| active.borrow_mut().replace(pool));
    BufferPoolGuard(previous)
}

pub(crate) fn active() -> Option<Arc<BufferPool>> {
    ACTIVE_BUFFER_POOL.with(|active| {
        active
            .borrow()
            .as_ref()
            .filter(|pool| pool.max_bytes > 0)
            .cloned()
    })
}

impl Drop for BufferPoolGuard {
    fn drop(&mut self) {
        ACTIVE_BUFFER_POOL.with(|active| *active.borrow_mut() = self.0.take());
    }
}
