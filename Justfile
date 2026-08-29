#!/usr/bin/env just --justfile
name := 'xdg-desktop-portal-wlrix'

rootdir := ''
prefix := '/usr'

usrdir := absolute_path(clean(rootdir / prefix))
# Not `bin`. This is bus-activated and never run by hand, unlike every other wlRIX component --
# those are started *by name* off PATH, which is why they go in bin. `lib` rather than `libexec`
# because that is where xdg-desktop-portal's own backends live on the distributions wlRIX
# targets (Arch merged libexec into lib years ago).
libexecdir := usrdir / 'lib'
systemddir := usrdir / 'lib' / 'systemd' / 'user'
dbusdir := usrdir / 'share' / 'dbus-1' / 'services'
portaldir := usrdir / 'share' / 'xdg-desktop-portal'

bin-src := 'target' / 'release' / name
bin-dst := libexecdir / name

# Which interfaces this backend implements, for xdg-desktop-portal's backend discovery.
portal-src := 'data' / 'wlrix.portal'
portal-dst := portaldir / 'portals' / 'wlrix.portal'

# Which backend wlRIX prefers for which interface. See the file for why the name is lowercase.
conf-src := 'data' / 'wlrix-portals.conf'
conf-dst := portaldir / 'wlrix-portals.conf'

# The two activation files carry an absolute path to the binary, so they are templates with
# @LIBEXECDIR@ substituted at install time -- a staged or non-/usr install has to point at
# where the binary actually landed.
dbus-src := 'data' / 'org.freedesktop.impl.portal.desktop.wlrix.service.in'
dbus-dst := dbusdir / 'org.freedesktop.impl.portal.desktop.wlrix.service'

unit-src := 'data' / name + '.service.in'
unit-dst := systemddir / name + '.service'

# List available recipes.
default:
  @just --list

release:
  cargo build --release

lint:
  cargo clippy

test:
  cargo test

# Install the portal backend and the four files that make it discoverable.
#
# Deliberately does not build: this is normally run as root, and building as root leaves a
# target directory nobody can write to afterwards.
#
#     just release && sudo just install
[doc("Install the portal backend and its data files (build first; run as root)")]
install:
  #!/usr/bin/env bash
  set -euo pipefail
  if [ ! -x '{{bin-src}}' ]; then
      echo "no release build -- run 'just release' first" >&2
      exit 1
  fi
  install -Dm0755 '{{bin-src}}' '{{bin-dst}}'
  install -Dm0644 '{{portal-src}}' '{{portal-dst}}'
  install -Dm0644 '{{conf-src}}' '{{conf-dst}}'
  # DESTDIR must not leak into the paths the files themselves carry: a staged install is
  # assembled under rootdir but runs from prefix, so substitute the runtime location.
  runtime_libexecdir='{{ clean(prefix / "lib") }}'
  for pair in '{{dbus-src}}:{{dbus-dst}}' '{{unit-src}}:{{unit-dst}}'; do
      src="${pair%%:*}"; dst="${pair#*:}"
      install -d "$(dirname "$dst")"
      sed "s|@LIBEXECDIR@|${runtime_libexecdir}|g" "$src" > "$dst"
      chmod 0644 "$dst"
  done
  for f in '{{bin-dst}}' '{{portal-dst}}' '{{conf-dst}}' '{{dbus-dst}}' '{{unit-dst}}'; do
      echo "installed $f"
  done
  echo
  echo "Two helper programs are separate and must be on PATH:"
  echo "  wlrix-source-picker  (wlrix-apps)      asks which source to share"
  echo "  wlrix-screenshot     (wlrix-screenshot) takes the picture for Screenshot"
  echo "Without them, a request against that interface has no way to be answered."

[doc("Remove what install put down")]
uninstall:
  #!/usr/bin/env bash
  set -euo pipefail
  rm -f '{{bin-dst}}' '{{portal-dst}}' '{{conf-dst}}' '{{dbus-dst}}' '{{unit-dst}}'
  echo "removed the portal backend and its data files"

clean:
  cargo clean
