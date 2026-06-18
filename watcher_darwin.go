//go:build darwin

package main

import (
	"context"
	"log/slog"
	"net"
	"os"
	"path/filepath"
	"time"
)

func platformDefaultConfigPath() string {
	home, err := os.UserHomeDir()
	if err != nil {
		return "~/.config/ipv6rd/ipv6rd.toml"
	}
	return filepath.Join(home, ".config", "ipv6rd", "ipv6rd.toml")
}

// watchAddrEvents polls net.Interfaces() every 5 seconds and forwards a coalesced
// signal on out when the address set changes. The main loop debounces these signals.
func watchAddrEvents(ctx context.Context, out chan<- struct{}) {
	snapshot := currentAddrs()
	ticker := time.NewTicker(5 * time.Second)
	defer ticker.Stop()
	for {
		select {
		case <-ctx.Done():
			return
		case <-ticker.C:
			current := currentAddrs()
			if !addrSetsEqual(snapshot, current) {
				snapshot = current
				select {
				case out <- struct{}{}:
				default: // a signal is already pending; coalesce
				}
			}
		}
	}
}

func currentAddrs() map[string]struct{} {
	addrs := make(map[string]struct{})
	ifaces, err := net.Interfaces()
	if err != nil {
		slog.Warn("interface list failed", "err", err)
		return addrs
	}
	for _, iface := range ifaces {
		ifAddrs, _ := iface.Addrs()
		for _, a := range ifAddrs {
			addrs[a.String()] = struct{}{}
		}
	}
	return addrs
}

func addrSetsEqual(a, b map[string]struct{}) bool {
	if len(a) != len(b) {
		return false
	}
	for k := range a {
		if _, ok := b[k]; !ok {
			return false
		}
	}
	return true
}
