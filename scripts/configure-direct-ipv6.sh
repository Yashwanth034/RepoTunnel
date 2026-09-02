#!/usr/bin/env bash
set -euo pipefail

SERVICE_NAME="rt-direct"
LEGACY_SERVICE_NAME="repotunnel-direct"
WG_DIR="/etc/wireguard"
WG_CONFIG="$WG_DIR/$SERVICE_NAME.conf"
HTTPS_TARGET_PORT="43183"
ACME_TARGET_PORT="43184"
NFT_TABLE="repotunnel_direct"

usage() {
  cat <<'USAGE'
Usage:
  sudo ./scripts/configure-direct-ipv6.sh install <wireguard-config> <public-ipv6>
  sudo ./scripts/configure-direct-ipv6.sh remove

Installs a Route64-style WireGuard IPv6 tunnel for RepoTunnel Direct HTTPS.
The public IPv6 is one address chosen from the routed prefix assigned to you.
Only inbound TCP 80 and 443 on that IPv6 are redirected to RepoTunnel's
unprivileged ACME/HTTPS listeners (43184 and 43183).
USAGE
}

require_root() {
  if [[ ${EUID:-$(id -u)} -ne 0 ]]; then
    echo "This operation needs root network privileges. Re-run with sudo." >&2
    exit 1
  fi
}

validate_ipv6() {
  python3 - "$1" <<'PY'
import ipaddress, sys
try:
    ip = ipaddress.IPv6Address(sys.argv[1])
except Exception:
    raise SystemExit(1)
if not ip.is_global:
    raise SystemExit(1)
PY
}

install_tunnel() {
  local source_config=$1
  local public_ipv6=$2

  require_root
  [[ -f "$source_config" ]] || { echo "WireGuard config not found: $source_config" >&2; exit 1; }
  validate_ipv6 "$public_ipv6" || { echo "A globally routable IPv6 address is required." >&2; exit 1; }

  command -v wg >/dev/null || { echo "wireguard-tools is required." >&2; exit 1; }
  command -v wg-quick >/dev/null || { echo "wg-quick is required." >&2; exit 1; }
  command -v nft >/dev/null || { echo "nftables is required." >&2; exit 1; }
  command -v systemctl >/dev/null || { echo "systemd is required." >&2; exit 1; }

  grep -q '^\[Interface\]' "$source_config" || { echo "Invalid WireGuard config: missing [Interface]." >&2; exit 1; }
  grep -q '^PrivateKey[[:space:]]*=' "$source_config" || { echo "Invalid WireGuard config: missing PrivateKey." >&2; exit 1; }
  grep -q '^\[Peer\]' "$source_config" || { echo "Invalid WireGuard config: missing [Peer]." >&2; exit 1; }
  grep -q '^PublicKey[[:space:]]*=' "$source_config" || { echo "Invalid WireGuard config: missing peer PublicKey." >&2; exit 1; }
  grep -q '^Endpoint[[:space:]]*=' "$source_config" || { echo "Invalid WireGuard config: missing peer Endpoint." >&2; exit 1; }

  # Clean up only the legacy RepoTunnel tunnel name from the earlier implementation.
  systemctl disable --now "wg-quick@${LEGACY_SERVICE_NAME}.service" >/dev/null 2>&1 || true
  rm -f "$WG_DIR/$LEGACY_SERVICE_NAME.conf"
  nft delete table inet "$NFT_TABLE" >/dev/null 2>&1 || true

  if [[ -e "$WG_CONFIG" ]]; then
    echo "$WG_CONFIG already exists. Remove the existing RepoTunnel direct tunnel first." >&2
    exit 1
  fi

  install -d -m 700 "$WG_DIR"
  umask 077
  cp -- "$source_config" "$WG_CONFIG"
  chmod 600 "$WG_CONFIG"

  python3 - "$WG_CONFIG" "$public_ipv6" "$NFT_TABLE" "$HTTPS_TARGET_PORT" "$ACME_TARGET_PORT" <<'PYCONF'
from pathlib import Path
import sys

path = Path(sys.argv[1])
ip = sys.argv[2]
table = sys.argv[3]
https_port = sys.argv[4]
acme_port = sys.argv[5]
lines = path.read_text().splitlines()
try:
    peer_index = next(i for i, line in enumerate(lines) if line.strip() == "[Peer]")
except StopIteration:
    raise SystemExit("Invalid WireGuard config: missing [Peer].")

interface_lines = [
    "",
    "# RepoTunnel Direct HTTPS ingress. Route64 routes this IPv6 to the tunnel.",
    f"PostUp = nft delete table inet {table} 2>/dev/null || true",
    f"PostUp = nft add table inet {table}",
    f"PostUp = nft 'add chain inet {table} prerouting {{ type nat hook prerouting priority dstnat; policy accept; }}'",
    f"PostUp = nft add rule inet {table} prerouting ip6 daddr {ip} tcp dport 443 redirect to :{https_port}",
    f"PostUp = nft add rule inet {table} prerouting ip6 daddr {ip} tcp dport 80 redirect to :{acme_port}",
    f"PostDown = nft delete table inet {table} 2>/dev/null || true",
    "",
]
lines[peer_index:peer_index] = interface_lines

if not any(line.strip().startswith("PersistentKeepalive") for line in lines):
    peer_index = next(i for i, line in enumerate(lines) if line.strip() == "[Peer]")
    lines.insert(peer_index + 1, "PersistentKeepalive = 15")

path.write_text("\n".join(lines) + "\n")
PYCONF

  nft delete table inet "$NFT_TABLE" >/dev/null 2>&1 || true

  if ! systemctl enable --now "wg-quick@${SERVICE_NAME}.service"; then
    systemctl disable --now "wg-quick@${SERVICE_NAME}.service" >/dev/null 2>&1 || true
    systemctl disable --now "wg-quick@${LEGACY_SERVICE_NAME}.service" >/dev/null 2>&1 || true
    rm -f "$WG_CONFIG"
    nft delete table inet "$NFT_TABLE" >/dev/null 2>&1 || true
    echo "WireGuard activation failed; RepoTunnel direct IPv6 changes were rolled back." >&2
    exit 1
  fi

  if ! systemctl is-active --quiet "wg-quick@${SERVICE_NAME}.service"; then
    systemctl disable --now "wg-quick@${SERVICE_NAME}.service" >/dev/null 2>&1 || true
    rm -f "$WG_CONFIG"
    nft delete table inet "$NFT_TABLE" >/dev/null 2>&1 || true
    echo "WireGuard service did not become active; RepoTunnel direct IPv6 changes were rolled back." >&2
    exit 1
  fi

  echo "RepoTunnel Direct IPv6 tunnel enabled."
  echo "Public IPv6 (already assigned by Route64 config): $public_ipv6"
  echo "HTTPS: [${public_ipv6}]:443 -> localhost:${HTTPS_TARGET_PORT}"
  echo "ACME:  [${public_ipv6}]:80  -> localhost:${ACME_TARGET_PORT}"
}

remove_tunnel() {
  require_root
  systemctl disable --now "wg-quick@${SERVICE_NAME}.service" >/dev/null 2>&1 || true
  systemctl disable --now "wg-quick@${LEGACY_SERVICE_NAME}.service" >/dev/null 2>&1 || true
  nft delete table inet "$NFT_TABLE" >/dev/null 2>&1 || true
  rm -f "$WG_CONFIG" "$WG_DIR/$LEGACY_SERVICE_NAME.conf"
  echo "RepoTunnel Direct IPv6 tunnel removed. Existing Wi-Fi/IPv4 settings were not changed."
}

case "${1:-}" in
  install)
    [[ $# -eq 3 ]] || { usage; exit 2; }
    install_tunnel "$2" "$3"
    ;;
  remove)
    [[ $# -eq 1 ]] || { usage; exit 2; }
    remove_tunnel
    ;;
  *)
    usage
    exit 2
    ;;
esac
