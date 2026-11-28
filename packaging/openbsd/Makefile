# Free (open core) Overdesk for OpenBSD, X11 sessions only. A skeleton for the ports tree:
# see docs/PACKAGING.md "OpenBSD". Rust is tier 3 there and cannot be checked from other OSes, so
# nothing here has been built yet. Run `make makesum` to fill in `distinfo`.

COMMENT =	share one keyboard and mouse across your computers

V =		0.1.0
GH_ACCOUNT =	fkitsantas
GH_PROJECT =	overdesk
GH_TAGNAME =	v${V}
PKGNAME =	overdesk-${V}

CATEGORIES =	x11 net

HOMEPAGE =	https://overdesk.org
MAINTAINER =	Overdesk <support@overdesk.org>

# AGPL-3.0-only
PERMIT_PACKAGE =	Yes

WANTLIB += ${MODCARGO_WANTLIB} c m pthread

MODULES =		devel/cargo

# The window-system libraries are dlopen()ed, so WANTLIB cannot see them. X11 comes from the base
# system (xenocara); libxkbcommon is a port.
RUN_DEPENDS =		x11/xkbcommon

CONFIGURE_STYLE =	cargo
MODCARGO_INSTALL_TARGET_PATHS =	crates/overdesk-app

post-install:
	${INSTALL_DATA_DIR} ${PREFIX}/share/applications
	${INSTALL_DATA} ${WRKSRC}/crates/overdesk-app/linux/org.overdesk.Overdesk.desktop ${PREFIX}/share/applications/
	${INSTALL_DATA_DIR} ${PREFIX}/share/icons/hicolor/256x256/apps
	${INSTALL_DATA} ${WRKSRC}/assets/brand/icons/app-icon-256.png \
		${PREFIX}/share/icons/hicolor/256x256/apps/overdesk.png

.include "crates.inc"
.include <bsd.port.mk>
