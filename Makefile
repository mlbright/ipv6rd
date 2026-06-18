BIN         := ipv6rd
GIT_VERSION ?= $(shell git describe --tags --always --dirty 2>/dev/null || echo dev)

GO       := go
GOFLAGS  ?=
LDFLAGS  ?= -s -w

# Target platform. Supported: linux, darwin. The 1Password SDK requires cgo, so
# cross-compiling needs a matching C toolchain; defaults track the host.
GOOS    ?= $(shell $(GO) env GOOS)
GOARCH  ?= $(shell $(GO) env GOARCH)

# Packaging
DIST_DIR ?= dist
PKG_NAME := $(BIN)-$(GIT_VERSION)-$(GOOS)-$(GOARCH)

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
LAUNCHD_LABEL   ?= com.ipv6rd.ipv6rd
LAUNCHD_DIR     ?= $(HOME)/Library/LaunchAgents
LAUNCHD_PLIST   ?= $(LAUNCHD_DIR)/$(LAUNCHD_LABEL).plist
MACOS_CONFIGDIR ?= $(HOME)/.config/ipv6rd
MACOS_CONFIG    ?= $(MACOS_CONFIGDIR)/ipv6rd.toml
MACOS_ENVFILE   ?= $(MACOS_CONFIGDIR)/ipv6rd.env

# Binary that install-systemd installs. That target does NOT build it, so the
# service can be installed on a host without the Go toolchain (e.g. unpacked from a
# release tarball). Run `make build` to produce it, or point this at a prebuilt one.
PREBUILT     ?= $(BIN)

.PHONY: all build run install install-systemd uninstall-systemd \
	install-launchd uninstall-launchd package \
	test retest vet fmt tidy clean help

all: build  ## Build the ipv6rd binary

build:  ## Build the ipv6rd binary (./ipv6rd)
	GOOS=$(GOOS) GOARCH=$(GOARCH) $(GO) build $(GOFLAGS) -ldflags "$(LDFLAGS)" -o $(BIN) .

run: build  ## Build and run locally (pass args via ARGS, e.g. make run ARGS='--config ./ipv6rd.toml')
	./$(BIN) $(ARGS)

install:  ## Install the binary to $GOPATH/bin (go install)
	$(GO) install $(GOFLAGS) -ldflags "$(LDFLAGS)" .

install-systemd:  ## Install a prebuilt binary, systemd unit, config, and env-file scaffold (needs root, not Go)
	@test -e $(PREBUILT) || { echo "error: '$(PREBUILT)' not found; run 'make build' (needs Go) or set PREBUILT=/path/to/ipv6rd" >&2; exit 1; }
	$(INSTALL) -d $(DESTDIR)$(PREFIX)/bin
	$(INSTALL) -m 0755 $(PREBUILT) $(DESTDIR)$(PREFIX)/bin/$(BIN)
	$(INSTALL) -d $(DESTDIR)$(UNITDIR)
	sed -e 's,^ExecStart=[^ ]*,ExecStart=$(PREFIX)/bin/$(BIN),' \
	    -e 's,--config [^ ]*,--config $(CONFIG),' \
	    -e 's,^User=.*,User=$(SERVICE_USER),' \
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
	@test -e $(PREBUILT) || { echo "error: '$(PREBUILT)' not found; run 'make build' (needs Go) or set PREBUILT=/path/to/ipv6rd" >&2; exit 1; }
	$(INSTALL) -d $(DESTDIR)$(PREFIX)/bin
	$(INSTALL) -m 0755 $(PREBUILT) $(DESTDIR)$(PREFIX)/bin/$(BIN)
	@printf '#!/bin/sh\n. "%s/.config/ipv6rd/ipv6rd.env"\nexec "%s/bin/ipv6rd" --config "%s/.config/ipv6rd/ipv6rd.toml" "$$@"\n' \
		"$(HOME)" "$(PREFIX)" "$(HOME)" > $(DESTDIR)$(PREFIX)/bin/$(BIN)-wrapper
	@chmod 0755 $(DESTDIR)$(PREFIX)/bin/$(BIN)-wrapper
	$(INSTALL) -d $(MACOS_CONFIGDIR)
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
		printf '		<string>%s/bin/$(BIN)-wrapper</string>\n' "$(PREFIX)"; \
		echo '	</array>'; \
		echo '	<key>RunAtLoad</key>'; \
		echo '	<true/>'; \
		echo '	<key>KeepAlive</key>'; \
		echo '	<true/>'; \
		echo '	<key>StandardOutPath</key>'; \
		printf '	<string>%s/Library/Logs/$(BIN).log</string>\n' "$(HOME)"; \
		echo '	<key>StandardErrorPath</key>'; \
		printf '	<string>%s/Library/Logs/$(BIN).log</string>\n' "$(HOME)"; \
		echo '</dict>'; \
		echo '</plist>'; \
	} > $(LAUNCHD_PLIST)
	@echo
	@echo "Installed $(BIN) as a launchd user agent."
	@echo "Next steps:"
	@echo "  1. edit $(MACOS_CONFIG)        # SPA target, port, access list, op:// key refs"
	@echo "  2. edit $(MACOS_ENVFILE)       # set OP_SERVICE_ACCOUNT_TOKEN"
	@echo "  3. launchctl bootstrap gui/$$(id -u) $(LAUNCHD_PLIST)"

uninstall-launchd:  ## Stop and remove the launchd user agent and installed binary (macOS)
	-launchctl bootout gui/$$(id -u) $(LAUNCHD_PLIST) 2>/dev/null || true
	rm -f $(LAUNCHD_PLIST)
	rm -f $(DESTDIR)$(PREFIX)/bin/$(BIN)-wrapper
	rm -f $(DESTDIR)$(PREFIX)/bin/$(BIN)
	@echo "Removed the $(BIN) agent and binary. $(MACOS_CONFIG) and $(MACOS_ENVFILE) (holds a secret) were left in place."

package: build  ## Build a distributable tarball under dist/
	rm -rf $(DIST_DIR)/$(PKG_NAME)
	$(INSTALL) -d $(DIST_DIR)/$(PKG_NAME)
	$(INSTALL) -m 0755 $(BIN)            $(DIST_DIR)/$(PKG_NAME)/$(BIN)
	$(INSTALL) -m 0644 ipv6rd.service    $(DIST_DIR)/$(PKG_NAME)/ipv6rd.service
	$(INSTALL) -m 0644 README.md         $(DIST_DIR)/$(PKG_NAME)/README.md
	$(INSTALL) -m 0644 CONTEXT.md        $(DIST_DIR)/$(PKG_NAME)/CONTEXT.md
	$(INSTALL) -m 0644 $(EXAMPLE)        $(DIST_DIR)/$(PKG_NAME)/$(EXAMPLE)
	$(INSTALL) -m 0644 Makefile          $(DIST_DIR)/$(PKG_NAME)/Makefile
	cp -r docs                           $(DIST_DIR)/$(PKG_NAME)/docs
	tar -C $(DIST_DIR) -czf $(DIST_DIR)/$(PKG_NAME).tar.gz $(PKG_NAME)
	rm -rf $(DIST_DIR)/$(PKG_NAME)
	@echo "Packaged $(DIST_DIR)/$(PKG_NAME).tar.gz"

test:  ## Run all tests (may use cache)
	$(GO) test ./...

retest:  ## Run all tests (no cache)
	$(GO) test -count=1 ./...

vet:  ## Run go vet
	$(GO) vet ./...

fmt:  ## Run gofmt on all Go files
	gofmt -s -w .

tidy:  ## Tidy module dependencies
	$(GO) mod tidy

clean:  ## Remove build artifacts (the ./ipv6rd binary and dist/)
	rm -rf $(BIN) $(DIST_DIR)

help:  ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | \
		awk 'BEGIN {FS = ":.*?## "}; {printf "  %-18s %s\n", $$1, $$2}'
