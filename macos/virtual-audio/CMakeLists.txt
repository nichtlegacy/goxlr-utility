cmake_minimum_required(VERSION 3.24)
project(GoXLRVirtualAudio LANGUAGES CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_POSITION_INDEPENDENT_CODE ON)
set(BUILD_TESTING OFF CACHE BOOL "" FORCE)

include(FetchContent)
FetchContent_Declare(libASPL
    GIT_REPOSITORY https://github.com/gavv/libASPL.git
    GIT_TAG 633e0f70203edd87d320fc5a3cae901e1363aac5
)
FetchContent_MakeAvailable(libASPL)

set(DRIVER_UUID "9C89F8D2-3A8E-4CD7-9F58-F3D5D1B1327E")
set(DRIVER_ENTRYPOINT "GoXLRVirtualEntryPoint")
find_program(CODESIGN_EXECUTABLE codesign REQUIRED)

add_library(GoXLRVirtual MODULE Driver.cpp)
target_link_libraries(GoXLRVirtual PRIVATE libASPL "-framework CoreAudio" "-framework CoreFoundation")
set_target_properties(GoXLRVirtual PROPERTIES
    OUTPUT_NAME GoXLRVirtual
    PREFIX ""
    SUFFIX ""
    BUNDLE TRUE
    BUNDLE_EXTENSION driver
    MACOSX_BUNDLE TRUE
    MACOSX_BUNDLE_INFO_PLIST "${CMAKE_CURRENT_SOURCE_DIR}/Info.plist.in"
    MACOSX_BUNDLE_BUNDLE_NAME GoXLRVirtual
    MACOSX_BUNDLE_GUI_IDENTIFIER com.github.goxlr-on-linux.goxlr-virtual-audio
    MACOSX_BUNDLE_SHORT_VERSION_STRING 0.1.0
)

add_custom_command(TARGET GoXLRVirtual POST_BUILD
    COMMAND "${CMAKE_COMMAND}" -E make_directory "$<TARGET_BUNDLE_DIR:GoXLRVirtual>/Contents/Resources"
    COMMAND "${CMAKE_COMMAND}" -E copy "${libaspl_SOURCE_DIR}/LICENSE"
        "$<TARGET_BUNDLE_DIR:GoXLRVirtual>/Contents/Resources/libASPL-LICENSE.txt"
    COMMAND "${CODESIGN_EXECUTABLE}" --force --sign - --timestamp=none
        "$<TARGET_BUNDLE_DIR:GoXLRVirtual>"
)

enable_testing()
add_executable(GoXLRVirtualRingTest tests/StereoHistory.cpp)
target_include_directories(GoXLRVirtualRingTest PRIVATE "${CMAKE_CURRENT_SOURCE_DIR}")
add_test(NAME GoXLRVirtualRingTest COMMAND GoXLRVirtualRingTest)
