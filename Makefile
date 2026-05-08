IFACE     ?= can0
IFACE_TX  ?= can0
IFACE_RX  ?= can1
RATE      ?= 1000
BATCH     ?= 16
WORKERS   ?= 4
EXTRA     ?=

PREFIX    ?= $(HOME)/.local
BINDIR    ?= $(PREFIX)/bin

LIB       := mcanbus
BIN_NAME  := socketcan-mcp
BIN       := target/release/$(BIN_NAME)
BIN_DBG   := target/debug/$(BIN_NAME)

CAPS      := cap_net_admin,cap_net_raw=eip

# Workspace-wide version (both crates are kept in lockstep at workspace.package.version).
VERSION   := $(shell grep '^version' Cargo.toml | head -1 | sed 's/.*"\(.*\)"/\1/')

LIB_SRC   := $(shell find mcanbus/src -name '*.rs') mcanbus/Cargo.toml
BIN_SRC   := $(shell find socketcan-mcp/src -name '*.rs') socketcan-mcp/Cargo.toml
SRC       := $(LIB_SRC) $(BIN_SRC) Cargo.toml Cargo.lock

.PHONY: all build build-debug examples run-candump run-cangen run-multireader run-list \
        mcp-smoke test test-lib test-live test-vcan vcan vcanfd \
        fmt check clippy preflight \
        install install-dev-symlink uninstall clean \
        release publish publish-lib publish-bin help

all: help

# ── Build ─────────────────────────────────────────────────────────────────

build: $(BIN)

$(BIN): $(SRC)
	cargo build --release
	@echo "Applying capabilities ($(CAPS)) — may prompt for sudo password"
	sudo setcap $(CAPS) $(BIN)

build-debug: $(SRC)
	cargo build

examples:
	cargo build --release --examples

# ── Convenience runners (debug builds, fast iteration) ────────────────────

run-candump: build-debug
	./$(BIN_DBG:%/$(BIN_NAME)=%/examples/candump) $(IFACE) $(EXTRA)

run-cangen: build-debug
	./target/debug/examples/cangen $(IFACE) --rate $(RATE) --batch $(BATCH) $(EXTRA)

run-multireader: build-debug
	./target/debug/examples/multireader $(IFACE) --workers $(WORKERS) $(EXTRA)

run-list: build-debug
	./target/debug/examples/list_interfaces

# Smoke-test the MCP server with a hand-crafted JSON-RPC sequence.
# Prints initialize, tools/list, and a list_interfaces tool call result.
mcp-smoke: build-debug
	@printf '%s\n%s\n%s\n%s\n' \
	  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"smoke","version":"0"}}}' \
	  '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
	  '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' \
	  '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"list_interfaces","arguments":{}}}' \
	  | (cat; sleep 1) \
	  | SOCKETCAN_MCP_INTERFACES=$(IFACE_TX),$(IFACE_RX) RUST_LOG=warn $(BIN_DBG)

# ── Tests ─────────────────────────────────────────────────────────────────

# Default: unit + doctests only (no kernel CAN required).
test: test-lib

test-lib:
	cargo test -p $(LIB) --lib
	cargo test -p $(LIB) --doc

# Live integration tests against real hardware (two adapters on one wire).
# Requires can0 and can1 (or whatever IFACE_TX / IFACE_RX you set) to be UP.
test-live:
	MCANBUS_TEST_IFACE=$(IFACE_TX) MCANBUS_TEST_IFACE_RX=$(IFACE_RX) \
	    cargo test --test integration_vcan -- --test-threads=1

# Same suite but on a software vcan0. Run `make vcan` first.
test-vcan:
	MCANBUS_TEST_IFACE=vcan0 \
	    cargo test --test integration_vcan -- --test-threads=1

# ── vcan setup helpers ────────────────────────────────────────────────────

vcan:
	sudo modprobe vcan
	sudo ip link add dev vcan0 type vcan 2>/dev/null || true
	sudo ip link set up vcan0

vcanfd:
	sudo modprobe vcan
	sudo ip link add dev vcan0 type vcan mtu 72 2>/dev/null || true
	sudo ip link set up vcan0

# ── Lints ─────────────────────────────────────────────────────────────────

fmt:
	cargo fmt

check:
	cargo check --all-targets

clippy:
	cargo clippy --all-targets -- -D warnings

preflight:
	cargo fmt -- --check
	cargo clippy --all-targets -- -D warnings
	cargo test -p $(LIB) --lib
	cargo test -p $(LIB) --doc

# ── Install / uninstall ───────────────────────────────────────────────────

install: $(BIN)
	install -Dm755 $(BIN) $(BINDIR)/$(BIN_NAME)
	@echo "Applying capabilities ($(CAPS)) to $(BINDIR)/$(BIN_NAME)"
	sudo setcap $(CAPS) $(BINDIR)/$(BIN_NAME)

# Symlink ~/.local/bin/socketcan-mcp → debug build for live development.
# Every cargo build then automatically updates what consumers see.
install-dev-symlink: build-debug
	mkdir -p $(BINDIR)
	ln -sfn $(CURDIR)/$(BIN_DBG) $(BINDIR)/$(BIN_NAME)
	@echo "$(BINDIR)/$(BIN_NAME) → $(CURDIR)/$(BIN_DBG)"

uninstall:
	rm -f $(BINDIR)/$(BIN_NAME)

# ── Release / publish ─────────────────────────────────────────────────────

release: preflight build
	@if git diff --quiet && git diff --cached --quiet; then \
	    echo "Working tree clean — tagging v$(VERSION)"; \
	else \
	    echo "error: uncommitted changes — commit first"; exit 1; \
	fi
	@if git tag | grep -q "^v$(VERSION)$$"; then \
	    echo "error: tag v$(VERSION) already exists — bump workspace.package.version in Cargo.toml"; exit 1; \
	fi
	git tag -a v$(VERSION) -m "v$(VERSION)"
	git push --tags
	@echo "Tagged and pushed v$(VERSION)."
	@echo "Run 'make publish' to push both crates to crates.io (lib first, bin second)."

# Publish in the right order: mcanbus first, then socketcan-mcp (which depends on it).
publish: publish-lib publish-bin

publish-lib:
	@if curl -sfA "mcanbus-makefile" "https://crates.io/api/v1/crates/$(LIB)/$(VERSION)" -o /dev/null 2>/dev/null; then \
	    echo "error: $(LIB) v$(VERSION) already on crates.io — bump version"; exit 1; \
	fi
	cargo publish -p $(LIB) --dry-run
	@echo ""
	@echo "Dry run for $(LIB) v$(VERSION) passed. Publishing in 5s... (Ctrl-C to abort)"
	@sleep 5
	cargo publish -p $(LIB)
	@echo "Waiting 30s for the crates.io index to propagate before publishing $(BIN_NAME)…"
	@sleep 30

publish-bin:
	@if curl -sfA "mcanbus-makefile" "https://crates.io/api/v1/crates/$(BIN_NAME)/$(VERSION)" -o /dev/null 2>/dev/null; then \
	    echo "error: $(BIN_NAME) v$(VERSION) already on crates.io — bump version"; exit 1; \
	fi
	cargo publish -p $(BIN_NAME) --dry-run
	@echo ""
	@echo "Dry run for $(BIN_NAME) v$(VERSION) passed. Publishing in 5s... (Ctrl-C to abort)"
	@sleep 5
	cargo publish -p $(BIN_NAME)

clean:
	cargo clean

# ── Help ──────────────────────────────────────────────────────────────────

help:
	@echo "mcanbus workspace — version $(VERSION)"
	@echo ""
	@echo "Build:"
	@echo "  build              Release build (both crates), then setcap $(CAPS) on $(BIN_NAME)"
	@echo "  build-debug        Debug build (no setcap)"
	@echo "  examples           Build all examples in release mode"
	@echo ""
	@echo "Run examples (debug):"
	@echo "  run-candump        candump on IFACE (= $(IFACE))"
	@echo "  run-cangen         cangen on IFACE at RATE fps with batch BATCH"
	@echo "  run-multireader    fan-out demo with WORKERS subscribers"
	@echo "  run-list           list every CAN interface"
	@echo "  mcp-smoke          send a JSON-RPC sequence to socketcan-mcp and print results"
	@echo ""
	@echo "Tests:"
	@echo "  test               Library unit + doctests (default)"
	@echo "  test-live          Integration tests on IFACE_TX=$(IFACE_TX) → IFACE_RX=$(IFACE_RX)"
	@echo "  test-vcan          Integration tests on vcan0 (requires 'make vcan' first)"
	@echo ""
	@echo "Setup:"
	@echo "  vcan / vcanfd      Create vcan0 (CAN / CAN-FD MTU)"
	@echo ""
	@echo "Lints:"
	@echo "  fmt check clippy   Standard cargo wrappers"
	@echo "  preflight          fmt --check + clippy --all-targets -D warnings + lib tests"
	@echo ""
	@echo "Install:"
	@echo "  install              Install $(BIN_NAME) to BINDIR=$(BINDIR), apply CAP_NET_*"
	@echo "  install-dev-symlink  Symlink BINDIR/$(BIN_NAME) → debug build for live dev"
	@echo "  uninstall            Remove $(BINDIR)/$(BIN_NAME)"
	@echo ""
	@echo "Release:"
	@echo "  release            preflight + build + tag v$(VERSION) + push tag"
	@echo "  publish            Publish both crates to crates.io (lib first, bin second)"
	@echo "  publish-lib        Just publish $(LIB)"
	@echo "  publish-bin        Just publish $(BIN_NAME) (lib must already be on crates.io)"
	@echo ""
	@echo "Variables (override on the command line):"
	@echo "  IFACE=$(IFACE)  IFACE_TX=$(IFACE_TX)  IFACE_RX=$(IFACE_RX)"
	@echo "  RATE=$(RATE)  BATCH=$(BATCH)  WORKERS=$(WORKERS)"
	@echo "  PREFIX=$(PREFIX)"
	@echo ""
	@echo "Examples:"
	@echo "  make run-cangen IFACE=can0 RATE=5000 BATCH=32"
	@echo "  make run-multireader IFACE=can1 WORKERS=8"
	@echo "  make test-live IFACE_TX=can0 IFACE_RX=can1"
	@echo "  make install PREFIX=/usr/local"
