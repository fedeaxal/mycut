# vcpkg's own x64-osx-dynamic triplet, with the deployment target pinned the
# way the arm64 one beside it is: built for the runner's SDK instead, FFmpeg
# would not load on any macOS older than the Intel runner this build runs on.
# Release only, for the reason the arm64 triplet gives.
set(VCPKG_TARGET_ARCHITECTURE x64)
set(VCPKG_CRT_LINKAGE dynamic)
set(VCPKG_LIBRARY_LINKAGE dynamic)
set(VCPKG_CMAKE_SYSTEM_NAME Darwin)
set(VCPKG_OSX_ARCHITECTURES x86_64)
set(VCPKG_OSX_DEPLOYMENT_TARGET 13.0)
set(VCPKG_BUILD_TYPE release)
