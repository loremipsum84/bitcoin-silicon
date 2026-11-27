"use strict";

const $ = (id) => document.getElementById(id);
const painter = $("chart").getContext("2d");
const compactCount = new Intl.NumberFormat("en-US", {
  notation: "compact", compactDisplay: "long", maximumFractionDigits: 1,
});
const series = [];
let snapshot;
let token;
let busy = false;
let confirmedAddress = "";
let editingAddress = false;
let seenWallet = "";
let historyActive = false;
const checkedBalances = new Set();
let balanceRequest = null;
let balanceRequestError = null;

function text(id, value) { $(id).textContent = value; }
function show(id, visible) { $(id).hidden = !visible; }
function count(value) { return value.toLocaleString("en-US"); }
function btc(sats) {
  return (sats / 100_000_000).toLocaleString("en-US", { maximumFractionDigits: 8 }) + " BTC";
}
function balanceAddress() {
  const active = snapshot?.runtime.active;
  return active?.route === "mainnet" ? active.address : confirmedAddress;
}

function balanceNodeReady() {
  const { info } = snapshot.runtime.node;
  return Boolean(info && !info.initialblockdownload);
}

function maybeCheckBalance() {
  const address = balanceAddress();
  if (!address || editingAddress || checkedBalances.has(address) || balanceRequest) return;
  const { balance } = snapshot.runtime;
  if (balance.busy) {
    if (balance.chain === "mainnet" && balance.address === address) checkedBalances.add(address);
    return;
  }
  if (balanceNodeReady()) void checkBalance(address);
}

async function checkBalance(address) {
  if (!address || !token || balanceRequest || snapshot.runtime.balance.busy) return;
  if (balanceNodeReady()) checkedBalances.add(address);
  balanceRequest = address;
  balanceRequestError = null;
  render();
  try {
    await post("balance", { chain: "mainnet" });
    await poll();
  } catch (error) {
    balanceRequestError = { address, message: error.message };
  } finally {
    balanceRequest = null;
    render();
  }
}

function failure(message) {
  text("form-error", message);
  show("form-error", Boolean(message));
}

async function post(path, body) {
  const reply = await fetch("/api/" + path, {
    method: "POST",
    headers: { "Content-Type": "application/json", "X-Silicon-Token": token },
    body: JSON.stringify(body),
  });
  const payload = await reply.json();
  if (!reply.ok) throw new Error(payload.error ?? "The command failed.");
  return payload;
}

function setRate(id, value, running) {
  const factor = value >= 1e9 ? 1e9 : value >= 1e6 ? 1e6 : value >= 1e3 ? 1e3 : 1;
  const unit = { 1: "H/s", 1000: "kH/s", 1000000: "MH/s", 1000000000: "GH/s" }[factor];
  text(id + "-rate", running ? (value / factor).toLocaleString("en-US", { maximumFractionDigits: 2 }) : "—");
  text(id + "-unit", running ? unit : "");
}

function formatEta(seconds) {
  if (seconds < 60) return "< 1 min";
  const minutes = Math.ceil(seconds / 60);
  const hours = Math.floor(minutes / 60);
  return hours ? `${hours} hr ${minutes % 60} min` : `${minutes} min`;
}

function chart() {
  const canvas = $("chart");
  const width = canvas.clientWidth;
  const height = canvas.clientHeight;
  const scale = window.devicePixelRatio || 1;
  canvas.width = width * scale;
  canvas.height = height * scale;
  painter.scale(scale, scale);
  if (!width || !height) return;
  const plot = { left: 6, right: width - 6, top: 10, bottom: height - 21 };
  painter.strokeStyle = "#2c373e";
  painter.lineWidth = 1;
  for (let i = 0; i < 5; i++) {
    const y = plot.top + i * (plot.bottom - plot.top) / 4;
    painter.beginPath();
    painter.moveTo(plot.left, y);
    painter.lineTo(plot.right, y);
    painter.stroke();
  }
  const max = Math.max(1, ...series.flat());
  for (const [index, color] of [[0, "#a6ecb9"], [1, "#93bbf6"]]) {
    painter.beginPath();
    painter.strokeStyle = color;
    painter.lineWidth = 2;
    series.forEach((point, n) => {
      const x = plot.right - (series.length - 1 - n) * (plot.right - plot.left) / 59;
      const y = plot.bottom - point[index] / max * (plot.bottom - plot.top);
      if (n === 0) painter.moveTo(x, y); else painter.lineTo(x, y);
    });
    painter.stroke();
  }
  painter.fillStyle = "#a3b0b8";
  painter.font = "10px monospace";
  painter.fillText("−60 s", plot.left, height - 4);
  const label = "Now";
  painter.fillText(label, plot.right - painter.measureText(label).width, height - 4);
}

function render() {
  if (!snapshot) return;
  const { runtime, system } = snapshot;
  const { active: session, progress, settings, node, wallet, balance } = runtime;
  const active = session?.route === "mainnet" ? session : null;
  const running = Boolean(active);
  const occupied = Boolean(session);
  const relevant = (!occupied || running) && progress.backend === "mainnet";

  text("metal", system.metal ? "Metal active · " + system.metal : "Metal unavailable");
  $("metal").title = system.metal_error ?? "";
  $("metal-dot").classList.toggle("ready", Boolean(system.metal));
  text("sha", system.arm_sha ? "SHA‑256d · ARM instructions" : "SHA‑256d");
  text("version", "v" + system.version);
  text("loopback", location.host);
  text("session-state", session?.stopping ? "Stopping" : running ? "Mining" : occupied ? "Session active via API" : "Idle");
  $("session-badge").classList.toggle("running", occupied);

  $("session-fields").disabled = occupied || busy;
  $("address-settings").disabled = occupied || busy;
  $("threads").disabled = $("mode").value === "gpu";
  show("gpu-batch-field", $("mode").value !== "cpu");
  $("gpu-batch").disabled = $("mode").value === "cpu";
  $("wallet").disabled = wallet.busy;
  show("stop", occupied);
  $("stop").disabled = session?.stopping || busy;
  show("start", !occupied);
  $("start").disabled = occupied || busy || !confirmedAddress
    || (editingAddress && Boolean($("address").value.trim()));

  const destination = balanceAddress();
  show("confirmed", Boolean(destination));
  text("confirmation-label", running ? "Session destination" : "Confirmed destination address");
  text("confirmed-address", destination ?? "");
  show("wallet-status", wallet.chain === "mainnet" && Boolean(wallet.message));
  text("wallet-status", wallet.message ?? "");
  show("balance-panel", Boolean(destination));
  const balanceMatches = balance.chain === "mainnet" && balance.address === destination;
  const addressBalance = balanceMatches ? balance.result : null;
  const scanPending = destination && (balanceRequest === destination || (balanceMatches && balance.busy));
  const scanError = balanceRequestError && balanceRequestError.address === destination ? balanceRequestError.message :
    balanceMatches ? balance.error : null;
  $("check-balance").disabled = busy || Boolean(balanceRequest) || balance.busy || !destination;
  text("balance-status", scanPending ? "Scanning unspent outputs…" :
    scanError || (addressBalance ? `Unspent outputs at block ${count(addressBalance.height)}` :
      destination && !balanceNodeReady() ? "The balance will be checked automatically when the node is ready." : ""));
  show("balance-values", Boolean(addressBalance));
  if (addressBalance) {
    text("balance-total", btc(addressBalance.confirmed_sats));
    text("balance-coinbase", btc(addressBalance.coinbase_sats));
    text("balance-immature", btc(addressBalance.immature_sats));
  }
  const rewardCheck = balanceMatches ? balance.reward : null;
  show("balance-reward", Boolean(rewardCheck));
  if (rewardCheck) {
    const { block, confirmations } = rewardCheck;
    const confirmationStatus = confirmations === null ? "Confirmation count unavailable" :
      confirmations < 1 ? "Outside the active chain" :
        `Confirmations ${count(confirmations)} · ${confirmations <= 100 ? "immature" : "mature"}`;
    text("balance-reward", `Last recorded reward · Block ${count(block.height)} · ${btc(block.sats)} · ${confirmationStatus}`);
  }

  const cpu = relevant ? progress.cpu_rate : 0;
  const gpu = relevant ? progress.gpu_rate : 0;
  setRate("cpu", cpu, running);
  setRate("gpu", gpu, running);
  setRate("total", cpu + gpu, running);

  const engine = active?.mode === "gpu" ? "GPU" :
    active?.mode === "hybrid" ? "Hybrid" : "CPU";
  const profile = [engine];
  if (active?.mode !== "gpu" && active) profile.push(active.threads + " CPU threads");
  if (active?.gpu_batch) {
    profile.push(snapshot.system.metal + " · Threadgroup size " + snapshot.system.gpu_group);
    profile.push("Max batch size " + active.gpu_batch.toLocaleString("en-US"));
    if (progress.gpu_last_batch) {
      profile.push("Last batch size " + progress.gpu_last_batch.toLocaleString("en-US"));
    }
  }
  show("engine-config", running);
  text("engine-config", profile.join(" · "));
  show("block-changes", relevant && Boolean(progress.previous_block));
  text("block-changes", "Block changes: " + progress.block_changes);
  $("block-changes").title = progress.previous_block ?? "";

  const info = node.info;
  const tipChanged = info && info.bestblockhash !== progress.previous_block;
  const nonces = relevant && !tipChanged ? progress.nonces : null;
  show("nonce-panel", Boolean(nonces));
  if (nonces) {
    const { tested, resumed } = nonces;
    const unit = tested === 1 ? "attempt" : "attempts";
    text("nonce-context",
      `Target block #${count(progress.height)}${running ? (resumed ? " · Resumed" : "") : " · Saved"}`);
    text("nonce-count", `${compactCount.format(tested)} ${unit}`);
    $("nonce-count").title = `${count(tested)} ${unit}`;
  }
  const activity = relevant && progress.message ? progress.message :
    (running ? progress.message : "");
  if (activity) {
    text("backend-status", activity);
  } else if (info) {
    text("backend-status", info.initialblockdownload ? "Initial sync in progress" : "Node synced");
  } else {
    text("backend-status", "Node unavailable. Run scripts/node start mainnet in the terminal.");
  }
  show("node-data", Boolean(info));
  if (info) {
    const pct = Math.min(100, Math.max(0, info.verificationprogress * 100));
    text("node-height", "Height " + info.blocks.toLocaleString("en-US") + " / " + info.headers.toLocaleString("en-US") + (info.pruned ? " · Pruned" : ""));
    const eta = info.initialblockdownload && Number.isFinite(node.sync_eta_seconds)
      ? ` (ETA: ${formatEta(node.sync_eta_seconds)})` : "";
    text("node-pct", pct.toFixed(2) + "%" + eta);
    $("sync").value = pct;
  }
  show("unconfirmed-wrap", relevant && progress.unconfirmed > 0);
  for (const key of ["accepted", "rejected", "stale", "unconfirmed"]) text(key, relevant ? progress[key] : "0");
  text("last-submission", relevant && progress.last_submission
    ? progress.last_submission : "No submissions for this session.");
  const reward = relevant ? progress.reward : null;
  show("submitted-reward", Boolean(reward));
  if (reward) {
    text("submitted-reward", `Block reward at height ${count(reward.height)} · ${btc(reward.sats)} · TXID ${reward.txid}`);
  }

  if (!editingAddress && !running && (settings.mainnet ?? "") !== confirmedAddress) {
    confirmedAddress = settings.mainnet ?? "";
    $("address").value = confirmedAddress;
  }
  chart();
}

async function poll() {
  try {
    const response = await fetch("/api/state");
    if (!response.ok) throw new Error("The local dashboard is unreachable.");
    const next = await response.json();
    if (!snapshot) {
      const { system, runtime } = next;
      const active = runtime.active?.route === "mainnet" ? runtime.active : null;
      $("mode").value = active?.mode ?? system.default_mode;
      $("threads").value = active?.threads ?? system.default_threads;
      $("threads").max = system.max_threads;
      $("gpu-batch").max = system.max_gpu_batch;
      $("gpu-batch").value = active?.gpu_batch ?? system.default_gpu_batch;
      $("mode").querySelectorAll("option").forEach((option) => {
        option.disabled = option.value !== "cpu" && !system.metal;
      });
      confirmedAddress = runtime.settings.mainnet ?? "";
      $("address").value = confirmedAddress;
      if (!system.metal) failure("Metal is unavailable. CPU mode is selected.");
    }
    snapshot = next;
    token = next.csrf;
    const { runtime } = next;
    const active = runtime.active?.route === "mainnet";
    if (active && !historyActive) series.length = 0;
    if (active) {
      const { cpu_rate, gpu_rate } = runtime.progress;
      series.push([cpu_rate, gpu_rate]);
      if (series.length > 60) series.shift();
    } else if (historyActive) {
      series.push([0, 0]);
      if (series.length > 60) series.shift();
    }
    historyActive = active;
    const walletAddress = runtime.wallet.address;
    if (runtime.wallet.chain === "mainnet" && walletAddress && walletAddress !== seenWallet) {
      seenWallet = walletAddress;
      confirmedAddress = runtime.settings.mainnet ?? "";
      $("address").value = walletAddress;
      editingAddress = walletAddress !== confirmedAddress;
    }
    render();
    maybeCheckBalance();
  } catch (error) {
    failure(error.message);
  }
}

async function action(run) {
  if (!token || busy) return;
  busy = true;
  failure("");
  render();
  try { await run(); }
  catch (error) { failure(error.message); }
  finally { busy = false; await poll(); }
}

$("mode").addEventListener("change", render);
$("existing").addEventListener("click", () => $("address").focus());
$("address").addEventListener("input", () => {
  editingAddress = true;
  confirmedAddress = "";
  failure("");
  render();
});
$("save-address").addEventListener("click", () => action(async () => {
  const data = await post("address", { chain: "mainnet", address: $("address").value });
  confirmedAddress = data.address;
  editingAddress = false;
  $("address").value = data.address;
}));
$("wallet").addEventListener("click", () => action(() => post("wallet", { chain: "mainnet" })));
$("check-balance").addEventListener("click", () => void checkBalance(balanceAddress()));
$("session-form").addEventListener("submit", (event) => {
  event.preventDefault();
  action(async () => {
    const body = { route: "mainnet", address: confirmedAddress, mode: $("mode").value, threads: Number($("threads").value) };
    if (body.mode !== "cpu") body.gpu_batch = Number($("gpu-batch").value);
    await post("start", body);
  });
});
$("stop").addEventListener("click", () => action(() => post("stop", {})));
new ResizeObserver(chart).observe($("chart"));
poll();
setInterval(() => { if (!busy) poll(); }, 1000);
