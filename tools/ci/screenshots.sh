#!/usr/bin/env bash
# Run the installer's demo mode headlessly and save screenshots of each page to
# ./target-shots/. Use it to check the layout without a desktop.
#
#   tools/ci/screenshots.sh          # the whole scripted flow, including a wrong code
#   tools/ci/screenshots.sh welcome  # one page
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/../.."
mkdir -p target-shots
inside='cargo build -q -p icloud-installer && '
if [ "${1:-flow}" = "flow" ]; then
  inside+='ICLOUD_INSTALLER_AUTOPLAY=1 SHOTS="2.4:02-folder 1.2:03-account 1.8:03b-signing-in 2.4:05-install 5.0:06-done" xvfb-run -a -s "-screen 0 700x640x24" dbus-run-session -- bash /src/tools/ci/screenshot-inner.sh'
else
  inside+="SHOTS=\"3:$1\" xvfb-run -a -s \"-screen 0 700x640x24\" dbus-run-session -- bash /src/tools/ci/screenshot-inner.sh --page $1"
fi
tools/ci/run.sh bash -c "$inside"
echo "screenshots in $(pwd)/target-shots"
