use crate::{
    core::NodeInfo,
    engine::{Coverage, Engine},
};
use serde::Serialize;
use std::{
    sync::atomic::{AtomicBool, Ordering::Relaxed},
    thread,
    time::{Duration, Instant},
};

#[derive(Clone, Default, Serialize)]
pub struct Progress {
    pub message: String,
    pub cpu_rate: f64,
    pub gpu_rate: f64,
    pub gpu_last_batch: Option<u64>,
    pub previous_block: Option<String>,
    pub block_changes: u64,
    pub node: Option<NodeInfo>,
    pub height: Option<u64>,
    pub hash: Option<String>,
    pub accepted: u64,
    pub rejected: u64,
    pub stale: u64,
    pub unconfirmed: u64,
    pub backend: Option<String>,
    pub last_submission: Option<String>,
    pub difficulty: Option<String>,
    pub job: Option<String>,
    pub nonces: Option<NonceProgress>,
    pub reward: Option<BlockReward>,
}

#[derive(Clone, Serialize)]
pub struct NonceProgress {
    pub tested: u64,
    pub resumed: bool,
}

impl NonceProgress {
    pub fn new(coverage: &Coverage, previous_tested: u64, resumed: bool) -> Self {
        Self {
            tested: previous_tested.saturating_add(coverage.tested()),
            resumed,
        }
    }
}

#[derive(Clone, serde::Deserialize, Serialize)]
pub struct BlockReward {
    pub height: u64,
    pub block_hash: String,
    pub txid: String,
    pub sats: u64,
}

pub struct Rates {
    at: Instant,
    counts: (u64, u64, u64),
}

impl Progress {
    pub fn observe_tip(&mut self, hash: String) {
        if self
            .previous_block
            .as_ref()
            .is_some_and(|previous| *previous != hash)
        {
            self.block_changes += 1;
            self.nonces = None;
        }
        self.previous_block = Some(hash);
    }
}

impl Rates {
    pub fn new() -> Self {
        Self {
            at: Instant::now(),
            counts: (0, 0, 0),
        }
    }

    pub fn begin(&mut self, engine: &Engine) {
        self.at = Instant::now();
        self.counts = engine.counts();
    }

    pub fn flush_stale(&mut self, engine: &Engine, status: &mut Progress) {
        let stale = engine.counts().2;
        status.stale += stale - self.counts.2;
        self.counts.2 = stale;
    }

    pub fn update(&mut self, engine: &Engine, status: &mut Progress) -> bool {
        let elapsed = self.at.elapsed();
        if elapsed < Duration::from_secs(1) {
            return false;
        }
        let counts = engine.counts();
        status.gpu_last_batch = engine.gpu_last_batch();
        status.cpu_rate = (counts.0 - self.counts.0) as f64 / elapsed.as_secs_f64();
        status.gpu_rate = (counts.1 - self.counts.1) as f64 / elapsed.as_secs_f64();
        status.stale += counts.2 - self.counts.2;
        self.at = Instant::now();
        self.counts = counts;
        true
    }
}

pub fn pause(cancel: &AtomicBool, duration: Duration) {
    let until = Instant::now() + duration;
    while !cancel.load(Relaxed) {
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        thread::sleep(left.min(Duration::from_millis(100)));
    }
}
