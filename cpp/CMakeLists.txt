cmake_minimum_required(VERSION 3.21)

project(bv_wire VERSION 0.1.0 LANGUAGES CXX)

include(CMakePackageConfigHelpers)
include(GNUInstallDirs)

option(BV_WIRE_CPP_BUILD_TESTS "Build bv-wire C++ tests" ${PROJECT_IS_TOP_LEVEL})

add_library(bv_wire INTERFACE)
add_library(bv_wire::bv_wire ALIAS bv_wire)

target_compile_features(bv_wire INTERFACE cxx_std_17)
target_include_directories(
  bv_wire
  INTERFACE
    $<BUILD_INTERFACE:${CMAKE_CURRENT_SOURCE_DIR}/include>
    $<INSTALL_INTERFACE:include>
)

if(WIN32)
  target_link_libraries(bv_wire INTERFACE ws2_32)
endif()

install(
  TARGETS bv_wire
  EXPORT bv_wireTargets
  INCLUDES DESTINATION ${CMAKE_INSTALL_INCLUDEDIR}
)
install(
  DIRECTORY include/
  DESTINATION ${CMAKE_INSTALL_INCLUDEDIR}
)
install(
  EXPORT bv_wireTargets
  NAMESPACE bv_wire::
  DESTINATION ${CMAKE_INSTALL_LIBDIR}/cmake/bv_wire
)
configure_package_config_file(
  cmake/bv_wireConfig.cmake.in
  ${CMAKE_CURRENT_BINARY_DIR}/bv_wireConfig.cmake
  INSTALL_DESTINATION ${CMAKE_INSTALL_LIBDIR}/cmake/bv_wire
)
write_basic_package_version_file(
  ${CMAKE_CURRENT_BINARY_DIR}/bv_wireConfigVersion.cmake
  VERSION ${PROJECT_VERSION}
  COMPATIBILITY SameMajorVersion
)
install(
  FILES
    ${CMAKE_CURRENT_BINARY_DIR}/bv_wireConfig.cmake
    ${CMAKE_CURRENT_BINARY_DIR}/bv_wireConfigVersion.cmake
  DESTINATION ${CMAKE_INSTALL_LIBDIR}/cmake/bv_wire
)

if(BV_WIRE_CPP_BUILD_TESTS)
  enable_testing()
  set(CMAKE_EXPORT_COMPILE_COMMANDS ON)

  add_executable(smoke tests/smoke.cpp)
  target_link_libraries(smoke PRIVATE bv_wire::bv_wire)
  add_test(NAME smoke COMMAND $<TARGET_FILE:smoke>)

  add_executable(example tests/example.cpp)
  target_link_libraries(example PRIVATE bv_wire::bv_wire)
  add_test(NAME example COMMAND $<TARGET_FILE:example>)
endif()
