BIN         := ipv6rd
GIT_VERSION ?= $(shell git describe --tags --always --dirty 2>/dev/null || echo dev)

GO       := go
GOFLAGS  ?=
LDFLAGS  ?= -s -w

# Target platform. ipv6rd is Linux-only (rtnetlink) and the 1Password SDK requires
# cgo, so cross-compiling needs a matching C toolchain; defaults track the host.
GOOS    ?= $(shell $(GO) env GOOS)
GOARCH  ?= $(shell $(GO) env GOARCH)

# Packaging
DIST_DIR ?= dist
PKG_NAME := $(BIN)-$(GIT_VERSION)-$(GOOS)-$(GOARCH)

# Install locations (override on the command line, e.g. make install-systemd PREFIX=/usr)
PREFIX       ?= /usr/local
UNITDIR      ?= /etc/systemd/system
ENVFILE      ?= /etc/ipv6rd/ipv6rd.env
CONFIGDIR    ?= /etc/ipv6rd
CONFIG       ?= $(CONFIGDIR)/ipv6rd.toml
EXAMPLE      ?= ipv6rd.toml.example
SERVICE_USER ?= ubuntu
INSTALL      ?= install

# Binary that install-systemd installs. That target does NOT build it, so the
# service can be installed on a host without the Go toolchain (e.g. unpacked from a
# release tarball). Run `make build` to produce it, or point this at a prebuilt one.
PREBUILT     ?= $(BIN)

.PHONY: all build run install install-systemd uninstall-systemd package \
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
