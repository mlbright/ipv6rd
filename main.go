// Command ipv6rd watches the host's outbound (egress) IPv6 address and re-knocks
// whenever that address rotates, keeping an fwknop SPA grant in sync with a
// rotating IPv6 temporary/dynamic address.
//
// It supports two modes:
//   - Built-in fwknop mode (--fwknop-* flags): sends the SPA packet in process
//     via the fwknop library, fetching the encryption and HMAC keys from
//     1Password through a service account.
//   - Generic command mode (-- command): runs a configured command with the new
//     address appended as the final argument (also exported as $IPV6RD_ADDR).
//
// The egress address is determined by the kernel's source-address selection via
// netlink RouteGet (see docs/adr/0001-egress-address-via-route-get.md), not by
// parsing the interface address table. Linux-only.
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
	"os/exec"
	"os/signal"
	"strings"
	"syscall"
	"time"

	onepassword "github.com/1password/onepassword-sdk-go"
	"github.com/damienstuart/fwknop-go/fwknop"
	"github.com/vishvananda/netlink"
)

// opIntegrationVersion identifies this integration to the 1Password SDK.
const opIntegrationVersion = "1.0.0"

type config struct {
	dest      net.IP        // destination used for kernel source-address selection
	destRaw   string        // dest as given on the command line (IP or hostname)
	debounce  time.Duration // quiet period after an address event before recomputing
	reconcile time.Duration // reconciliation poll interval (backstop for missed events)
	timeout   time.Duration // per-attempt command timeout
	attempts  int           // max command attempts before giving up
	command   []string      // generic mode: command argv; the new address is appended last
	fwknop    *fwknopConfig // built-in fwknop mode: non-nil when --fwknop-* flags are set
}

// fwknopConfig holds the built-in fwknop SPA mode settings. The encryption and HMAC
// keys are not stored here; they are resolved from 1Password at startup.
type fwknopConfig struct {
	access  string // requested access specification, e.g. "tcp/443"
	dest    string // SPA target host (where the packet is sent)
	port    int    // UDP port fwknopd listens on
	keyRef  string // 1Password secret reference (op://...) for the base64 encryption key
	hmacRef string // 1Password secret reference (op://...) for the base64 HMAC key
}

// actionFunc performs one re-knock attempt for addr. Cancelled via ctx (per-attempt
// timeout). exec mode and fwknop mode each provide one implementation.
type actionFunc func(ctx context.Context, addr string) error

func main() {
	slog.SetDefault(slog.New(slog.NewTextHandler(os.Stderr, &slog.HandlerOptions{Level: slog.LevelInfo})))

	cfg, err := parseFlags(os.Args[1:])
	if err != nil {
		if !errors.Is(err, flag.ErrHelp) {
			fmt.Fprintln(os.Stderr, "ipv6rd:", err)
		}
		os.Exit(2)
	}

	if err := run(cfg); err != nil {
		slog.Error("exiting", "err", err)
		os.Exit(1)
	}
}

func parseFlags(args []string) (config, error) {
	fs := flag.NewFlagSet("ipv6rd", flag.ContinueOnError)
	dest := fs.String("dest", "2606:4700:4700::1111", "IPv6 address or hostname used for kernel source-address selection (the egress address is the source the kernel picks to reach it); for fwknop precision, point this at the SPA target")
	debounce := fs.Duration("debounce", 2*time.Second, "quiet period after an address event before recomputing the egress address")
	reconcile := fs.Duration("reconcile", 5*time.Minute, "reconciliation poll interval; backstop that recomputes even if a netlink event is missed")
	timeout := fs.Duration("timeout", 60*time.Second, "per-attempt timeout for the command")
	attempts := fs.Int("retries", 3, "maximum command attempts before giving up (bounded exponential backoff between attempts)")
	fwknopAccess := fs.String("fwknop-access", "tcp/443", "fwknop mode: access specification to request")
	fwknopDest := fs.String("fwknop-dest", "", "fwknop mode: SPA target host the packet is sent to (any --fwknop-* flag enables built-in fwknop mode)")
	fwknopPort := fs.Int("fwknop-port", 0, "fwknop mode: UDP port fwknopd listens on (required in fwknop mode)")
	fwknopKeyRef := fs.String("fwknop-key-ref", "", "fwknop mode: 1Password secret reference (op://vault/item/field) for the base64 encryption key")
	fwknopHMACRef := fs.String("fwknop-hmac-ref", "", "fwknop mode: 1Password secret reference (op://vault/item/field) for the base64 HMAC key")
	fs.Usage = func() {
		fmt.Fprintf(fs.Output(),
			"usage:\n"+
				"  ipv6rd [flags] --fwknop-dest HOST --fwknop-port N \\\n"+
				"         --fwknop-key-ref op://... --fwknop-hmac-ref op://...\n"+
				"  ipv6rd [flags] -- command [args...]\n\n"+
				"Re-knocks whenever the egress IPv6 address rotates. In built-in fwknop mode\n"+
				"(--fwknop-* flags) the SPA packet is sent in process and the keys are read from\n"+
				"1Password (OP_SERVICE_ACCOUNT_TOKEN). In generic mode the command is run with the\n"+
				"new address appended as the final argument (also exported as $IPV6RD_ADDR).\n\n"+
				"flags:\n")
		fs.PrintDefaults()
	}
	if err := fs.Parse(args); err != nil {
		return config{}, err
	}

	if *attempts < 1 {
		return config{}, errors.New("--retries must be >= 1")
	}

	// Built-in fwknop mode is enabled when any --fwknop-* flag is explicitly set.
	fwknopMode := false
	fs.Visit(func(f *flag.Flag) {
		if strings.HasPrefix(f.Name, "fwknop-") {
			fwknopMode = true
		}
	})

	ip, err := resolveDest(*dest)
	if err != nil {
		return config{}, err
	}

	cfg := config{
		dest:      ip,
		destRaw:   *dest,
		debounce:  *debounce,
		reconcile: *reconcile,
		timeout:   *timeout,
		attempts:  *attempts,
	}

	command := fs.Args()
	if fwknopMode {
		if len(command) > 0 {
			return config{}, errors.New("built-in fwknop mode (--fwknop-* flags) cannot be combined with a trailing '-- command'")
		}
		if *fwknopDest == "" {
			return config{}, errors.New("--fwknop-dest is required in fwknop mode")
		}
		if *fwknopPort < 1 || *fwknopPort > 65535 {
			return config{}, errors.New("--fwknop-port must be in 1..65535 in fwknop mode")
		}
		if *fwknopKeyRef == "" {
			return config{}, errors.New("--fwknop-key-ref is required in fwknop mode")
		}
		if *fwknopHMACRef == "" {
			return config{}, errors.New("--fwknop-hmac-ref is required in fwknop mode")
		}
		cfg.fwknop = &fwknopConfig{
			access:  *fwknopAccess,
			dest:    *fwknopDest,
			port:    *fwknopPort,
			keyRef:  *fwknopKeyRef,
			hmacRef: *fwknopHMACRef,
		}
		return cfg, nil
	}

	if len(command) == 0 {
		return config{}, errors.New("no command given; put it after '--' (e.g. ipv6rd -- /home/ubuntu/ipv6rd/poke-hole.sh), or use --fwknop-* flags for built-in fwknop mode")
	}
	cfg.command = command
	return cfg, nil
}

// resolveDest turns an IPv6 literal or a hostname into an IPv6 address.
func resolveDest(s string) (net.IP, error) {
	if ip := net.ParseIP(s); ip != nil {
		if ip.To4() != nil {
			return nil, fmt.Errorf("--dest %q is IPv4; an IPv6 destination is required", s)
		}
		return ip, nil
	}
	ips, err := net.LookupIP(s)
	if err != nil {
		return nil, fmt.Errorf("resolving --dest %q: %w", s, err)
	}
	for _, ip := range ips {
		if ip.To4() == nil && ip.To16() != nil {
			return ip, nil
		}
	}
	return nil, fmt.Errorf("--dest %q has no AAAA (IPv6) address", s)
}

// egressAddr returns the source IPv6 address the kernel would use to reach dest,
// or "" if there is currently no usable source (e.g. no IPv6 connectivity).
func egressAddr(dest net.IP) (string, error) {
	routes, err := netlink.RouteGet(dest)
	if err != nil {
		return "", err
	}
	for _, r := range routes {
		if r.Src != nil {
			return r.Src.String(), nil
		}
	}
	return "", nil
}

type fireResult struct {
	target string
	err    error
}

func run(cfg config) error {
	ctx, stop := signal.NotifyContext(context.Background(), syscall.SIGINT, syscall.SIGTERM)
	defer stop()

	attrs := []any{
		"dest", cfg.destRaw, "dest_ip", cfg.dest.String(),
		"debounce", cfg.debounce.String(), "reconcile", cfg.reconcile.String(),
		"timeout", cfg.timeout.String(), "attempts", cfg.attempts,
	}
	if cfg.fwknop != nil {
		attrs = append(attrs, "mode", "fwknop",
			"spa_dest", cfg.fwknop.dest, "spa_port", cfg.fwknop.port, "access", cfg.fwknop.access)
	} else {
		attrs = append(attrs, "mode", "command", "command", cfg.command)
	}
	slog.Info("starting", attrs...)

	// Select the per-rotation action. In fwknop mode this resolves the keys from
	// 1Password up front (fail fast on misconfiguration) and caches them.
	act, err := buildAction(ctx, cfg)
	if err != nil {
		return err
	}

	// Address-event source: a goroutine owns the netlink subscription (including
	// resubscribe on failure) and forwards a coalesced signal on every event.
	events := make(chan struct{}, 1)
	go watchAddrEvents(ctx, events)

	debounce := time.NewTimer(0)
	if !debounce.Stop() {
		<-debounce.C
	}
	reconcile := time.NewTicker(cfg.reconcile)
	defer reconcile.Stop()

	fireDone := make(chan fireResult, 1)

	var (
		latest    string // most recent observed egress address (the desired target)
		lastFired string // address of the last successful command run
		running   bool   // a command is currently in flight
	)

	// maybeFire starts a command run if one isn't already running and the latest
	// target differs from what we last successfully fired with. Serialized: at
	// most one run in flight.
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

		case res := <-fireDone:
			running = false
			if res.err != nil {
				slog.Error("fire failed; will retry on next rotation or reconcile", "addr", res.target, "err", res.err)
			} else {
				lastFired = res.target
				slog.Info("fire succeeded", "addr", res.target)
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

// watchAddrEvents owns the netlink address-event subscription and forwards a
// coalesced signal on out for every event, resubscribing if the subscription
// dies. The main loop debounces these signals.
func watchAddrEvents(ctx context.Context, out chan<- struct{}) {
	for ctx.Err() == nil {
		updates := make(chan netlink.AddrUpdate, 64)
		done := make(chan struct{})
		err := netlink.AddrSubscribeWithOptions(updates, done, netlink.AddrSubscribeOptions{
			ErrorCallback: func(e error) {
				if ctx.Err() == nil { // suppress the receive-interrupted error during shutdown
					slog.Warn("netlink subscription error", "err", e)
				}
			},
		})
		if err != nil {
			slog.Warn("address-event subscribe failed; retrying", "err", err)
			close(done)
			if !sleepCtx(ctx, 5*time.Second) {
				return
			}
			continue
		}

		closed := drainUpdates(ctx, updates, out)
		close(done)
		if ctx.Err() != nil {
			return
		}
		if closed {
			slog.Warn("netlink updates channel closed; resubscribing")
			sleepCtx(ctx, time.Second)
		}
	}
}

// drainUpdates forwards coalesced signals until the updates channel closes or ctx
// is cancelled. It reports whether the channel closed (vs. ctx cancellation).
func drainUpdates(ctx context.Context, updates <-chan netlink.AddrUpdate, out chan<- struct{}) bool {
	for {
		select {
		case <-ctx.Done():
			return false
		case _, ok := <-updates:
			if !ok {
				return true
			}
			select {
			case out <- struct{}{}:
			default: // a signal is already pending; coalesce
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

// buildAction selects the per-rotation action: an in-process fwknop SPA knock (when
// fwknop mode is configured) or running the configured command. In fwknop mode it
// resolves the encryption and HMAC keys from 1Password once, up front, so re-knocks
// never touch the network beyond the SPA packet itself.
func buildAction(ctx context.Context, cfg config) (actionFunc, error) {
	if cfg.fwknop == nil {
		return execAction(cfg.command), nil
	}
	encKey, hmacKey, err := resolveFwknopKeys(ctx, cfg.fwknop)
	if err != nil {
		return nil, err
	}
	slog.Info("resolved fwknop keys from 1Password", "key_ref", cfg.fwknop.keyRef, "hmac_ref", cfg.fwknop.hmacRef)
	return fwknopAction(cfg.fwknop, encKey, hmacKey), nil
}

// execAction runs the configured command with addr appended as the final argument
// and exported as $IPV6RD_ADDR.
func execAction(command []string) actionFunc {
	return func(ctx context.Context, addr string) error {
		args := append(append([]string{}, command[1:]...), addr)
		c := exec.CommandContext(ctx, command[0], args...)
		c.Env = append(os.Environ(), "IPV6RD_ADDR="+addr)
		c.Stdout = os.Stderr
		c.Stderr = os.Stderr
		return c.Run()
	}
}

// fwknopAction sends an fwknop SPA packet authorizing addr, using the pre-resolved
// encryption and HMAC keys.
func fwknopAction(fc *fwknopConfig, encKey, hmacKey []byte) actionFunc {
	return func(ctx context.Context, addr string) error {
		return fwknop.Knock{
			Access:      fc.access,
			AllowIP:     addr,
			Destination: fc.dest,
			Port:        fc.port,
			EncKey:      encKey,
			HMACKey:     hmacKey,
		}.Send(ctx)
	}
}

// resolveFwknopKeys fetches the base64 encryption and HMAC keys from 1Password via a
// service account, decoding them to raw key bytes. The service-account token is read
// from the OP_SERVICE_ACCOUNT_TOKEN environment variable.
func resolveFwknopKeys(ctx context.Context, fc *fwknopConfig) ([]byte, []byte, error) {
	token := strings.TrimSpace(os.Getenv("OP_SERVICE_ACCOUNT_TOKEN"))
	if token == "" {
		return nil, nil, errors.New("OP_SERVICE_ACCOUNT_TOKEN is not set (required for fwknop mode)")
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
