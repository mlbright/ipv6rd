#!/bin/bash
set -euo pipefail

# Re-knock fwknop SPA for the given egress IPv6 address.
# The address is passed as $1 by the ipv6rd watcher (also available as $IPV6RD_ADDR).
addr="${1:?usage: poke-hole.sh <ipv6-address>}"

fwknop -v -A tcp/443 -a "$addr" -D notes.martyloo.com --key-base64 "$(op item get --vault fwknop 'fwknop key' --format json | jq -r '.fields[1].value')" --key-base64-hmac "$(op item get --vault fwknop "fwknop hmac" --format json | jq -r '.fields[1].value')" -p 62159
