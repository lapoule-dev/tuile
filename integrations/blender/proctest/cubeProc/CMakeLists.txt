# SPDX-License-Identifier: MIT OR Apache-2.0
cmake_minimum_required(VERSION 3.20)
project(cubeProc CXX)
set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

find_package(pxr REQUIRED)

add_library(cubeProc MODULE cubeProc.cpp)
set_target_properties(cubeProc PROPERTIES PREFIX "lib")
target_link_libraries(cubeProc PRIVATE hdGp hd sdf tf vt gf)

install(TARGETS cubeProc DESTINATION plugin)
install(FILES plugInfo.json DESTINATION plugin/resources)
