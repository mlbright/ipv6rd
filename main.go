// Command ipv6rd watches the host's outbound (egress) IPv6 address and runs a
// configured command whenever that address rotates, passing the new address as
// the final argument (and in $IPV6RD_ADDR). Its primary use is keeping an fwknop
// SPA grant in sync with a rotating IPv6 temporary/dynamic address.
//
// The egress address is determined by the kernel's source-address selection via
// netlink RouteGet (see docs/adr/0001-egress-address-via-route-get.md), not by
// parsing the interface address table. Linux-only.
package main

import (
	"context"
	"errors"
	"flag"
	"fmt"
	"log/slog"
	"net"
	"os"
	"os/exec"
	"os/signal"
	"syscall"
	"time"

	"github.com/vishvananda/netlink"
)

type config struct {
	dest      net.IP        // destination used for kernel source-address selection
	destRaw   string        // dest as given on the command line (IP or hostname)
	debounce  time.Duration // quiet period after an address event before recomputing
	reconcile time.Duration // reconciliation poll interval (backstop for missed events)
	timeout   time.Duration // per-attempt command timeout
	attempts  int           // max command attempts before giving up
	command   []string      // command argv; the new address is appended as the last arg
}

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
	fs.Usage = func() {
		fmt.Fprintf(fs.Output(),
			"usage: ipv6rd [flags] -- command [args...]\n\n"+
				"Runs command with the new egress IPv6 address appended as the final\n"+
				"argument (also exported as $IPV6RD_ADDR) whenever the egress address rotates.\n\n"+
				"flags:\n")
		fs.PrintDefaults()
	}
	if err := fs.Parse(args); err != nil {
		return config{}, err
	}

	command := fs.Args()
	if len(command) == 0 {
		return config{}, errors.New("no command given; put it after '--', e.g. ipv6rd -- /home/ubuntu/ipv6rd/poke-hole.sh")
	}
	if *attempts < 1 {
		return config{}, errors.New("--retries must be >= 1")
	}

	ip, err := resolveDest(*dest)
	if err != nil {
		return config{}, err
	}
	return config{
		dest:      ip,
		destRaw:   *dest,
		debounce:  *debounce,
		reconcile: *reconcile,
		timeout:   *timeout,
		attempts:  *attempts,
		command:   command,
	}, nil
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

	slog.Info("starting",
		"dest", cfg.destRaw, "dest_ip", cfg.dest.String(),
		"debounce", cfg.debounce.String(), "reconcile", cfg.reconcile.String(),
		"timeout", cfg.timeout.String(), "attempts", cfg.attempts, "command", cfg.command)

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
		slog.Info("firing command", "addr", target)
		go func() {
			fireDone <- fireResult{target: target, err: runCommand(ctx, cfg, target)}
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
				slog.Error("command failed; will retry on next rotation or reconcile", "addr", res.target, "err", res.err)
			} else {
				lastFired = res.target
				slog.Info("command succeeded", "addr", res.target)
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

// runCommand runs cfg.command with addr appended as the final argument, retrying
// with bounded exponential backoff up to cfg.attempts times.
func runCommand(ctx context.Context, cfg config, addr string) error {
	var lastErr error
	for attempt := 1; attempt <= cfg.attempts; attempt++ {
		if attempt > 1 {
			backoff := backoffDelay(attempt - 1)
			slog.Warn("retrying command", "addr", addr, "attempt", attempt, "of", cfg.attempts, "after", backoff.String())
			if !sleepCtx(ctx, backoff) {
				return ctx.Err()
			}
		}

		runCtx, cancel := context.WithTimeout(ctx, cfg.timeout)
		args := append(append([]string{}, cfg.command[1:]...), addr)
		c := exec.CommandContext(runCtx, cfg.command[0], args...)
		c.Env = append(os.Environ(), "IPV6RD_ADDR="+addr)
		c.Stdout = os.Stderr
		c.Stderr = os.Stderr
		err := c.Run()
		cancel()

		if err == nil {
			return nil
		}
		lastErr = err
		slog.Warn("command attempt failed", "addr", addr, "attempt", attempt, "err", err)
		if ctx.Err() != nil { // shutting down: stop retrying
			return ctx.Err()
		}
	}
	return fmt.Errorf("all %d attempts failed: %w", cfg.attempts, lastErr)
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
