BIN         := ipv6rd
GIT_VERSION ?= $(shell git describe --tags --always --dirty 2>/dev/null || echo dev)

CARGO      ?= cargo
CARGOFLAGS ?=

# Target triple to build for; empty builds for the host. Supported: Linux and
# macOS (e.g. x86_64-unknown-linux-gnu, aarch64-apple-darwin). Cross-compiling
# needs that target's Rust standard library and a linker for it.
TARGET ?=
ifeq ($(TARGET),)
  RELEASE_DIR := target/release
  TRIPLE      := $(shell rustc -vV 2>/dev/null | sed -n 's/^host: //p')
else
  RELEASE_DIR := target/$(TARGET)/release
  TRIPLE      := $(TARGET)
  CARGOFLAGS  += --target $(TARGET)
endif

# Packaging
DIST_DIR ?= dist
PKG_NAME := $(BIN)-$(GIT_VERSION)-$(TRIPLE)

# The fwknop-rs commit fkospa is pinned to (single source of truth: Cargo.toml).
# `make interop` builds fwknopd from the same commit.
FWKNOP_RS_URL := https://github.com/mlbright/fwknop-rs
FWKNOP_RS_REV := $(shell sed -n 's/^fkospa = .*rev = "\([0-9a-f]*\)".*/\1/p' Cargo.toml)
INTEROP_DIR   := target/interop

# Install locations (override on the command line, e.g. make install-systemd PREFIX=/usr)
PREFIX       ?= /usr/local
INSTALL      ?= install
EXAMPLE      ?= ipv6rd.toml.example

# Linux / systemd
UNITDIR      ?= /etc/systemd/system
ENVFILE      ?= /etc/ipv6rd/ipv6rd.env
CONFIGDIR    ?= /etc/ipv6rd
CONFIG       ?= $(CONFIGDIR)/ipv6rd.toml
SERVICE_USER ?= ubuntu

# macOS / launchd
MACOS_PREFIX    ?= $(HOME)/.local
LAUNCHD_LABEL   ?= com.ipv6rd.ipv6rd
LAUNCHD_DIR     ?= $(HOME)/Library/LaunchAgents
LAUNCHD_PLIST   ?= $(LAUNCHD_DIR)/$(LAUNCHD_LABEL).plist
MACOS_CONFIGDIR ?= $(HOME)/.config/ipv6rd
MACOS_CONFIG    ?= $(MACOS_CONFIGDIR)/ipv6rd.toml
MACOS_ENVFILE   ?= $(MACOS_CONFIGDIR)/ipv6rd.env

# Binary that install-systemd / install-launchd install. Those targets do NOT
# build it, so the service can be installed on a host without the Rust
# toolchain (e.g. unpacked from a release tarball). Run `make build` to produce
# it, or point this at a prebuilt one.
PREBUILT     ?= $(BIN)

.PHONY: all build run install install-systemd uninstall-systemd \
	install-launchd uninstall-launchd package \
	test retest interop vet audit fmt tidy clean help

all: build  ## Build the ipv6rd binary

build:  ## Build the release binary (./ipv6rd)
	$(CARGO) build --release --locked $(CARGOFLAGS)
	cp $(RELEASE_DIR)/$(BIN) $(BIN)

run: build  ## Build and run locally (pass args via ARGS, e.g. make run ARGS='--config ./ipv6rd.toml')
	./$(BIN) $(ARGS)

install:  ## Install the binary to ~/.cargo/bin (cargo install)
	$(CARGO) install --locked --path .

install-systemd:  ## Install a prebuilt binary, hardened systemd unit, config, and env-file scaffold (needs root, not Rust)
	@test -e $(PREBUILT) || { echo "error: '$(PREBUILT)' not found; run 'make build' (needs Rust) or set PREBUILT=/path/to/ipv6rd" >&2; exit 1; }
	$(INSTALL) -d $(DESTDIR)$(PREFIX)/bin
	$(INSTALL) -m 0755 $(PREBUILT) $(DESTDIR)$(PREFIX)/bin/$(BIN)
	$(INSTALL) -d $(DESTDIR)$(UNITDIR)
	sed -e 's,^ExecStart=[^ ]*,ExecStart=$(PREFIX)/bin/$(BIN),' \
	    -e 's,--config [^ ]*,--config $(CONFIG),' \
	    -e 's,^User=.*,User=$(SERVICE_USER),' \
	    -e 's,^EnvironmentFile=.*,EnvironmentFile=$(ENVFILE),' \
	    ipv6rd.service > $(DESTDIR)$(UNITDIR)/$(BIN).service
	$(INSTALL) -d $(DESTDIR)$(CONFIGDIR)
	@if [ ! -e $(DESTDIR)$(CONFIG) ]; then \
		test -e $(EXAMPLE) || { echo "error: '$(EXAMPLE)' not found; cannot scaffold $(CONFIG)" >&2; exit 1; }; \
		$(INSTALL) -m 0644 $(EXAMPLE) $(DESTDIR)$(CONFIG); \
		echo "Created $(CONFIG) (0644) from $(EXAMPLE) - edit it before starting."; \
	else \
		echo "$(CONFIG) already exists; left untouched."; \
	fi
	@if [ ! -e $(DESTDIR)$(ENVFILE) ]; then \
		$(INSTALL) -m 0600 /dev/null $(DESTDIR)$(ENVFILE); \
		printf 'OP_SERVICE_ACCOUNT_TOKEN=\n' > $(DESTDIR)$(ENVFILE); \
		echo "Created $(ENVFILE) (0600) - add your OP_SERVICE_ACCOUNT_TOKEN."; \
	else \
		echo "$(ENVFILE) already exists; left untouched."; \
	fi
	@command -v op >/dev/null 2>&1 || echo "warning: the 1Password CLI 'op' is not on PATH; install it or set op_path in $(CONFIG)."
	@echo
	@echo "Installed $(BIN) and the systemd unit."
	@echo "Next steps:"
	@echo "  1. edit $(CONFIG)         # SPA target, port, access list, op:// key refs"
	@echo "  2. edit $(ENVFILE)        # set OP_SERVICE_ACCOUNT_TOKEN"
	@echo "  3. systemctl daemon-reload"
	@echo "  4. systemctl enable --now $(BIN)"

uninstall-systemd:  ## Stop and remove the systemd unit and installed binary (needs root)
	-systemctl disable --now $(BIN) 2>/dev/null || true
	rm -f $(DESTDIR)$(UNITDIR)/$(BIN).service
	rm -f $(DESTDIR)$(PREFIX)/bin/$(BIN)
	-systemctl daemon-reload 2>/dev/null || true
	@echo "Removed the $(BIN) unit and binary. $(CONFIG) and $(ENVFILE) (holds a secret) were left in place."

install-launchd:  ## Install a prebuilt binary, launchd user agent, config, and env-file scaffold (macOS, no root)
	@test -e $(PREBUILT) || { echo "error: '$(PREBUILT)' not found; run 'make build' (needs Rust) or set PREBUILT=/path/to/ipv6rd" >&2; exit 1; }
	$(INSTALL) -d $(DESTDIR)$(MACOS_PREFIX)/bin
	$(INSTALL) -m 0755 $(PREBUILT) $(DESTDIR)$(MACOS_PREFIX)/bin/$(BIN)
	@# The wrapper: private umask, Homebrew's bin dirs on PATH (launchd's default
	@# PATH has neither, and `op` usually lives there), and the env file's
	@# variables exported (set -a) so the token reaches ipv6rd.
	@{ \
		echo '#!/bin/sh'; \
		echo 'umask 077'; \
		echo 'PATH="/opt/homebrew/bin:/usr/local/bin:$$PATH"; export PATH'; \
		echo 'set -a'; \
		printf '. "%s"\n' "$(MACOS_ENVFILE)"; \
		echo 'set +a'; \
		printf 'exec "%s/bin/$(BIN)" --config "%s" "$$@"\n' "$(MACOS_PREFIX)" "$(MACOS_CONFIG)"; \
	} > $(DESTDIR)$(MACOS_PREFIX)/bin/$(BIN)-wrapper
	@chmod 0755 $(DESTDIR)$(MACOS_PREFIX)/bin/$(BIN)-wrapper
	$(INSTALL) -d -m 0700 $(MACOS_CONFIGDIR)
	@if [ ! -e $(MACOS_CONFIG) ]; then \
		test -e $(EXAMPLE) || { echo "error: '$(EXAMPLE)' not found; cannot scaffold $(MACOS_CONFIG)" >&2; exit 1; }; \
		$(INSTALL) -m 0644 $(EXAMPLE) $(MACOS_CONFIG); \
		echo "Created $(MACOS_CONFIG) (0644) from $(EXAMPLE) - edit it before starting."; \
	else \
		echo "$(MACOS_CONFIG) already exists; left untouched."; \
	fi
	@if [ ! -e $(MACOS_ENVFILE) ]; then \
		$(INSTALL) -m 0600 /dev/null $(MACOS_ENVFILE); \
		printf 'OP_SERVICE_ACCOUNT_TOKEN=\n' > $(MACOS_ENVFILE); \
		echo "Created $(MACOS_ENVFILE) (0600) - add your OP_SERVICE_ACCOUNT_TOKEN."; \
	else \
		echo "$(MACOS_ENVFILE) already exists; left untouched."; \
	fi
	@mkdir -p $(LAUNCHD_DIR)
	@mkdir -p $(HOME)/Library/Logs
	@{ \
		echo '<?xml version="1.0" encoding="UTF-8"?>'; \
		echo '<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">'; \
		echo '<plist version="1.0">'; \
		echo '<dict>'; \
		echo '	<key>Label</key>'; \
		echo '	<string>$(LAUNCHD_LABEL)</string>'; \
		echo '	<key>ProgramArguments</key>'; \
		echo '	<array>'; \
		printf '		<string>%s/bin/$(BIN)-wrapper</string>\n' "$(MACOS_PREFIX)"; \
		echo '	</array>'; \
		echo '	<key>RunAtLoad</key>'; \
		echo '	<true/>'; \
		echo '	<key>KeepAlive</key>'; \
		echo '	<true/>'; \
		echo '	<!-- At most one restart per 5 minutes: each start spends 1Password'; \
		echo '	     service-account requests (ADR-0007). -->'; \
		echo '	<key>ThrottleInterval</key>'; \
		echo '	<integer>300</integer>'; \
		echo '	<key>StandardOutPath</key>'; \
		printf '	<string>%s/Library/Logs/$(BIN).log</string>\n' "$(HOME)"; \
		echo '	<key>StandardErrorPath</key>'; \
		printf '	<string>%s/Library/Logs/$(BIN).log</string>\n' "$(HOME)"; \
		echo '</dict>'; \
		echo '</plist>'; \
	} > $(LAUNCHD_PLIST)
	@command -v op >/dev/null 2>&1 || [ -x /opt/homebrew/bin/op ] || [ -x /usr/local/bin/op ] || echo "warning: the 1Password CLI 'op' was not found; install it or set op_path in $(MACOS_CONFIG)."
	@echo
	@echo "Installed $(BIN) as a launchd user agent (binary: $(MACOS_PREFIX)/bin/$(BIN))."
	@echo "Next steps:"
	@echo "  1. edit $(MACOS_CONFIG)        # SPA target, port, access list, op:// key refs"
	@echo "  2. edit $(MACOS_ENVFILE)       # set OP_SERVICE_ACCOUNT_TOKEN"
	@echo "  3. launchctl bootstrap gui/$$(id -u) $(LAUNCHD_PLIST)"

uninstall-launchd:  ## Stop and remove the launchd user agent and installed binary (macOS)
	-launchctl bootout gui/$$(id -u) $(LAUNCHD_PLIST) 2>/dev/null || true
	rm -f $(LAUNCHD_PLIST)
	rm -f $(DESTDIR)$(MACOS_PREFIX)/bin/$(BIN)-wrapper
	rm -f $(DESTDIR)$(MACOS_PREFIX)/bin/$(BIN)
	@echo "Removed the $(BIN) agent and binary. $(MACOS_CONFIG) and $(MACOS_ENVFILE) (holds a secret) were left in place."

package: build  ## Build a distributable tarball under dist/
	rm -rf $(DIST_DIR)/$(PKG_NAME)
	$(INSTALL) -d $(DIST_DIR)/$(PKG_NAME)
	$(INSTALL) -m 0755 $(BIN)            $(DIST_DIR)/$(PKG_NAME)/$(BIN)
	$(INSTALL) -m 0644 ipv6rd.service    $(DIST_DIR)/$(PKG_NAME)/ipv6rd.service
	$(INSTALL) -m 0644 README.md         $(DIST_DIR)/$(PKG_NAME)/README.md
	$(INSTALL) -m 0644 CONTEXT.md        $(DIST_DIR)/$(PKG_NAME)/CONTEXT.md
	$(INSTALL) -m 0644 LICENSE           $(DIST_DIR)/$(PKG_NAME)/LICENSE
	$(INSTALL) -m 0644 $(EXAMPLE)        $(DIST_DIR)/$(PKG_NAME)/$(EXAMPLE)
	$(INSTALL) -m 0644 Makefile          $(DIST_DIR)/$(PKG_NAME)/Makefile
	cp -r docs                           $(DIST_DIR)/$(PKG_NAME)/docs
	tar -C $(DIST_DIR) -czf $(DIST_DIR)/$(PKG_NAME).tar.gz $(PKG_NAME)
	rm -rf $(DIST_DIR)/$(PKG_NAME)
	@echo "Packaged $(DIST_DIR)/$(PKG_NAME).tar.gz"

test:  ## Run all tests (the end-to-end tests run via `make interop`)
	$(CARGO) test --locked

retest:  ## Rebuild ipv6rd and run all tests again
	$(CARGO) clean -p $(BIN)
	$(CARGO) test --locked

interop:  ## End-to-end tests against fwknop-rs's fwknopd from the pinned commit (Linux)
	$(CARGO) install --locked --quiet --git $(FWKNOP_RS_URL) --rev $(FWKNOP_RS_REV) \
		--root $(INTEROP_DIR) fwknopd
	FWKNOPD=$(abspath $(INTEROP_DIR))/bin/fwknopd $(CARGO) test --locked --test interop -- --ignored

vet:  ## Run clippy with warnings as errors
	$(CARGO) clippy --locked --all-targets -- -D warnings

audit:  ## Check dependencies for advisories, licenses and sources (cargo deny)
	$(CARGO) deny --locked check

fmt:  ## Format all Rust code
	$(CARGO) fmt

tidy:  ## Refresh Cargo.lock entries for this crate only (no dependency upgrades)
	$(CARGO) update --workspace

clean:  ## Remove build artifacts (target/, the ./ipv6rd binary and dist/)
	$(CARGO) clean
	rm -rf $(BIN) $(DIST_DIR)

help:  ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | \
		awk 'BEGIN {FS = ":.*?## "}; {printf "  %-18s %s\n", $$1, $$2}'
