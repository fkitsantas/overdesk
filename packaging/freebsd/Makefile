# Free (open core) Overdesk for FreeBSD. Builds the tagged source with cargo.
#
# Needs, on a FreeBSD machine (see docs/PACKAGING.md "FreeBSD"):
#   make makesum              # writes distinfo for the tag tarball and every crate
#   make cargo-crates > Makefile.crates   # after any Cargo.lock change
#   make stage && make package
# The official app with Pro is built by overdesk-pro's scripts/package-freebsd.sh instead.

PORTNAME=	overdesk
DISTVERSIONPREFIX=	v
DISTVERSION=	0.1.0
CATEGORIES=	x11 net

MAINTAINER=	support@overdesk.org
COMMENT=	Share one keyboard and mouse across your computers
WWW=		https://overdesk.org

LICENSE=	AGPLv3
LICENSE_FILE=	${WRKSRC}/LICENSE

# The window-system libraries are dlopen()ed (winit, xkbcommon-dl, wgpu), so ldd never lists them:
# they are real run-time dependencies. libxkbcommon also provides libxkbcommon-x11.
RUN_DEPENDS=	libxkbcommon>0:x11/libxkbcommon \
		libX11>0:x11/libX11 \
		libXcursor>0:x11/libXcursor \
		libXrandr>0:x11/libXrandr \
		libXi>0:x11/libXi \
		wayland>0:graphics/wayland \
		mesa-libs>0:graphics/mesa-libs

USES=		cargo
USE_GITHUB=	yes
GH_ACCOUNT=	fkitsantas

CARGO_INSTALL_PATH=	crates/overdesk-app
post-install:
	${INSTALL_DATA} ${WRKSRC}/crates/overdesk-app/linux/org.overdesk.Overdesk.desktop \
		${STAGEDIR}${PREFIX}/share/applications/org.overdesk.Overdesk.desktop
.for s in 32 128 256 512
	@${MKDIR} ${STAGEDIR}${PREFIX}/share/icons/hicolor/${s}x${s}/apps
	${INSTALL_DATA} ${WRKSRC}/assets/brand/icons/app-icon-${s}.png \
		${STAGEDIR}${PREFIX}/share/icons/hicolor/${s}x${s}/apps/overdesk.png
.endfor

.include "Makefile.crates"
.include <bsd.port.mk>
