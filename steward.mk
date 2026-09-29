#
# Shared by the packages' Makefiles: include it after rules.mk, with PKG_NAME
# set. Sets PKG_VERSION, PKG_RELEASE and the license, and for the Rust
# packages how the cargo workspace reaches the build directory.
#

STEWARD_ROOT:=$(abspath $(CURDIR)/..)

# One version for the whole system: the cargo workspace's.
PKG_VERSION:=$(shell sed -n '/^\[workspace\.package\]/,/^\[/s/^version *= *"\(.*\)"/\1/p' $(STEWARD_ROOT)/Cargo.toml)

# The UTC date (YYYYMMDDHHMM) of the last commit on main's first-parent line
# that changed what this package is built from (pkg_inputs in packages.sh,
# sdk-build.sh included): a change elsewhere leaves its version, and so every
# device, alone, and a merge dates the change by when it reached main, so the
# number goes up as long as main's commit dates do (feed-plan.sh refuses to
# publish a build older than the feed's). The feed rebuilds a package it
# has only when one of those changed, so the rebuild has a new version, and it
# never replaces a published file (publish.sh): a published name-version never
# changes its bytes. 1 outside a git checkout.
STEWARD_INPUTS:=$(shell . $(STEWARD_ROOT)/.github/scripts/packages.sh && pkg_inputs $(PKG_NAME))
PKG_RELEASE:=$(or $(shell cd $(STEWARD_ROOT) && TZ=UTC git log -1 --first-parent --format=%cd --date=format-local:%Y%m%d%H%M -- $(STEWARD_INPUTS) 2>/dev/null),1)

PKG_MAINTAINER:=fonix232
PKG_LICENSE:=MIT
PKG_LICENSE_FILES:=LICENSE

STEWARD_URL:=https://github.com/fonix232/steward
STEWARD_FEED:=https://fonix232.github.io/steward/apk

# rust/host builds rustc and LLVM from source. The feed's CI sets
# STEWARD_RUSTUP=1 and puts a rustup toolchain on PATH instead (see
# .github/scripts/sdk-build.sh); rust-package.mk works the same with either.
STEWARD_RUST_BUILD_DEPENDS:=$(if $(filter 1,$(STEWARD_RUSTUP)),,rust/host)

# The Rust packages build from the cargo workspace at the repository's top,
# so the build directory gets its manifest, lock file and every member.
define Steward/Prepare/Cargo
	$(INSTALL_DIR) $(PKG_BUILD_DIR)
	$(CP) $(STEWARD_ROOT)/Cargo.toml $(STEWARD_ROOT)/Cargo.lock $(STEWARD_ROOT)/LICENSE $(PKG_BUILD_DIR)/
	$(CP) $(STEWARD_ROOT)/crates $(STEWARD_ROOT)/steward-agent $(STEWARD_ROOT)/steward-controller $(PKG_BUILD_DIR)/
endef
