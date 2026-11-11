#!/bin/sh
# Runs the page's headless swipe-to-dismiss and sound-cue tests with JavaScriptCore (ships with macOS).
# Usage: scripts/test-page.sh
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
jsc=/System/Library/Frameworks/JavaScriptCore.framework/Versions/A/Helpers/jsc
test_js="$root/crates/nearpushd/tests/page/swipe_test.js"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
python3 - "$root/crates/nearpushd/assets/index.html" "$test_js" "$tmp/run.js" <<'EOF'
import re, sys
page = re.search(r"<script>(.*)</script>", open(sys.argv[1]).read(), re.S).group(1)
test = open(sys.argv[2]).read()
open(sys.argv[3], "w").write(test.replace("/* PAGE */", page, 1))
EOF
out=$("$jsc" "$tmp/run.js" 2>&1) || true
printf '%s\n' "$out"
case "$out" in
  *"PASS page swipe-to-dismiss"*"PASS page sound cue"*) exit 0 ;;
  *) exit 1 ;;
esac
