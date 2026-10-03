cmake_minimum_required(VERSION 3.13)
project(runner LANGUAGES CXX)

# Define the application target. To change its name, change BINARY_NAME in the
# top-level CMakeLists.txt, not the value here, or `flutter run` will no longer
# work.
#
# Any new source files that you add to the application should be added here.
add_executable(${BINARY_NAME}
  "main.cc"
  "my_application.cc"
  "${FLUTTER_MANAGED_DIR}/generated_plugin_registrant.cc"
)

# Apply the standard set of build settings. This can be removed for applications
# that need different build settings.
apply_standard_settings(${BINARY_NAME})

# Add preprocessor definitions for the application ID.
add_definitions(-DAPPLICATION_ID="${APPLICATION_ID}")

# Add dependency libraries. Add any application-specific dependencies here.
target_link_libraries(${BINARY_NAME} PRIVATE flutter)
target_link_libraries(${BINARY_NAME} PRIVATE PkgConfig::GTK)

target_include_directories(${BINARY_NAME} PRIVATE "${CMAKE_SOURCE_DIR}")



# Phase 7 bridge: install the Rust cdylib into the bundle's lib/ dir.
# install() — NOT POST_BUILD: POST_BUILD copies into the BUILD-tree exe
# dir, which the bundle-assembly install step never reads. This rule runs
# in the same install pass that assembles bundle/, landing the .so in
# bundle/lib/ — where the exe's $ORIGIN/lib rpath (Flutter's own .so
# resolution path) finds it. Three levels up = repo root (runner →
# linux → app), where the workspace target/ lives.
if(CMAKE_BUILD_TYPE STREQUAL "Debug")
  set(PARLEY_RUST_PROFILE_DIR debug)
else()
  # Release and Profile builds use the cargo release profile
  set(PARLEY_RUST_PROFILE_DIR release)
endif()
install(
  FILES "${CMAKE_CURRENT_SOURCE_DIR}/../../../target/${PARLEY_RUST_PROFILE_DIR}/libparley_flutter.so"
  DESTINATION "lib"
)
