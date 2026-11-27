use crate::{
    core::{self, AddressBalance, Chain, NodeInfo, Rpc},
    engine::{
        configured_gpu_batch, default_threads, max_threads, validate_gpu_batch, Engine, Mode,
        MAX_GPU_BATCH,
    },
    hash, metal, pool,
    progress::{BlockReward, Progress},
    services::{DashboardControl, CONTROL_HEADER, CONTROL_STATUS, CONTROL_STOP},
    solo,
};
use anyhow::{anyhow, bail, ensure, Context, Result};
use bitcoin::Address;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::VecDeque,
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering::Relaxed},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

pub const PORT: u16 = 7878;
pub const URL: &str = "http://127.0.0.1:7878";
const MAX_BODY: usize = 4096;

#[derive(Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum Route {
    Mainnet,
    Regtest,
    Pool,
}

#[derive(Clone, Default, Deserialize, Serialize)]
struct Settings {
    mainnet: Option<String>,
    regtest: Option<String>,
}

impl Settings {
    fn address(&self, chain: Chain) -> Option<&str> {
        match chain {
            Chain::Mainnet => self.mainnet.as_deref(),
            Chain::Regtest => self.regtest.as_deref(),
        }
    }
    fn set_address(&mut self, state: &Path, chain: Chain, address: String) -> Result<()> {
        let mut next = self.clone();
        match chain {
            Chain::Mainnet => next.mainnet = Some(address),
            Chain::Regtest => next.regtest = Some(address),
        }
        fs::create_dir_all(state)?;
        core::write_private(
            &state.join("settings.json"),
            &serde_json::to_vec_pretty(&next)?,
        )?;
        *self = next;
        Ok(())
    }
}

#[derive(Clone, Serialize)]
struct Active {
    route: Route,
    mode: Mode,
    threads: usize,
    gpu_batch: Option<u32>,
    address: Option<String>,
    stopping: bool,
    #[serde(skip)]
    cancel: Arc<AtomicBool>,
}

#[derive(Default, Serialize)]
struct Node {
    info: Option<NodeInfo>,
    error: Option<String>,
    sync_eta_seconds: Option<u64>,
}

#[derive(Default)]
struct SyncEta {
    samples: VecDeque<(Instant, f64)>,
    last_block: Option<(u64, Instant)>,
}

impl SyncEta {
    fn observe(&mut self, now: Instant, info: &NodeInfo) -> Option<u64> {
        const WINDOW: Duration = Duration::from_secs(120);
        const MIN_SAMPLE: Duration = Duration::from_secs(30);
        const STALL: Duration = Duration::from_secs(45);

        let progress = info.verificationprogress;
        if !info.initialblockdownload || !(0.0..1.0).contains(&progress) {
            *self = Self::default();
            return None;
        }
        if self
            .last_block
            .is_some_and(|(height, at)| info.blocks < height || now.duration_since(at) >= STALL)
        {
            self.samples.clear();
        }
        if self
            .last_block
            .is_none_or(|(height, _)| info.blocks != height)
        {
            self.last_block = Some((info.blocks, now));
        }
        while self
            .samples
            .front()
            .is_some_and(|(at, _)| now.duration_since(*at) > WINDOW)
        {
            self.samples.pop_front();
        }
        self.samples.push_back((now, progress));
        let (first_at, first_progress) = *self.samples.front()?;
        let elapsed = now.duration_since(first_at);
        let gained = progress - first_progress;
        if elapsed < MIN_SAMPLE || gained <= 0.0 {
            return None;
        }
        let seconds = ((1.0 - progress) * elapsed.as_secs_f64() / gained).ceil();
        Duration::try_from_secs_f64(seconds)
            .ok()
            .map(|eta| eta.as_secs())
    }
}

#[derive(Default, Serialize)]
struct Wallet {
    busy: bool,
    message: Option<String>,
    chain: Option<Chain>,
    address: Option<String>,
}

#[derive(Default, Serialize)]
struct BalanceQuery {
    chain: Option<Chain>,
    address: Option<String>,
    busy: bool,
    error: Option<String>,
    result: Option<AddressBalance>,
    reward: Option<RewardCheck>,
}

#[derive(Serialize)]
struct RewardCheck {
    block: BlockReward,
    confirmations: Option<i64>,
}

#[derive(Deserialize)]
struct BlockHeaderStatus {
    confirmations: i64,
}

#[derive(Default, Serialize)]
struct Runtime {
    settings: Settings,
    active: Option<Active>,
    progress: Progress,
    node: Node,
    wallet: Wallet,
    balance: BalanceQuery,
}

struct App {
    path: PathBuf,
    runtime: Mutex<Runtime>,
    token: String,
    device: Option<String>,
    gpu_group: Option<usize>,
    gpu_batch: u32,
    gpu_error: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Start {
    route: Route,
    mode: Option<Mode>,
    threads: Option<usize>,
    gpu_batch: Option<u32>,
    address: Option<String>,
    server: Option<String>,
    worker: Option<String>,
    password: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PublicAddress {
    chain: Chain,
    address: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WalletRequest {
    chain: Chain,
}

enum Backend {
    Solo(Chain, Address),
    Pool(pool::PoolConfig),
}

impl App {
    fn default_mode(&self) -> Mode {
        if self.device.is_some() {
            Mode::default()
        } else {
            Mode::Cpu
        }
    }

    fn start(self: &Arc<Self>, request: Start) -> Result<Value> {
        let mode = request.mode.unwrap_or_else(|| self.default_mode());
        let threads = request.threads.unwrap_or_else(default_threads);
        ensure!(
            (1..=max_threads()).contains(&threads),
            "Thread count is out of range"
        );
        let gpu_batch = request.gpu_batch.unwrap_or(self.gpu_batch);
        validate_gpu_batch(gpu_batch)?;
        if mode != Mode::Cpu {
            if let Some(error) = &self.gpu_error {
                bail!("{error}. Select CPU mode.");
            }
        }
        let mut runtime = self.runtime.lock().unwrap();
        ensure!(runtime.active.is_none(), "A session is already running");
        let backend = match request.route {
            Route::Pool => Backend::Pool(pool::PoolConfig::new(
                request
                    .server
                    .as_deref()
                    .context("Stratum server is required")?,
                request.worker.context("Stratum worker is required")?,
                request.password,
            )?),
            Route::Mainnet | Route::Regtest => {
                let chain = if request.route == Route::Regtest {
                    Chain::Regtest
                } else {
                    Chain::Mainnet
                };
                let address = if let Some(value) =
                    request.address.as_deref().filter(|value| !value.is_empty())
                {
                    let address = chain.address(value)?;
                    ensure!(
                        runtime.settings.address(chain) == Some(address.to_string().as_str()),
                        "The destination has changed. Validate the displayed address again."
                    );
                    address
                } else {
                    ensure!(
                        chain == Chain::Regtest,
                        "Validate a mainnet address before starting"
                    );
                    chain.demo_address()
                };
                Backend::Solo(chain, address)
            }
        };

        let active = Active {
            route: request.route,
            mode,
            threads,
            gpu_batch: (mode != Mode::Cpu).then_some(gpu_batch),
            address: match &backend {
                Backend::Solo(_, address) => Some(address.to_string()),
                Backend::Pool(_) => None,
            },
            stopping: false,
            cancel: Arc::new(AtomicBool::new(false)),
        };
        runtime.active = Some(active.clone());
        runtime.progress = Progress {
            message: "Starting the engine".into(),
            ..Progress::default()
        };
        let app = self.clone();
        if let Err(error) = thread::Builder::new()
            .name("session".into())
            .spawn(move || {
                let result = (|| {
                    let engine = Engine::start(active.mode, active.threads, gpu_batch)?;
                    let mut report = |progress: &Progress| {
                        app.runtime.lock().unwrap().progress = progress.clone()
                    };
                    match backend {
                        Backend::Solo(chain, address) => solo::run(
                            &app.path,
                            chain,
                            &address,
                            engine,
                            &active.cancel,
                            &mut report,
                        ),
                        Backend::Pool(config) => {
                            pool::run(config, engine, &active.cancel, &mut report)
                        }
                    }
                })();
                let mut runtime = app.runtime.lock().unwrap();
                runtime.active = None;
                runtime.progress.cpu_rate = 0.0;
                runtime.progress.gpu_rate = 0.0;
                if let Err(error) = result {
                    runtime.progress.message = format!("Error · {error:#}");
                } else if active.cancel.load(Relaxed) {
                    runtime.progress.message = "Mining stopped".into();
                }
            })
        {
            runtime.active = None;
            return Err(error.into());
        }
        Ok(json!({"ok":true}))
    }

    fn wallet(self: &Arc<Self>, chain: Chain) -> Result<Value> {
        let miner = std::env::current_exe()?;
        let assistant = miner
            .with_file_name("Silicon Wallet.app")
            .join("Contents/MacOS/wallet-assistant");
        ensure!(
            assistant.is_file(),
            "Wallet assistant is missing. Run make build again."
        );
        let mut runtime = self.runtime.lock().unwrap();
        ensure!(!runtime.wallet.busy, "The wallet assistant is already open");
        runtime.wallet = Wallet {
            busy: true,
            chain: Some(chain),
            message: Some("Wallet assistant opened".into()),
            ..Wallet::default()
        };
        let app = self.clone();
        if let Err(error) = thread::Builder::new()
            .name("wallet-assistant".into())
            .spawn(move || {
                let result = (|| {
                    let output = Command::new(assistant)
                        .arg(chain.service())
                        .arg(miner)
                        .stdin(Stdio::null())
                        .stderr(Stdio::null())
                        .output()
                        .context("Could not open the wallet assistant")?;
                    ensure!(output.status.success(), "Wallet assistant interrupted");
                    let public = std::str::from_utf8(&output.stdout)?.trim();
                    if public.is_empty() {
                        return Ok(None);
                    }
                    let address = chain.address(public)?.to_string();
                    Ok::<_, anyhow::Error>(Some(address))
                })();
                let mut runtime = app.runtime.lock().unwrap();
                runtime.wallet.busy = false;
                runtime.wallet.message = Some(match result {
                    Ok(Some(address)) => {
                        runtime.wallet.address = Some(address.clone());
                        match runtime.settings.set_address(&app.path, chain, address) {
                            Ok(()) => "Backup restored and address verified".into(),
                            Err(error) => {
                                format!("Backup verified. Validate the address manually ({error}).")
                            }
                        }
                    }
                    Ok(None) => "Wallet assistant closed".into(),
                    Err(error) => format!("{error:#}"),
                });
            })
        {
            runtime.wallet.busy = false;
            return Err(error.into());
        }
        Ok(json!({"ok":true}))
    }

    fn balance(self: &Arc<Self>, chain: Chain) -> Result<Value> {
        let mut runtime = self.runtime.lock().unwrap();
        ensure!(!runtime.balance.busy, "A balance scan is already running");
        let route = match chain {
            Chain::Mainnet => Route::Mainnet,
            Chain::Regtest => Route::Regtest,
        };
        let destination = runtime
            .active
            .as_ref()
            .filter(|active| active.route == route)
            .and_then(|active| active.address.as_deref())
            .or_else(|| runtime.settings.address(chain));
        let address = match destination {
            Some(address) => chain.address(address)?,
            None if chain == Chain::Regtest => chain.demo_address(),
            None => bail!("Validate a mainnet address before checking its balance"),
        };
        runtime.balance = BalanceQuery {
            chain: Some(chain),
            address: Some(address.to_string()),
            busy: true,
            ..BalanceQuery::default()
        };
        let app = self.clone();
        if let Err(error) = thread::Builder::new()
            .name("address-balance".into())
            .spawn(move || {
                let result = (|| {
                    let rpc = Rpc::new(&app.path, chain)?;
                    let balance = rpc.address_balance(chain, &address)?;
                    let previous_reward = solo::saved_reward(&app.path, chain, &address)
                        .unwrap_or_else(|error| {
                            eprintln!("Reward history unavailable · {error:#}");
                            None
                        });
                    let reward = previous_reward.map(|block| {
                        let confirmations = rpc
                            .call::<BlockHeaderStatus>("getblockheader", json!([block.block_hash]))
                            .ok()
                            .map(|header| header.confirmations);
                        RewardCheck {
                            block,
                            confirmations,
                        }
                    });
                    Ok::<_, anyhow::Error>((balance, reward))
                })();
                let mut runtime = app.runtime.lock().unwrap();
                runtime.balance.busy = false;
                match result {
                    Ok((balance, reward)) => {
                        runtime.balance.result = Some(balance);
                        runtime.balance.reward = reward;
                    }
                    Err(error) => runtime.balance.error = Some(format!("{error:#}")),
                }
            })
        {
            runtime.balance.busy = false;
            return Err(error.into());
        }
        Ok(json!({"ok":true}))
    }

    fn state(&self) -> Value {
        json!({
            "runtime": &*self.runtime.lock().unwrap(), "csrf": self.token,
            "system": { "metal":self.device, "metal_error":self.gpu_error, "arm_sha":hash::arm_sha(),
                "default_mode":self.default_mode(),
                "default_threads":default_threads(), "max_threads":max_threads(), "version":env!("CARGO_PKG_VERSION"),
                "gpu_group":self.gpu_group, "default_gpu_batch":self.gpu_batch, "max_gpu_batch":MAX_GPU_BATCH },
            "demo_address":Chain::Regtest.demo_address().to_string()
        })
    }
}

fn one_header<'a>(request: &'a Request, name: &'static str) -> Option<&'a str> {
    let mut headers = request
        .headers()
        .iter()
        .filter(|header| header.field.equiv(name));
    let value = headers.next()?.value.as_str();
    if headers.next().is_some() {
        return None;
    }
    Some(value)
}

fn body<T: DeserializeOwned>(request: &mut Request) -> Result<T> {
    let length = request
        .body_length()
        .context("Request length is required")?;
    ensure!((1..=MAX_BODY).contains(&length), "Invalid request length");
    let mut bytes = vec![];
    request
        .as_reader()
        .take(MAX_BODY as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() == length, "Incomplete request body");
    serde_json::from_slice(&bytes).context("Invalid JSON parameters")
}

fn respond(request: Request, code: u16, kind: &str, bytes: Vec<u8>) {
    let headers = [
        ("Content-Type", kind), ("Cache-Control", "no-store"), ("X-Content-Type-Options", "nosniff"),
        ("Referrer-Policy", "no-referrer"), ("X-Frame-Options", "DENY"),
        ("Content-Security-Policy", "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'"),
    ];
    let mut response = Response::from_data(bytes).with_status_code(StatusCode(code));
    for (name, value) in headers {
        response.add_header(Header::from_bytes(name, value).expect("Static headers must be valid"));
    }
    let _ = request.respond(response);
}

fn json_response(request: Request, code: u16, value: Value) {
    respond(
        request,
        code,
        "application/json; charset=utf-8",
        serde_json::to_vec(&value).unwrap(),
    );
}

fn handle(app: Arc<App>, mut request: Request, control: &DashboardControl, shutdown: &AtomicBool) {
    let allowed = [format!("127.0.0.1:{PORT}"), format!("localhost:{PORT}")];
    let Some(host) =
        one_header(&request, "Host").filter(|host| allowed.iter().any(|ok| ok == host))
    else {
        return json_response(request, 403, json!({"error":"A local host is required"}));
    };
    let path = request.url().to_owned();
    if matches!(path.as_str(), CONTROL_STATUS | CONTROL_STOP) {
        if request.method() != &Method::Post
            || !control.accepts(one_header(&request, CONTROL_HEADER))
            || request
                .headers()
                .iter()
                .any(|header| header.field.equiv("Origin"))
        {
            return json_response(request, 403, json!({"error":"Unauthorized request"}));
        }
        if path == CONTROL_STOP {
            shutdown.store(true, Relaxed);
        }
        return json_response(request, 200, json!({"ok":true}));
    }

    if request.method() == &Method::Get {
        let asset = match path.as_str() {
            "/" => Some((
                "text/html; charset=utf-8",
                include_bytes!("../web/index.html").as_slice(),
            )),
            "/style.css" => Some((
                "text/css; charset=utf-8",
                include_bytes!("../web/style.css").as_slice(),
            )),
            "/app.js" => Some((
                "text/javascript; charset=utf-8",
                include_bytes!("../web/app.js").as_slice(),
            )),
            _ => None,
        };
        if let Some((kind, data)) = asset {
            return respond(request, 200, kind, data.to_vec());
        }
        if path == "/api/state" {
            return json_response(request, 200, app.state());
        }
        return json_response(request, 404, json!({"error":"Resource not found"}));
    }
    if request.method() != &Method::Post {
        return json_response(request, 405, json!({"error":"Method not allowed"}));
    }

    let expected_origin = format!("http://{host}");
    let same_origin = one_header(&request, "Origin") == Some(expected_origin.as_str());
    let token = one_header(&request, "X-Silicon-Token") == Some(app.token.as_str());
    let same_site = one_header(&request, "Sec-Fetch-Site").is_none_or(|site| site == "same-origin");
    let json_body = one_header(&request, "Content-Type").is_some_and(|kind| {
        kind.split(';')
            .next()
            .is_some_and(|value| value.trim() == "application/json")
    });
    if !(same_origin && token && same_site && json_body) {
        return json_response(request, 403, json!({"error":"Unauthorized request"}));
    }

    let result = (|| -> Result<Value> {
        match path.as_str() {
            "/api/start" => app.start(body(&mut request)?),
            "/api/stop" => {
                let _: Value = body(&mut request)?;
                if let Some(active) = &mut app.runtime.lock().unwrap().active {
                    active.cancel.store(true, Relaxed);
                    active.stopping = true;
                }
                Ok(json!({"ok":true}))
            }
            "/api/address" => {
                let input: PublicAddress = body(&mut request)?;
                let address = input.chain.address(&input.address)?.to_string();
                app.runtime.lock().unwrap().settings.set_address(
                    &app.path,
                    input.chain,
                    address.clone(),
                )?;
                Ok(json!({"address":address}))
            }
            "/api/wallet" => {
                let input: WalletRequest = body(&mut request)?;
                app.wallet(input.chain)
            }
            "/api/balance" => {
                let input: WalletRequest = body(&mut request)?;
                app.balance(input.chain)
            }
            _ => bail!("Unknown operation"),
        }
    })();
    match result {
        Ok(data) => json_response(request, 200, data),
        Err(error) => json_response(request, 400, json!({"error":format!("{error:#}")})),
    }
}

fn watch_node(app: Arc<App>) {
    let mut sync_eta = SyncEta::default();
    loop {
        let active_pool = app
            .runtime
            .lock()
            .unwrap()
            .active
            .as_ref()
            .is_some_and(|session| session.route == Route::Pool);
        if active_pool {
            sync_eta = SyncEta::default();
            app.runtime.lock().unwrap().node.sync_eta_seconds = None;
        } else {
            let result =
                Rpc::new(&app.path, Chain::Mainnet).and_then(|rpc| rpc.node(Chain::Mainnet));
            app.runtime.lock().unwrap().node = match result {
                Ok(info) => Node {
                    sync_eta_seconds: sync_eta.observe(Instant::now(), &info),
                    info: Some(info),
                    error: None,
                },
                Err(error) => {
                    sync_eta = SyncEta::default();
                    Node {
                        error: Some(error.to_string()),
                        ..Node::default()
                    }
                }
            };
        }
        thread::sleep(Duration::from_secs(3));
    }
}

pub fn run(path: PathBuf, shutdown: &AtomicBool, control: &DashboardControl) -> Result<()> {
    let settings = match fs::read(path.join("settings.json")) {
        Ok(bytes) => serde_json::from_slice(&bytes).context("Invalid settings.json")?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => Settings::default(),
        Err(error) => return Err(error.into()),
    };
    let (device, gpu_group, gpu_error) = match metal::Gpu::new().and_then(|gpu| {
        metal::verify(&gpu)?;
        Ok((gpu.name.clone(), gpu.width))
    }) {
        Ok((name, width)) => (Some(name), Some(width), None),
        Err(error) => (None, None, Some(format!("{error:#}"))),
    };
    let app = Arc::new(App {
        path,
        token: core::random_hex(32)?,
        device,
        gpu_group,
        gpu_batch: configured_gpu_batch()?,
        gpu_error,
        runtime: Mutex::new(Runtime {
            settings,
            ..Runtime::default()
        }),
    });
    let server = Server::http(("127.0.0.1", PORT))
        .map_err(|error| anyhow!("Could not listen on {URL}: {error}"))?;
    let monitor = app.clone();
    thread::Builder::new()
        .name("core-monitor".into())
        .spawn(move || watch_node(monitor))?;
    control.publish()?;
    println!("Dashboard · {URL}");
    let result = loop {
        if shutdown.load(Relaxed) {
            break Ok(());
        }
        match server.recv_timeout(Duration::from_millis(100)) {
            Ok(Some(request)) => handle(app.clone(), request, control, shutdown),
            Ok(None) => (),
            Err(error) => break Err(error.into()),
        }
    };
    drop(server);
    loop {
        {
            let mut runtime = app.runtime.lock().unwrap();
            let Some(active) = &mut runtime.active else {
                break;
            };
            active.cancel.store(true, Relaxed);
            active.stopping = true;
        }
        thread::sleep(Duration::from_millis(100));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn syncing(blocks: u64, verificationprogress: f64) -> NodeInfo {
        NodeInfo {
            chain: "main".into(),
            blocks,
            headers: 900_000,
            verificationprogress,
            initialblockdownload: true,
            pruned: true,
            bestblockhash: String::new(),
        }
    }

    #[test]
    fn sync_eta_uses_recent_progress_after_warmup() {
        let mut eta = SyncEta::default();
        let start = Instant::now();
        let at = |seconds| start + Duration::from_secs(seconds);
        assert_eq!(eta.observe(at(0), &syncing(100, 0.25)), None);
        assert_eq!(eta.observe(at(15), &syncing(105, 0.3125)), None);
        assert_eq!(eta.observe(at(30), &syncing(110, 0.375)), Some(150));
        eta.observe(at(60), &syncing(120, 0.5));
        eta.observe(at(90), &syncing(125, 0.5625));
        eta.observe(at(120), &syncing(129, 0.6));
        assert_eq!(eta.observe(at(135), &syncing(130, 0.625)), Some(144));
    }

    #[test]
    fn sync_eta_clears_when_stalled_rolled_back_or_finished() {
        let mut eta = SyncEta::default();
        let start = Instant::now();
        let at = |seconds| start + Duration::from_secs(seconds);
        eta.observe(at(0), &syncing(100, 0.25));
        assert_eq!(eta.observe(at(30), &syncing(110, 0.5)), Some(60));
        assert_eq!(eta.observe(at(75), &syncing(110, 0.5)), None);
        assert_eq!(eta.observe(at(78), &syncing(111, 0.625)), None);
        assert_eq!(eta.observe(at(108), &syncing(120, 0.75)), Some(60));
        assert_eq!(eta.observe(at(111), &syncing(119, 0.6875)), None);
        let mut finished = syncing(125, 0.99);
        finished.initialblockdownload = false;
        assert_eq!(eta.observe(at(144), &finished), None);
        assert_eq!(eta.observe(at(147), &syncing(127, 0.99)), None);
    }
}
