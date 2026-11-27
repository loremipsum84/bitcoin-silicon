use crate::{
    hash::{sha256d_bytes, with_nonce, Midstate, Target},
    metal::{self, Batch, Gpu},
};
use anyhow::{anyhow, bail, ensure, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    ops::Range,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering::Relaxed},
        mpsc::{self, Receiver, SyncSender, TrySendError},
        Arc, Condvar, Mutex,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

pub const MAX_THREADS: usize = 64;
pub const GPU_BATCH: u32 = 1_048_576;
pub const MAX_GPU_BATCH: u32 = 16_777_216;
const CPU_BATCH: u32 = 8192;

pub fn default_threads() -> usize {
    thread::available_parallelism().map_or(1, usize::from)
}

pub fn max_threads() -> usize {
    MAX_THREADS.max(default_threads())
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CpuQos {
    Inherit,
    Utility,
    Background,
}

impl CpuQos {
    fn configured() -> Result<Self> {
        match std::env::var("SILICON_CPU_QOS") {
            Err(std::env::VarError::NotPresent) => Ok(Self::Inherit),
            Ok(value) => match value.as_str() {
                "inherit" => Ok(Self::Inherit),
                "utility" => Ok(Self::Utility),
                "background" => Ok(Self::Background),
                _ => bail!("SILICON_CPU_QOS must be inherit, utility, or background"),
            },
            Err(error) => Err(anyhow!("Could not read SILICON_CPU_QOS: {error}")),
        }
    }

    fn apply(self) -> Result<()> {
        #[cfg(target_os = "macos")]
        {
            use libc::qos_class_t::*;
            let class = match self {
                Self::Inherit => return Ok(()),
                Self::Utility => QOS_CLASS_UTILITY,
                Self::Background => QOS_CLASS_BACKGROUND,
            };
            // This runs on the worker being configured. pthread returns an error code directly.
            let result = unsafe { libc::pthread_set_qos_class_self_np(class, 0) };
            ensure!(
                result == 0,
                "Could not set CPU worker QoS to {self:?}: {}",
                std::io::Error::from_raw_os_error(result)
            );
            Ok(())
        }
        #[cfg(not(target_os = "macos"))]
        {
            ensure!(
                matches!(self, Self::Inherit),
                "CPU worker QoS classes require macOS"
            );
            Ok(())
        }
    }
}

#[derive(Clone, Copy, Default)]
pub struct Metrics {
    pub cpu: u64,
    pub gpu: u64,
    pub obsolete: u64,
    pub cancelled_gpu: u64,
    pub candidates: u64,
    pub valid: u64,
    pub cpu_verified: u64,
    pub replayed: u64,
    pub gpu_full: u64,
    pub gpu_batches: u64,
    pub gpu_timed_batches: u64,
    pub gpu_nanos: u64,
    pub gpu_max_nanos: u64,
}

#[derive(Default)]
struct Counters {
    cpu: AtomicU64,
    gpu: AtomicU64,
    obsolete: AtomicU64,
    cancelled_gpu: AtomicU64,
    candidates: AtomicU64,
    valid: AtomicU64,
    cpu_verified: AtomicU64,
    replayed: AtomicU64,
    gpu_full: AtomicU64,
    gpu_batches: AtomicU64,
    gpu_timed_batches: AtomicU64,
    gpu_nanos: AtomicU64,
    gpu_max_nanos: AtomicU64,
    gpu_last_batch: AtomicU64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Cpu,
    #[default]
    Gpu,
    Hybrid,
}

#[derive(Clone, Debug, PartialEq, Eq, Default, Deserialize, Serialize)]
pub struct Coverage {
    completed: Vec<Range<u64>>,
}

impl Coverage {
    pub fn tested(&self) -> u64 {
        self.completed
            .iter()
            .map(|range| range.end - range.start)
            .sum()
    }

    pub fn validate(&self, range: &Range<u64>) -> Result<()> {
        let mut next = range.start;
        for done in &self.completed {
            ensure!(
                done.start >= next && done.start < done.end && done.end <= range.end,
                "Invalid saved nonce ranges"
            );
            next = done.end;
        }
        Ok(())
    }

    fn gaps(&self, range: Range<u64>) -> VecDeque<Range<u64>> {
        let mut gaps = VecDeque::new();
        let mut next = range.start;
        for done in &self.completed {
            if next < done.start {
                gaps.push_back(next..done.start);
            }
            next = done.end;
        }
        if next < range.end {
            gaps.push_back(next..range.end);
        }
        gaps
    }

    fn record(&mut self, range: Range<u64>) {
        if range.is_empty() {
            return;
        }
        let first = self
            .completed
            .partition_point(|done| done.end < range.start);
        let last = self
            .completed
            .partition_point(|done| done.start <= range.end);
        if first == last {
            self.completed.insert(first, range);
        } else {
            let start = self.completed[first].start.min(range.start);
            let end = self.completed[last - 1].end.max(range.end);
            self.completed[first] = start..end;
            self.completed.drain(first + 1..last);
        }
    }
}

impl Mode {
    pub fn parse(mode: &str) -> Result<Self> {
        match mode {
            "cpu" => Ok(Self::Cpu),
            "gpu" => Ok(Self::Gpu),
            "hybrid" => Ok(Self::Hybrid),
            _ => bail!("Expected engine: cpu, gpu, or hybrid"),
        }
    }
}

#[derive(Clone)]
pub struct Candidate {
    pub work: u64,
    pub nonce: u32,
    pub digest: [u8; 32],
}

struct Work {
    id: u64,
    header: [u8; 80],
    mid: Arc<Midstate>,
    target: Target,
    single: bool,
    solved: AtomicBool,
    obsolete: AtomicBool,
}

struct Queue {
    work: Option<Arc<Work>>,
    remaining: VecDeque<Range<u64>>,
    coverage: Coverage,
    total: u64,
    in_flight: usize,
    paused: bool,
    closed: bool,
}

impl Queue {
    fn reserve(&mut self, size: u32, gpu: bool) -> Option<Range<u64>> {
        let work = self.work.as_ref()?;
        if self.paused || work.solved.load(Relaxed) {
            return None;
        }
        let gap = self.remaining.front_mut()?;
        let top = work.target.top();
        let size = if work.single && top >= 1 << 24 {
            size.min(if gpu { 4096 } else { 128 })
        } else if gpu && !work.single {
            // Keep the expected number of hits below 64 for easy share targets.
            let limited = ((64u64 << 32) / (u64::from(top) + 1)).max(metal::HITS as u64);
            size.min(limited.min(u64::from(u32::MAX)) as u32)
        } else {
            size
        };
        let start = gap.start;
        let end = (start + u64::from(size)).min(gap.end);
        gap.start = end;
        if gap.is_empty() {
            self.remaining.pop_front();
        }
        self.in_flight += 1;
        Some(start..end)
    }
}

struct Shared {
    queue: Mutex<Queue>,
    wake: Condvar,
    tx: SyncSender<Candidate>,
    counters: Counters,
    error: Mutex<Option<String>>,
}

struct Lease {
    shared: Arc<Shared>,
    work: Arc<Work>,
    range: Range<u64>,
    completed_until: u64,
}

impl Drop for Lease {
    fn drop(&mut self) {
        let mut queue = self.shared.queue.lock().unwrap();
        if queue
            .work
            .as_ref()
            .is_some_and(|work| Arc::ptr_eq(work, &self.work))
        {
            queue
                .coverage
                .record(self.range.start..self.completed_until);
            queue.in_flight -= 1;
            if queue.paused && queue.in_flight == 0 {
                self.shared.wake.notify_all();
            }
        }
    }
}

impl Shared {
    fn lease(self: &Arc<Self>, size: u32, wait: bool, gpu: bool) -> Option<Lease> {
        let mut queue = self.queue.lock().unwrap();
        loop {
            if queue.closed {
                return None;
            }
            if let Some(range) = queue.reserve(size, gpu) {
                return Some(Lease {
                    shared: self.clone(),
                    work: queue.work.as_ref().unwrap().clone(),
                    completed_until: range.start,
                    range,
                });
            }
            if !wait {
                return None;
            }
            queue = self.wake.wait(queue).unwrap();
        }
    }

    fn candidate(&self, work: &Work, nonce: u32, digest: [u8; 32]) {
        if work.obsolete.load(Relaxed) {
            self.counters.obsolete.fetch_add(1, Relaxed);
            return;
        }
        if work.single && work.solved.swap(true, Relaxed) {
            return;
        }
        let mut candidate = Candidate {
            work: work.id,
            nonce,
            digest,
        };
        loop {
            match self.tx.try_send(candidate) {
                Ok(()) | Err(TrySendError::Disconnected(_)) => return,
                Err(TrySendError::Full(pending)) => {
                    if work.obsolete.load(Relaxed) {
                        self.counters.obsolete.fetch_add(1, Relaxed);
                        return;
                    }
                    candidate = pending;
                    thread::sleep(Duration::from_millis(1));
                }
            }
        }
    }

    fn fail(&self, error: anyhow::Error) {
        *self.error.lock().unwrap() = Some(format!("{error:#}"));
        self.close();
    }

    fn close(&self) {
        let mut queue = self.queue.lock().unwrap();
        queue.closed = true;
        if let Some(work) = queue.work.take() {
            work.obsolete.store(true, Relaxed);
        }
        self.wake.notify_all();
    }
}

pub struct Engine {
    shared: Arc<Shared>,
    workers: Vec<JoinHandle<()>>,
    candidates: Receiver<Candidate>,
    cached: Option<([u8; 64], Arc<Midstate>)>,
    id: u64,
    pub gpu_width: Option<usize>,
    pub cpu_qos: Option<CpuQos>,
}

impl Engine {
    pub fn start(mode: Mode, threads: usize, gpu_batch: u32) -> Result<Self> {
        ensure!(
            (1..=max_threads()).contains(&threads),
            "CPU thread count is out of range"
        );
        validate_gpu_batch(gpu_batch)?;
        let cpu_qos = (mode != Mode::Gpu).then(CpuQos::configured).transpose()?;
        let (tx, candidates) = mpsc::sync_channel(256);
        let shared = Arc::new(Shared {
            queue: Mutex::new(Queue {
                work: None,
                remaining: VecDeque::new(),
                coverage: Coverage::default(),
                total: 0,
                in_flight: 0,
                paused: false,
                closed: false,
            }),
            wake: Condvar::new(),
            tx,
            counters: Counters::default(),
            error: Mutex::new(None),
        });
        let mut engine = Self {
            shared,
            workers: vec![],
            candidates,
            cached: None,
            id: 0,
            gpu_width: None,
            cpu_qos,
        };
        if mode != Mode::Cpu {
            let shared = engine.shared.clone();
            let (ready, initialized) = mpsc::sync_channel(1);
            let worker = thread::Builder::new().name("metal".into()).spawn(move || {
                let gpu = match Gpu::new().and_then(|gpu| {
                    metal::verify(&gpu)?;
                    Ok(gpu)
                }) {
                    Ok(gpu) => {
                        let _ = ready.send(Ok(gpu.width));
                        gpu
                    }
                    Err(error) => {
                        let _ = ready.send(Err(error));
                        return;
                    }
                };
                if let Err(error) = gpu_worker(&shared, &gpu, gpu_batch) {
                    shared.fail(anyhow!("{error:#}. Retry in CPU mode."));
                }
            })?;
            engine.workers.push(worker);
            engine.gpu_width = Some(initialized.recv()??);
        }
        if let Some(qos) = cpu_qos {
            for id in 0..threads {
                let shared = engine.shared.clone();
                let sha2 = crate::hash::arm_sha();
                let (ready, initialized) = mpsc::sync_channel(1);
                let worker = thread::Builder::new()
                    .name(format!("cpu-{id}"))
                    .spawn(move || {
                        let result = qos.apply();
                        let configured = result.is_ok();
                        if ready.send(result).is_ok() && configured {
                            cpu_worker(&shared, sha2);
                        }
                    })?;
                engine.workers.push(worker);
                initialized.recv()??;
            }
        }
        Ok(engine)
    }

    pub fn set(
        &mut self,
        header: [u8; 80],
        target: Target,
        range: Range<u64>,
        single: bool,
    ) -> Result<u64> {
        self.set_with_coverage(header, target, range, single, Coverage::default())
    }

    pub fn set_with_coverage(
        &mut self,
        header: [u8; 80],
        target: Target,
        range: Range<u64>,
        single: bool,
        coverage: Coverage,
    ) -> Result<u64> {
        ensure!(
            range.start < range.end && range.end <= 1u64 << 32,
            "Invalid nonce range"
        );
        coverage.validate(&range)?;
        self.id = self
            .id
            .checked_add(1)
            .ok_or_else(|| anyhow!("Work IDs exhausted"))?;
        // Reuse the compression state even when nTime or the tail of the Merkle root changes.
        let prefix = header[..64].try_into()?;
        let mid = match &self.cached {
            Some((previous, mid)) if *previous == prefix => Arc::new(mid.with_tail(&header)),
            _ => Arc::new(Midstate::new(&header)),
        };
        self.cached = Some((prefix, mid.clone()));
        let work = Arc::new(Work {
            id: self.id,
            header,
            mid,
            target,
            single,
            solved: false.into(),
            obsolete: false.into(),
        });
        let mut queue = self.shared.queue.lock().unwrap();
        ensure!(!queue.closed, "Engine stopped");
        if let Some(old) = queue.work.replace(work) {
            old.obsolete.store(true, Relaxed);
        }
        queue.total = range.end - range.start;
        queue.remaining = coverage.gaps(range);
        queue.coverage = coverage;
        queue.in_flight = 0;
        queue.paused = false;
        self.shared.wake.notify_all();
        Ok(self.id)
    }

    pub fn clear(&self) {
        self.retire();
    }

    pub fn retire(&self) -> Option<Coverage> {
        let mut queue = self.shared.queue.lock().unwrap();
        let work = queue.work.take()?;
        work.obsolete.store(true, Relaxed);
        Some(std::mem::take(&mut queue.coverage))
    }

    pub fn exhausted(&self) -> bool {
        let queue = self.shared.queue.lock().unwrap();
        queue.remaining.is_empty() && queue.in_flight == 0
    }

    pub fn coverage(&self) -> Option<(Coverage, u64)> {
        let queue = self.shared.queue.lock().unwrap();
        queue.work.as_ref()?;
        Some((queue.coverage.clone(), queue.total))
    }

    pub fn pause_work(&self) {
        let mut queue = self.shared.queue.lock().unwrap();
        queue.paused = true;
        while queue.in_flight != 0 && !queue.closed {
            queue = self.shared.wake.wait(queue).unwrap();
        }
    }

    pub fn resume_work(&self) -> bool {
        let mut queue = self.shared.queue.lock().unwrap();
        let paused = queue.paused;
        if paused {
            queue.paused = false;
            self.shared.wake.notify_all();
        }
        paused
    }

    pub fn check_error(&self) -> Result<()> {
        if let Some(error) = self.shared.error.lock().unwrap().as_ref() {
            bail!("{error}");
        }
        Ok(())
    }

    pub fn receive(&self, duration: Duration) -> Result<Option<Candidate>> {
        self.check_error()?;
        match self.candidates.recv_timeout(duration) {
            Ok(candidate) => Ok(Some(candidate)),
            Err(mpsc::RecvTimeoutError::Timeout) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    pub fn counts(&self) -> (u64, u64, u64) {
        let c = &self.shared.counters;
        (
            c.cpu.load(Relaxed),
            c.gpu.load(Relaxed),
            c.obsolete.load(Relaxed),
        )
    }

    pub fn metrics(&self) -> Metrics {
        let c = &self.shared.counters;
        Metrics {
            cpu: c.cpu.load(Relaxed),
            gpu: c.gpu.load(Relaxed),
            obsolete: c.obsolete.load(Relaxed),
            cancelled_gpu: c.cancelled_gpu.load(Relaxed),
            candidates: c.candidates.load(Relaxed),
            valid: c.valid.load(Relaxed),
            cpu_verified: c.cpu_verified.load(Relaxed),
            replayed: c.replayed.load(Relaxed),
            gpu_full: c.gpu_full.load(Relaxed),
            gpu_batches: c.gpu_batches.load(Relaxed),
            gpu_timed_batches: c.gpu_timed_batches.load(Relaxed),
            gpu_nanos: c.gpu_nanos.load(Relaxed),
            gpu_max_nanos: c.gpu_max_nanos.load(Relaxed),
        }
    }

    pub fn gpu_last_batch(&self) -> Option<u64> {
        let batch = self.shared.counters.gpu_last_batch.load(Relaxed);
        (batch != 0).then_some(batch)
    }

    pub fn reset_gpu_max(&self) {
        self.shared.counters.gpu_max_nanos.store(0, Relaxed);
    }
}

pub fn validate_gpu_batch(batch: u32) -> Result<()> {
    ensure!(
        (1..=MAX_GPU_BATCH).contains(&batch),
        "GPU batch size is out of range"
    );
    Ok(())
}

pub fn configured_gpu_batch() -> Result<u32> {
    let batch = match std::env::var("SILICON_GPU_BATCH") {
        Ok(value) => value.parse()?,
        Err(std::env::VarError::NotPresent) => GPU_BATCH,
        Err(error) => return Err(error.into()),
    };
    validate_gpu_batch(batch)?;
    Ok(batch)
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.shared.close();
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

fn cpu_worker(shared: &Arc<Shared>, sha2: bool) {
    let mut hits = Vec::new();
    let counters = &shared.counters;
    while let Some(mut lease) = shared.lease(CPU_BATCH, true, false) {
        let work = &lease.work;
        let mut computed = 0;
        let mut verified = 0;
        let mut valid = 0;
        let mut candidates = 0;
        let top = work.target.top();
        let mut start = lease.range.start;
        while start < lease.range.end && !work.obsolete.load(Relaxed) && !work.solved.load(Relaxed)
        {
            let count = (lease.range.end - start).min(2048) as u32;
            scan_cpu(&work.mid, top, start as u32, count, sha2, &mut hits);
            if work.obsolete.load(Relaxed) || work.solved.load(Relaxed) {
                hits.clear();
                break;
            }
            computed += u64::from(count);
            candidates += hits.len() as u64;
            for nonce in hits.drain(..) {
                if work.solved.load(Relaxed) {
                    break;
                }
                let digest = sha256d_bytes(&with_nonce(work.header, nonce));
                verified += 1;
                if work.target.meets(&digest) {
                    valid += 1;
                    shared.candidate(work, nonce, digest);
                }
            }
            start += u64::from(count);
            // Leave any candidate-bearing chunk eligible for replay if submission is interrupted.
            if !work.solved.load(Relaxed) {
                lease.completed_until = start;
            }
        }
        counters.candidates.fetch_add(candidates, Relaxed);
        counters.valid.fetch_add(valid, Relaxed);
        counters.cpu_verified.fetch_add(verified, Relaxed);
        counters.cpu.fetch_add(computed, Relaxed);
    }
}

fn scan_cpu(mid: &Midstate, top: u32, start: u32, count: u32, sha2: bool, hits: &mut Vec<u32>) {
    #[cfg(target_arch = "aarch64")]
    if sha2 {
        // The engine detects ARM SHA-2 once per worker. Leases never wrap.
        unsafe { crate::hash_arm::scan(mid, top, start, count, hits) };
        return;
    }
    let _ = sha2;
    for nonce in (0..count).map(|offset| start + offset) {
        let digest = mid.hash(nonce);
        if u32::from_le_bytes(digest[28..].try_into().unwrap()) <= top {
            hits.push(nonce);
        }
    }
}

fn gpu_worker(shared: &Arc<Shared>, gpu: &Gpu, size: u32) -> Result<()> {
    let mut pending = VecDeque::<(Lease, Batch<'_>)>::new();
    let counters = &shared.counters;
    loop {
        while pending.len() < metal::SLOTS {
            let Some(lease) = shared.lease(size, pending.is_empty(), true) else {
                break;
            };
            let start = lease.range.start as u32;
            let count = (lease.range.end - lease.range.start) as u32;
            let batch = gpu.submit(&lease.work.mid, lease.work.target, start, count)?;
            pending.push_back((lease, batch));
        }

        let Some((mut lease, batch)) = pending.pop_front() else {
            return Ok(());
        };
        let scan = batch.hits()?;
        let size = lease.range.end - lease.range.start;
        let work = &lease.work;
        if work.obsolete.load(Relaxed) || work.solved.load(Relaxed) {
            if work.obsolete.load(Relaxed) {
                counters.obsolete.fetch_add(u64::from(scan.count), Relaxed);
            }
            counters.cancelled_gpu.fetch_add(size, Relaxed);
            continue;
        }

        counters.gpu_last_batch.store(size, Relaxed);
        counters.gpu_full.fetch_add(u64::from(scan.full), Relaxed);
        counters.gpu_batches.fetch_add(1, Relaxed);
        let nanos = (scan.gpu_seconds * 1e9) as u64;
        if nanos != 0 {
            counters.gpu_nanos.fetch_add(nanos, Relaxed);
            counters.gpu_max_nanos.fetch_max(nanos, Relaxed);
            counters.gpu_timed_batches.fetch_add(1, Relaxed);
        }
        counters
            .candidates
            .fetch_add(u64::from(scan.count), Relaxed);
        counters.gpu.fetch_add(size, Relaxed);
        if work.solved.load(Relaxed) {
            continue;
        }

        if scan.count as usize > scan.nonces.len() && !work.single {
            // An exceptionally easy target can still fill the result buffer.
            // Replay this lease exactly once before it is released.
            for nonce in lease.range.clone().map(|nonce| nonce as u32) {
                if work.obsolete.load(Relaxed) {
                    break;
                }
                let digest = sha256d_bytes(&with_nonce(work.header, nonce));
                counters.replayed.fetch_add(1, Relaxed);
                if work.target.meets(&digest) {
                    counters.valid.fetch_add(1, Relaxed);
                    shared.candidate(work, nonce, digest);
                }
            }
            if !work.obsolete.load(Relaxed) {
                lease.completed_until = lease.range.end;
            }
            continue;
        }
        for nonce in scan.nonces {
            ensure!(
                lease.range.contains(&u64::from(nonce)),
                "Metal nonce is outside the batch"
            );
            let digest = sha256d_bytes(&with_nonce(work.header, nonce));
            counters.cpu_verified.fetch_add(1, Relaxed);
            ensure!(
                work.target.meets(&digest),
                "Metal candidate failed CPU verification"
            );
            counters.valid.fetch_add(1, Relaxed);
            shared.candidate(work, nonce, digest);
            if work.single {
                break;
            }
        }
        if !work.solved.load(Relaxed) {
            lease.completed_until = lease.range.end;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn explicit_qos_is_applied_on_the_current_worker() {
        use libc::qos_class_t::*;

        for (qos, expected) in [
            (CpuQos::Utility, QOS_CLASS_UTILITY),
            (CpuQos::Background, QOS_CLASS_BACKGROUND),
        ] {
            thread::spawn(move || {
                qos.apply().unwrap();
                let mut observed = QOS_CLASS_UNSPECIFIED;
                let result = unsafe {
                    libc::pthread_get_qos_class_np(
                        libc::pthread_self(),
                        &mut observed,
                        std::ptr::null_mut(),
                    )
                };
                assert_eq!(result, 0);
                assert_eq!(observed as u32, expected as u32);
            })
            .join()
            .unwrap();
        }
    }

    #[test]
    fn interleaved_scanner_matches_reference_at_nonce_boundaries() {
        let accelerated = crate::hash::arm_sha();
        let headers = [
            crate::hash::decode(crate::hash::GENESIS).unwrap(),
            [0; 80],
            std::array::from_fn(|i| (i * 73 + 11) as u8),
        ];
        for header in headers {
            let mid = Midstate::new(&header);
            for (start, count) in [(0, 1), (0x10203040, 87), (u32::MAX - 2048, 2049)] {
                let first = sha256d_bytes(&with_nonce(header, start));
                let exact_top = u32::from_le_bytes(first[28..].try_into().unwrap());
                for top in [0, exact_top, 0x40000000, u32::MAX] {
                    let expected: Vec<_> = (0..count)
                        .map(|offset| start + offset)
                        .filter(|&nonce| {
                            let hash = sha256d_bytes(&with_nonce(header, nonce));
                            u32::from_le_bytes(hash[28..].try_into().unwrap()) <= top
                        })
                        .collect();
                    for sha2 in [false, accelerated] {
                        let mut hits = Vec::new();
                        scan_cpu(&mid, top, start, count, sha2, &mut hits);
                        assert_eq!(hits, expected, "SHA-2 ARM={sha2}");
                    }
                }
            }
        }
    }

    #[test]
    #[ignore = "Requires access to a Metal GPU"]
    fn metal_equality_and_hybrid_lease_accounting() {
        let genesis = crate::hash::decode(crate::hash::GENESIS).unwrap();
        let nonce = u64::from(u32::from_le_bytes(genesis[76..].try_into().unwrap()));
        let mut engine = Engine::start(Mode::Gpu, 1, GPU_BATCH).unwrap();
        engine
            .set(genesis, Target([0; 32]), nonce..nonce + 1, false)
            .unwrap();
        assert!(engine
            .receive(Duration::from_millis(500))
            .unwrap()
            .is_none());
        assert!(engine.exhausted());
        let metrics = engine.metrics();
        assert_eq!(
            (metrics.gpu, metrics.gpu_full, metrics.candidates),
            (1, 1, 0)
        );
        assert_eq!((metrics.cpu_verified, metrics.valid), (0, 0));
        drop(engine);

        let mut engine = Engine::start(Mode::Hybrid, 4, GPU_BATCH).unwrap();
        let end = 1u64 << 32;
        let range = end - u64::from(CPU_BATCH) * 2 - 3..end;
        let mut hits = Vec::new();
        let id = engine
            .set([0; 80], Target([255; 32]), range.clone(), false)
            .unwrap();
        for _ in range.clone() {
            let hit = engine.receive(Duration::from_secs(2)).unwrap().unwrap();
            assert_eq!(hit.work, id);
            hits.push(u64::from(hit.nonce));
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !engine.exhausted() {
            assert!(std::time::Instant::now() < deadline);
            thread::yield_now();
        }
        hits.sort_unstable();
        assert_eq!(hits, range.clone().collect::<Vec<_>>());
        let metrics = engine.metrics();
        assert_eq!(metrics.cpu + metrics.gpu, range.end - range.start);
        assert_eq!(metrics.cpu_verified, metrics.valid);
        assert_eq!(metrics.valid, metrics.cpu + metrics.gpu);
    }

    #[test]
    #[ignore = "Requires access to a Metal GPU"]
    fn metal_work_replacement_and_stop() {
        use std::time::Instant;

        let genesis = crate::hash::decode(crate::hash::GENESIS).unwrap();
        let nonce = u64::from(u32::from_le_bytes(genesis[76..].try_into().unwrap()));
        let mut engine = Engine::start(Mode::Gpu, 1, GPU_BATCH).unwrap();
        let mut replacements = Vec::new();
        for _ in 0..8 {
            let start = engine.counts().1;
            engine
                .set([0; 80], Target([0; 32]), 0..1u64 << 32, false)
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(3);
            while engine.counts().1 == start {
                assert!(Instant::now() < deadline);
                thread::sleep(Duration::from_millis(1));
            }
            let at = Instant::now();
            let id = engine
                .set(
                    genesis,
                    Target::compact(0x1d00ffff).unwrap(),
                    nonce..nonce + 1,
                    true,
                )
                .unwrap();
            let hit = engine.receive(Duration::from_secs(3)).unwrap().unwrap();
            assert_eq!((hit.work, u64::from(hit.nonce)), (id, nonce));
            replacements.push(at.elapsed().as_secs_f64() * 1000.0);
        }
        let start = engine.counts().1;
        engine
            .set([255; 80], Target([0; 32]), 0..1u64 << 32, false)
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while engine.counts().1 == start {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(1));
        }
        let at = Instant::now();
        drop(engine);
        println!(
            "Work publication to result (ms): {replacements:.2?} | Engine shutdown (ms): {:.2}",
            at.elapsed().as_secs_f64() * 1000.0
        );
    }

    #[test]
    fn cpu_leases_cover_boundaries_once() {
        let mut engine = Engine::start(Mode::Cpu, 4, GPU_BATCH).unwrap();
        let end = 1u64 << 32;
        let id = engine
            .set([0; 80], Target([255; 32]), end - 500..end, false)
            .unwrap();
        let mut nonces = Vec::new();
        for _ in 0..500 {
            let hit = engine.receive(Duration::from_secs(2)).unwrap().unwrap();
            assert_eq!(hit.work, id);
            nonces.push(u64::from(hit.nonce));
        }
        nonces.sort_unstable();
        assert_eq!(nonces, (end - 500..end).collect::<Vec<_>>());
    }

    #[test]
    fn resume_schedules_only_gaps_in_completed_leases() {
        let end = 1u64 << 32;
        let mut coverage = Coverage::default();
        coverage.record(end - 12..end - 4);
        coverage.record(end - 20..end - 16);
        coverage.record(end - 8..end - 4);
        assert_eq!(coverage.tested(), 12);

        let mut engine = Engine::start(Mode::Cpu, 2, GPU_BATCH).unwrap();
        let header = crate::hash::decode(crate::hash::GENESIS).unwrap();
        engine
            .set_with_coverage(header, Target([255; 32]), end - 20..end, false, coverage)
            .unwrap();
        let mut hits = Vec::new();
        for _ in 0..8 {
            let hit = engine.receive(Duration::from_secs(1)).unwrap().unwrap();
            hits.push(u64::from(hit.nonce));
        }
        engine.pause_work();
        hits.sort_unstable();
        assert_eq!(
            hits,
            (end - 16..end - 12).chain(end - 4..end).collect::<Vec<_>>()
        );
        assert!(engine.exhausted());
        let (covered, total) = engine.coverage().unwrap();
        assert_eq!((covered.tested(), total), (20, 20));
        assert!(engine.receive(Duration::ZERO).unwrap().is_none());
    }
}
