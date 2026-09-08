# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev

cmake_minimum_required(VERSION 3.20)
project(tuileHydra LANGUAGES CXX)

# OpenUSD's own headers require it, and mixing standards across the boundary is
# an ODR violation waiting to happen.
set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_POSITION_INDEPENDENT_CODE ON)

if(NOT CMAKE_BUILD_TYPE)
    set(CMAKE_BUILD_TYPE Release)
endif()

find_package(pxr CONFIG REQUIRED)
find_package(Threads REQUIRED)

# ------------------------------------------------------------------ rust ----
# Cargo decides for itself whether anything needs rebuilding, so this target is
# always considered out of date and the real staleness check is delegated. The
# alternative — globbing the crate's sources into DEPENDS — silently misses a
# changed dependency in the workspace.
set(TUILE_CARGO_PROFILE "release" CACHE STRING "cargo profile for tuile-hydra")
set(TUILE_WORKSPACE_ROOT "${CMAKE_CURRENT_SOURCE_DIR}/../.." CACHE PATH
    "root of the tuile cargo workspace")
set(TUILE_STATICLIB
    "${TUILE_WORKSPACE_ROOT}/target/${TUILE_CARGO_PROFILE}/libtuile_hydra.a")

if(TUILE_CARGO_PROFILE STREQUAL "release")
    set(_cargo_profile_flag --release)
else()
    set(_cargo_profile_flag "")
endif()

add_custom_target(tuile_hydra_rust ALL
    COMMAND cargo build ${_cargo_profile_flag} -p tuile-hydra
    WORKING_DIRECTORY "${TUILE_WORKSPACE_ROOT}"
    BYPRODUCTS "${TUILE_STATICLIB}"
    COMMENT "cargo build -p tuile-hydra (${TUILE_CARGO_PROFILE})"
    VERBATIM)

# ---------------------------------------------------------------- plugin ----
add_library(tuileHydra SHARED
    src/procedural.cpp
    src/proceduralPlugin.cpp
    src/resolver.cpp
    src/tiles.cpp
    src/view.cpp)

add_dependencies(tuileHydra tuile_hydra_rust)

target_include_directories(tuileHydra PRIVATE src)

target_link_libraries(tuileHydra PRIVATE
    ar hd hdGp hf gf plug pxOsd sdf tf vt
    ${CMAKE_DL_LIBS} Threads::Threads)

# A Rust staticlib carries its whole runtime, including symbols from crates that
# OpenUSD may also link. Exporting them would let the dynamic linker resolve
# someone else's call into our copy — the classic way a plugin corrupts its
# host. Nothing here is meant to be visible: the plugin is reached through
# OpenUSD's registries, never by symbol.
if(APPLE)
    # `-load_hidden` is per-archive and must precede the path it applies to,
    # which is why the staticlib is linked here rather than above.
    target_link_options(tuileHydra PRIVATE
        "LINKER:-load_hidden,${TUILE_STATICLIB}")
    # A link OPTION is not a link DEPENDENCY: without this, a fresh cargo
    # build leaves the dylib stale, and every render quietly runs yesterday's
    # Rust — measured, and maddening.
    set_target_properties(tuileHydra PROPERTIES LINK_DEPENDS "${TUILE_STATICLIB}")
    # The Rust side's TLS stack (native HTTP to ion and Bing) resolves against
    # the system frameworks; a staticlib carries the references, not the link.
    target_link_libraries(tuileHydra PRIVATE
        "-framework Security"
        "-framework CoreFoundation"
        "-framework SystemConfiguration")
else()
    target_link_libraries(tuileHydra PRIVATE "${TUILE_STATICLIB}")
    target_link_options(tuileHydra PRIVATE "LINKER:--exclude-libs,ALL")
endif()

# ----------------------------------------------------------------- probe ----
# The only way to exercise the C++ half without a renderer: cook N frames and
# report what each cook did. Off unless asked for, because it links the same
# heavy USD libraries as the plugin and most builds do not need it.
option(TUILE_BUILD_PROBE "build the frame probe" OFF)
if(TUILE_BUILD_PROBE)
    add_executable(frameProbe tests/frameProbe.cpp)
    add_dependencies(frameProbe tuileHydra)
    target_link_libraries(frameProbe PRIVATE
        hd hdGp usdImaging usd sdf tf gf vt
        ${CMAKE_DL_LIBS} Threads::Threads)
endif()

# OpenUSD's plugin discovery reads `LibraryPath` from plugInfo.json literally,
# so the file name below and the one configured into it must agree. The `lib`
# prefix CMake would add is dropped to match OpenUSD's own convention.
set(TUILE_PLUGIN_SUFFIX ".so")
if(APPLE)
    set(TUILE_PLUGIN_SUFFIX ".dylib")
elseif(WIN32)
    set(TUILE_PLUGIN_SUFFIX ".dll")
endif()
set_target_properties(tuileHydra PROPERTIES
    PREFIX ""
    SUFFIX "${TUILE_PLUGIN_SUFFIX}"
    LIBRARY_OUTPUT_DIRECTORY "${CMAKE_BINARY_DIR}/plugin/tuileHydra")

# --------------------------------------------------------------- plugInfo ---
# The layout OpenUSD expects:
#
#   plugin/tuileHydra/tuileHydra.so
#   plugin/tuileHydra/resources/plugInfo.json
#
# with PXR_PLUGINPATH_NAME pointing at `plugin/tuileHydra/resources`.
#
# The three paths below are NOT relative to the same thing, and that is the
# trap. `Root` is relative to the directory holding plugInfo.json, so ".."
# resolves to plugin/tuileHydra. `LibraryPath` is then relative to **Root**, not
# to plugInfo.json — so a library sitting beside Root is named plainly, and a
# "../" here looks one level too high. Getting it wrong loads nothing and
# reports `cannot open shared object file`, which reads like a missing build
# rather than a wrong path.
#
# OpenUSD's own plugins obey the same rule with a deeper tree: hdGp ships
# lib/libusd_hdGp.so beside lib/usd/hdGp/resources/plugInfo.json, with Root ".."
# and LibraryPath "../../libusd_hdGp.so" — two levels up from lib/usd/hdGp.
set(PLUG_INFO_LIBRARY_PATH "tuileHydra${TUILE_PLUGIN_SUFFIX}")
set(PLUG_INFO_RESOURCE_PATH "resources")
set(PLUG_INFO_ROOT "..")
configure_file(
    resources/plugInfo.json
    "${CMAKE_BINARY_DIR}/plugin/tuileHydra/resources/plugInfo.json"
    @ONLY)
