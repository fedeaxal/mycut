# vcpkg's own arm64-osx-dynamic triplet, with the macOS FFmpeg and what it
# links built for the oldest macOS filmcraft runs on. Without it each library
# is built for the runner's own SDK, and an executable built that way does not
# load on any macOS older than the runner, whatever it promises.
#
# Kept in step with apps/reaction's overlay of the same name: the two trees
# this repository builds against must agree on a deployment target, or the
# FFmpeg libraries the vendored `media` resolves are the wrong ones to ship.
#
# Release only: nothing this repository builds links a debug FFmpeg, and
# without this vcpkg builds every port twice — Debug is about half of the
# runner time a miss costs, for a tree the build never reads.
set(VCPKG_TARGET_ARCHITECTURE arm64)
set(VCPKG_CRT_LINKAGE dynamic)
set(VCPKG_LIBRARY_LINKAGE dynamic)
set(VCPKG_CMAKE_SYSTEM_NAME Darwin)
set(VCPKG_OSX_ARCHITECTURES arm64)
set(VCPKG_OSX_DEPLOYMENT_TARGET 13.0)
set(VCPKG_BUILD_TYPE release)
