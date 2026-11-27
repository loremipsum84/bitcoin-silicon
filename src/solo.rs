use crate::{
    core::{self, Chain, Rpc},
    engine::{Coverage, Engine},
    hash::{decode, display_hash, header, sha256d_bytes, with_nonce, Target},
    progress::{pause, BlockReward, NonceProgress, Progress, Rates},
};
use anyhow::{anyhow, ensure, Context, Result};
use bitcoin::{
    absolute,
    consensus::{deserialize, serialize},
    hashes::Hash,
    script::{Builder, PushBytesBuf},
    transaction, Address, Amount, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut, VarInt,
    Witness,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io,
    ops::Range,
    os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering::Relaxed},
    time::{Duration, Instant},
};

#[derive(Deserialize, Serialize)]
struct TemplateTx {
    data: String,
    txid: String,
    hash: String,
}

#[derive(Deserialize, Serialize)]
struct Gbt {
    version: u32,
    previousblockhash: String,
    bits: String,
    target: String,
    noncerange: String,
    curtime: u32,
    mintime: u32,
    height: u64,
    coinbasevalue: u64,
    #[serde(default)]
    coinbaseaux: BTreeMap<String, String>,
    default_witness_commitment: Option<String>,
    rules: Vec<String>,
    vbrequired: u32,
    transactions: Vec<TemplateTx>,
    weightlimit: u64,
}

struct Template {
    version: u32,
    previous: [u8; 32],
    bits: u32,
    target: Target,
    nonce_range: Range<u64>,
    time: u32,
    height: u64,
    value: u64,
    auxiliary: Vec<PushBytesBuf>,
    witness: Option<ScriptBuf>,
    transactions: Vec<Vec<u8>>,
    txids: Vec<[u8; 32]>,
    weight: u64,
    weight_limit: u64,
}

struct BlockWork {
    header: [u8; 80],
    coinbase: Vec<u8>,
    coinbase_txid: String,
}

fn template(rpc: &Rpc) -> Result<Gbt> {
    rpc.call("getblocktemplate", json!([{"rules":["segwit"]}]))
}

fn merkle(mut leaves: Vec<[u8; 32]>) -> [u8; 32] {
    while leaves.len() > 1 {
        for index in 0..leaves.len().div_ceil(2) {
            let mut pair = [0; 64];
            pair[..32].copy_from_slice(&leaves[index * 2]);
            pair[32..].copy_from_slice(&leaves[(index * 2 + 1).min(leaves.len() - 1)]);
            leaves[index] = sha256d_bytes(&pair);
        }
        leaves.truncate(leaves.len().div_ceil(2));
    }
    leaves[0]
}

impl Template {
    fn parse(gbt: &Gbt) -> Result<Self> {
        for rule in &gbt.rules {
            ensure!(
                !rule.starts_with('!') || rule == "!segwit",
                "Unknown required rule: {rule}"
            );
        }
        ensure!(
            gbt.version & gbt.vbrequired == gbt.vbrequired,
            "Incompatible block version"
        );
        ensure!(
            gbt.curtime >= gbt.mintime,
            "Block template timestamp is before the minimum"
        );
        let bits = u32::from_str_radix(&gbt.bits, 16)?;
        let target = Target::compact(bits)?;
        ensure!(
            target.0 == decode(&gbt.target)?,
            "Inconsistent getblocktemplate targets"
        );
        let mut previous = decode(&gbt.previousblockhash)?;
        previous.reverse();
        let nonce_range = decode::<8>(&gbt.noncerange)?;
        let start = u32::from_be_bytes(nonce_range[..4].try_into()?);
        let end = u32::from_be_bytes(nonce_range[4..].try_into()?);
        ensure!(start <= end, "Reversed noncerange");
        let mut transactions = Vec::with_capacity(gbt.transactions.len());
        let mut txids = Vec::with_capacity(gbt.transactions.len());
        let mut wtxids = vec![[0; 32]];
        let mut weight = 0;
        let mut has_witness = false;
        for entry in &gbt.transactions {
            let data = hex::decode(&entry.data)?;
            let tx: Transaction =
                deserialize(&data).context("Invalid getblocktemplate transaction")?;
            let txid = tx.compute_txid().to_byte_array();
            let wtxid = tx.compute_wtxid().to_byte_array();
            ensure!(
                display_hash(txid) == entry.txid && display_hash(wtxid) == entry.hash,
                "Transaction IDs do not match"
            );
            has_witness |= txid != wtxid;
            txids.push(txid);
            wtxids.push(wtxid);
            weight += tx.weight().to_wu();
            transactions.push(data);
        }
        let witness = match &gbt.default_witness_commitment {
            Some(script) => {
                let mut preimage = [0; 64];
                preimage[..32].copy_from_slice(&merkle(wtxids));
                let mut expected = hex::decode("6a24aa21a9ed")?;
                expected.extend(sha256d_bytes(&preimage));
                let bytes = hex::decode(script)?;
                ensure!(
                    bytes.starts_with(&expected),
                    "Block template has an inconsistent SegWit commitment"
                );
                Some(ScriptBuf::from_bytes(bytes))
            }
            None => {
                ensure!(!has_witness, "Missing SegWit commitment");
                None
            }
        };
        let auxiliary = gbt
            .coinbaseaux
            .values()
            .map(|value| Ok(PushBytesBuf::try_from(hex::decode(value)?)?))
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            version: gbt.version,
            previous,
            bits,
            target,
            nonce_range: u64::from(start)..u64::from(end) + 1,
            time: gbt.curtime,
            height: gbt.height,
            value: gbt.coinbasevalue,
            auxiliary,
            witness,
            transactions,
            txids,
            weight,
            weight_limit: gbt.weightlimit,
        })
    }

    fn work(&self, address: &Address, extra: u64) -> Result<BlockWork> {
        let mut script = Builder::new()
            .push_int(i64::try_from(self.height)?)
            .push_slice(extra.to_le_bytes());
        for value in &self.auxiliary {
            script = script.push_slice(value);
        }
        let script = script.into_script();
        ensure!(
            (2..=100).contains(&script.len()),
            "Coinbase scriptSig is out of range"
        );
        let mut coinbase = Transaction {
            version: transaction::Version::ONE,
            lock_time: absolute::LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint::null(),
                script_sig: script,
                sequence: Sequence::MAX,
                witness: Witness::default(),
            }],
            output: vec![TxOut {
                value: Amount::from_sat(self.value),
                script_pubkey: address.script_pubkey(),
            }],
        };
        if let Some(script) = &self.witness {
            coinbase.input[0].witness.push([0; 32]);
            coinbase.output.push(TxOut {
                value: Amount::ZERO,
                script_pubkey: script.clone(),
            });
        }
        let count = VarInt(self.transactions.len() as u64 + 1);
        ensure!(
            self.weight + coinbase.weight().to_wu() + 4 * (80 + count.size() as u64)
                <= self.weight_limit,
            "Assembled block exceeds the weight limit"
        );
        let mut ids = Vec::with_capacity(self.txids.len() + 1);
        let txid = coinbase.compute_txid();
        ids.push(txid.to_byte_array());
        ids.extend_from_slice(&self.txids);
        Ok(BlockWork {
            header: header(
                self.version,
                self.previous,
                merkle(ids),
                self.time,
                self.bits,
            ),
            coinbase: serialize(&coinbase),
            coinbase_txid: txid.to_string(),
        })
    }

    fn block(&self, work: &BlockWork, nonce: u32) -> String {
        let mut bytes = with_nonce(work.header, nonce).to_vec();
        bytes.extend(serialize(&VarInt(self.transactions.len() as u64 + 1)));
        bytes.extend_from_slice(&work.coinbase);
        for tx in &self.transactions {
            bytes.extend_from_slice(tx);
        }
        hex::encode(bytes)
    }
}

fn proposal(rpc: &Rpc, tpl: &Template, work: &BlockWork) -> Result<Value> {
    rpc.call(
        "getblocktemplate",
        json!([{"mode":"proposal", "rules":["segwit"], "data":tpl.block(work, 0)}]),
    )
}

fn approve(rpc: &Rpc, tpl: &Template, work: &BlockWork) -> Result<()> {
    let result = proposal(rpc, tpl, work)?;
    ensure!(
        result.is_null(),
        "Bitcoin Core rejected the block proposal: {result}"
    );
    Ok(())
}

#[derive(Deserialize, Serialize)]
struct SavedTemplate {
    seed: u64,
    gbt: Gbt,
}

#[derive(Clone, PartialEq, Eq, Deserialize, Serialize)]
struct SavedCoverage {
    seed: u64,
    extra: u64,
    header: String,
    coverage: Coverage,
    #[serde(default)]
    tip: String,
    #[serde(default)]
    previous_tested: u64,
}

impl SavedCoverage {
    fn tested(&self) -> u64 {
        self.previous_tested.saturating_add(self.coverage.tested())
    }
}

fn address_state(state: &Path, chain: Chain, address: &Address) -> PathBuf {
    let destination = hex::encode(sha256d_bytes(address.script_pubkey().as_bytes()));
    state.join("nonces").join(chain.service()).join(destination)
}

pub fn saved_reward(state: &Path, chain: Chain, address: &Address) -> Result<Option<BlockReward>> {
    read_state(&address_state(state, chain, address).join("reward.json"))
}

fn read_state<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    match fs::read(path) {
        Ok(bytes) => {
            Ok(Some(serde_json::from_slice(&bytes).with_context(|| {
                format!("Could not decode {}", path.display())
            })?))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

struct Checkpoint {
    directory: PathBuf,
    _lock: File,
    last: Option<SavedCoverage>,
}

impl Checkpoint {
    fn open(state: &Path, chain: Chain, address: &Address) -> Result<Self> {
        let directory = address_state(state, chain, address);
        fs::create_dir_all(&directory)?;
        let lock = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(directory.join("lock"))?;
        // Hold the advisory lock across checkpoints, including when the files are replaced.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::WouldBlock {
                return Err(anyhow!("Another session is already using this destination"));
            }
            return Err(error).context("Could not acquire the resume lock");
        }
        Ok(Self {
            _lock: lock,
            last: read_state::<SavedCoverage>(&directory.join("coverage.json"))
                .and_then(|saved| {
                    if let Some(cursor) = &saved {
                        cursor.coverage.validate(&(0..1u64 << 32))?;
                    }
                    Ok(saved)
                })
                .unwrap_or_else(|error| {
                    eprintln!("Could not load saved nonce ranges · {error:#}");
                    None
                }),
            directory,
        })
    }

    fn read<T: DeserializeOwned>(&self, name: &str) -> Result<Option<T>> {
        read_state(&self.directory.join(name))
    }

    fn tested_on(&self, tip: &str) -> u64 {
        self.last
            .as_ref()
            .filter(|cursor| cursor.tip == tip)
            .map_or(0, SavedCoverage::tested)
    }

    fn restore(&self, tip: &str, address: &Address) -> Result<Option<Prepared>> {
        let Some(saved) = self.read::<SavedTemplate>("template.json")? else {
            return Ok(None);
        };
        if saved.gbt.previousblockhash != tip {
            return Ok(None);
        }
        let tpl = Template::parse(&saved.gbt)?;
        let cursor = self.last.as_ref().filter(|cursor| {
            cursor.seed == saved.seed && (cursor.tip.is_empty() || cursor.tip == tip)
        });
        let extra = cursor.as_ref().map_or(saved.seed, |cursor| cursor.extra);
        let work = tpl.work(address, extra)?;
        let coverage = match &cursor {
            Some(cursor) => {
                ensure!(
                    cursor.header == hex::encode(&work.header[..76]),
                    "Saved block header is inconsistent"
                );
                cursor.coverage.validate(&tpl.nonce_range)?;
                cursor.coverage.clone()
            }
            None => Coverage::default(),
        };
        let previous_tested =
            cursor.map_or_else(|| self.tested_on(tip), |cursor| cursor.previous_tested);
        let resumed = previous_tested != 0 || coverage.tested() != 0;
        Ok(Some(Prepared {
            tpl,
            work,
            seed: saved.seed,
            extra,
            coverage,
            previous_tested,
            resumed,
        }))
    }

    fn install(&mut self, rpc: &Rpc, tip: &str, address: &Address) -> Result<Prepared> {
        let gbt = template(rpc)?;
        ensure!(
            gbt.previousblockhash == tip,
            "The chain tip changed while reading the block template"
        );
        let tpl = Template::parse(&gbt)?;
        let seed = u64::from_be_bytes(decode(&core::random_hex(8)?)?);
        let work = tpl.work(address, seed)?;
        approve(rpc, &tpl, &work)?;
        self.save_template(SavedTemplate { seed, gbt })?;
        Ok(Prepared {
            tpl,
            work,
            seed,
            extra: seed,
            coverage: Coverage::default(),
            previous_tested: self.tested_on(tip),
            resumed: false,
        })
    }

    fn save_template(&self, saved: SavedTemplate) -> Result<()> {
        // The old coverage file is ignored if the process stops before the new one is saved.
        core::write_private(
            &self.directory.join("template.json"),
            &serde_json::to_vec(&saved)?,
        )
        .context("Could not save the block template")
    }

    fn save(
        &mut self,
        seed: u64,
        extra: u64,
        work: &BlockWork,
        previous_tested: u64,
        coverage: &Coverage,
    ) -> Result<()> {
        let cursor = SavedCoverage {
            seed,
            extra,
            header: hex::encode(&work.header[..76]),
            coverage: coverage.clone(),
            tip: display_hash(work.header[4..36].try_into()?),
            previous_tested,
        };
        if self.last.as_ref() != Some(&cursor) {
            core::write_private(
                &self.directory.join("coverage.json"),
                &serde_json::to_vec(&cursor)?,
            )
            .context("Could not save nonce progress")?;
            self.last = Some(cursor);
        }
        Ok(())
    }

    fn save_reward(&self, reward: &BlockReward) -> Result<()> {
        core::write_private(
            &self.directory.join("reward.json"),
            &serde_json::to_vec(reward)?,
        )
    }
}

struct Prepared {
    tpl: Template,
    work: BlockWork,
    seed: u64,
    extra: u64,
    coverage: Coverage,
    previous_tested: u64,
    resumed: bool,
}

struct Search {
    tpl: Template,
    work: BlockWork,
    seed: u64,
    extra: u64,
    id: u64,
    previous_tested: u64,
    resumed: bool,
}

impl Search {
    fn message(&self) -> String {
        format!(
            "Bitcoin Core accepted the template · Mining at height {}",
            self.tpl.height
        )
    }

    fn start(
        prepared: Prepared,
        engine: &mut Engine,
        checkpoint: &mut Checkpoint,
        status: &mut Progress,
    ) -> Result<Self> {
        let Prepared {
            tpl,
            work,
            seed,
            extra,
            coverage,
            previous_tested,
            resumed,
        } = prepared;
        checkpoint.save(seed, extra, &work, previous_tested, &coverage)?;
        status.height = Some(tpl.height);
        status.nonces = Some(NonceProgress::new(&coverage, previous_tested, resumed));
        let id = engine.set_with_coverage(
            work.header,
            tpl.target,
            tpl.nonce_range.clone(),
            true,
            coverage,
        )?;
        Ok(Self {
            tpl,
            work,
            seed,
            extra,
            id,
            previous_tested,
            resumed,
        })
    }

    fn save(
        &self,
        engine: &Engine,
        checkpoint: &mut Checkpoint,
        status: &mut Progress,
    ) -> Result<()> {
        if let Some((coverage, _)) = engine.coverage() {
            checkpoint.save(
                self.seed,
                self.extra,
                &self.work,
                self.previous_tested,
                &coverage,
            )?;
            status.nonces = Some(NonceProgress::new(
                &coverage,
                self.previous_tested,
                self.resumed,
            ));
        }
        Ok(())
    }
}

pub fn run(
    state: &Path,
    chain: Chain,
    address: &Address,
    mut engine: Engine,
    cancel: &AtomicBool,
    report: &mut impl FnMut(&Progress),
) -> Result<()> {
    let rpc = Rpc::new(state, chain)?;
    let mut checkpoint = Checkpoint::open(state, chain, address)?;
    let mut status = Progress {
        backend: Some(chain.service().into()),
        ..Progress::default()
    };
    if chain == Chain::Regtest {
        status.message = "Starting the isolated regtest node".into();
        report(&status);
        core::node_start(state, chain)?;
    }
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut rates = Rates::new();
    let mut next_check = Instant::now();
    let mut refresh = Instant::now();
    let mut active: Option<Search> = None;
    let mut restore_pending = true;

    while !cancel.load(Relaxed) {
        engine.check_error()?;
        if rates.update(&engine, &mut status) {
            if let Some(search) = &active {
                search.save(&engine, &mut checkpoint, &mut status)?;
            }
            report(&status);
        }
        if Instant::now() >= next_check {
            next_check = Instant::now() + Duration::from_secs(1);
            let info = match rpc.node(chain) {
                Ok(info) => info,
                Err(error) => {
                    if let Some(search) = &active {
                        engine.pause_work();
                        search.save(&engine, &mut checkpoint, &mut status)?;
                    }
                    status.node = None;
                    status.message = format!("Waiting for the node: {error:#}");
                    report(&status);
                    ensure!(
                        chain != Chain::Regtest || Instant::now() < deadline,
                        "{}",
                        status.message
                    );
                    pause(cancel, Duration::from_secs(1));
                    continue;
                }
            };
            let tip = info.bestblockhash.clone();
            if cancel.load(Relaxed) {
                break;
            }
            status.observe_tip(tip.clone());
            let ready = chain == Chain::Regtest || !info.initialblockdownload;
            status.node = Some(info);

            if active
                .as_ref()
                .is_some_and(|search| display_hash(search.tpl.previous) != tip)
            {
                engine.clear();
                active = None;
                status.nonces = None;
            }
            if !ready {
                if let Some(search) = &active {
                    engine.pause_work();
                    search.save(&engine, &mut checkpoint, &mut status)?;
                }
                status.message = "Bitcoin Core is performing its initial sync".into();
                report(&status);
                pause(cancel, Duration::from_secs(1));
                continue;
            }
            if active.is_none() {
                let updated = (|| {
                    if restore_pending {
                        match checkpoint.restore(&tip, address) {
                            Ok(Some(prepared)) => {
                                let result = proposal(&rpc, &prepared.tpl, &prepared.work)?;
                                if result.is_null() {
                                    return Ok(prepared);
                                }
                                eprintln!(
                                    "Bitcoin Core rejected the saved block template · {result}"
                                );
                            }
                            Ok(None) => (),
                            Err(error) => {
                                eprintln!("Could not resume the saved block template · {error:#}")
                            }
                        }
                    }
                    checkpoint.install(&rpc, &tip, address)
                })();
                match updated {
                    Ok(prepared) => {
                        let search =
                            Search::start(prepared, &mut engine, &mut checkpoint, &mut status)?;
                        restore_pending = false;
                        status.message = search.message();
                        active = Some(search);
                        refresh = Instant::now() + Duration::from_secs(15);
                    }
                    Err(error) => {
                        status.message = format!("Waiting for a valid block template: {error:#}");
                        if chain == Chain::Regtest && Instant::now() >= deadline {
                            return Err(anyhow!("{}", status.message));
                        }
                    }
                }
            }
            if let Some(search) = &active {
                if engine.resume_work() {
                    status.message = search.message();
                }
            }
            report(&status);
        }
        if cancel.load(Relaxed) {
            break;
        }
        let mut candidate = engine.receive(Duration::from_millis(40))?;
        let exhausted = candidate.is_none() && engine.exhausted();
        if exhausted {
            // The final worker may have published a result just after the timed receive.
            candidate = engine.receive(Duration::ZERO)?;
        }
        let Some(search) = &mut active else {
            continue;
        };
        if let Some(hit) = candidate {
            if hit.work != search.id {
                status.stale += 1;
                continue;
            }
            status.gpu_last_batch = engine.gpu_last_batch();
            let final_coverage = engine.retire();
            let Search { tpl, work, .. } = search;
            let hash = display_hash(hit.digest);
            let height = tpl.height;
            let submission: Result<Option<Value>> = (|| {
                let best: String = rpc.call("getbestblockhash", json!([]))?;
                if display_hash(tpl.previous) != best {
                    return Ok(None);
                }
                let result: Value = rpc.call("submitblock", json!([tpl.block(work, hit.nonce)]))?;
                Ok(Some(result))
            })()
            .or_else(|error| {
                let current: Result<String> = rpc.call("getblockhash", json!([height]));
                if current.is_ok_and(|current| current == hash) {
                    Ok(Some(Value::Null))
                } else {
                    Err(error)
                }
            });
            match submission {
                Ok(Some(result)) if result.is_null() => {
                    status.accepted += 1;
                    status.hash = Some(hash.clone());
                    let reward = BlockReward {
                        height,
                        block_hash: hash.clone(),
                        txid: work.coinbase_txid.clone(),
                        sats: tpl.value,
                    };
                    let mut message = format!("Block accepted at height {height} · {hash}");
                    if let Err(error) = checkpoint.save_reward(&reward) {
                        message.push_str(&format!(" · Local history unavailable ({error:#})"));
                    }
                    status.reward = Some(reward);
                    status.last_submission = Some(message);
                    status.message = status.last_submission.clone().unwrap();
                    if chain == Chain::Regtest {
                        report(&status);
                        let verified: String = rpc.call("getblockhash", json!([height]))?;
                        ensure!(
                            verified == hash,
                            "Bitcoin Core returned a different hash at height {height}"
                        );
                        let info = rpc.node(chain)?;
                        status.observe_tip(info.bestblockhash.clone());
                        status.node = Some(info);
                        status.nonces = None;
                        status.message =
                            format!("Regtest block accepted and verified · Height {height}");
                        report(&status);
                        return Ok(());
                    }
                }
                Ok(Some(result)) => {
                    status.rejected += 1;
                    status.last_submission = Some(format!("Block rejected · {result}"));
                }
                Ok(None) => {
                    status.stale += 1;
                    status.last_submission =
                        Some("Stale block · New Bitcoin Core chain tip".into());
                }
                Err(error) => {
                    status.unconfirmed += 1;
                    status.last_submission =
                        Some(format!("Submission result unavailable · {error:#}"));
                }
            }
            if let Some(coverage) = final_coverage {
                checkpoint.save(
                    search.seed,
                    search.extra,
                    &search.work,
                    search.previous_tested,
                    &coverage,
                )?;
                status.nonces = Some(NonceProgress::new(
                    &coverage,
                    search.previous_tested,
                    search.resumed,
                ));
            }
            report(&status);
            active = None;
            next_check = Instant::now();
        } else if exhausted {
            search.save(&engine, &mut checkpoint, &mut status)?;
            // Refresh between nonce ranges so an in-progress header can be resumed as-is.
            if Instant::now() >= refresh {
                engine.clear();
                active = None;
                next_check = Instant::now();
                continue;
            }
            let extra = search.extra.wrapping_add(1);
            let next = search.tpl.work(address, extra)?;
            match approve(&rpc, &search.tpl, &next) {
                Ok(()) => {
                    let coverage = Coverage::default();
                    let tested = checkpoint.tested_on(&display_hash(search.tpl.previous));
                    checkpoint.save(search.seed, extra, &next, tested, &coverage)?;
                    search.id = engine.set(
                        next.header,
                        search.tpl.target,
                        search.tpl.nonce_range.clone(),
                        true,
                    )?;
                    search.work = next;
                    search.extra = extra;
                    search.previous_tested = tested;
                    status.nonces = Some(NonceProgress::new(&coverage, tested, search.resumed));
                    report(&status);
                }
                Err(error) => {
                    engine.clear();
                    active = None;
                    next_check = Instant::now();
                    status.message = format!("Could not refresh the block template: {error:#}");
                    report(&status);
                }
            }
        }
    }
    if let Some(search) = &active {
        engine.pause_work();
        engine.check_error()?;
        search.save(&engine, &mut checkpoint, &mut status)?;
        report(&status);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coinbase_height_encodings() {
        assert_eq!(Builder::new().push_int(1).into_script().as_bytes(), &[0x51]);
        assert_eq!(
            Builder::new().push_int(128).into_script().as_bytes(),
            &[2, 128, 0]
        );
        assert_eq!(
            Builder::new().push_int(227_836).into_script().as_bytes(),
            &[3, 0xfc, 0x79, 0x03]
        );
    }

    #[test]
    fn address_network_is_enforced() {
        let regtest = Chain::Regtest.demo_address().to_string();
        assert!(Chain::Regtest.address(&regtest).is_ok());
        assert!(Chain::Mainnet.address(&regtest).is_err());
    }

    #[test]
    fn checkpoint_restores_exact_header_and_ignores_other_tips_or_templates() {
        let dir = tempfile::tempdir().unwrap();
        let chain = Chain::Mainnet;
        let address = chain.demo_address();
        let tip = "01".repeat(32);
        let mut checkpoint = Checkpoint::open(dir.path(), chain, &address).unwrap();
        assert!(Checkpoint::open(dir.path(), chain, &address).is_err());
        let gbt: Gbt = serde_json::from_value(json!({
            "version": 0x20000000, "previousblockhash": tip, "bits": "1d00ffff",
            "target": hex::encode(Target::compact(0x1d00ffff).unwrap().0),
            "noncerange": "00000000ffffffff", "curtime": 1000, "mintime": 999,
            "height": 1000, "coinbasevalue": 5_000_000_000u64, "rules": ["!segwit"],
            "vbrequired": 0, "transactions": [], "weightlimit": 4_000_000
        }))
        .unwrap();
        let seed = 42;
        let extra = seed + 1;
        let work = Template::parse(&gbt)
            .unwrap()
            .work(&address, extra)
            .unwrap();
        let coinbase: Transaction = deserialize(&work.coinbase).unwrap();
        assert_eq!(coinbase.output[0].script_pubkey, address.script_pubkey());
        assert_eq!(coinbase.output[0].value.to_sat(), 5_000_000_000);
        assert_eq!(work.coinbase_txid, coinbase.compute_txid().to_string());
        let coverage = serde_json::from_value(json!({
            "completed": [{"start": 0, "end": 8192}, {"start": 16384, "end": 24576}]
        }))
        .unwrap();
        checkpoint
            .save_template(SavedTemplate { seed, gbt })
            .unwrap();
        checkpoint
            .save(seed, extra, &work, 1u64 << 32, &coverage)
            .unwrap();
        drop(checkpoint);

        let checkpoint = Checkpoint::open(dir.path(), chain, &address).unwrap();
        let restored = checkpoint.restore(&tip, &address).unwrap().unwrap();
        assert_eq!(restored.work.header, work.header);
        assert_eq!(restored.work.coinbase, work.coinbase);
        assert_eq!(restored.extra, extra);
        assert_eq!(restored.coverage, coverage);
        assert_eq!(restored.previous_tested, 1u64 << 32);
        assert!(checkpoint
            .restore(&"02".repeat(32), &address)
            .unwrap()
            .is_none());

        let mut fresh: SavedTemplate = checkpoint.read("template.json").unwrap().unwrap();
        fresh.seed += 10;
        fresh.gbt.curtime += 1;
        checkpoint.save_template(fresh).unwrap();
        let restored = checkpoint.restore(&tip, &address).unwrap().unwrap();
        assert_ne!(restored.work.header, work.header);
        assert_eq!(restored.coverage.tested(), 0);
        assert_eq!(restored.previous_tested, (1u64 << 32) + coverage.tested());
        assert_eq!(checkpoint.tested_on(&"02".repeat(32)), 0);
    }
}
