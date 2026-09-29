#
# steward-web: the controller's web interface, served by steward-controller.
#

include $(TOPDIR)/rules.mk

PKG_NAME:=steward-web
include $(CURDIR)/../steward.mk

include $(INCLUDE_DIR)/package.mk

define Package/steward-web
  SECTION:=net
  CATEGORY:=Network
  SUBMENU:=Steward
  TITLE:=Steward web interface
  URL:=$(STEWARD_URL)
  PKGARCH:=all
  DEPENDS:=@USE_APK +steward-controller
endef

define Package/steward-web/description
  The Steward controller's web interface: devices, clients, topology,
  traffic and settings. steward-controller serves it.
endef

define Build/Compile
endef

define Package/steward-web/install
	$(INSTALL_DIR) $(1)/usr/share/steward/www
	$(CP) ./www/. $(1)/usr/share/steward/www/
endef

$(eval $(call BuildPackage,steward-web))
