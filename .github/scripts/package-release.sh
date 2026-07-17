#!/usr/bin/env bash

set -euo pipefail

if [[ $# -ne 6 ]]; then
  echo "usage: $0 VERSION ARCH BINARY MAN_PAGE OUTPUT_DIR SOURCE_DATE_EPOCH" >&2
  exit 2
fi

version=$1
architecture=$2
binary=$3
man_page=$4
output=$5
source_date_epoch=$6

if [[ $version != tip ]] && [[ ! $version =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "invalid package version: $version" >&2
  exit 1
fi

case "$architecture" in
  aarch64 | armv7 | i686 | x86_64) ;;
  *)
    echo "unsupported package architecture: $architecture" >&2
    exit 1
    ;;
esac

if [[ ! $source_date_epoch =~ ^[0-9]+$ ]]; then
  echo "invalid source date epoch: $source_date_epoch" >&2
  exit 1
fi

for input in "$binary" "$man_page" contrib/tuigreet.toml packaging/aur/tuigreet.conf LICENSE README.md \
  contrib/release/INSTALL.md; do
  if [[ ! -f $input ]] || [[ -L $input ]]; then
    echo "missing or unsafe package input: $input" >&2
    exit 1
  fi
done

package="tuigreety-$version-$architecture"
root="$output/$package"

if [[ -e $root ]] || [[ -e $output/$package.tar.gz ]] || [[ -e $output/$package.tar.gz.sha256 ]]; then
  echo "release output already exists for $package" >&2
  exit 1
fi

install -Dm755 "$binary" "$root/usr/bin/tuigreet"
install -Dm644 contrib/tuigreet.toml "$root/etc/tuigreet/config.toml"
install -Dm644 packaging/aur/tuigreet.conf "$root/usr/lib/tmpfiles.d/tuigreet.conf"
install -Dm644 LICENSE "$root/usr/share/licenses/tuigreety/LICENSE"
install -Dm644 README.md "$root/usr/share/doc/tuigreety/README.md"
install -Dm644 contrib/release/INSTALL.md "$root/usr/share/doc/tuigreety/INSTALL.md"
install -Dm644 "$man_page" "$root/usr/share/man/man1/tuigreet.1"

tar --create --sort=name --owner=0 --group=0 --numeric-owner --mtime="@$source_date_epoch" \
  --directory "$output" "$package" | gzip --no-name > "$output/$package.tar.gz"
(
  cd "$output"
  sha256sum "$package.tar.gz" > "$package.tar.gz.sha256"
)
