use crate::core::{bitcoin_command, daemon_command, write_private, Chain, OFFLINE};
use anyhow::{bail, ensure, Context, Result};
use serde_json::Value;
use std::{
    fs,
    io::{self, Read, Write},
    net::{Ipv4Addr, TcpListener},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Stdio},
    thread,
    time::{Duration, Instant},
};
use zeroize::Zeroizing;

struct OfflineWalletNode {
    child: Child,
    data: tempfile::TempDir,
    chain: Chain,
    port: u16,
}

impl OfflineWalletNode {
    fn start(chain: Chain) -> Result<Self> {
        let data = tempfile::Builder::new()
            .prefix("silicon-wallet-")
            .tempdir()?;
        let port = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?
            .local_addr()?
            .port();
        let child = daemon_command(data.path(), chain, port)?
            .args(OFFLINE)
            .args(["-persistmempool=0", "-nodebuglogfile", "-printtoconsole=0"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("Could not start Bitcoin Core for the wallet")?;
        let mut node = Self {
            child,
            data,
            chain,
            port,
        };
        let deadline = Instant::now() + Duration::from_secs(25);
        while Instant::now() < deadline {
            if let Some(status) = node.child.try_wait()? {
                bail!("The temporary Bitcoin Core node exited with {status}");
            }
            if node.cli(None, &["getblockchaininfo"], None).is_ok() {
                return Ok(node);
            }
            thread::sleep(Duration::from_millis(200));
        }
        bail!("The temporary Bitcoin Core node did not start");
    }

    fn backup_path(&self) -> PathBuf {
        self.data.path().join("verified-wallet.dat")
    }

    fn cli(&self, wallet: Option<&str>, args: &[&str], input: Option<&[u8]>) -> Result<String> {
        let mut command = bitcoin_command("bitcoin-cli", self.data.path(), self.chain, self.port)?;
        command.args(["-rpcconnect=127.0.0.1", "-rpcclienttimeout=10"]);
        if let Some(name) = wallet {
            command.arg(format!("-rpcwallet={name}"));
        }
        if input.is_some() {
            command.arg("-stdin");
        }
        let mut child = command
            .args(args)
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("Could not run bitcoin-cli")?;
        let written = if let Some(bytes) = input {
            child.stdin.take().unwrap().write_all(bytes)
        } else {
            Ok(())
        };
        let output = child.wait_with_output()?;
        written.context("Could not write securely to Bitcoin Core")?;
        ensure!(
            output.status.success(),
            "Bitcoin Core rejected the operation"
        );
        Ok(String::from_utf8(output.stdout)?.trim().to_owned())
    }
}

impl Drop for OfflineWalletNode {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(Some(_))) {
            return;
        }
        if self.cli(None, &["stop"], None).is_ok() {
            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline {
                if matches!(self.child.try_wait(), Ok(Some(_))) {
                    return;
                }
                thread::sleep(Duration::from_millis(50));
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub fn create(chain: Chain, destination: &Path, password: &[u8]) -> Result<String> {
    ensure!(
        (12..=512).contains(&password.len())
            && !password.iter().any(|b| matches!(b, b'\r' | b'\n' | 0)),
        "Use 12 to 512 bytes with no line breaks."
    );
    std::str::from_utf8(password).context("Passphrase must be valid UTF-8")?;
    ensure!(destination.file_name().is_some(), "Invalid backup location");
    let parent = destination
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    ensure!(
        !destination.exists(),
        "A file already exists at the selected location."
    );

    let origin = OfflineWalletNode::start(chain)?;
    let mut arg = Zeroizing::new(b"passphrase=".to_vec());
    arg.extend_from_slice(password);
    arg.push(b'\n');
    origin
        .cli(
            None,
            &[
                "-named",
                "createwallet",
                "wallet_name=receive",
                "load_on_startup=false",
            ],
            Some(&arg),
        )
        .context("Could not create the encrypted wallet")?;
    let address = origin
        .cli(Some("receive"), &["getnewaddress", "", "bech32"], None)
        .context("Could not create the receiving address")?;
    let address = chain.address(&address)?.to_string();
    let backup_path = origin.backup_path();
    origin
        .cli(
            Some("receive"),
            &[
                "backupwallet",
                backup_path.to_str().context("Invalid backup path")?,
            ],
            None,
        )
        .context("Could not back up the wallet")?;
    let backup = fs::read(&backup_path).context("Could not read the backup")?;
    ensure!(
        backup.starts_with(b"SQLite format 3\0"),
        "Unexpected backup format"
    );
    drop(origin);

    let restored = OfflineWalletNode::start(chain)?;
    let backup_path = restored.backup_path();
    write_private(&backup_path, &backup)
        .context("Could not copy the backup to the restore node")?;
    restored
        .cli(
            None,
            &[
                "restorewallet",
                "verify",
                backup_path.to_str().context("Invalid backup path")?,
                "false",
            ],
            None,
        )
        .context("Could not restore the backup")?;
    let info: Value = serde_json::from_str(&restored.cli(
        Some("verify"),
        &["getaddressinfo", &address],
        None,
    )?)?;
    ensure!(
        info["ismine"] == true && info["address"] == address,
        "The receiving address is missing from the restored wallet"
    );
    let info: Value =
        serde_json::from_str(&restored.cli(Some("verify"), &["getwalletinfo"], None)?)?;
    ensure!(
        info["unlocked_until"].as_u64() == Some(0) && info["private_keys_enabled"] == true,
        "The restored wallet is unlocked or has no private keys"
    );
    arg.truncate(0);
    arg.extend_from_slice(b"passphrase=");
    arg.extend_from_slice(password);
    arg.push(b'\n');
    restored
        .cli(
            Some("verify"),
            &["-named", "walletpassphrase", "timeout=2"],
            Some(&arg),
        )
        .context("Could not unlock the restored wallet")?;
    restored
        .cli(Some("verify"), &["walletlock"], None)
        .context("Could not lock the restored wallet")?;
    drop(restored);

    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))?;
    file.write_all(&backup)?;
    file.as_file().sync_all()?;
    file.persist_noclobber(destination)
        .context("Could not save the verified backup")?;
    Ok(address)
}

pub fn command(chain: Chain, destination: &Path) -> Result<()> {
    let mut password = Zeroizing::new(Vec::new());
    io::stdin().take(513).read_to_end(&mut password)?;
    println!("{}", create(chain, destination, &password)?);
    Ok(())
}
