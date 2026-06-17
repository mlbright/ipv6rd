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
ENVFILE      ?= /etc/ipv6rd.env
SERVICE_USER ?= mbright
INSTALL      ?= install

.PHONY: all build run install install-systemd uninstall-systemd package \
	test retest vet fmt tidy clean help

all: build  ## Build the ipv6rd binary

build:  ## Build the ipv6rd binary (./ipv6rd)
	GOOS=$(GOOS) GOARCH=$(GOARCH) $(GO) build $(GOFLAGS) -ldflags "$(LDFLAGS)" -o $(BIN) .

run: build  ## Build and run locally (pass flags via ARGS, e.g. make run ARGS='-- ./poke-hole.sh')
	./$(BIN) $(ARGS)

install:  ## Install the binary to $GOPATH/bin (go install)
	$(GO) install $(GOFLAGS) -ldflags "$(LDFLAGS)" .

install-systemd: build  ## Install the binary, systemd unit, and an env-file scaffold (needs root)
	$(INSTALL) -d $(DESTDIR)$(PREFIX)/bin
	$(INSTALL) -m 0755 $(BIN) $(DESTDIR)$(PREFIX)/bin/$(BIN)
	$(INSTALL) -d $(DESTDIR)$(UNITDIR)
	sed -e 's,^ExecStart=[^ ]*,ExecStart=$(PREFIX)/bin/$(BIN),' \
	    -e 's,^User=.*,User=$(SERVICE_USER),' \
	    ipv6rd.service > $(DESTDIR)$(UNITDIR)/$(BIN).service
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
	@echo "  1. edit $(ENVFILE)        # set OP_SERVICE_ACCOUNT_TOKEN (fwknop mode)"
	@echo "  2. systemctl daemon-reload"
	@echo "  3. systemctl enable --now $(BIN)"

uninstall-systemd:  ## Stop and remove the systemd unit and installed binary (needs root)
	-systemctl disable --now $(BIN) 2>/dev/null || true
	rm -f $(DESTDIR)$(UNITDIR)/$(BIN).service
	rm -f $(DESTDIR)$(PREFIX)/bin/$(BIN)
	-systemctl daemon-reload 2>/dev/null || true
	@echo "Removed the $(BIN) unit and binary. $(ENVFILE) (holds a secret) was left in place."

package: build  ## Build a distributable tarball under dist/
	rm -rf $(DIST_DIR)/$(PKG_NAME)
	$(INSTALL) -d $(DIST_DIR)/$(PKG_NAME)
	$(INSTALL) -m 0755 $(BIN)            $(DIST_DIR)/$(PKG_NAME)/$(BIN)
	$(INSTALL) -m 0644 ipv6rd.service    $(DIST_DIR)/$(PKG_NAME)/ipv6rd.service
	$(INSTALL) -m 0644 README.md         $(DIST_DIR)/$(PKG_NAME)/README.md
	$(INSTALL) -m 0644 CONTEXT.md        $(DIST_DIR)/$(PKG_NAME)/CONTEXT.md
	$(INSTALL) -m 0755 poke-hole.sh      $(DIST_DIR)/$(PKG_NAME)/poke-hole.sh
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
