#!/bin/sh
# Build Steward's packages with an official OpenWrt SDK image (docker
# openwrt/sdk:<sdk>-openwrt-25.12, SDK in /builder; 25.12 or later). Each SDK
# builds for one package architecture, into <out>/<arch>/.
#
#   sdk-build.sh <repo> <out> [package...]     (inside the SDK container)
#
# <repo> becomes a src-link feed. Without a package list, every package in
# packages.sh is built. STEWARD_ARCH=<arch> (feed.yml passes the matrix's):
# fail unless the SDK builds for that package architecture, whose repository
# the feed publishes the output in.
#
# The Rust packages build with rust-package.mk from the SDK's packages feed,
# but on a rustup toolchain instead of rust/host, which builds rustc and LLVM
# from source (hours, per SDK): STEWARD_RUSTUP=1 drops that dependency
# (steward.mk) and the toolchain goes first on PATH. rust-values.mk still
# picks the target triple and makes the SDK's gcc the linker, so the binaries
# link against the device's musl like any other package's. A target rustup
# ships no std for (tier 3: mips, mipsel) gets std built from source on
# nightly.
set -e
REPO=$1 OUT=$2
if [ ! -f "$REPO/steward.mk" ] || [ -z "$OUT" ]; then
	echo "usage: $0 <repo> <out> [package...]" >&2
	exit 2
fi
shift 2
# shellcheck source=.github/scripts/packages.sh
. "$REPO/.github/scripts/packages.sh"
[ $# -eq 0 ] || PKGS="$*"
# The Makefiles number releases by commit date; the checkout may belong to
# another uid inside the container.
git config --global --add safe.directory '*'
export STEWARD_RUSTUP=1

cd /builder
# The moving tags (x86-64-openwrt-25.12) ship only setup.sh, which downloads
# and verifies that branch's current SDK; the versioned tags include it.
[ -f rules.mk ] || ./setup.sh >/dev/null
# From the packages feed, only lang/rust's rust-package.mk and rust-values.mk,
# where the Makefiles look for them: a sparse checkout of that directory at
# the SDK's revision, not a feed (nothing in it is built, and indexing the
# whole feed takes minutes). It comes before the feeds: indexing the steward
# feed reads the Rust packages' Makefiles, which include it.
spec=$(sed -n 's/^src-git[^ ]* packages \(.*\)$/\1/p' feeds.conf.default)
case $spec in
*^*) url=${spec%^*} ref=${spec#*^} ;;
*\;*) url=${spec%;*} ref=${spec#*;} ;;
*) url=$spec ref=HEAD ;;
esac
rm -rf feeds/packages
git init -q feeds/packages
git -C feeds/packages sparse-checkout init --cone
git -C feeds/packages sparse-checkout set lang/rust
git -C feeds/packages fetch -q --depth 1 --filter=blob:none "$url" "$ref"
git -C feeds/packages checkout -q FETCH_HEAD
grep -E '^src-git[^ ]*( --root=[^ ]+)? base ' feeds.conf.default > feeds.conf
echo "src-link steward $REPO" >> feeds.conf
./scripts/feeds update -a >/dev/null
./scripts/feeds install -a -p steward >/dev/null
make defconfig >/dev/null
ARCH=$(sed -n 's/^CONFIG_TARGET_ARCH_PACKAGES="\(.*\)"$/\1/p' .config)
# The output goes into the repository named after ARCH. Built for another
# architecture than the matrix's, it would never be published, and the plan
# would ask for the missing packages again on every run.
[ -n "$ARCH" ] || { echo "sdk-build: no CONFIG_TARGET_ARCH_PACKAGES in .config" >&2; exit 1; }
if [ -n "${STEWARD_ARCH:-}" ] && [ "$ARCH" != "$STEWARD_ARCH" ]; then
	echo "sdk-build: this SDK builds for $ARCH, not $STEWARD_ARCH" >&2
	exit 1
fi

rust=""
for p in $PKGS; do
	case $p in steward-agent|steward-controller) rust=1 ;; esac
done
if [ -n "$rust" ]; then
	triple=$(make -s -C package/feeds/steward/steward-agent TOPDIR=/builder val.RUSTC_TARGET_ARCH)
	[ -n "$triple" ] || { echo "sdk-build: no Rust target for $ARCH" >&2; exit 1; }
	command -v rustup >/dev/null || {
		curl -fsSL https://sh.rustup.rs | sh -s -- -y -q --profile minimal --default-toolchain none --no-modify-path
		PATH="$HOME/.cargo/bin:$PATH"
	}
	if rustup toolchain install stable --profile minimal --target "$triple" >/dev/null 2>&1; then
		toolchain=stable
	else
		rustup toolchain install nightly --profile minimal --component rust-src >/dev/null
		toolchain=nightly
		export CARGO_UNSTABLE_BUILD_STD=std
	fi
	# The toolchain's own binaries, not rustup's proxies: rust-values.mk moves
	# CARGO_HOME into the SDK's download directory.
	PATH="$(dirname "$(rustup which --toolchain "$toolchain" cargo)"):$PATH"
	export PATH
	echo "sdk-build: $ARCH: $triple on $(rustc --version)" >&2
fi

targets=$(for p in $PKGS; do printf 'package/%s/compile ' "$p"; done)
# shellcheck disable=SC2086 # a list of make targets, by design
make -j"$(nproc)" $targets || make $targets V=s
mkdir -p "$OUT/$ARCH"
for p in $PKGS; do
	find bin/packages -name "$p-[0-9]*.apk" -exec cp {} "$OUT/$ARCH/" \;
done
set -- "$OUT/$ARCH"/*.apk
[ -e "$1" ] || { echo "sdk-build: no packages were built" >&2; exit 1; }
ls -l "$OUT/$ARCH"
