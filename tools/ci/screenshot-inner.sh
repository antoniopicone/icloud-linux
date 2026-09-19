#!/usr/bin/env bash
# Runs inside Xvfb (see screenshots.sh). SHOTS="delay:name ..." with relative delays in seconds.
export GDK_BACKEND=x11 GSK_RENDERER=cairo LIBGL_ALWAYS_SOFTWARE=1 NO_AT_BRIDGE=1 GTK_A11Y=none
/cargo-target/debug/icloud-installer --demo "$@" >/tmp/installer.log 2>&1 &
app=$!
for item in $SHOTS; do
  sleep "${item%%:*}"
  import -window root "/src/target-shots/${item##*:}.png"
done
kill "$app" 2>/dev/null
tail -3 /tmp/installer.log
