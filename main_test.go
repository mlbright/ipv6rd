package main

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

// writeConfig writes a config file with the given watcher tunables above a valid
// [fwknop] section (IPv6-literal dest, so no DNS lookup) and returns its path.
func writeConfig(t *testing.T, tunables string) string {
	t.Helper()
	body := tunables + `
[fwknop]
dest     = "2001:db8::1"
port     = 62159
key_ref  = "op://vault/item/key"
hmac_ref = "op://vault/item/hmac"
`
	path := filepath.Join(t.TempDir(), "ipv6rd.toml")
	if err := os.WriteFile(path, []byte(body), 0o644); err != nil {
		t.Fatal(err)
	}
	return path
}

func TestLoadConfigDefaults(t *testing.T) {
	cfg, err := loadConfig(writeConfig(t, ""))
	if err != nil {
		t.Fatalf("loadConfig: %v", err)
	}
	if cfg.debounce != 2*time.Second {
		t.Errorf("debounce = %v, want 2s", cfg.debounce)
	}
	if cfg.reconcile != 5*time.Minute {
		t.Errorf("reconcile = %v, want 5m", cfg.reconcile)
	}
	if cfg.refresh != 0 {
		t.Errorf("refresh = %v, want 0 (disabled)", cfg.refresh)
	}
	if cfg.timeout != 60*time.Second {
		t.Errorf("timeout = %v, want 60s", cfg.timeout)
	}
	if cfg.attempts != 3 {
		t.Errorf("attempts = %d, want 3", cfg.attempts)
	}
}

func TestLoadConfigRefresh(t *testing.T) {
	tests := []struct {
		name     string
		tunables string
		want     time.Duration
		wantErr  string
	}{
		{name: "set", tunables: `refresh = "10m"`, want: 10 * time.Minute},
		{name: "explicit zero disables", tunables: `refresh = "0s"`, want: 0},
		{name: "negative rejected", tunables: `refresh = "-1s"`, wantErr: "refresh must be positive"},
		{name: "garbage rejected", tunables: `refresh = "soon"`, wantErr: "refresh"},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			cfg, err := loadConfig(writeConfig(t, tt.tunables))
			if tt.wantErr != "" {
				if err == nil || !strings.Contains(err.Error(), tt.wantErr) {
					t.Fatalf("err = %v, want it to contain %q", err, tt.wantErr)
				}
				return
			}
			if err != nil {
				t.Fatalf("loadConfig: %v", err)
			}
			if cfg.refresh != tt.want {
				t.Errorf("refresh = %v, want %v", cfg.refresh, tt.want)
			}
		})
	}
}
