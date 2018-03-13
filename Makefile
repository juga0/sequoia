# Makefile for Sequoia.

# Configuration.
PREFIX		?= /usr/local
DESTDIR		?=

# Signing source distributions.
SIGN_WITH	?= XXXXXXXXXXXXXXXX

# Deploying documentation.
DOC_TARGET	?= sequoia-pgp.org:docs.sequoia-pgp.org
RSYNC_FLAGS	?=

# Tools.
CARGO		?= cargo
GIT		?= git
RSYNC		?= rsync
INSTALL		?= install
TAR		?= tar
XZ		?= xz
GPG		?= gpg

VERSION		?= $(shell grep '^version = ' Cargo.toml | cut -d'"' -f2)
LIB_VERSION	?= $(shell grep '^version = ' ffi/Cargo.toml | cut -d'"' -f2)
LIB_VERSION_MAJ	= $(shell echo $(LIB_VERSION) | cut -d'.' -f1)

# Make sure subprocesses pick these up.
export PREFIX
export DESTDIR

all: build ffi/examples

.PHONY: build
build:
	$(CARGO) build --all
	$(MAKE) -Cffi build

# Testing and examples.
.PHONY: test check
test check:
	$(CARGO) test --all
	$(MAKE) -Cffi test

.PHONY: examples
examples:
	$(CARGO) build --examples
	$(MAKE) -Cffi examples

# Documentation.
.PHONY: doc
doc:
	$(CARGO) doc --no-deps --all

.PHONY: deploy-doc
deploy-doc: doc
	$(RSYNC) $(RSYNC_FLAGS) -r target/doc/* $(DOC_TARGET)

# Installation.
.PHONY: build-release
build-release:
	$(CARGO) build --release --all
	$(MAKE) -Cffi build-release

.PHONY: install
install: build-release
	$(INSTALL) -d $(DESTDIR)$(PREFIX)/include
	$(INSTALL) -t $(DESTDIR)$(PREFIX)/include ffi/include/sequoia.h
	$(INSTALL) -d $(DESTDIR)$(PREFIX)/include/sequoia
	$(INSTALL) -t $(DESTDIR)$(PREFIX)/include/sequoia \
		ffi/include/sequoia/*.h
	$(INSTALL) -d $(DESTDIR)$(PREFIX)/lib
	$(INSTALL) target/release/libsequoia_ffi.so \
		$(DESTDIR)$(PREFIX)/lib/libsequoia.so.$(LIB_VERSION)
	ln -fs libsequoia.so.$(LIB_VERSION) \
		$(DESTDIR)$(PREFIX)/lib/libsequoia.so.$(LIB_VERSION_MAJ)
	$(INSTALL) -d $(DESTDIR)$(PREFIX)/lib/sequoia
	$(INSTALL) -t $(DESTDIR)$(PREFIX)/lib/sequoia target/release/keystore
	$(INSTALL) -d $(DESTDIR)$(PREFIX)/bin
	$(INSTALL) -t $(DESTDIR)$(PREFIX)/bin target/release/sq
	$(MAKE) -Cffi install

# Infrastructure for creating source distributions.
.PHONY: dist
dist: target/dist/sequoia-$(VERSION).tar.xz.sig

target/dist/sequoia-$(VERSION):
	$(GIT) clone . target/dist/sequoia-$(VERSION)
	cd target/dist/sequoia-$(VERSION) && \
		mkdir .cargo && \
		$(CARGO) vendor \
			| sed 's/^directory = ".*"$$/directory = "vendor"/' \
			> .cargo/config && \
		rm -rf .git

target/dist/sequoia-$(VERSION).tar: target/dist/sequoia-$(VERSION)
	$(TAR) cf $@ -C target/dist sequoia-$(VERSION)

%.xz: %
	$(XZ) -c $< >$@

%.sig: %
	$(GPG) --local-user $(SIGN_WITH) --detach-sign --armor $<

.PHONY: dist-test dist-check
dist-test dist-check: target/dist/sequoia-$(VERSION).tar.xz
	rm -rf target/dist-check/sequoia-$(VERSION)
	mkdir target/dist-check
	$(TAR) xf $< -C target/dist-check
	cd target/dist-check/sequoia-$(VERSION) && \
		CARGO_HOME=$$(mktemp -d) $(MAKE) test
	rm -rf target/dist-check/sequoia-$(VERSION)

# Housekeeping.
.PHONY: clean
clean:
	rm -rf target
	$(MAKE) -Cffi clean