# Build and install Canoe system-wide:
#
#     make
#     sudo make install
#
# PREFIX defaults to /usr/local; DESTDIR is honored for staged installs.
# SYSCONFDIR stays /etc whatever PREFIX is: xdg-desktop-portal-wlr reads its
# system-wide configuration from /etc/xdg only.

PREFIX ?= /usr/local
BINDIR ?= $(PREFIX)/bin
DATADIR ?= $(PREFIX)/share
MANDIR ?= $(DATADIR)/man
SYSCONFDIR ?= /etc
XDPWDIR = $(SYSCONFDIR)/xdg/xdg-desktop-portal-wlr

CARGO ?= cargo
INSTALL ?= install

BIN = target/release/canoe

all:
	$(CARGO) build --release --locked

# Only built if missing, so `sudo make install` doesn't run cargo as root.
$(BIN):
	$(CARGO) build --release --locked

# canoe.1 is checked in; regenerate it after editing canoe.1.md (needs pandoc).
man:
	pandoc canoe.1.md -s -t man -o canoe.1

# The session file and the screen-sharing chooser name canoe by its full
# path: display managers and the portal don't necessarily have $(BINDIR) on
# their PATH.
install: $(BIN)
	mkdir -p $(DESTDIR)$(BINDIR) $(DESTDIR)$(MANDIR)/man1 \
		$(DESTDIR)$(DATADIR)/wayland-sessions \
		$(DESTDIR)$(DATADIR)/xdg-desktop-portal \
		$(DESTDIR)$(XDPWDIR)
	$(INSTALL) -m 755 $(BIN) $(DESTDIR)$(BINDIR)/canoe
	$(INSTALL) -m 644 canoe.1 $(DESTDIR)$(MANDIR)/man1/canoe.1
	sed 's|^Exec=canoe|Exec=$(BINDIR)/canoe|' contrib/canoe.desktop \
		> $(DESTDIR)$(DATADIR)/wayland-sessions/canoe.desktop
	chmod 644 $(DESTDIR)$(DATADIR)/wayland-sessions/canoe.desktop
	$(INSTALL) -m 644 contrib/canoe-portals.conf \
		$(DESTDIR)$(DATADIR)/xdg-desktop-portal/canoe-portals.conf
	sed 's|^chooser_cmd=canoe |chooser_cmd=$(BINDIR)/canoe |' \
		contrib/xdg-desktop-portal-wlr/canoe > $(DESTDIR)$(XDPWDIR)/canoe
	chmod 644 $(DESTDIR)$(XDPWDIR)/canoe

uninstall:
	rm -f $(DESTDIR)$(BINDIR)/canoe \
		$(DESTDIR)$(MANDIR)/man1/canoe.1 \
		$(DESTDIR)$(DATADIR)/wayland-sessions/canoe.desktop \
		$(DESTDIR)$(DATADIR)/xdg-desktop-portal/canoe-portals.conf \
		$(DESTDIR)$(XDPWDIR)/canoe

clean:
	$(CARGO) clean

.PHONY: all man install uninstall clean
