#
# steward: the whole system for the device that hosts the controller: the
# controller, its web interface, and the agent that lets it manage itself.
# Every other device needs steward-agent only.
#

include $(TOPDIR)/rules.mk

PKG_NAME:=steward
include $(CURDIR)/../steward.mk

include $(INCLUDE_DIR)/package.mk

define Package/steward
  SECTION:=net
  CATEGORY:=Network
  SUBMENU:=Steward
  TITLE:=Steward: controller, web interface and agent
  URL:=$(STEWARD_URL)
  PKGARCH:=all
  DEPENDS:=@USE_APK +steward-agent +steward-controller +steward-web
endef

define Package/steward/description
  Installs everything the device hosting the Steward controller needs:
  steward-controller, steward-web and steward-agent.
endef

define Build/Compile
endef

define Package/steward/install
	$(INSTALL_DIR) $(1)/usr/share/steward
endef

$(eval $(call BuildPackage,steward))
