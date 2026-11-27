use anyhow::{anyhow, bail, ensure, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use bitcoin::{address::NetworkUnchecked, Address, Amount, Denomination, Network, ScriptBuf};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    fs,
    io::Write,
    net::{Ipv4Addr, TcpListener},
    os::{fd::AsRawFd, unix::fs::PermissionsExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    str::FromStr,
    thread,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Chain {
    Mainnet,
    Regtest,
}

impl Chain {
    pub fn network(self) -> Network {
        match self {
            Self::Mainnet => Network::Bitcoin,
            Self::Regtest => Network::Regtest,
        }
    }
    pub fn service(self) -> &'static str {
        match self {
            Self::Mainnet => "mainnet",
            Self::Regtest => "regtest",
        }
    }
    pub fn rpc_chain(self) -> &'static str {
        match self {
            Self::Mainnet => "main",
            Self::Regtest => "regtest",
        }
    }
    fn port(self) -> Result<u16> {
        let (key, default) = match self {
            Self::Mainnet => ("SILICON_MAINNET_RPC_PORT", 18332),
            Self::Regtest => ("SILICON_REGTEST_RPC_PORT", 18443),
        };
        match std::env::var(key) {
            Ok(value) => {
                let port = value.parse().with_context(|| format!("Invalid {key}"))?;
                ensure!(port != 0, "{key} must be a nonzero TCP port");
                Ok(port)
            }
            Err(std::env::VarError::NotPresent) => Ok(default),
            Err(error) => Err(error).with_context(|| format!("Invalid {key}")),
        }
    }
    fn network_dir(self, datadir: &Path) -> PathBuf {
        match self {
            Self::Mainnet => datadir.to_owned(),
            Self::Regtest => datadir.join(self.rpc_chain()),
        }
    }
    pub fn address(self, value: &str) -> Result<Address> {
        ensure!(value.len() <= 128, "Bitcoin address is too long");
        Address::<NetworkUnchecked>::from_str(value.trim())
            .context("Invalid Bitcoin address")?
            .require_network(self.network())
            .context("Address is incompatible with the selected network")
    }
    pub fn demo_address(self) -> Address {
        Address::p2wsh(&ScriptBuf::from_bytes(vec![0x51]), self.network())
    }
}

pub fn random_hex(bytes: usize) -> Result<String> {
    let mut data = vec![0; bytes];
    getrandom::getrandom(&mut data).map_err(|err| anyhow!("Random number generator: {err}"))?;
    Ok(hex::encode(data))
}

pub fn state_dir() -> PathBuf {
    std::env::var_os("SILICON_STATE_DIR").map_or_else(|| PathBuf::from(".local"), PathBuf::from)
}

pub fn write_private(path: &Path, content: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .filter(|value| !value.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))?;
    file.write_all(content)?;
    file.as_file().sync_all()?;
    file.persist(path)?;
    Ok(())
}

fn node_datadir(state: &Path) -> PathBuf {
    state.join("bitcoin")
}

pub fn prepare(state: &Path) -> Result<()> {
    for directory in [state.to_owned(), node_datadir(state)] {
        fs::create_dir_all(&directory)?;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

pub fn bitcoin_executable(program: &str) -> Result<PathBuf> {
    let bin = std::env::var_os("SILICON_BITCOIN_BIN_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(".local/bitcoin-core/bin"));
    let executable = std::path::absolute(bin.join(program))?;
    ensure!(
        executable.is_file(),
        "Could not find {program}. Run make setup or set SILICON_BITCOIN_BIN_DIR."
    );
    Ok(executable)
}

pub fn bitcoin_command(program: &str, datadir: &Path, chain: Chain, port: u16) -> Result<Command> {
    let mut command = Command::new(bitcoin_executable(program)?);
    command
        .arg("-conf=/dev/null")
        .arg(format!(
            "-datadir={}",
            std::path::absolute(datadir)?.display()
        ))
        .arg(format!("-chain={}", chain.rpc_chain()))
        .arg(format!("-rpcport={port}"));
    Ok(command)
}

pub const OFFLINE: [&str; 5] = [
    "-networkactive=0",
    "-listen=0",
    "-listenonion=0",
    "-dnsseed=0",
    "-discover=0",
];

pub fn daemon_command(datadir: &Path, chain: Chain, port: u16) -> Result<Command> {
    let mut command = bitcoin_command("bitcoind", datadir, chain, port)?;
    command.args([
        "-nosettings",
        "-server=1",
        "-rpcbind=127.0.0.1",
        "-rpcallowip=127.0.0.1",
    ]);
    Ok(command)
}

fn node_lock(state: &Path, chain: Chain) -> Result<fs::File> {
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(state.join(format!("{}.lock", chain.service())))?;
    // Serialize the lifecycle of this node across the CLI and dashboard.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(std::io::Error::last_os_error())
            .context("Could not lock the Bitcoin Core node");
    }
    Ok(lock)
}

pub fn node_start(state: &Path, chain: Chain) -> Result<()> {
    prepare(state)?;
    let _lock = node_lock(state, chain)?;
    let rpc = Rpc::new(state, chain)?;
    if rpc.node(chain).is_ok() {
        return Ok(());
    }
    let port = chain.port()?;
    TcpListener::bind((Ipv4Addr::LOCALHOST, port))
        .with_context(|| format!("Local RPC port {port} is unavailable"))?;

    let mut command = daemon_command(&node_datadir(state), chain, port)?;
    command.args(["-daemonwait", "-disablewallet=1"]);
    match chain {
        Chain::Mainnet => {
            let cache = std::env::var("SILICON_MAINNET_DBCACHE")
                .ok()
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| "450".into());
            let cache: u32 = cache.parse().context("Invalid SILICON_MAINNET_DBCACHE")?;
            command
                .args(["-prune=2048", "-listen=0"])
                .arg(format!("-dbcache={cache}"));
        }
        Chain::Regtest => {
            command.args(OFFLINE);
        }
    }
    let status = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .status()
        .with_context(|| format!("Could not start Bitcoin Core {}", chain.service()))?;
    ensure!(
        status.success(),
        "Bitcoin Core failed. Run bitcoin-silicon node logs {} for details.",
        chain.service()
    );
    Ok(())
}

pub fn node_stop(state: &Path, chain: Chain) -> Result<()> {
    if !state.is_dir() {
        return Ok(());
    }
    let _lock = node_lock(state, chain)?;
    let pid_file = chain.network_dir(&node_datadir(state)).join("bitcoind.pid");
    if !pid_file.exists() {
        return Ok(());
    }
    let rpc = Rpc::new(state, chain)?;
    if let Err(error) = rpc.call::<Value>("stop", json!([])) {
        if pid_file.exists() {
            return Err(error);
        }
    }
    let deadline = Instant::now() + Duration::from_secs(120);
    while pid_file.exists() {
        ensure!(
            Instant::now() < deadline,
            "The {} node is still shutting down",
            chain.service()
        );
        thread::sleep(Duration::from_millis(200));
    }
    Ok(())
}

pub fn node_status(state: &Path, chain: Chain) -> Result<()> {
    let service = chain.service();
    match Rpc::new(state, chain)?.node(chain) {
        Ok(info) => println!(
            "{service}: running, {} blocks / {} headers",
            info.blocks, info.headers
        ),
        Err(_)
            if !chain
                .network_dir(&node_datadir(state))
                .join("bitcoind.pid")
                .exists() =>
        {
            println!("{service}: stopped");
        }
        Err(error) => println!("{service}: RPC unavailable ({error:#})"),
    }
    Ok(())
}

pub fn node_cli(state: &Path, chain: Chain, args: &[String]) -> Result<()> {
    let status = bitcoin_command("bitcoin-cli", &node_datadir(state), chain, chain.port()?)?
        .arg("-rpcconnect=127.0.0.1")
        .args(args)
        .status()?;
    ensure!(status.success(), "bitcoin-cli exited with {status}");
    Ok(())
}

pub fn show_log(log: &Path) -> Result<()> {
    let status = Command::new("tail")
        .args(["-n", "80", "--"])
        .arg(log)
        .status()?;
    ensure!(status.success(), "Could not read {}", log.display());
    Ok(())
}

pub fn node_logs(state: &Path, chain: Chain) -> Result<()> {
    show_log(&chain.network_dir(&node_datadir(state)).join("debug.log"))
}

pub struct Rpc {
    endpoint: String,
    cookie: PathBuf,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct NodeInfo {
    pub chain: String,
    pub blocks: u64,
    pub headers: u64,
    pub verificationprogress: f64,
    pub initialblockdownload: bool,
    pub pruned: bool,
    pub bestblockhash: String,
}

#[derive(Serialize)]
pub struct AddressBalance {
    pub height: u64,
    pub confirmed_sats: u64,
    pub coinbase_sats: u64,
    pub immature_sats: u64,
}

#[derive(Deserialize)]
struct AddressScan {
    success: bool,
    height: u64,
    unspents: Vec<ScannedOutput>,
}

#[derive(Deserialize)]
struct ScannedOutput {
    amount: serde_json::Number,
    coinbase: bool,
    confirmations: u64,
}

impl AddressScan {
    fn balance(self) -> Result<AddressBalance> {
        ensure!(self.success, "Bitcoin output scan was interrupted");
        let mut balance = AddressBalance {
            height: self.height,
            confirmed_sats: 0,
            coinbase_sats: 0,
            immature_sats: 0,
        };
        for output in self.unspents {
            let amount =
                Amount::from_str_in(&output.amount.to_string(), Denomination::Bitcoin)?.to_sat();
            balance.confirmed_sats += amount;
            if output.coinbase {
                balance.coinbase_sats += amount;
                // Match Bitcoin Core wallet accounting, including the mined block itself.
                if output.confirmations <= 100 {
                    balance.immature_sats += amount;
                }
            }
        }
        Ok(balance)
    }
}

impl Rpc {
    pub fn new(state: &Path, chain: Chain) -> Result<Self> {
        Ok(Self {
            endpoint: format!("http://127.0.0.1:{}/", chain.port()?),
            cookie: chain.network_dir(&node_datadir(state)).join(".cookie"),
        })
    }

    pub fn call<T: DeserializeOwned>(&self, method: &str, params: Value) -> Result<T> {
        self.call_with_timeout(method, params, 5)
    }

    fn call_with_timeout<T: DeserializeOwned>(
        &self,
        method: &str,
        params: Value,
        timeout: u64,
    ) -> Result<T> {
        let body = json!({"jsonrpc":"2.0","id":"silicon-miner","method":method,"params":params});
        let cookie = fs::read_to_string(&self.cookie).context(
            "RPC cookie is missing. Start Bitcoin Core with bitcoin-silicon node start.",
        )?;
        let auth = format!("Basic {}", STANDARD.encode(cookie.trim_end()));
        let response = minreq::post(&self.endpoint)
            .with_timeout(timeout)
            .with_max_redirects(0)
            .with_header("Authorization", auth)
            .with_header("Content-Type", "application/json")
            .with_body(serde_json::to_vec(&body)?)
            .send()
            .with_context(|| format!("RPC {method} is unreachable at {}", self.endpoint))?;
        if response.status_code == 503 {
            bail!("Bitcoin Core is loading its data (HTTP 503)");
        }
        if response.status_code == 401 {
            bail!("RPC credentials were rejected. Check the node configuration.");
        }
        let payload: Value = serde_json::from_slice(response.as_bytes())
            .with_context(|| format!("RPC {method}: HTTP {}", response.status_code))?;
        if let Some(error) = payload.get("error").filter(|error| !error.is_null()) {
            bail!(
                "RPC {method}: {} ({})",
                error["message"].as_str().unwrap_or("Bitcoin Core error"),
                error["code"]
            );
        }
        let result = payload
            .get("result")
            .ok_or_else(|| anyhow!("RPC {method} response has no result"))?;
        serde_json::from_value(result.clone())
            .with_context(|| format!("Invalid RPC {method} result"))
    }

    pub fn node(&self, chain: Chain) -> Result<NodeInfo> {
        let info: NodeInfo = self.call("getblockchaininfo", json!([]))?;
        ensure!(
            info.chain == chain.rpc_chain(),
            "Unexpected Bitcoin Core network: {}",
            info.chain
        );
        Ok(info)
    }

    pub fn address_balance(&self, chain: Chain, address: &Address) -> Result<AddressBalance> {
        ensure!(
            !self.node(chain)?.initialblockdownload,
            "Bitcoin Core must finish syncing"
        );
        let descriptor = format!("raw({})", hex::encode(address.script_pubkey().as_bytes()));
        self.call_with_timeout::<AddressScan>("scantxoutset", json!(["start", [descriptor]]), 300)?
            .balance()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_balance_separates_immature_coinbase_and_keeps_satoshis_exact() {
        let scan: AddressScan = serde_json::from_str(
            r#"{
            "success": true, "height": 1234, "unspents": [
                {"amount": 3.12500001, "coinbase": true, "confirmations": 100},
                {"amount": 3.12500002, "coinbase": true, "confirmations": 101},
                {"amount": 0.00000007, "coinbase": false, "confirmations": 1}
            ]
        }"#,
        )
        .unwrap();
        let balance = scan.balance().unwrap();
        assert_eq!(balance.height, 1234);
        assert_eq!(balance.confirmed_sats, 625_000_010);
        assert_eq!(balance.coinbase_sats, 625_000_003);
        assert_eq!(balance.immature_sats, 312_500_001);
    }
}
