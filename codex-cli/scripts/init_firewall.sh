#!/usr/bin/env bash
set -euo pipefail
IFS=$'\n\t'

allowed_domains_file="/etc/codex/allowed_domains.txt"
temporary_ipset="allowed-domains-new-$$"

fail_closed() {
  local status=$?
  trap - ERR
  set +e
  printf '%s\n' '*filter' ':INPUT DROP [0:0]' ':FORWARD DROP [0:0]' ':OUTPUT DROP [0:0]' \
    '-A INPUT -i lo -j ACCEPT' '-A OUTPUT -o lo -j ACCEPT' \
    '-A INPUT -m conntrack --ctstate ESTABLISHED,RELATED -j ACCEPT' 'COMMIT' | iptables-restore
  printf '%s\n' '*filter' ':INPUT DROP [0:0]' ':FORWARD DROP [0:0]' ':OUTPUT DROP [0:0]' \
    '-A INPUT -i lo -j ACCEPT' '-A OUTPUT -o lo -j ACCEPT' \
    '-A INPUT -m conntrack --ctstate ESTABLISHED,RELATED -j ACCEPT' 'COMMIT' | ip6tables-restore
  ipset destroy "$temporary_ipset" >/dev/null 2>&1
  echo "ERROR: Firewall setup failed; fail-closed policy installed" >&2
  exit "$status"
}
trap fail_closed ERR

for command_name in dig curl ipset iptables-restore ip6tables-restore; do
  command -v "$command_name" >/dev/null 2>&1 || { echo "ERROR: $command_name is required" >&2; false; }
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
[ "${#dns_resolvers[@]}" -gt 0 ] || { echo "ERROR: No IPv4 DNS resolver configured" >&2; false; }

ipset destroy "$temporary_ipset" >/dev/null 2>&1 || true
ipset create "$temporary_ipset" hash:net
for domain in "${allowed_domains[@]}"; do
  mapfile -t ips < <(dig +short A "$domain" | sed '/^[[:space:]]*$/d' | sort -u)
  [ "${#ips[@]}" -gt 0 ] || { echo "ERROR: Failed to resolve $domain" >&2; false; }
  for ip in "${ips[@]}"; do
    [[ "$ip" =~ ^[0-9]{1,3}(\.[0-9]{1,3}){3}$ ]] || { echo "ERROR: Invalid IPv4 address: $ip" >&2; false; }
    ipset add "$temporary_ipset" "$ip" -exist
  done
done
ipset create allowed-domains hash:net -exist
ipset swap "$temporary_ipset" allowed-domains
ipset destroy "$temporary_ipset"

ipv4_rules="$(mktemp)"
ipv6_rules="$(mktemp)"
trap 'rm -f "$ipv4_rules" "$ipv6_rules"' EXIT
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
printf '%s\n' '*filter' ':INPUT DROP [0:0]' ':FORWARD DROP [0:0]' ':OUTPUT DROP [0:0]' \
  '-A INPUT -i lo -j ACCEPT' '-A OUTPUT -o lo -j ACCEPT' \
  '-A INPUT -m conntrack --ctstate ESTABLISHED,RELATED -j ACCEPT' \
  '-A OUTPUT -m conntrack --ctstate ESTABLISHED,RELATED -j ACCEPT' 'COMMIT' >"$ipv6_rules"
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
trap - ERR
echo "Firewall verification passed"
