# Installing a tuigreety release archive

The archive is a staged Linux root filesystem, not an installer. Its `usr/`
and `etc/` directories show the exact destination of every file. Prefer a
distribution package when one is available so upgrades and configuration
backups remain under the package manager's control.

First verify the downloaded archive:

```sh
sha256sum --check tuigreety-VERSION-ARCH.tar.gz.sha256
gh attestation verify tuigreety-VERSION-ARCH.tar.gz \
  --repo Tobiichi-Origuchi/tuigreety
```

Then extract it as an ordinary user and install the files explicitly:

```sh
tar --extract --gzip --file tuigreety-VERSION-ARCH.tar.gz
cd tuigreety-VERSION-ARCH

sudo install -Dm755 usr/bin/tuigreet /usr/bin/tuigreet
sudo install -Dm644 usr/share/man/man1/tuigreet.1 /usr/share/man/man1/tuigreet.1
sudo install -Dm644 usr/share/licenses/tuigreety/LICENSE \
  /usr/share/licenses/tuigreety/LICENSE
sudo install -Dm644 usr/share/doc/tuigreety/README.md \
  /usr/share/doc/tuigreety/README.md
sudo install -Dm644 usr/share/doc/tuigreety/INSTALL.md \
  /usr/share/doc/tuigreety/INSTALL.md
sudo install -Dm644 usr/lib/tmpfiles.d/tuigreet.conf \
  /usr/lib/tmpfiles.d/tuigreet.conf
```

Install the example configuration only when `/etc/tuigreet/config.toml` does
not already exist. Review and merge it manually on upgrades; never replace an
administrator's active login-manager configuration without checking it.

```sh
if [ ! -e /etc/tuigreet/config.toml ]; then
  sudo install -Dm644 etc/tuigreet/config.toml /etc/tuigreet/config.toml
else
  echo '/etc/tuigreet/config.toml already exists; leaving it unchanged'
fi
```

Create `/var/cache/tuigreet` using your system's tmpfiles implementation, or
follow the cache setup documented in the README. Finally configure greetd to
run `/usr/bin/tuigreet` as the `greeter` user.
