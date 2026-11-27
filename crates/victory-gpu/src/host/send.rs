//! What goes up to copies in the card's memory goes up as it was when its
//! submission was prepared (`VICTORY_SEND_OWN`, on by default; `0`: as
//! before).
//!
//! The sends were copies out of the copies' CPU sides, which the card makes
//! when it comes to the submission. By then later submissions are prepared
//! (two frames of them with `VICTORY_RELAY`), and their refreshes have
//! written the same CPU sides: the earlier submission's shaders then read a
//! later one's bytes. For what the title's CPU writes that is as on the
//! console (it does not write what the card still reads). But the title's
//! own buffer copies are commands in its lists, and those are done by us
//! when a submission is prepared: "a value to a buffer, a draw that reads
//! it" in one submission and the same with another value in the next is
//! ordinary, and both draws read the second value.
//!
//! So each submission's bytes are taken out of the CPU sides into memory of
//! the submission's own when they are taken from the dirty lists, and the
//! card copies them from there. Runs of a few words, which a refresh that
//! sends only what changed makes many of, go as pairs of offset and value,
//! stored by one dispatch per copy; the others as a copy each.

use super::*;
use crate::shadow::Shadow;

pub(super) fn own() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_SEND_OWN").is_none_or(|v| v != "0"))
}

/// Runs up to this many bytes go as pairs.
const SHORT: usize = 256;

/// Whether a run of `len` bytes at offset `at` of a copy goes as pairs.
fn short(at: usize, len: usize) -> bool {
    len <= SHORT && at.is_multiple_of(4) && len.is_multiple_of(4)
}

/// Whether a run of `len` bytes at offset `at` of a copy in the card's memory
/// is no copy command of its own, but pairs of one dispatch.
pub(crate) fn goes_as_words(at: usize, len: usize) -> bool {
    own() && short(at, len)
}

/// Whether the title's bytes that go in under flights' writes
/// (`Shadow::merge_under_writes`) go in by whole words
/// (`VICTORY_UNDER_WORDS=0`: by the bytes that changed, as before).
///
/// By bytes nearly every run (a float's three low bytes) was a copy command
/// of its own, and a submission with more than 4,096 of them stood until the
/// flights had landed: 4 to 14 ms of a steady frame (session play-1125-0203:
/// "more changed runs than a submission sends up"), for some 20 KB. Whole
/// words are pairs of one dispatch, and are not counted.
///
/// A word is taken to be the title's or the shaders', not shared: where the
/// title changed a byte of a word of which a flight's shader writes another
/// byte, that byte is as it was before the flight. The landing rests on the
/// same ([`crate::shadow::exact_back`]).
pub(crate) fn words_under() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| own() && std::env::var_os("VICTORY_UNDER_WORDS").is_none_or(|v| v != "0"))
}

const SCATTER: &str = r#"
#define RS "RootConstants(num32BitConstants=2, b0), SRV(t0), UAV(u0)"
cbuffer Part : register(b0) { uint at; uint count; };
ByteAddressBuffer pairs : register(t0);
RWByteAddressBuffer card : register(u0);
[RootSignature(RS)]
[numthreads(64, 1, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    if (id.x >= count) return;
    uint2 p = pairs.Load2(at + id.x * 8u);
    card.Store(p.x, p.y);
}
"#;

struct Pass {
    root: ID3D12RootSignature,
    pso: ID3D12PipelineState,
}

fn pass(host: &Host) -> Option<&'static Pass> {
    static PASS: OnceLock<Option<Pass>> = OnceLock::new();
    PASS.get_or_init(|| {
        let code = compile(
            SCATTER,
            windows::core::s!("main"),
            windows::core::s!("cs_5_1"),
        )
        .ok()?;
        // SAFETY: the code lives through the calls; the root signature is
        // inside it.
        unsafe {
            let root: ID3D12RootSignature = host.device.CreateRootSignature(0, &code).ok()?;
            let pso: ID3D12PipelineState = host
                .device
                .CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                    pRootSignature: std::mem::transmute_copy(&root),
                    CS: D3D12_SHADER_BYTECODE {
                        pShaderBytecode: code.as_ptr().cast(),
                        BytecodeLength: code.len(),
                    },
                    ..Default::default()
                })
                .ok()?;
            name(&pso, || "victory: changed words sent to the card".into());
            Some(Pass { root, pso })
        }
    })
    .as_ref()
}

/// Memory a submission's sends come from, mapped.
pub(super) struct Memory {
    buffer: ID3D12Resource,
    data: usize,
    size: u64,
}

// SAFETY: a D3D12 object (free-threaded) and its mapping, with one owner.
unsafe impl Send for Memory {}

/// Uploads whose submissions have completed. Kept sorted for best-fit reuse.
struct Spare {
    buffers: Vec<Memory>,
    bytes: u64,
    limit: u64,
}

static SPARE: Mutex<Spare> = Mutex::new(Spare {
    buffers: Vec::new(),
    bytes: 0,
    limit: u64::MAX,
});

impl Spare {
    fn take(&mut self, size: u64) -> Option<Memory> {
        let i = self.buffers.partition_point(|m| m.size < size);
        if i == self.buffers.len() {
            return None;
        }
        let memory = self.buffers.remove(i);
        self.bytes -= memory.size;
        Some(memory)
    }

    fn trim(&mut self, limit: u64) -> Vec<Memory> {
        let mut retired = Vec::new();
        while self.bytes > limit {
            let memory = self.buffers.pop().expect("the pool has spare bytes");
            self.bytes -= memory.size;
            retired.push(memory);
        }
        retired
    }
}

/// Limit unused committed upload buffers while the title is short of RAM or commit.
pub(super) fn trim_spare(headroom: Option<u64>) {
    let normal = kept();
    let limit = match headroom {
        Some(bytes) if bytes < 1 << 30 => normal.min(64 << 20),
        Some(bytes) if bytes < 2 << 30 => normal.min(192 << 20),
        _ => normal,
    };
    let retired = {
        let mut spare = SPARE.lock().unwrap_or_else(|e| e.into_inner());
        spare.limit = limit;
        spare.trim(limit)
    };
    if !retired.is_empty() {
        let released = retired.iter().map(|m| m.size).sum::<u64>();
        tracing::info!(
            released_mib = released >> 20,
            limit_mib = limit >> 20,
            "idle submission uploads reclaimed"
        );
    }
}

/// At most this much is kept (of the card's memory, with [`on_card`]).
fn kept() -> u64 {
    if on_card() { 256 << 20 } else { 768 << 20 }
}

/// Whether the memory lies in the card's memory, written by the CPU through
/// the bus (`VICTORY_SEND_BAR=1`): the copies on the card's drawing queue, which
/// nothing overlaps, then cross no bus. In a captured frame they took 5.4 ms of
/// the card's 30 (342 copies), and take 0.4 this way; but filling it takes the
/// submitting thread 8-9 ms a frame instead of 3 (73-93 MB, run card-3 against
/// card-1), so it is a loss while the card waits for that thread: off by
/// default.
fn on_card() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_SEND_BAR").is_some_and(|v| v == "1"))
}

fn memory(host: &Host, size: u64) -> Option<Memory> {
    if let Some(memory) = SPARE.lock().unwrap_or_else(|e| e.into_inner()).take(size) {
        return Some(memory);
    }
    // By doubles, so that the next of about this size finds it: what
    // submissions send differs from frame to frame.
    let size = size.max(8 << 20).next_power_of_two();
    let mapped = if on_card() {
        host.mapped_card_buffer(size, false)
    } else {
        None
    };
    let (buffer, data) = match mapped {
        Some(made) => made,
        None => host.upload_buffer(size)?,
    };
    name(&buffer, || "bytes a submission sends up".into());
    Some(Memory {
        buffer,
        data: data as usize,
        size,
    })
}

/// The submission has run.
pub(super) fn give_back(memory: Memory) {
    let retired = {
        let mut spare = SPARE.lock().unwrap_or_else(|e| e.into_inner());
        let limit = spare.limit.min(kept());
        let i = spare.buffers.partition_point(|m| m.size <= memory.size);
        spare.bytes += memory.size;
        spare.buffers.insert(i, memory);
        spare.trim(limit)
    };
    drop(retired);
}

/// Since the last frame-rate line: bulk bytes, bulk runs, scatter words,
/// microseconds preparing the snapshot and microseconds filling it.
pub(super) static STATS: [std::sync::atomic::AtomicU64; 5] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 5];

/// One copy's sends: ranges copied (offset in the copy, in the memory,
/// bytes), and its pairs (where in the memory, how many).
struct Copy {
    runs: Vec<(usize, u64, usize)>,
    pairs: (u64, u32),
}

/// Where a submission's bytes are taken to.
pub(super) struct Own {
    memory: Memory,
    copies: Vec<Copy>,
}

/// Largest contiguous copy one worker takes, to balance mixed upload batches.
const COPY_JOB: usize = 512 << 10;
/// Bound the scanning any one scatter job does, even with mostly bulk ranges.
const RUNS_AT_A_TIME: usize = 4096;

enum Upload<'a> {
    Bulk {
        source: usize,
        offset: u64,
        len: usize,
    },
    Pairs {
        source: usize,
        ranges: &'a [(usize, usize)],
        offset: u64,
    },
}

impl Upload<'_> {
    /// # Safety
    /// All source CPU sides and the upload mapping stay live. This job's
    /// destination is disjoint from the others in the batch.
    unsafe fn fill(&self, data: usize) {
        match *self {
            Self::Bulk {
                source,
                offset,
                len,
            } => {
                // SAFETY: the planner bounds this disjoint stretch.
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        source as *const u8,
                        (data + offset as usize) as *mut u8,
                        len,
                    )
                };
            }
            Self::Pairs {
                source,
                ranges,
                offset,
            } => {
                // Sequential writes to the mapped upload allocation. Pack
                // in CPU memory first to avoid isolated stores to WC memory.
                let mut scratch = [[0u32; 2]; 128];
                let mut used = 0;
                let mut to = (data + offset as usize) as *mut [u32; 2];
                for &(a, b) in ranges.iter().filter(|&&(a, b)| short(a, b - a)) {
                    for offset in (a..b).step_by(4) {
                        // SAFETY: this dword belongs to the CPU side's range.
                        let value = unsafe { ((source + offset) as *const u32).read_unaligned() };
                        scratch[used] = [offset as u32, value];
                        used += 1;
                        if used == scratch.len() {
                            // SAFETY: each job has room for its counted pairs.
                            unsafe {
                                std::ptr::copy_nonoverlapping(scratch.as_ptr(), to, used);
                                to = to.add(used);
                            }
                            used = 0;
                        }
                    }
                }
                // SAFETY: space for the trailing pairs, also within the job.
                unsafe { std::ptr::copy_nonoverlapping(scratch.as_ptr(), to, used) };
            }
        }
    }
}

struct UploadBatch<'a> {
    copies: Vec<Copy>,
    work: Vec<Upload<'a>>,
    size: u64,
    bulk_bytes: u64,
    bulk_runs: u64,
    words: u64,
}

impl<'a> UploadBatch<'a> {
    fn plan(copies: impl Iterator<Item = (usize, &'a [(usize, usize)])>) -> Self {
        let mut out = Self {
            copies: Vec::new(),
            work: Vec::new(),
            size: 0,
            bulk_bytes: 0,
            bulk_runs: 0,
            words: 0,
        };
        for (source, ranges) in copies {
            let first_job = out.work.len();
            let mut runs = Vec::new();
            let mut words = 0;
            for chunk in ranges.chunks(RUNS_AT_A_TIME) {
                let before = words;
                for &(a, b) in chunk {
                    let len = b - a;
                    if short(a, len) {
                        words += len / 4;
                        continue;
                    }
                    runs.push((a, out.size, len));
                    for at in (0..len).step_by(COPY_JOB) {
                        out.work.push(Upload::Bulk {
                            source: source + a + at,
                            offset: out.size + at as u64,
                            len: COPY_JOB.min(len - at),
                        });
                    }
                    out.size += (len as u64).next_multiple_of(16);
                    out.bulk_bytes += len as u64;
                    out.bulk_runs += 1;
                }
                if words > before {
                    out.work.push(Upload::Pairs {
                        source,
                        ranges: chunk,
                        offset: before as u64 * 8,
                    });
                }
            }
            // Scatter words for this shadow follow its bulk copies in the
            // mapping, regardless of how the workers finish their jobs.
            let pairs = (out.size, words as u32);
            for job in &mut out.work[first_job..] {
                if let Upload::Pairs { offset, .. } = job {
                    *offset += pairs.0;
                }
            }
            out.size += (words as u64 * 8).next_multiple_of(16);
            out.words += words as u64;
            out.copies.push(Copy { runs, pairs });
        }
        out
    }
}

/// Snapshot the upload bytes before this submission can see later refreshes.
pub(super) fn take(
    host: &Host,
    dirty: &[(std::sync::Arc<Shadow>, Vec<(usize, usize)>)],
) -> Option<Own> {
    let began = std::time::Instant::now();
    pass(host)?;
    let batch = UploadBatch::plan(
        dirty
            .iter()
            .map(|(shadow, ranges)| (shadow.card_data() as usize, ranges.as_slice())),
    );
    if batch.size == 0 {
        return None;
    }
    let memory = memory(host, batch.size)?;
    let filling = std::time::Instant::now();
    // Both bulk and scatter jobs write disjoint ranges of the same upload
    // buffer. All work joins before the GPU can see the snapshot.
    let fill = |i: usize| unsafe { batch.work[i].fill(memory.data) };
    if batch.size >= 256 << 10 {
        crate::land::each_under(batch.work.len(), &fill);
    } else {
        (0..batch.work.len()).for_each(fill);
    }
    STATS[0].fetch_add(batch.bulk_bytes, Ordering::Relaxed);
    STATS[1].fetch_add(batch.bulk_runs, Ordering::Relaxed);
    STATS[2].fetch_add(batch.words, Ordering::Relaxed);
    STATS[3].fetch_add(began.elapsed().as_micros() as u64, Ordering::Relaxed);
    STATS[4].fetch_add(filling.elapsed().as_micros() as u64, Ordering::Relaxed);
    Some(Own {
        memory,
        copies: batch.copies,
    })
}

impl Own {
    /// Records the sends into `list`; the copies that `looked` says are in
    /// `COPY_DEST`, the others in the common state. Those with a "before" are
    /// left readable by compute shaders.
    ///
    /// # Safety
    /// An open list of ours, whose compute state is set anew behind this.
    pub(super) unsafe fn record(
        &self,
        host: &Host,
        list: &ID3D12GraphicsCommandList,
        dirty: &[(std::sync::Arc<Shadow>, Vec<(usize, usize)>)],
        looked: impl Fn(&Shadow) -> bool,
    ) {
        let read = D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE;
        let write = D3D12_RESOURCE_STATE_UNORDERED_ACCESS;
        let dest = D3D12_RESOURCE_STATE_COPY_DEST;
        let (mut first, mut last) = (Vec::new(), Vec::new());
        for ((shadow, _), copy) in dirty.iter().zip(&self.copies) {
            for &(a, from, n) in &copy.runs {
                // SAFETY: the caller's; live buffers, ranges inside them.
                unsafe {
                    list.CopyBufferRegion(
                        &shadow.buffer,
                        a as u64,
                        &self.memory.buffer,
                        from,
                        n as u64,
                    )
                };
            }
            let mut state = (looked(shadow) || !copy.runs.is_empty()).then_some(dest);
            if copy.pairs.1 > 0 {
                if let Some(was) = state {
                    first.push(transition(&shadow.buffer, was, write));
                }
                state = Some(write);
            }
            let hashed = shadow
                .card_sides()
                .is_some_and(|(_, before, _)| before.is_some());
            if let Some(was) = state
                && hashed
                && (!copy.runs.is_empty() || copy.pairs.1 > 0)
            {
                last.push(transition(&shadow.buffer, was, read));
            }
        }
        // SAFETY: the caller's; live objects.
        unsafe {
            if !first.is_empty() {
                list.ResourceBarrier(&first);
            }
            if let Some(pass) = pass(host)
                && self.copies.iter().any(|c| c.pairs.1 > 0)
            {
                list.SetComputeRootSignature(&pass.root);
                list.SetPipelineState(&pass.pso);
                list.SetComputeRootShaderResourceView(1, self.memory.buffer.GetGPUVirtualAddress());
                for ((shadow, _), copy) in dirty
                    .iter()
                    .zip(&self.copies)
                    .filter(|(_, c)| c.pairs.1 > 0)
                {
                    list.SetComputeRootUnorderedAccessView(2, shadow.buffer.GetGPUVirtualAddress());
                    const MOST: u32 = 64 * D3D12_CS_DISPATCH_MAX_THREAD_GROUPS_PER_DIMENSION;
                    let (at, words) = copy.pairs;
                    let mut done = 0;
                    while done < words {
                        let n = (words - done).min(MOST);
                        let constants = [at as u32 + done * 8, n];
                        list.SetComputeRoot32BitConstants(0, 2, constants.as_ptr().cast(), 0);
                        list.Dispatch(n.div_ceil(64), 1, 1);
                        done += n;
                    }
                }
            }
            if !last.is_empty() {
                list.ResourceBarrier(&last);
            }
        }
    }

    /// The memory, to be kept until the submission has run.
    pub(super) fn memory(self) -> Memory {
        self.memory
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mixed_snapshot_jobs_preserve_bulk_bytes_and_scatter_offsets() {
        let source: Vec<u8> = (0..COPY_JOB + 40_000)
            .map(|i| (i ^ (i >> 8)) as u8)
            .collect();
        let mut ranges: Vec<_> = (0..RUNS_AT_A_TIME + 1)
            .map(|i| (i * 8, i * 8 + 4))
            .collect();
        ranges.push((36_003, source.len()));
        let small = [3u8; 24];
        let small_ranges = [(4, 12), (13, 19)];
        let batch = UploadBatch::plan(
            [
                (source.as_ptr() as usize, ranges.as_slice()),
                (small.as_ptr() as usize, &small_ranges[..]),
            ]
            .into_iter(),
        );
        let mut target = vec![0xcd; batch.size as usize];
        let target_base = target.as_mut_ptr() as usize;
        for job in batch.work.iter().rev() {
            // SAFETY: both source mappings and the planned target are live.
            unsafe { job.fill(target_base) };
        }
        assert_eq!(batch.bulk_runs, 2);
        assert_eq!(batch.words, (RUNS_AT_A_TIME + 3) as u64);
        for ((copy, bytes), ranges) in batch
            .copies
            .iter()
            .zip([source.as_slice(), &small])
            .zip([ranges.as_slice(), &small_ranges])
        {
            for &(at, offset, size) in &copy.runs {
                let start = offset as usize;
                assert_eq!(&target[start..start + size], &bytes[at..at + size]);
            }
            let expected: Vec<_> = ranges
                .iter()
                .filter(|&&(a, b)| short(a, b - a))
                .flat_map(|&(a, b)| (a..b).step_by(4))
                .flat_map(|at| {
                    (at as u32)
                        .to_le_bytes()
                        .into_iter()
                        .chain(bytes[at..at + 4].iter().copied())
                })
                .collect();
            let (start, count) = (copy.pairs.0 as usize, copy.pairs.1 as usize);
            assert_eq!(&target[start..start + count * 8], expected);
        }
    }
}
