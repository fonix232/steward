#
# steward-agent: runs on every device a Steward controller manages.
#
# Built by the OpenWrt SDK or buildroot from this feed, with rust-package.mk
# from the packages feed (see the repository's README).
#

include $(TOPDIR)/rules.mk

PKG_NAME:=steward-agent
include $(CURDIR)/../steward.mk

PKG_BUILD_DEPENDS:=$(STEWARD_RUST_BUILD_DEPENDS)
PKG_BUILD_PARALLEL:=1

include $(INCLUDE_DIR)/package.mk
include $(TOPDIR)/feeds/packages/lang/rust/rust-package.mk

define Package/steward-agent
  SECTION:=net
  CATEGORY:=Network
  SUBMENU:=Steward
  TITLE:=Steward agent (lets a Steward controller manage this device)
  URL:=$(STEWARD_URL)
  DEPENDS:=@USE_APK $(RUST_ARCH_DEPENDS)
endef

define Package/steward-agent/description
  Connects this device to a Steward controller, applies the configuration
  the controller sends through UCI with a rollback if the device loses the
  controller, and reports the device's state, clients and topology back.
  Settings: /etc/config/steward-agent.
endef

define Package/steward-agent/conffiles
/etc/config/steward-agent
endef

Build/Prepare=$(Steward/Prepare/Cargo)
Build/Compile=$(call Build/Compile/Cargo,steward-agent)

# The agent is on every device, so it carries the feed: later versions of
# every Steward package arrive with apk upgrade.
define Package/steward-agent/install
	$(INSTALL_DIR) $(1)/usr/sbin $(1)/etc/init.d $(1)/etc/config
	$(INSTALL_BIN) $(PKG_INSTALL_DIR)/bin/steward-agent $(1)/usr/sbin/
	$(INSTALL_BIN) ./files/steward-agent.init $(1)/etc/init.d/steward-agent
	$(INSTALL_CONF) ./files/steward-agent.config $(1)/etc/config/steward-agent
	$(INSTALL_DIR) $(1)/etc/apk/keys $(1)/etc/apk/repositories.d
	$(INSTALL_DATA) $(STEWARD_ROOT)/feed/steward.pem $(1)/etc/apk/keys/steward.pem
	echo "$(STEWARD_FEED)/$(ARCH_PACKAGES)/packages.adb" > $(1)/etc/apk/repositories.d/steward.list
endef

$(eval $(call RustBinPackage,steward-agent))
$(eval $(call BuildPackage,steward-agent))
