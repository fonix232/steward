#!/bin/sh
# dnsmasq-check.sh <dir> [<dhcp section> <device> <subnet>]: what dnsmasq would make of the
# dhcp config `stage-export` wrote to <dir>, offline. It runs dnsmasq's own init-script helpers
# over it (hosts, domains, CNAMEs, and the named dhcp section's range, whose network isn't up,
# so its device and subnet are given), writes the options they make to <dir>/dnsmasq.conf and
# the names to <dir>/dnsmasq.hosts, and runs `dnsmasq --test` on them. Nothing starts, and
# dhcp_check (a DHCP client probing the network for another server) is skipped.
DIR=$1
SECTION=${2:-}
# shellcheck disable=SC2034 # read by the network_get_* stand-ins below, through eval
DEVICE=${3:-} SUBNET=${4:-}
# shellcheck source=/dev/null
. /lib/functions.sh
# shellcheck source=/dev/null
. /lib/functions/network.sh
# The init script's helpers and settings; sourced, it runs nothing.
# shellcheck source=/dev/null
. /etc/init.d/dnsmasq
network_get_device() { eval "$1=\$DEVICE"; }
network_get_subnet() { eval "$1=\$SUBNET"; }
network_get_protocol() { eval "$1=static"; }
network_get_dnsserver() { return 1; }
dhcp_check() { return 0; }
# shellcheck disable=SC2034 # the init script's: no names for the device's own addresses
ADD_LOCAL_FQDN=0
CONFIGFILE_TMP=$DIR/dnsmasq.conf
HOSTFILE_TMP=$DIR/dnsmasq.hosts
: > "$CONFIGFILE_TMP"
: > "$HOSTFILE_TMP"
UCI_CONFIG_DIR=$DIR
export UCI_CONFIG_DIR
config_load dhcp
config_foreach dhcp_host_add host
config_foreach dhcp_domain_add domain
[ -n "$SECTION" ] && dhcp_add "$SECTION"
config_foreach dhcp_cname_add cname
{
	grep -v '^#' "$CONFIGFILE_TMP"
	echo "addn-hosts=$HOSTFILE_TMP"
} > "$DIR/dnsmasq.test.conf"
dnsmasq --test -C "$DIR/dnsmasq.test.conf"
