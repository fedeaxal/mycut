# vcpkg's own x64-windows triplet, copied rather than used from the built-in
# set for one line: `VCPKG_BUILD_TYPE release`. Nothing this repository
# builds links a debug FFmpeg, and without it vcpkg builds every port twice
# — the Debug pass the release job's log shows as "Building ffmpeg ...
# x64-windows-debug" is about half of what a cache miss costs, for a tree
# nothing reads. An overlay with the built-in triplet's own name is how
# vcpkg is told to use this one in its place.
#
# The rest is the built-in triplet verbatim, so the two stay comparable.
set(VCPKG_TARGET_ARCHITECTURE x64)
set(VCPKG_CRT_LINKAGE dynamic)
set(VCPKG_LIBRARY_LINKAGE dynamic)
set(VCPKG_BUILD_TYPE release)
