#!/bin/bash
#
# What `cargo run` runs FilmCraft through on macOS — see `.cargo/config.toml`
# at the repository root.
#
# The Reaction page's Browser Source draws with CEF, and on a Mac CEF is a
# framework only an application bundle carries, with Chromium's render, GPU
# and other processes as applications beside it — an executable run on its
# own finds none. This assembles the development bundle beside the executable
# cargo just built — `target/<profile>/FilmCraft.app`, a fraction of a second
# once the framework is in — and runs the executable inside it.
#
# Runs it, rather than asking the system to open the bundle: the process is
# still this terminal's child, so its output is here, Ctrl+C stops it, and a
# permission it is given — screen recording, the camera — is the terminal's,
# which stays given however often FilmCraft is rebuilt. Opened as an
# application it would be FilmCraft asking, and an ad-hoc-signed FilmCraft is
# a new application to the system every build.
#
# The bundle is only the shape CEF needs, on this machine, fast: the
# executable is cloned in rather than copied, the framework is copied once per
# CEF version and kept, and nothing is rewritten or signed. The release
# bundle — `packaging/macos/package.sh`, the DMG — does not carry the browser
# engine yet.
#
# `FILMCRAFT_RUN_APP_DRY_RUN=1` lays the bundle out and says what it would
# run, without running it.
#
# Anything else cargo runs — a test binary, an example, filmcraft-cli — is
# run as it is.

set -euo pipefail

executable=$1
shift
if [ "$(basename "$executable")" != "filmcraft" ]; then
    exec "$executable" "$@"
fi

here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
app="$(dirname "$executable")/FilmCraft.app"
contents="$app/Contents"
frameworks="$contents/Frameworks"
mkdir -p "$contents/MacOS" "$contents/Resources" "$frameworks"

# The executable under its bundle name (`CFBundleExecutable`): a clone — a
# file of its own sharing the original's blocks until either is written —
# where the disk can make one.
place() {
    rm -f "$2"
    if cp -c "$1" "$2" 2> /dev/null; then
        return
    fi
    cp "$1" "$2"
    chmod u+w "$2"
}
place "$executable" "$contents/MacOS/FilmCraft"

# The description of the bundle: the release template, with the version the
# executable itself reports.
version=$("$executable" --version 2> /dev/null | awk '{ print $2 }') || version=
[ -n "$version" ] || version=0.0.0
sed -e "s/@VERSION@/$version/g" \
    -e "s/@SHORT_VERSION@/${version%%-*}/g" \
    -e "s/@BUILD_SHA@/dev/g" \
    "$here/Info.plist.in" > "$contents/Info.plist"
plutil -lint "$contents/Info.plist" > /dev/null
printf 'APPL????' > "$contents/PkgInfo"
cp -f "$root/assets/app-icon/filmcraft.icns" "$contents/Resources/FilmCraft.icns"

# The browser engine, where this machine has one: CEF's framework for the
# version Cargo.lock names, out of CEF_PATH — copied once, marked with that
# version, and kept (it is hundreds of megabytes, and the same on every build
# of one version). Without one the bundle still runs, and a Browser Source
# says there is no engine when it is opened.
cef_version=$(sed -n '/^name = "cef-dll-sys"$/{n;s/^version = ".*+\(.*\)"$/\1/p;}' "$root/Cargo.lock")
framework=
if [ -n "${CEF_PATH:-}" ] && [ -n "$cef_version" ] && [ -d "$CEF_PATH/$cef_version" ]; then
    framework=$(find "$CEF_PATH/$cef_version" -maxdepth 2 -type d \
        -name "Chromium Embedded Framework.framework" | head -n 1)
fi
if [ -n "$framework" ]; then
    stamp="$app/.cef-version"
    if [ "$(cat "$stamp" 2> /dev/null)" != "$cef_version" ]; then
        rm -rf "$frameworks/Chromium Embedded Framework.framework"
        cp -Rc "$framework" "$frameworks/" 2> /dev/null || cp -R "$framework" "$frameworks/"
        echo "$cef_version" > "$stamp"
    fi
else
    echo "no browser engine: no CEF $cef_version under CEF_PATH (${CEF_PATH:-unset})" >&2
fi

# Chromium starts its helper processes as applications of their own beside
# the framework, named for this bundle's executable — and each runs FilmCraft
# itself, which knows it is one of them from its arguments (the first thing
# `main` does).
for role in "" " (GPU)" " (Renderer)" " (Plugin)" " (Alerts)"; do
    name="FilmCraft Helper$role"
    suffix=$(echo "$role" | tr -d ' ()' | tr '[:upper:]' '[:lower:]')
    helper="$frameworks/$name.app/Contents"
    mkdir -p "$helper/MacOS"
    place "$contents/MacOS/FilmCraft" "$helper/MacOS/$name"
    cat > "$helper/Info.plist" << PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleExecutable</key>
	<string>$name</string>
	<key>CFBundleIdentifier</key>
	<string>ai.storyteller.filmcraft.helper${suffix:+.$suffix}</string>
	<key>CFBundleName</key>
	<string>$name</string>
	<key>CFBundleInfoDictionaryVersion</key>
	<string>6.0</string>
	<key>CFBundlePackageType</key>
	<string>APPL</string>
	<key>CFBundleShortVersionString</key>
	<string>${version%%-*}</string>
	<key>CFBundleVersion</key>
	<string>${version%%-*}</string>
	<key>LSMinimumSystemVersion</key>
	<string>11.0</string>
	<key>LSUIElement</key>
	<true/>
</dict>
</plist>
PLIST
done

if [ "${FILMCRAFT_RUN_APP_DRY_RUN:-0}" = 1 ]; then
    echo "dry run: made $app; would run $contents/MacOS/FilmCraft $*"
    exit 0
fi

exec "$contents/MacOS/FilmCraft" "$@"
