#!/usr/bin/env bash
# Downloads WDK and SDK headers/libs from NuGet and prepares them for a
# case-sensitive filesystem. Needed once before building the Windows driver.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WDKROOT="$ROOT/.wdk"
VERSION="${WDK_VERSION:-10.0.28000.2526}"
mkdir -p "$WDKROOT"
cd "$WDKROOT"

for pkg in microsoft.windows.wdk.x64 microsoft.windows.sdk.cpp microsoft.windows.sdk.cpp.x64; do
  if [[ ! -d "$pkg.d" ]]; then
    echo "downloading $pkg $VERSION"
    curl -sL --max-time 600 -o "$pkg.zip" \
      "https://api.nuget.org/v3-flatcontainer/$pkg/$VERSION/$pkg.$VERSION.nupkg"
    mkdir -p "$pkg.d"
    (cd "$pkg.d" && bsdtar -xf "../$pkg.zip")
  fi
done

echo "adding case-variant header symlinks"
python3 - <<'PY'
import os, re
roots = ["microsoft.windows.wdk.x64.d/c/Include", "microsoft.windows.sdk.cpp.d/c/Include"]
dirindex = {}
for root in roots:
    for dirpath, _dirs, files in os.walk(root):
        dirindex[dirpath] = {f.lower(): f for f in files}
inc_re = re.compile(rb'^\s*#\s*include\s*[<"]([^>"]+)[>"]', re.M)
made = 0
for root in roots:
    for dirpath, _dirs, files in os.walk(root):
        idx = dirindex.get(dirpath, {})
        for f in files:
            if not f.lower().endswith(('.h', '.hpp')):
                continue
            try:
                data = open(os.path.join(dirpath, f), 'rb').read()
            except OSError:
                continue
            for m in inc_re.finditer(data):
                name = os.path.basename(m.group(1).decode('utf-8', 'replace').replace('\\', '/'))
                if name in idx.values():
                    continue
                real = idx.get(name.lower())
                if real and real != name:
                    link = os.path.join(dirpath, name)
                    if not os.path.exists(link):
                        try:
                            os.symlink(real, link); idx[name.lower()] = real; made += 1
                        except OSError:
                            pass
print(f"case-variant symlinks: {made}")
PY
echo "WDK ready at $WDKROOT"
