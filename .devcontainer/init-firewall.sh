#!/usr/bin/env bash
set -euo pipefail
IFS=$'\n\t'

allowed_domains_file="/etc/codex/allowed_domains.txt"
include_github_meta_ranges="${CODEX_INCLUDE_GITHUB_META_RANGES:-1}"
temporary_ipset="allowed-domains-new-$$"

install_fail_closed_policy() {
  local status=$?
  trap - ERR
  set +e
  printf '%s\n' \
    '*filter' \
    ':INPUT DROP [0:0]' \
    ':FORWARD DROP [0:0]' \
    ':OUTPUT DROP [0:0]' \
    '-A INPUT -i lo -j ACCEPT' \
    '-A OUTPUT -o lo -j ACCEPT' \
    '-A INPUT -m conntrack --ctstate ESTABLISHED,RELATED -j ACCEPT' \
    'COMMIT' | iptables-restore
  printf '%s\n' \
    '*filter' \
    ':INPUT DROP [0:0]' \
    ':FORWARD DROP [0:0]' \
    ':OUTPUT DROP [0:0]' \
    '-A INPUT -i lo -j ACCEPT' \
    '-A OUTPUT -o lo -j ACCEPT' \
    '-A INPUT -m conntrack --ctstate ESTABLISHED,RELATED -j ACCEPT' \
    'COMMIT' | ip6tables-restore
  ipset destroy "$temporary_ipset" >/dev/null 2>&1
  echo "ERROR: Firewall setup failed; fail-closed policy installed" >&2
  exit "$status"
}
trap install_fail_closed_policy ERR

for command_name in dig curl jq ipset iptables-restore ip6tables-restore; do
  command -v "$command_name" >/dev/null 2>&1 || {
    echo "ERROR: $command_name is required" >&2
    false
  }
done

if [ -f "$allowed_domains_file" ]; then
  mapfile -t allowed_domains < <(sed '/^[[:space:]]*#/d;/^[[:space:]]*$/d' "$allowed_domains_file")
else
  allowed_domains=("api.openai.com")
fi
[ "${#allowed_domains[@]}" -gt 0 ] || { echo "ERROR: No allowed domains configured" >&2; false; }

mapfile -t dns_resolvers < <(
  awk '$1 == "nameserver" && $2 ~ /^[0-9]+(\.[0-9]+){3}$/ { print $2 }' /etc/resolv.conf | sort -u
)
[ "${#dns_resolvers[@]}" -gt 0 ] || {
  echo "ERROR: No IPv4 DNS resolver found in /etc/resolv.conf" >&2
  false
}

ipset destroy "$temporary_ipset" >/dev/null 2>&1 || true
ipset create "$temporary_ipset" hash:net

add_ipv4_network() {
  local source="$1"
  local network="$2"
  if [[ ! "$network" =~ ^[0-9]{1,3}(\.[0-9]{1,3}){3}(/[0-9]{1,2})?$ ]]; then
    echo "ERROR: Invalid IPv4 address or CIDR from $source: $network" >&2
    return 1
  fi
  ipset add "$temporary_ipset" "$network" -exist
}

for domain in "${allowed_domains[@]}"; do
  echo "Resolving $domain"
  mapfile -t ips < <(dig +short A "$domain" | sed '/^[[:space:]]*$/d' | sort -u)
  [ "${#ips[@]}" -gt 0 ] || { echo "ERROR: Failed to resolve $domain" >&2; false; }
  for ip in "${ips[@]}"; do
    add_ipv4_network "DNS for $domain" "$ip"
  done
done

if [ "$include_github_meta_ranges" = "1" ]; then
  echo "Fetching GitHub meta ranges"
  github_meta="$(curl -fsSL --connect-timeout 10 https://api.github.com/meta)"
  echo "$github_meta" | jq -e '.web and .api and .git' >/dev/null
  while IFS= read -r cidr; do
    [ -z "$cidr" ] && continue
    [[ "$cidr" == *:* ]] && continue
    add_ipv4_network "GitHub metadata" "$cidr"
  done < <(echo "$github_meta" | jq -r '((.web // []) + (.api // []) + (.git // []))[]' | sort -u)
fi

ipset create allowed-domains hash:net -exist
ipset swap "$temporary_ipset" allowed-domains
ipset destroy "$temporary_ipset"

ipv4_rules="$(mktemp)"
ipv6_rules="$(mktemp)"
cleanup_rules() { rm -f "$ipv4_rules" "$ipv6_rules"; }
trap cleanup_rules EXIT

{
  printf '%s\n' '*filter' ':INPUT DROP [0:0]' ':FORWARD DROP [0:0]' ':OUTPUT DROP [0:0]'
  printf '%s\n' '-A INPUT -i lo -j ACCEPT' '-A OUTPUT -o lo -j ACCEPT'
  printf '%s\n' '-A INPUT -m conntrack --ctstate ESTABLISHED,RELATED -j ACCEPT'
  printf '%s\n' '-A OUTPUT -m conntrack --ctstate ESTABLISHED,RELATED -j ACCEPT'
  for resolver in "${dns_resolvers[@]}"; do
    printf '%s\n' "-A OUTPUT -p udp -d $resolver/32 --dport 53 -j ACCEPT"
    printf '%s\n' "-A OUTPUT -p tcp -d $resolver/32 --dport 53 -j ACCEPT"
  done
  printf '%s\n' '-A OUTPUT -m set --match-set allowed-domains dst -j ACCEPT'
  printf '%s\n' '-A INPUT -j REJECT --reject-with icmp-admin-prohibited'
  printf '%s\n' '-A OUTPUT -j REJECT --reject-with icmp-admin-prohibited'
  printf '%s\n' '-A FORWARD -j REJECT --reject-with icmp-admin-prohibited' 'COMMIT'
} >"$ipv4_rules"

printf '%s\n' \
  '*filter' \
  ':INPUT DROP [0:0]' \
  ':FORWARD DROP [0:0]' \
  ':OUTPUT DROP [0:0]' \
  '-A INPUT -i lo -j ACCEPT' \
  '-A OUTPUT -o lo -j ACCEPT' \
  '-A INPUT -m conntrack --ctstate ESTABLISHED,RELATED -j ACCEPT' \
  '-A OUTPUT -m conntrack --ctstate ESTABLISHED,RELATED -j ACCEPT' \
  'COMMIT' >"$ipv6_rules"

iptables-restore <"$ipv4_rules"
ip6tables-restore <"$ipv6_rules"

if curl --connect-timeout 5 https://example.com >/dev/null 2>&1; then
  echo "ERROR: Firewall verification reached a disallowed domain" >&2
  false
fi
curl --connect-timeout 5 "https://${allowed_domains[0]}" >/dev/null 2>&1 || {
  echo "ERROR: Firewall verification could not reach ${allowed_domains[0]}" >&2
  false
}
if curl --connect-timeout 5 -6 https://example.com >/dev/null 2>&1; then
  echo "ERROR: Firewall verification reached a disallowed IPv6 destination" >&2
  false
fi

trap - ERR
echo "Firewall verification passed"
