cmake_minimum_required(VERSION 3.24)
project(FutureboardVisualizer LANGUAGES CXX C)

# Futureboard Visualizer: the lcdemu virtual LCD fed by Futureboard Studio's
# shared-memory visualizer link (crates/VisualizerLink). See README.md.

set(CMAKE_CXX_STANDARD 20)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

# SDL3: an installed package, else built statically from source. Offline
# builds point FetchContent at a local checkout:
#   -DFETCHCONTENT_SOURCE_DIR_SDL3=<path to SDL source>
find_package(SDL3 CONFIG QUIET)
if(NOT SDL3_FOUND)
    include(FetchContent)
    set(SDL_SHARED OFF CACHE BOOL "" FORCE)
    set(SDL_STATIC ON CACHE BOOL "" FORCE)
    set(SDL_TEST_LIBRARY OFF CACHE BOOL "" FORCE)
    FetchContent_Declare(SDL3
        GIT_REPOSITORY https://github.com/libsdl-org/SDL.git
        GIT_TAG        release-3.4.20
        GIT_SHALLOW    TRUE)
    FetchContent_MakeAvailable(SDL3)
endif()

function(fbvis_warnings target)
    if(MSVC)
        target_compile_options(${target} PRIVATE /W4 /utf-8)
        target_compile_definitions(${target} PRIVATE _CRT_SECURE_NO_WARNINGS)
    else()
        target_compile_options(${target} PRIVATE -Wall -Wextra)
    endif()
endfunction()

# lcdemu's virtual LCD
add_library(lcdcore STATIC
    lcd/Canvas.cpp
    lcd/LcdRenderer.cpp)
target_include_directories(lcdcore PUBLIC lcd)
target_link_libraries(lcdcore PUBLIC SDL3::SDL3)
fbvis_warnings(lcdcore)

# The link reader: no SDL, shared by the app and the probe.
add_library(fblink STATIC src/link/FutureboardLink.cpp)
target_include_directories(fblink PUBLIC src)
if(WIN32)
    target_compile_definitions(fblink PRIVATE WIN32_LEAN_AND_MEAN NOMINMAX)
elseif(NOT APPLE)
    target_link_libraries(fblink PUBLIC rt)
endif()
fbvis_warnings(fblink)

set(FBVIS_SOURCES
    src/main.cpp
    src/Analysis.cpp
    src/MeterViews.cpp
    src/mixviz/Dsp.cpp
    src/mixviz/Views.cpp)
if(WIN32)
    list(APPEND FBVIS_SOURCES src/mixviz/DesktopBar.cpp)
endif()

add_executable(FutureboardVisualizer WIN32 ${FBVIS_SOURCES})
target_link_libraries(FutureboardVisualizer PRIVATE lcdcore fblink)
if(WIN32)
    target_link_libraries(FutureboardVisualizer PRIVATE shell32 comctl32)
    target_compile_definitions(FutureboardVisualizer PRIVATE WIN32_LEAN_AND_MEAN NOMINMAX)
endif()
fbvis_warnings(FutureboardVisualizer)

add_executable(FutureboardVisualizerProbe src/probe.cpp)
target_link_libraries(FutureboardVisualizerProbe PRIVATE fblink)
fbvis_warnings(FutureboardVisualizerProbe)
