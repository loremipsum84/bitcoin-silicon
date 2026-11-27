use crate::{
    engine::Engine,
    hash::{decode, display_hash, header, sha256d_bytes, with_nonce, Target},
    progress::{pause, NonceProgress, Progress, Rates},
};
use anyhow::{anyhow, bail, ensure, Context, Result};
use num_bigint::BigUint;
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    io::{self, Read, Write},
    net::{IpAddr, TcpStream, ToSocketAddrs},
    sync::atomic::{AtomicBool, Ordering::Relaxed},
    time::{Duration, Instant},
};
use zeroize::Zeroizing;

const MAX_LINE: usize = 128 * 1024;
const MAX_PENDING: usize = 128;
const WAIT: Duration = Duration::from_millis(25);

pub struct PoolConfig {
    pub host: String,
    pub port: u16,
    pub worker: String,
    password: Zeroizing<String>,
}

impl PoolConfig {
    pub fn new(server: &str, worker: String, password: Option<String>) -> Result<Self> {
        let endpoint = server
            .trim()
            .strip_prefix("stratum+tcp://")
            .unwrap_or(server.trim());
        let (host, port) = endpoint
            .rsplit_once(':')
            .context("Expected server in host:port format")?;
        let host = if host.starts_with('[') && host.ends_with(']') {
            &host[1..host.len() - 1]
        } else {
            host
        };
        ensure!(
            !host.is_empty()
                && host.len() <= 253
                && (host.parse::<IpAddr>().is_ok()
                    || host.split('.').all(|label| !label.is_empty()
                        && label.len() <= 63
                        && label
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'))),
            "Invalid Stratum host"
        );
        let port: u16 = port.parse().context("Invalid Stratum port")?;
        ensure!(port != 0, "Invalid Stratum port");
        let password = Zeroizing::new(
            password
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| "x".into()),
        );
        ensure!(
            !worker.is_empty()
                && worker.len() <= 128
                && worker.bytes().all(|byte| byte.is_ascii_graphic()),
            "Invalid Stratum worker"
        );
        ensure!(
            password.len() <= 128 && password.bytes().all(|byte| byte.is_ascii_graphic()),
            "Invalid worker password"
        );
        Ok(Self {
            host: host.into(),
            port,
            worker,
            password,
        })
    }

    fn connect(&self, cancel: &AtomicBool) -> Result<Wire> {
        // BSD DNS resolution is blocking. Keep it off the controller so stop stays responsive.
        let host = self.host.clone();
        let port = self.port;
        let (send, receive) = std::sync::mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("stratum-dns".into())
            .spawn(move || {
                let _ = send.send(
                    (host.as_str(), port)
                        .to_socket_addrs()
                        .map(|ips| ips.collect::<Vec<_>>()),
                );
            })?;
        let addresses = loop {
            ensure!(!cancel.load(Relaxed), "Connection interrupted");
            match receive.recv_timeout(Duration::from_millis(100)) {
                Ok(result) => break result.context("Could not resolve the pool's hostname")?,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(error) => return Err(error.into()),
            }
        };
        for addr in addresses {
            if cancel.load(Relaxed) {
                break;
            }
            if let Ok(stream) = TcpStream::connect_timeout(&addr, Duration::from_secs(2)) {
                stream.set_read_timeout(Some(WAIT))?;
                stream.set_write_timeout(Some(Duration::from_secs(2)))?;
                stream.set_nodelay(true)?;
                return Ok(Wire {
                    stream,
                    buffer: vec![],
                });
            }
        }
        bail!("Could not connect to the pool over TCP")
    }
}

struct Wire {
    stream: TcpStream,
    buffer: Vec<u8>,
}

impl Wire {
    fn send(&mut self, msg: Value) -> Result<()> {
        let mut bytes = Zeroizing::new(serde_json::to_vec(&msg)?);
        bytes.push(b'\n');
        self.stream
            .write_all(&bytes)
            .context("Could not write to the Stratum connection")
    }

    fn receive(&mut self) -> Result<Vec<Value>> {
        let mut buf = [0; 8192];
        match self.stream.read(&mut buf) {
            Ok(0) => bail!("Stratum connection closed"),
            Ok(n) => self.buffer.extend_from_slice(&buf[..n]),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) => {}
            Err(error) => return Err(error).context("Could not read from the Stratum connection"),
        }
        let mut messages = vec![];
        let mut used = 0;
        for line in self.buffer.split_inclusive(|byte| *byte == b'\n') {
            ensure!(line.len() <= MAX_LINE, "Stratum message is too long");
            if !line.ends_with(b"\n") {
                break;
            }
            used += line.len();
            if line.iter().all(|byte| byte.is_ascii_whitespace()) {
                continue;
            }
            messages.push(serde_json::from_slice(line).context("Invalid Stratum JSON")?);
        }
        self.buffer.drain(..used);
        Ok(messages)
    }
}

#[derive(Clone)]
struct Difficulty {
    display: String,
    target: Target,
}

impl Difficulty {
    fn parse(value: &Value) -> Result<Self> {
        let text = value
            .as_str()
            .map(str::to_owned)
            .or_else(|| value.as_number().map(ToString::to_string))
            .context("Missing Stratum difficulty")?;
        ensure!(text.len() <= 96, "Stratum difficulty is too long");
        let (base, exponent) = match text.split_once(['e', 'E']) {
            Some((base, exp)) => (
                base,
                exp.parse::<i32>().context("Invalid Stratum exponent")?,
            ),
            None => (text.as_str(), 0),
        };
        ensure!(
            (-128..=128).contains(&exponent),
            "Stratum exponent is out of range"
        );
        let (integer, fraction) = base.split_once('.').unwrap_or((base, ""));
        let digits = format!("{}{}", integer.trim_start_matches('+'), fraction);
        ensure!(
            !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()),
            "Invalid Stratum difficulty"
        );
        let numerator =
            BigUint::parse_bytes(digits.as_bytes(), 10).context("Invalid Stratum difficulty")?;
        ensure!(numerator.bits() > 0, "Stratum difficulty is zero");
        let scale = i32::try_from(fraction.len())? - exponent;
        let one = BigUint::from_bytes_be(&Target::compact(0x1d00ffff)?.0);
        let ten = BigUint::from(10u32);
        let target = if scale >= 0 {
            one * ten.pow(scale as u32) / numerator
        } else {
            one / (numerator * ten.pow((-scale) as u32))
        };
        let bytes = target.to_bytes_be();
        let mut result = [0; 32];
        if bytes.len() > 32 {
            result.fill(0xff);
        } else {
            result[32 - bytes.len()..].copy_from_slice(&bytes);
        }
        Ok(Self {
            display: text,
            target: Target(result),
        })
    }
}

struct Job {
    id: String,
    prev: [u8; 32],
    coin1: Vec<u8>,
    coin2: Vec<u8>,
    branches: Vec<[u8; 32]>,
    version: u32,
    bits: u32,
    time: u32,
    clean: bool,
    difficulty: Difficulty,
}

fn string(params: &Value, index: usize) -> Result<&str> {
    params[index]
        .as_str()
        .ok_or_else(|| anyhow!("Invalid Stratum parameter {index}"))
}

fn word(text: &str) -> Result<u32> {
    Ok(u32::from_be_bytes(decode(text)?))
}

impl Job {
    fn parse(params: &Value, difficulty: Difficulty) -> Result<Self> {
        let id = string(params, 0)?;
        ensure!(
            !id.is_empty() && id.len() <= 128 && id.bytes().all(|byte| byte.is_ascii_graphic()),
            "Invalid job ID"
        );
        let mut prev = decode::<32>(string(params, 1)?)?;
        for chunk in prev.chunks_exact_mut(4) {
            chunk.reverse();
        }
        let branches = params[4]
            .as_array()
            .context("Missing Stratum Merkle branches")?;
        ensure!(branches.len() <= 32, "Too many Stratum Merkle branches");
        let branches = branches
            .iter()
            .map(|item| decode(item.as_str().context("Invalid Stratum Merkle branch")?))
            .collect::<Result<_>>()?;
        let bits = word(string(params, 6)?)?;
        Target::compact(bits)?;
        Ok(Self {
            id: id.into(),
            prev,
            coin1: hex::decode(string(params, 2)?)?,
            coin2: hex::decode(string(params, 3)?)?,
            branches,
            version: word(string(params, 5)?)?,
            bits,
            time: word(string(params, 7)?)?,
            clean: params[8].as_bool().context("Invalid clean_jobs value")?,
            difficulty,
        })
    }

    fn work(&self, extra1: &[u8], extra2: &[u8]) -> Submission {
        let mut coinbase = self.coin1.clone();
        coinbase.extend_from_slice(extra1);
        coinbase.extend_from_slice(extra2);
        coinbase.extend_from_slice(&self.coin2);
        let mut merkle = sha256d_bytes(&coinbase);
        let mut pair = [0; 64];
        for branch in &self.branches {
            pair[..32].copy_from_slice(&merkle);
            pair[32..].copy_from_slice(branch);
            merkle = sha256d_bytes(&pair);
        }
        Submission {
            job: self.id.clone(),
            extra: hex::encode(extra2),
            time: format!("{:08x}", self.time),
            header: header(self.version, self.prev, merkle, self.time, self.bits),
            target: self.difficulty.target,
        }
    }
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Extra {
    first: Vec<u8>,
    size: usize,
}
impl Extra {
    fn parse(first: &Value, size: &Value) -> Result<Self> {
        let first = hex::decode(first.as_str().context("Missing extranonce1")?)?;
        ensure!(first.len() <= 64, "extranonce1 is too large");
        let size = size.as_u64().context("Invalid extranonce2 size")?;
        ensure!((1..=32).contains(&size), "extranonce2 size is out of range");
        Ok(Self {
            first,
            size: size as usize,
        })
    }
}

#[derive(Default)]
struct WorkHistory {
    previous_tested: u64,
    cursors: BTreeMap<Extra, Option<Vec<u8>>>,
}

impl WorkHistory {
    fn reserve(&mut self, extra: &Extra) -> Option<(Vec<u8>, bool)> {
        // A reservation survives replacement and reconnect, even if its range was only partially scanned.
        let next = self
            .cursors
            .entry(extra.clone())
            .or_insert_with(|| Some(vec![0; extra.size]));
        let current = next.as_mut()?;
        let reserved = current.clone();
        for byte in current.iter_mut().rev() {
            *byte = byte.wrapping_add(1);
            if *byte != 0 {
                return Some((reserved, false));
            }
        }
        *next = None;
        Some((reserved, true))
    }
}

struct Submission {
    job: String,
    extra: String,
    time: String,
    header: [u8; 80],
    target: Target,
}

struct Session {
    authorized: bool,
    extra: Option<Extra>,
    job: Option<Job>,
    difficulty: Difficulty,
    new_job: bool,
    waiting_for_extra: bool,
    works: BTreeMap<u64, Submission>,
    pending: BTreeMap<u64, Instant>,
    next_id: u64,
}

fn retire_work(engine: &Engine, previous_tested: &mut u64) {
    if let Some(coverage) = engine.retire() {
        *previous_tested = previous_tested.saturating_add(coverage.tested());
    }
}

impl Session {
    fn new() -> Result<Self> {
        Ok(Self {
            authorized: false,
            extra: None,
            job: None,
            difficulty: Difficulty::parse(&json!(1))?,
            new_job: false,
            waiting_for_extra: false,
            works: BTreeMap::new(),
            pending: BTreeMap::new(),
            next_id: 4,
        })
    }

    fn line(
        &mut self,
        wire: &mut Wire,
        message: Value,
        engine: &Engine,
        status: &mut Progress,
        previous_tested: &mut u64,
    ) -> Result<()> {
        if let Some(method) = message["method"].as_str() {
            let params = &message["params"];
            match method {
                "mining.set_difficulty" => self.difficulty = Difficulty::parse(&params[0])?,
                "mining.set_extranonce" => {
                    let received = Extra::parse(&params[0], &params[1])?;
                    if self.extra.as_ref() == Some(&received) {
                        return Ok(());
                    }
                    self.extra = Some(received);
                    self.works.clear();
                    self.job = None;
                    self.new_job = false;
                    self.waiting_for_extra = false;
                    retire_work(engine, previous_tested);
                    status.job = None;
                    status.message = "Extranonce updated · Waiting for a job".into();
                }
                "mining.notify" => {
                    let job = Job::parse(params, self.difficulty.clone())?;
                    if job.clean
                        || self
                            .job
                            .as_ref()
                            .is_some_and(|old| old.prev != job.prev || old.id == job.id)
                    {
                        self.works.clear();
                    }
                    retire_work(engine, previous_tested);
                    self.job = Some(job);
                    self.new_job = true;
                }
                "client.reconnect" => bail!("The pool requested a reconnect"),
                "client.get_version" if !message["id"].is_null() => {
                    wire.send(json!({"id":message["id"],"result":concat!("silicon-miner/",env!("CARGO_PKG_VERSION")),"error":null}))?;
                }
                "mining.ping" if !message["id"].is_null() => {
                    wire.send(json!({"id":message["id"],"result":"pong","error":null}))?;
                }
                _ => (),
            }
            return Ok(());
        }
        let id = message["id"]
            .as_u64()
            .or_else(|| message["id"].as_str().and_then(|id| id.parse().ok()));
        match id {
            Some(1) => {
                ensure!(message["error"].is_null(), "Stratum subscription rejected");
                let result = &message["result"];
                self.extra = Some(Extra::parse(&result[1], &result[2])?);
                wire.send(json!({"id":3,"method":"mining.extranonce.subscribe","params":[]}))?;
            }
            Some(2) => {
                ensure!(
                    message["result"] == true && message["error"].is_null(),
                    "Worker authorization rejected"
                );
                self.authorized = true;
                status.message = "Worker authorized · Waiting for a job".into();
            }
            Some(id) if self.pending.remove(&id).is_some() => {
                let error = &message["error"];
                let code = error[0].as_i64().or_else(|| error["code"].as_i64());
                let kind = if message["result"] == true && error.is_null() {
                    status.accepted += 1;
                    "Share accepted"
                } else if code == Some(21) {
                    status.stale += 1;
                    "Stale share · Unknown job (21)"
                } else {
                    status.rejected += 1;
                    match code {
                        Some(22) => "Share rejected · Duplicate (22)",
                        Some(23) => "Share rejected · Difficulty too low (23)",
                        Some(24) => "Share rejected · Unauthorized worker (24)",
                        Some(25) => "Share rejected · Missing subscription (25)",
                        _ => "Share rejected by the pool",
                    }
                };
                status.last_submission = Some(format!("{kind} · Submission {id}"));
            }
            _ => (),
        }
        Ok(())
    }

    fn activate(
        &mut self,
        engine: &mut Engine,
        status: &mut Progress,
        history: &mut WorkHistory,
    ) -> Result<()> {
        if !self.authorized || self.waiting_for_extra {
            return Ok(());
        }
        let (Some(job), Some(extra)) = (&self.job, &self.extra) else {
            return Ok(());
        };
        if !self.new_job && !engine.exhausted() {
            return Ok(());
        }
        retire_work(engine, &mut history.previous_tested);
        // Job IDs can change independently of the header, so the cursor belongs to the extranonce pair.
        let Some((extra2, final_range)) = history.reserve(extra) else {
            self.waiting_for_extra = true;
            status.message = "Extranonce2 space exhausted · Waiting for updated extranonce".into();
            return Ok(());
        };
        let tip = display_hash(job.prev);
        if status
            .previous_block
            .as_ref()
            .is_some_and(|previous| *previous != tip)
        {
            history.previous_tested = 0;
        }
        status.observe_tip(tip);
        let work = job.work(&extra.first, &extra2);
        self.new_job = false;
        let id = engine.set(work.header, work.target, 0..1u64 << 32, false)?;
        self.works.insert(id, work);
        while self.works.len() > 16 {
            self.works.pop_first();
        }
        status.message = if final_range {
            "Final pool extranonce2 range".into()
        } else {
            "Stratum job active".into()
        };
        status.job = Some(job.id.clone());
        status.difficulty = Some(job.difficulty.display.clone());
        Ok(())
    }

    fn submit(
        &mut self,
        wire: &mut Wire,
        config: &PoolConfig,
        engine: &Engine,
        status: &mut Progress,
    ) -> Result<()> {
        for _ in 0..32 {
            if self.pending.len() >= MAX_PENDING {
                break;
            }
            let Some(hit) = engine.receive(Duration::ZERO)? else {
                break;
            };
            let Some(work) = self.works.get(&hit.work) else {
                status.stale += 1;
                continue;
            };
            let digest = sha256d_bytes(&with_nonce(work.header, hit.nonce));
            ensure!(
                digest == hit.digest && work.target.meets(&digest),
                "Share failed local verification"
            );
            let id = self.next_id;
            self.next_id += 1;
            self.pending.insert(id, Instant::now());
            wire.send(json!({"id":id,"method":"mining.submit",
                "params":[config.worker,work.job,work.extra,work.time,format!("{:08x}",hit.nonce)]}))?;
        }
        Ok(())
    }
}

fn session(
    config: &PoolConfig,
    wire: &mut Wire,
    engine: &mut Engine,
    cancel: &AtomicBool,
    status: &mut Progress,
    report: &mut impl FnMut(&Progress),
    history: &mut WorkHistory,
) -> (bool, Result<()>) {
    let mut session = match Session::new() {
        Ok(value) => value,
        Err(error) => return (false, Err(error)),
    };
    let mut rates = Rates::new();
    rates.begin(engine);
    let result = (|| {
        wire.send(json!({"id":1,"method":"mining.subscribe","params":[concat!("silicon-miner/",env!("CARGO_PKG_VERSION"))]}))?;
        wire.send(json!({"id":2,"method":"mining.authorize","params":[config.worker,config.password.as_str()]}))?;
        let start = Instant::now();
        let mut last_input = start;
        while !cancel.load(Relaxed) {
            let messages = wire.receive()?;
            let changed = !messages.is_empty();
            if changed {
                last_input = Instant::now();
            }
            for line in messages {
                session.line(wire, line, engine, status, &mut history.previous_tested)?;
            }
            if cancel.load(Relaxed) {
                break;
            }
            session.submit(wire, config, engine, status)?;
            session.activate(engine, status, history)?;
            let sampled = rates.update(engine, status);
            if changed || sampled {
                status.nonces = engine
                    .coverage()
                    .map(|(done, _)| NonceProgress::new(&done, history.previous_tested, false));
                report(status);
            }
            ensure!(
                (session.authorized && session.extra.is_some())
                    || start.elapsed() < Duration::from_secs(15),
                "Stratum handshake timed out"
            );
            ensure!(
                last_input.elapsed() < Duration::from_secs(180),
                "Stratum pool stopped responding"
            );
            ensure!(
                session
                    .pending
                    .first_key_value()
                    .is_none_or(|(_, at)| at.elapsed() < Duration::from_secs(15)),
                "The pool stopped responding to submissions"
            );
        }
        Ok(())
    })();
    rates.flush_stale(engine, status);
    status.unconfirmed += session.pending.len() as u64;
    (session.authorized, result)
}

pub fn run(
    config: PoolConfig,
    mut engine: Engine,
    cancel: &AtomicBool,
    report: &mut impl FnMut(&Progress),
) -> Result<()> {
    let mut delay = 1;
    let mut history = WorkHistory::default();
    let mut status = Progress {
        backend: Some("pool".into()),
        ..Progress::default()
    };
    while !cancel.load(Relaxed) {
        status.message = format!("Connecting to {}:{} over Stratum", config.host, config.port);
        report(&status);
        let (authorized, result) = match config.connect(cancel) {
            Ok(mut wire) => session(
                &config,
                &mut wire,
                &mut engine,
                cancel,
                &mut status,
                report,
                &mut history,
            ),
            Err(error) => (false, Err(error)),
        };
        retire_work(&engine, &mut history.previous_tested);
        status.nonces = None;
        let obsolete_before = engine.counts().2;
        for hit in std::iter::from_fn(|| engine.receive(Duration::ZERO).transpose()) {
            hit?;
            status.stale += 1;
        }
        // GPU failures must stop the session and surface the CPU fallback message.
        engine.check_error()?;
        if cancel.load(Relaxed) {
            break;
        }
        if authorized {
            delay = 1;
        }
        if let Err(error) = result {
            status.message = format!("Stratum · {error:#} · Retrying in {delay} s");
        }
        status.job = None;
        status.difficulty = None;
        status.cpu_rate = 0.0;
        status.gpu_rate = 0.0;
        report(&status);
        pause(cancel, Duration::from_secs(delay));
        status.stale += engine.counts().2 - obsolete_before;
        delay = (delay * 2).min(30);
    }
    report(&status);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_share_difficulty() {
        let one = Difficulty::parse(&json!(1)).unwrap().target;
        assert_eq!(one, Target::compact(0x1d00ffff).unwrap());
        let tiny = Difficulty::parse(&json!(0.00001)).unwrap().target;
        assert_eq!(tiny, Difficulty::parse(&json!("1e-5")).unwrap().target);
        assert_eq!(
            BigUint::from_bytes_be(&tiny.0),
            BigUint::from_bytes_be(&one.0) * 100_000u32
        );
        assert_eq!(
            Difficulty::parse(&json!("1e-50")).unwrap().target,
            Target([255; 32])
        );
        assert!(Difficulty::parse(&json!(-1.0)).is_err());
        assert!(Difficulty::parse(&json!(0)).is_err());
    }

    fn example_job() -> Job {
        Job::parse(
            &json!([
                "job",
                "03020100070605040b0a09080f0e0d0c13121110171615141b1a19181f1e1d1c",
                "00",
                "ff",
                [],
                "20000000",
                "1d00ffff",
                "10203040",
                true
            ]),
            Difficulty::parse(&json!(1)).unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn stratum_word_order() {
        let work = example_job().work(&[1], &[2]);
        assert_eq!(&work.header[4..36], &(0..32).collect::<Vec<u8>>());
        assert_eq!(
            &work.header[68..76],
            &[0x40, 0x30, 0x20, 0x10, 0xff, 0xff, 0, 0x1d]
        );
        assert_eq!(&work.header[36..68], &sha256d_bytes(&[0, 1, 2, 255]));
    }

    #[test]
    fn exhausted_extranonce_waits_without_reusing_a_range() {
        let mut history = WorkHistory::default();
        let extra = Extra::parse(&json!("01"), &json!(1)).unwrap();
        for value in 0..=u8::MAX {
            assert_eq!(
                history.reserve(&extra),
                Some((vec![value], value == u8::MAX))
            );
        }
        assert_eq!(history.reserve(&extra), None);
        let wider = Extra::parse(&json!("01"), &json!(2)).unwrap();
        assert_eq!(history.reserve(&wider), Some((vec![0, 0], false)));
        let mut engine =
            Engine::start(crate::engine::Mode::Cpu, 1, crate::engine::GPU_BATCH).unwrap();
        let mut session = Session::new().unwrap();
        session.authorized = true;
        session.extra = Some(extra);
        session.job = Some(example_job());
        session.new_job = true;
        let mut status = Progress::default();
        session
            .activate(&mut engine, &mut status, &mut history)
            .unwrap();
        assert!(session.waiting_for_extra);
        assert!(engine.coverage().is_none());
        assert!(status.message.contains("Extranonce2 space exhausted"));
    }
}
