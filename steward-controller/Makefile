#
# steward-controller: the controller. It runs on the network's router, or on
# any OpenWrt host the devices can reach.
#
# Built by the OpenWrt SDK or buildroot from this feed, with rust-package.mk
# from the packages feed (see the repository's README).
#

include $(TOPDIR)/rules.mk

PKG_NAME:=steward-controller
include $(CURDIR)/../steward.mk

PKG_BUILD_DEPENDS:=$(STEWARD_RUST_BUILD_DEPENDS)
PKG_BUILD_PARALLEL:=1

include $(INCLUDE_DIR)/package.mk
include $(TOPDIR)/feeds/packages/lang/rust/rust-package.mk

define Package/steward-controller
  SECTION:=net
  CATEGORY:=Network
  SUBMENU:=Steward
  TITLE:=Steward controller (adopts and manages OpenWrt devices)
  URL:=$(STEWARD_URL)
  DEPENDS:=@USE_APK $(RUST_ARCH_DEPENDS)
endef

define Package/steward-controller/description
  Adopts the OpenWrt devices that run steward-agent, keeps their
  configuration, and collects their state, clients, traffic and topology.
  Devices connect to it on port 15002 (uCentral's WebSocket protocol); the
  web interface is steward-web. Settings: /etc/config/steward-controller.
  Its API signs in the router's accounts through rpcd; an account needs the
  access group steward, which root's has.
endef

define Package/steward-controller/conffiles
/etc/config/steward-controller
endef

Build/Prepare=$(Steward/Prepare/Cargo)
Build/Compile=$(call Build/Compile/Cargo,steward-controller)

define Package/steward-controller/install
	$(INSTALL_DIR) $(1)/usr/sbin $(1)/etc/init.d $(1)/etc/config
	$(INSTALL_BIN) $(PKG_INSTALL_DIR)/bin/steward-controller $(1)/usr/sbin/
	$(INSTALL_BIN) ./files/steward-controller.init $(1)/etc/init.d/steward-controller
	$(INSTALL_CONF) ./files/steward-controller.config $(1)/etc/config/steward-controller
	$(INSTALL_DIR) $(1)/lib/upgrade/keep.d
	$(INSTALL_DATA) ./files/steward-controller.keep $(1)/lib/upgrade/keep.d/steward-controller
	$(INSTALL_DIR) $(1)/usr/share/rpcd/acl.d
	$(INSTALL_DATA) ./files/steward-controller.acl.json $(1)/usr/share/rpcd/acl.d/steward-controller.json
endef

$(eval $(call RustBinPackage,steward-controller))
$(eval $(call BuildPackage,steward-controller))
