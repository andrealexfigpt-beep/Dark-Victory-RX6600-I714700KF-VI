//! Descriptor store tracking adapted from Belobug/VIctory-Belobug-AMD, 4eb6cfd (GPL-3.0-or-later).

use std::sync::atomic::{AtomicPtr, AtomicU8, AtomicU64, Ordering::{AcqRel, Acquire, Relaxed, Release}};

/// Whether renderer port #2 is on.
pub fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    // Read once, here: the switch of renderer port #2.
    *ON.get_or_init(|| std::env::var("VICTORY_PORT_DESCRIPTORS").is_ok_and(|v| v.trim() == "1"))
}

/// What the patched stores read (`victory_loader::descriptor_sites`,
/// `MARKS_LEN` bytes): the heap's first byte, its length (0: none), the map
/// of slots, the map of chunks of [`CHUNK`] slots.
#[repr(C)]
pub struct Marks {
    start: AtomicU64,
    len: AtomicU64,
    slots: AtomicPtr<AtomicU8>,
    chunks: AtomicPtr<AtomicU8>,
}

const _: () = assert!(size_of::<Marks>() == 32);

pub static MARKS: Marks = Marks {
    start: AtomicU64::new(0),
    len: AtomicU64::new(0),
    slots: AtomicPtr::new(std::ptr::null_mut()),
    chunks: AtomicPtr::new(std::ptr::null_mut()),
};

/// Slots a byte of the map of chunks stands for (4 KiB of records).
const CHUNK: usize = 128;

/// Where the stores' cells are, for `descriptor_sites::apply`.
pub fn marks_address() -> usize {
    std::ptr::from_ref(&MARKS) as usize
}

static HOOKED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static ACTIVE: std::sync::Mutex<bool> = std::sync::Mutex::new(false);
static MARKED_SLOTS: AtomicU64 = AtomicU64::new(0);

/// The loader enables marking only if every expected instruction matched.
pub fn hooked(all_sites: bool) {
    HOOKED.store(all_sites, Release);
}

/// Register the first eligible heap. Patched stores use separate pointer and
/// length loads, so this storage is kept at a fixed address for the process.
pub(crate) fn adopt(start: u64, count: u32) -> bool {
    let mut seen = ACTIVE.lock().unwrap_or_else(|e| e.into_inner());
    if !HOOKED.load(Acquire) || *seen || count == 0 {
        return false;
    }
    let count = count as usize;
    let slots: &'static mut [AtomicU8] = Box::leak((0..count).map(|_| AtomicU8::new(0)).collect());
    let chunks: &'static mut [AtomicU8] = Box::leak((0..count.div_ceil(CHUNK)).map(|_| AtomicU8::new(0)).collect());
    MARKS.slots.store(slots.as_mut_ptr(), Relaxed);
    MARKS.chunks.store(chunks.as_mut_ptr(), Relaxed);
    MARKS.start.store(start, Relaxed);
    MARKS.len.store((count * 32) as u64, Release);
    *seen = true;
    tracing::info!(start, count, "descriptor marks active alongside write watch");
    true
}

pub(crate) fn let_go(start: u64) {
    if MARKS.start.load(Relaxed) == start {
        MARKS.len.store(0, Release);
    }
}

/// Mark every descriptor touched by a copy, including partial writes and clears.
pub(crate) fn mark(handle: u64, len: usize) {
    let size = MARKS.len.load(Acquire);
    if size == 0 || len == 0 {
        return;
    }
    let start = MARKS.start.load(Relaxed);
    let from = handle.max(start);
    let to = handle.saturating_add(len as u64).min(start + size);
    if from >= to {
        return;
    }
    let slots = MARKS.slots.load(Relaxed);
    let chunks = MARKS.chunks.load(Relaxed);
    for slot in ((from - start) / 32) as usize..((to - start).div_ceil(32)) as usize {
        // SAFETY: The heap range was clipped above and points to process-long maps.
        unsafe {
            (*slots.add(slot)).store(1, Release);
            (*chunks.add(slot / CHUNK)).store(1, Release);
        }
    }
}

/// Drain exact dirty ranges. The caller holds the registered heap alive.
pub(crate) fn take(start: u64) -> Option<Vec<(usize, usize)>> {
    let count = MARKS.len.load(Acquire) as usize / 32;
    if count == 0 || MARKS.start.load(Relaxed) != start {
        return None;
    }
    // SAFETY: Both maps have process lifetime and the registered length.
    let slots = unsafe { std::slice::from_raw_parts(MARKS.slots.load(Relaxed), count) };
    let chunks = unsafe { std::slice::from_raw_parts(MARKS.chunks.load(Relaxed), count.div_ceil(CHUNK)) };
    let ranges = drain(start as usize, slots, chunks);
    let marked = ranges.iter().map(|&(from, to)| (to - from) / 32).sum::<usize>();
    MARKED_SLOTS.fetch_add(marked as u64, Relaxed);
    Some(ranges)
}

pub(crate) fn line(frames: f64) {
    let marked = MARKED_SLOTS.swap(0, Relaxed);
    if marked != 0 {
        tracing::info!(per_frame = marked as f64 / frames.max(1.0), "descriptor slots marked directly");
    }
}

fn drain(start: usize, slots: &[AtomicU8], chunks: &[AtomicU8]) -> Vec<(usize, usize)> {
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for (chunk, dirty) in chunks.iter().enumerate() {
        if dirty.load(Relaxed) == 0 || dirty.swap(0, AcqRel) == 0 {
            continue;
        }
        for (slot, dirty) in slots.chunks(CHUNK).nth(chunk).unwrap_or_default().iter().enumerate() {
            if dirty.load(Relaxed) == 0 || dirty.swap(0, AcqRel) == 0 {
                continue;
            }
            let from = start + (chunk * CHUNK + slot) * 32;
            match ranges.last_mut() {
                Some((_, end)) if *end == from => *end += 32,
                _ => ranges.push((from, from + 32)),
            }
        }
    }
    ranges
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adjacent_marks_across_chunks_are_drained_once() {
        let slots: Vec<_> = (0..130).map(|slot| AtomicU8::new(u8::from(matches!(slot, 125 | 127 | 128)))).collect();
        let chunks = [AtomicU8::new(1), AtomicU8::new(1)];
        assert_eq!(drain(0, &slots, &chunks), [(4000, 4032), (4064, 4128)]);
        assert!(drain(0, &slots, &chunks).is_empty());
    }
}
