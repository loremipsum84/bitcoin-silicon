.DEFAULT_GOAL := build

CARGO ?= cargo
PYTHON ?= python3
BINARY := target/release/bitcoin-silicon
WALLET := target/release/Silicon Wallet.app

.PHONY: build setup check-platform install-bitcoin-core sync-bench sync-replay

build:
	@set -eu; \
	if ! command -v "$(CARGO)" >/dev/null 2>&1; then \
		export PATH="$${CARGO_HOME:-$$HOME/.cargo}/bin:$$PATH"; \
	fi; \
	if ! command -v "$(CARGO)" >/dev/null 2>&1; then \
		echo 'Cargo was not found. Install Rust with rustup (https://rustup.rs/).' >&2; exit 1; \
	fi; \
	"$(CARGO)" build --release --locked --target-dir target
	mkdir -p target/swift-cache "$(WALLET)/Contents/MacOS"
	cp native/WalletInfo.plist "$(WALLET)/Contents/Info.plist"
	xcrun swiftc -O -module-cache-path target/swift-cache -target arm64-apple-macos13.0 \
		native/WalletAssistant.swift -o "$(WALLET)/Contents/MacOS/wallet-assistant"
	codesign --force --sign - --timestamp=none "$(WALLET)"

setup: check-platform
	$(MAKE) build
	$(MAKE) install-bitcoin-core
	$(BINARY) prepare-state
	@$(BINARY) metal-test || { \
		echo 'The CPU engine is still available. Run bitcoin-silicon diagnose for Metal details.' >&2; \
	}
	@echo 'Setup complete. Run $(BINARY) start.'

check-platform:
	@set -eu; \
	if [ "$$(uname -s)" != Darwin ] || [ "$$(uname -m)" != arm64 ]; then \
		echo 'Run make setup from a native arm64 terminal on macOS (Apple Silicon).' >&2; exit 1; \
	fi; \
	os_version="$$(sw_vers -productVersion)"; \
	if [ "$${os_version%%.*}" -lt 13 ]; then \
		echo 'This build requires macOS 13 or later.' >&2; exit 1; \
	fi; \
	if ! xcode-select -p >/dev/null 2>&1 || ! xcrun -f clang >/dev/null 2>&1 || ! xcrun -f swiftc >/dev/null 2>&1; then \
		echo 'Install the Apple command line tools with xcode-select --install.' >&2; exit 1; \
	fi

install-bitcoin-core:
	@set -eu; umask 077; \
	default_dir="$(CURDIR)/.local/bitcoin-core/bin"; \
	bin_dir="$${SILICON_BITCOIN_BIN_DIR:-$$default_dir}"; \
	if [ "$$bin_dir" != "$$default_dir" ]; then \
		if [ -f "$$bin_dir/bitcoind" ] && [ -x "$$bin_dir/bitcoind" ] && \
		   [ -f "$$bin_dir/bitcoin-cli" ] && [ -x "$$bin_dir/bitcoin-cli" ]; then exit 0; fi; \
		echo 'SILICON_BITCOIN_BIN_DIR must contain executable bitcoind and bitcoin-cli binaries.' >&2; exit 1; \
	fi; \
	os_version="$$(sw_vers -productVersion)"; \
	if [ "$${os_version%%.*}" -ge 14 ]; then \
		version=32.1; checksum=cd3dcdacb4ff861e862b2d7df4786ecef66472e2468dca1673a70dd761e064e7; \
	else \
		version=30.5; checksum=4c69d3519af2dafe94b695a8c128dd7756a8ded73799fae609085f2e4aeb5a9e; \
	fi; \
	if [ -f "$$bin_dir/bitcoind" ] && [ -x "$$bin_dir/bitcoind" ] && \
	   [ -f "$$bin_dir/bitcoin-cli" ] && [ -x "$$bin_dir/bitcoin-cli" ] && \
	   [ -f "$$bin_dir/.version" ] && [ "$$(cat "$$bin_dir/.version")" = "$$version" ]; then exit 0; fi; \
	archive_dir="bitcoin-$$version"; archive="$$archive_dir-arm64-apple-darwin.tar.gz"; \
	mkdir -p "$$bin_dir"; \
	staging="$$(mktemp -d "$(CURDIR)/.local/.bitcoin-core-install.XXXXXX")"; \
	trap 'rm -rf -- "$$staging"' EXIT; \
	cd "$$staging"; \
	curl --fail --show-error --location --proto '=https' --proto-redir '=https' --retry 2 \
		--output "$$archive" "https://bitcoincore.org/bin/bitcoin-core-$$version/$$archive"; \
	printf '%s  %s\n' "$$checksum" "$$archive" | shasum -a 256 -c -; \
	tar -xzf "$$archive" "$$archive_dir/bin/bitcoind" "$$archive_dir/bin/bitcoin-cli"; \
	mv -f "$$archive_dir/bin/bitcoind" "$$bin_dir/bitcoind"; \
	mv -f "$$archive_dir/bin/bitcoin-cli" "$$bin_dir/bitcoin-cli"; \
	printf '%s\n' "$$version" > .version; \
	mv -f .version "$$bin_dir/.version"

sync-bench:
	@$(PYTHON) -B tools/sync_bench.py $(ARGS)

sync-replay:
	@$(PYTHON) -B tools/sync_replay.py $(ARGS)
