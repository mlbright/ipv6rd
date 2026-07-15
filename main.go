// Command ipv6rd watches the host's outbound (egress) IPv6 address and re-knocks an
// fwknop SPA grant whenever that address rotates, keeping the grant pinned to a
// rotating IPv6 temporary/dynamic address.
//
// All settings are read from a TOML configuration file (default path is
// platform-specific; override with --config); the only input from outside the file
// is the 1Password service-account token in OP_SERVICE_ACCOUNT_TOKEN. The SPA packet
// is sent in process via the fwknop-go library, with the encryption and HMAC keys
// resolved once at startup from 1Password. See docs/adr/0002 and docs/adr/0003.
//
// The egress address is determined by the kernel's source-address selection via a UDP
// socket dial to the SPA target (see docs/adr/0001-egress-address-via-route-get.md),
// not by parsing the interface address table.
package main

import (
	"context"
	"encoding/base64"
	"errors"
	"flag"
	"fmt"
	"log/slog"
	"net"
	"os"
	"os/signal"
	"strconv"
	"strings"
	"syscall"
	"time"

	"github.com/BurntSushi/toml"

	onepassword "github.com/1password/onepassword-sdk-go"
	"github.com/damienstuart/fwknop-go/fwknop"
)

// opIntegrationVersion identifies this integration to the 1Password SDK.
const opIntegrationVersion = "1.0.0"

// defaultConfigPath is read when --config is not given. Platform-specific; set by
// platformDefaultConfigPath() in the platform watcher file.
var defaultConfigPath = platformDefaultConfigPath()

type config struct {
	debounce  time.Duration // quiet period after an address event before recomputing
	reconcile time.Duration // reconciliation poll interval (backstop for missed events)
	refresh   time.Duration // grant refresh: re-knock interval for an unchanged address; 0 disables
	timeout   time.Duration // per-attempt knock timeout
	attempts  int           // max knock attempts before giving up
	dest      net.IP        // probe destination (the resolved SPA target) for source-address selection
	fwknop    fwknopConfig  // SPA target and access request
}

// fwknopConfig holds the SPA knock settings. The encryption and HMAC keys are not
// stored here; they are resolved from 1Password at startup.
type fwknopConfig struct {
	dest    string   // SPA target host (where the packet is sent; also the probe destination)
	port    int      // UDP port fwknopd listens on
	access  []string // access request: proto/port entries, e.g. ["tcp/443", "tcp/22"]
	keyRef  string   // 1Password secret reference (op://...) for the base64 encryption key
	hmacRef string   // 1Password secret reference (op://...) for the base64 HMAC key
}

// actionFunc performs one re-knock attempt for addr. Cancelled via ctx (per-attempt
// timeout).
type actionFunc func(ctx context.Context, addr string) error

func main() {
	slog.SetDefault(slog.New(slog.NewTextHandler(os.Stderr, &slog.HandlerOptions{Level: slog.LevelInfo})))

	fs := flag.NewFlagSet("ipv6rd", flag.ContinueOnError)
	configPath := fs.String("config", defaultConfigPath, "path to the TOML configuration file")
	fs.Usage = func() {
		fmt.Fprintf(fs.Output(),
			"usage: ipv6rd [--config PATH]\n\n"+
				"Re-knocks an fwknop SPA grant whenever the egress IPv6 address rotates.\n"+
				"All settings are read from the TOML config file (default %s); the\n"+
				"1Password service-account token is read from OP_SERVICE_ACCOUNT_TOKEN.\n\n"+
				"flags:\n", defaultConfigPath)
		fs.PrintDefaults()
	}
	if err := fs.Parse(os.Args[1:]); err != nil {
		os.Exit(2) // flag already printed the error and usage
	}

	cfg, err := loadConfig(*configPath)
	if err != nil {
		fmt.Fprintln(os.Stderr, "ipv6rd:", err)
		os.Exit(2)
	}

	if err := run(cfg); err != nil {
		slog.Error("exiting", "err", err)
		os.Exit(1)
	}
}

// fileConfig mirrors the TOML file. Durations are Go strings (e.g. "2s"); retries is
// a pointer so an absent key (default) is distinguishable from an explicit 0 (error).
type fileConfig struct {
	Debounce  string      `toml:"debounce"`
	Reconcile string      `toml:"reconcile"`
	Refresh   string      `toml:"refresh"`
	Timeout   string      `toml:"timeout"`
	Retries   *int        `toml:"retries"`
	Fwknop    *fwknopFile `toml:"fwknop"`
}

type fwknopFile struct {
	Dest    string   `toml:"dest"`
	Port    int      `toml:"port"`
	Access  []string `toml:"access"`
	KeyRef  string   `toml:"key_ref"`
	HMACRef string   `toml:"hmac_ref"`
}

// loadConfig reads, strictly decodes, and validates the TOML config at path. A
// missing file or any unknown key is a fatal error.
func loadConfig(path string) (config, error) {
	var fc fileConfig
	md, err := toml.DecodeFile(path, &fc)
	if err != nil {
		return config{}, fmt.Errorf("reading config %s: %w", path, err)
	}
	if undecoded := md.Undecoded(); len(undecoded) > 0 {
		keys := make([]string, len(undecoded))
		for i, k := range undecoded {
			keys[i] = k.String()
		}
		return config{}, fmt.Errorf("unknown key(s) in config: %s", strings.Join(keys, ", "))
	}

	debounce, err := parseDuration("debounce", fc.Debounce, 2*time.Second, true)
	if err != nil {
		return config{}, err
	}
	reconcile, err := parseDuration("reconcile", fc.Reconcile, 5*time.Minute, false)
	if err != nil {
		return config{}, err
	}
	refresh, err := parseDuration("refresh", fc.Refresh, 0, true)
	if err != nil {
		return config{}, err
	}
	timeout, err := parseDuration("timeout", fc.Timeout, 60*time.Second, false)
	if err != nil {
		return config{}, err
	}

	attempts := 3
	if fc.Retries != nil {
		attempts = *fc.Retries
	}
	if attempts < 1 {
		return config{}, errors.New("retries must be >= 1")
	}

	if fc.Fwknop == nil {
		return config{}, errors.New("missing [fwknop] section")
	}
	f := fc.Fwknop
	if f.Dest == "" {
		return config{}, errors.New("fwknop.dest is required (the SPA target host)")
	}
	if f.Port < 1 || f.Port > 65535 {
		return config{}, errors.New("fwknop.port must be in 1..65535")
	}
	if f.KeyRef == "" {
		return config{}, errors.New("fwknop.key_ref is required")
	}
	if f.HMACRef == "" {
		return config{}, errors.New("fwknop.hmac_ref is required")
	}

	// Absent access (nil) defaults to tcp/443; an explicit empty list is an error.
	access := f.Access
	if access == nil {
		access = []string{"tcp/443"}
	} else if len(access) == 0 {
		return config{}, errors.New("fwknop.access is empty; list at least one protocol/port (e.g. \"tcp/443\") or omit it for the default")
	}
	for _, e := range access {
		if err := validateAccessEntry(e); err != nil {
			return config{}, fmt.Errorf("fwknop.access entry %q: %w", e, err)
		}
	}

	ip, err := resolveDest(f.Dest)
	if err != nil {
		return config{}, err
	}

	return config{
		debounce:  debounce,
		reconcile: reconcile,
		refresh:   refresh,
		timeout:   timeout,
		attempts:  attempts,
		dest:      ip,
		fwknop: fwknopConfig{
			dest:    f.Dest,
			port:    f.Port,
			access:  access,
			keyRef:  f.KeyRef,
			hmacRef: f.HMACRef,
		},
	}, nil
}

// parseDuration parses a Go duration string, returning def when blank. If allowZero
// is false the result must be strictly positive.
func parseDuration(name, val string, def time.Duration, allowZero bool) (time.Duration, error) {
	if strings.TrimSpace(val) == "" {
		return def, nil
	}
	d, err := time.ParseDuration(val)
	if err != nil {
		return 0, fmt.Errorf("%s: %w", name, err)
	}
	if d < 0 || (d == 0 && !allowZero) {
		return 0, fmt.Errorf("%s must be positive", name)
	}
	return d, nil
}

// validateAccessEntry checks one access-request entry: "proto/port" where proto is
// tcp or udp and port is a single 1..65535 value or a "lo-hi" range.
func validateAccessEntry(e string) error {
	if strings.ContainsAny(e, ", ") {
		return errors.New(`one protocol/port per entry; do not put several in one string`)
	}
	proto, port, ok := strings.Cut(e, "/")
	if !ok {
		return errors.New(`expected "proto/port", e.g. tcp/443`)
	}
	if proto != "tcp" && proto != "udp" {
		return fmt.Errorf("protocol %q must be tcp or udp", proto)
	}
	if lo, hi, isRange := strings.Cut(port, "-"); isRange {
		l, err := parsePort(lo)
		if err != nil {
			return err
		}
		h, err := parsePort(hi)
		if err != nil {
			return err
		}
		if l > h {
			return fmt.Errorf("port range %s-%s is reversed", lo, hi)
		}
		return nil
	}
	_, err := parsePort(port)
	return err
}

func parsePort(s string) (int, error) {
	n, err := strconv.Atoi(s)
	if err != nil {
		return 0, fmt.Errorf("port %q is not a number", s)
	}
	if n < 1 || n > 65535 {
		return 0, fmt.Errorf("port %d out of range 1..65535", n)
	}
	return n, nil
}

// resolveDest turns an IPv6 literal or a hostname into an IPv6 address.
func resolveDest(s string) (net.IP, error) {
	if ip := net.ParseIP(s); ip != nil {
		if ip.To4() != nil {
			return nil, fmt.Errorf("fwknop.dest %q is IPv4; an IPv6 SPA target is required", s)
		}
		return ip, nil
	}
	ips, err := net.LookupIP(s)
	if err != nil {
		return nil, fmt.Errorf("resolving fwknop.dest %q: %w", s, err)
	}
	for _, ip := range ips {
		if ip.To4() == nil && ip.To16() != nil {
			return ip, nil
		}
	}
	return nil, fmt.Errorf("fwknop.dest %q has no AAAA (IPv6) address", s)
}

// egressAddr returns the source IPv6 address the kernel would use to reach dest by
// dialing a UDP socket (no packet is sent). Returns "" if there is currently no
// usable source (e.g. no IPv6 connectivity). See docs/adr/0001.
func egressAddr(dest net.IP) (string, error) {
	conn, err := net.Dial("udp6", net.JoinHostPort(dest.String(), "1"))
	if err != nil {
		return "", err
	}
	defer conn.Close()
	addr, ok := conn.LocalAddr().(*net.UDPAddr)
	if !ok || addr.IP == nil || addr.IP.IsUnspecified() {
		return "", nil
	}
	return addr.IP.String(), nil
}

type fireResult struct {
	target string
	err    error
}

func run(cfg config) error {
	ctx, stop := signal.NotifyContext(context.Background(), syscall.SIGINT, syscall.SIGTERM)
	defer stop()

	refreshDesc := "(disabled)"
	if cfg.refresh > 0 {
		refreshDesc = cfg.refresh.String()
	}
	slog.Info("starting",
		"spa_target", cfg.fwknop.dest, "dest_ip", cfg.dest.String(),
		"spa_port", cfg.fwknop.port, "access", cfg.fwknop.access,
		"debounce", cfg.debounce.String(), "reconcile", cfg.reconcile.String(),
		"refresh", refreshDesc,
		"timeout", cfg.timeout.String(), "attempts", cfg.attempts)

	// Resolve the keys from 1Password up front (fail fast on misconfiguration) and
	// cache them in the per-rotation action.
	act, err := buildAction(ctx, cfg)
	if err != nil {
		return err
	}

	// Address-event source: a goroutine watches for address changes and forwards a
	// coalesced signal on every event.
	events := make(chan struct{}, 1)
	go watchAddrEvents(ctx, events)

	debounce := time.NewTimer(0)
	if !debounce.Stop() {
		<-debounce.C
	}
	reconcile := time.NewTicker(cfg.reconcile)
	defer reconcile.Stop()

	// Grant refresh: re-knock the unchanged egress address on an interval so a
	// server-side grant that fwknopd expires (FW_ACCESS_TIMEOUT) is renewed. A
	// timer rather than a ticker so the interval measures time since the last
	// successful knock. When disabled the nil channel blocks forever in the select.
	var refresh *time.Timer
	var refreshC <-chan time.Time
	if cfg.refresh > 0 {
		refresh = time.NewTimer(cfg.refresh)
		defer refresh.Stop()
		refreshC = refresh.C
	}

	fireDone := make(chan fireResult, 1)

	var (
		latest    string // most recent observed egress address (the desired target)
		lastFired string // address of the last successful knock
		running   bool   // a knock is currently in flight
	)

	// maybeFire starts a knock if one isn't already running and the latest target
	// differs from what we last successfully fired with. Serialized: at most one run
	// in flight.
	maybeFire := func() {
		if running || latest == "" || latest == lastFired {
			return
		}
		running = true
		target := latest
		slog.Info("firing", "addr", target)
		go func() {
			fireDone <- fireResult{target: target, err: fireWithRetry(ctx, cfg, target, act)}
		}()
	}

	recompute := func(reason string) {
		ip, err := egressAddr(cfg.dest)
		if err != nil {
			slog.Warn("egress lookup failed", "reason", reason, "err", err)
			return
		}
		if ip == "" {
			slog.Warn("no egress source address (no IPv6 connectivity?)", "reason", reason)
			return
		}
		if ip != latest {
			slog.Info("egress address changed", "reason", reason, "addr", ip, "previous", orNone(latest))
		}
		latest = ip
		maybeFire()
	}

	// Fire once on startup with the current address.
	recompute("startup")

	for {
		select {
		case <-ctx.Done():
			slog.Info("shutdown signal received, stopping")
			return nil

		case <-events:
			// Coalesce a burst of address events: (re)arm the debounce timer and
			// only recompute once it goes quiet.
			if !debounce.Stop() {
				select {
				case <-debounce.C:
				default:
				}
			}
			debounce.Reset(cfg.debounce)

		case <-debounce.C:
			recompute("address-event")

		case <-reconcile.C:
			recompute("reconcile")

		case <-refreshC:
			slog.Info("refresh interval elapsed; re-knocking", "last_fired", orNone(lastFired))
			lastFired = "" // fire again even though the address is unchanged
			recompute("refresh")
			refresh.Reset(cfg.refresh)

		case res := <-fireDone:
			running = false
			if res.err != nil {
				slog.Error("fire failed; will retry on next rotation or reconcile", "addr", res.target, "err", res.err)
			} else {
				lastFired = res.target
				slog.Info("fire succeeded", "addr", res.target)
				if refresh != nil {
					// Restart the countdown so refresh measures from this success,
					// not from the previous tick.
					if !refresh.Stop() {
						select {
						case <-refresh.C:
						default:
						}
					}
					refresh.Reset(cfg.refresh)
				}
			}
			// Converge-to-latest: only re-fire immediately if the address moved on
			// to a *newer* value while this run was in flight. If the run failed
			// for the still-current address, we wait for the next event/reconcile
			// rather than busy-looping.
			if latest != res.target && latest != lastFired {
				maybeFire()
			}
		}
	}
}

// fireWithRetry runs action for addr, retrying with bounded exponential backoff up
// to cfg.attempts times and applying cfg.timeout to each attempt.
func fireWithRetry(ctx context.Context, cfg config, addr string, action actionFunc) error {
	var lastErr error
	for attempt := 1; attempt <= cfg.attempts; attempt++ {
		if attempt > 1 {
			backoff := backoffDelay(attempt - 1)
			slog.Warn("retrying", "addr", addr, "attempt", attempt, "of", cfg.attempts, "after", backoff.String())
			if !sleepCtx(ctx, backoff) {
				return ctx.Err()
			}
		}

		runCtx, cancel := context.WithTimeout(ctx, cfg.timeout)
		err := action(runCtx, addr)
		cancel()

		if err == nil {
			return nil
		}
		lastErr = err
		slog.Warn("attempt failed", "addr", addr, "attempt", attempt, "err", err)
		if ctx.Err() != nil { // shutting down: stop retrying
			return ctx.Err()
		}
	}
	return fmt.Errorf("all %d attempts failed: %w", cfg.attempts, lastErr)
}

// buildAction resolves the encryption and HMAC keys from 1Password once, up front,
// so re-knocks never touch the network beyond the SPA packet itself.
func buildAction(ctx context.Context, cfg config) (actionFunc, error) {
	encKey, hmacKey, err := resolveFwknopKeys(ctx, &cfg.fwknop)
	if err != nil {
		return nil, err
	}
	slog.Info("resolved fwknop keys from 1Password", "key_ref", cfg.fwknop.keyRef, "hmac_ref", cfg.fwknop.hmacRef)
	return fwknopAction(cfg.fwknop, encKey, hmacKey), nil
}

// fwknopAction sends one fwknop SPA packet per configured access entry, authorizing
// addr for each, using the pre-resolved encryption and HMAC keys. The SPA message
// format encodes a single proto/port per packet, so each entry is sent as its own
// knock rather than combined into one request. Entries are sent in order; the first
// failure aborts and is retried (with the full set) by fireWithRetry.
func fwknopAction(fc fwknopConfig, encKey, hmacKey []byte) actionFunc {
	return func(ctx context.Context, addr string) error {
		for _, entry := range fc.access {
			err := fwknop.Knock{
				Access:      entry,
				AllowIP:     addr,
				Destination: fc.dest,
				Port:        fc.port,
				EncKey:      encKey,
				HMACKey:     hmacKey,
			}.Send(ctx)
			if err != nil {
				return fmt.Errorf("access %s: %w", entry, err)
			} else {
				slog.Info("sending access", "entry", entry, "addr", addr)
			}
		}
		return nil
	}
}

// resolveFwknopKeys fetches the base64 encryption and HMAC keys from 1Password via a
// service account, decoding them to raw key bytes. The service-account token is read
// from the OP_SERVICE_ACCOUNT_TOKEN environment variable.
func resolveFwknopKeys(ctx context.Context, fc *fwknopConfig) ([]byte, []byte, error) {
	token := strings.TrimSpace(os.Getenv("OP_SERVICE_ACCOUNT_TOKEN"))
	if token == "" {
		return nil, nil, errors.New("OP_SERVICE_ACCOUNT_TOKEN is not set")
	}

	ctx, cancel := context.WithTimeout(ctx, 30*time.Second)
	defer cancel()

	client, err := onepassword.NewClient(ctx,
		onepassword.WithServiceAccountToken(token),
		onepassword.WithIntegrationInfo("ipv6rd", opIntegrationVersion))
	if err != nil {
		return nil, nil, fmt.Errorf("creating 1Password client: %w", err)
	}

	encKey, err := resolveKeyRef(ctx, client, fc.keyRef)
	if err != nil {
		return nil, nil, fmt.Errorf("resolving encryption key %q: %w", fc.keyRef, err)
	}
	hmacKey, err := resolveKeyRef(ctx, client, fc.hmacRef)
	if err != nil {
		return nil, nil, fmt.Errorf("resolving HMAC key %q: %w", fc.hmacRef, err)
	}
	return encKey, hmacKey, nil
}

// resolveKeyRef resolves a single op:// secret reference and base64-decodes the
// result, mirroring the fwknop client's --key-base64 handling.
func resolveKeyRef(ctx context.Context, client *onepassword.Client, ref string) ([]byte, error) {
	val, err := client.Secrets().Resolve(ctx, ref)
	if err != nil {
		return nil, err
	}
	key, err := base64.StdEncoding.DecodeString(strings.TrimSpace(val))
	if err != nil {
		return nil, fmt.Errorf("base64-decoding resolved secret: %w", err)
	}
	if len(key) == 0 {
		return nil, errors.New("resolved secret is empty")
	}
	return key, nil
}

// backoffDelay returns the wait before retry n (1-indexed): 2s, 8s, 32s, ...
func backoffDelay(n int) time.Duration {
	d := 2 * time.Second
	for i := 1; i < n; i++ {
		d *= 4
	}
	return d
}

// sleepCtx sleeps for d, returning false if ctx is cancelled first.
func sleepCtx(ctx context.Context, d time.Duration) bool {
	t := time.NewTimer(d)
	defer t.Stop()
	select {
	case <-ctx.Done():
		return false
	case <-t.C:
		return true
	}
}

func orNone(s string) string {
	if s == "" {
		return "(none)"
	}
	return s
}
