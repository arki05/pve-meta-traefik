PREFIX ?= /usr
DESTDIR ?=
CARGO ?= cargo

.PHONY: build test check deb install clean

build:
	$(CARGO) build --release --locked

test:
	$(CARGO) test --locked

check:
	$(CARGO) fmt --check
	$(CARGO) clippy --all-targets --locked -- -D warnings
	$(CARGO) test --locked

install: build
	install -D -m 0755 target/release/pve-meta-traefik $(DESTDIR)$(PREFIX)/bin/pve-meta-traefik
	install -D -m 0644 pve-meta-traefik.service $(DESTDIR)$(PREFIX)/lib/systemd/system/pve-meta-traefik.service
	install -D -m 0644 config.example.yaml $(DESTDIR)$(PREFIX)/share/doc/pve-meta-traefik/config.example.yaml

deb:
	dpkg-buildpackage -b -us -uc

clean:
	$(CARGO) clean
	rm -rf debian/pve-meta-traefik debian/.debhelper debian/files debian/*.substvars debian/*.log debian/debhelper-build-stamp
