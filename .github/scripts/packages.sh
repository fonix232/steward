# shellcheck shell=sh disable=SC2034 # sourced: the variables are for the scripts that source it
# The packages this repository builds, one directory each at the top level.
# Sourced by sdk-build.sh, feed-plan.sh, publish.sh and steward.mk.
PKGS="steward steward-agent steward-controller steward-web"

# The feed is one apk repository per package architecture (apk/<arch>/), each
# built by that architecture's OpenWrt SDK image
# (openwrt/sdk:<sdk>-openwrt-25.12), as <arch>:<sdk>. The packages built for
# every architecture (steward, steward-web) go into each repository too, so
# a device needs only its own.
TARGETS="aarch64_cortex-a53:mediatek-filogic arm_cortex-a7_neon-vfpv4:ipq40xx-generic mipsel_24kc:ramips-mt7621 x86_64:x86-64"

# What a package is built from: everything that changes its bytes, the build
# script included. A change to any of these rebuilds it, and its release is
# the date of the last one (steward.mk), so a rebuilt package gets a new
# version: a published name-version never changes. Every package takes its
# version from the cargo workspace's manifest.
pkg_inputs() {
	case $1 in
	steward-agent) printf '%s ' "$1 crates Cargo.toml Cargo.lock feed/steward.pem" ;;
	steward-controller) printf '%s ' "$1 crates Cargo.toml Cargo.lock" ;;
	*) printf '%s ' "$1 Cargo.toml" ;;
	esac
	echo "steward.mk LICENSE .github/scripts/sdk-build.sh"
}
