#!/bin/sh
# Build the hashterm .deb from an existing release build.
#   packaging/build-deb.sh            -> dist/hashterm_<version>_<arch>.deb
# Requires: dpkg-deb, fakeroot, gzip; run `cargo build --release` first.
set -eu
# pipefail isn't POSIX; enable it where the shell supports it so a failing
# stage in a pipe (e.g. the version sed) aborts instead of yielding "".
# shellcheck disable=SC3040
(set -o pipefail) 2>/dev/null && set -o pipefail
umask 022 # deterministic file modes regardless of the caller's umask

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
# Tight regex + validation: the version flows into the control file and the
# output filename, so reject anything but a Debian-ish version string.
version=$(sed -n 's/^version = "\([0-9A-Za-z.+~-]\{1,\}\)".*/\1/p' "$root/Cargo.toml" | head -1)
case $version in
    "" | *[!0-9A-Za-z.+~-]*)
        echo "refusing to build: bad version '$version'" >&2
        exit 1
        ;;
esac
arch=$(dpkg --print-architecture)
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
pkg="$stage/hashterm"

for bin in hashterm hashterm-ctl; do
    [ -x "$root/target/release/$bin" ] || {
        echo "missing target/release/$bin — run: cargo build --release" >&2
        exit 1
    }
done

install -Dm755 "$root/target/release/hashterm"     "$pkg/usr/bin/hashterm"
install -Dm755 "$root/target/release/hashterm-ctl" "$pkg/usr/bin/hashterm-ctl"
install -Dm644 "$root/assets/com.redasgard.Hashterm.desktop" \
    "$pkg/usr/share/applications/com.redasgard.Hashterm.desktop"
install -Dm644 "$root/assets/com.redasgard.Hashterm.svg" \
    "$pkg/usr/share/icons/hicolor/scalable/apps/com.redasgard.Hashterm.svg"
install -Dm644 "$root/packaging/hashterm.1" "$pkg/usr/share/man/man1/hashterm.1"
gzip -9n "$pkg/usr/share/man/man1/hashterm.1"

# No D-Bus activation service is installed on purpose: auto-activation would
# let any session-bus peer cold-start hashterm and drive its command line.
# Single-instance forwarding works through GApplication without it.

install -d "$pkg/usr/share/doc/hashterm"
cat > "$pkg/usr/share/doc/hashterm/copyright" <<EOF
Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/
Upstream-Name: hashterm
Source: https://redasgard.com

Files: *
Copyright: 2026 Red Asgard <yevhen@redasgard.com>
License: MIT
EOF
printf 'hashterm (%s-1) unstable; urgency=medium\n\n  * Initial release.\n\n -- Red Asgard <yevhen@redasgard.com>  Mon, 18 Aug 2026 00:00:00 +0000\n' \
    "$version" > "$pkg/usr/share/doc/hashterm/changelog.Debian"
gzip -9n "$pkg/usr/share/doc/hashterm/changelog.Debian"

# md5sums so `dpkg --verify hashterm` can detect post-install tampering.
install -d "$pkg/DEBIAN"
( cd "$pkg" && find . -type f ! -path './DEBIAN/*' -printf '%P\0' \
    | LC_ALL=C sort -z \
    | xargs -0 md5sum > DEBIAN/md5sums )
chmod 0644 "$pkg/DEBIAN/md5sums"

size=$(du -sk "$pkg" | cut -f1)
cat > "$pkg/DEBIAN/control" <<EOF
Package: hashterm
Version: $version-1
Section: x11
Priority: optional
Architecture: $arch
Installed-Size: $size
Maintainer: Red Asgard <yevhen@redasgard.com>
Depends: libgtk-4-1 (>= 4.18), libvte-2.91-gtk4-0 (>= 0.84), libgtk4-layer-shell0, libglib2.0-0t64 (>= 2.80), tmux
Recommends: xdg-desktop-portal, xclip | wl-clipboard | xsel
Description: tmux-native terminal with saved sessions and quake drop-down
 Frameless tabbed GTK4/VTE terminal. Every tab is a session on a dedicated
 tmux server, so terminals survive GUI crashes. Full session dump/restore
 (layout, cwds, programs, scrollback), a quake-style always-on-top drop-down
 on a global hotkey, and a 2D tab matrix: groups on one edge, the active
 group's terminals on the perpendicular one.
EOF

install -d "$root/dist"
fakeroot dpkg-deb --build "$pkg" "$root/dist/hashterm_${version}-1_${arch}.deb"
echo "built: dist/hashterm_${version}-1_${arch}.deb"
