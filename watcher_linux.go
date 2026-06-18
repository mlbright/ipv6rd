//go:build linux

package main

import (
	"context"
	"log/slog"
	"time"

	"github.com/vishvananda/netlink"
)

func platformDefaultConfigPath() string {
	return "/etc/ipv6rd/ipv6rd.toml"
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
