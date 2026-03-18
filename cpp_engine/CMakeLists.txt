cmake_minimum_required(VERSION 3.16)

project(gb_mahjong_rule_engine LANGUAGES CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_CXX_EXTENSIONS OFF)

find_package(Protobuf REQUIRED)
find_package(gRPC CONFIG QUIET)

if(NOT TARGET protobuf::libprotobuf)
  add_library(protobuf::libprotobuf UNKNOWN IMPORTED)
  set_target_properties(protobuf::libprotobuf PROPERTIES
    IMPORTED_LOCATION "${Protobuf_LIBRARIES}"
    INTERFACE_INCLUDE_DIRECTORIES "${Protobuf_INCLUDE_DIRS}"
  )
endif()

if(NOT TARGET gRPC::grpc++)
  find_path(GRPC_INCLUDE_DIR grpcpp/grpcpp.h REQUIRED)
  find_library(GRPCPP_LIBRARY NAMES grpc++ REQUIRED)

  add_library(gRPC::grpc++ UNKNOWN IMPORTED)
  set_target_properties(gRPC::grpc++ PROPERTIES
    IMPORTED_LOCATION "${GRPCPP_LIBRARY}"
    INTERFACE_INCLUDE_DIRECTORIES "${GRPC_INCLUDE_DIR}"
  )
endif()

if(TARGET protobuf::protoc)
  set(PROTOC_BIN "$<TARGET_FILE:protobuf::protoc>")
else()
  find_program(PROTOC_BIN protoc REQUIRED)
endif()

if(TARGET gRPC::grpc_cpp_plugin)
  set(GRPC_CPP_PLUGIN_BIN "$<TARGET_FILE:gRPC::grpc_cpp_plugin>")
else()
  find_program(GRPC_CPP_PLUGIN_BIN grpc_cpp_plugin REQUIRED)
endif()

set(PROTO_DIR "${CMAKE_CURRENT_SOURCE_DIR}/../proto")
set(PROTO_FILE "${PROTO_DIR}/internal_engine.proto")
set(GENERATED_DIR "${CMAKE_CURRENT_BINARY_DIR}/generated")
set(GB_MAHJONG_VENDOR_DIR "${CMAKE_CURRENT_SOURCE_DIR}/third_party/GB-Mahjong")
set(GB_MAHJONG_VENDOR_MAHJONG_DIR "${GB_MAHJONG_VENDOR_DIR}/mahjong")
set(GB_MAHJONG_VENDOR_CONSOLE_DIR "${GB_MAHJONG_VENDOR_DIR}/console")

file(MAKE_DIRECTORY "${GENERATED_DIR}")

set(GENERATED_PROTO_SRC "${GENERATED_DIR}/internal_engine.pb.cc")
set(GENERATED_PROTO_HDR "${GENERATED_DIR}/internal_engine.pb.h")
set(GENERATED_GRPC_SRC "${GENERATED_DIR}/internal_engine.grpc.pb.cc")
set(GENERATED_GRPC_HDR "${GENERATED_DIR}/internal_engine.grpc.pb.h")

add_custom_command(
  OUTPUT
    "${GENERATED_PROTO_SRC}"
    "${GENERATED_PROTO_HDR}"
    "${GENERATED_GRPC_SRC}"
    "${GENERATED_GRPC_HDR}"
  COMMAND
    "${PROTOC_BIN}"
  ARGS
    "--proto_path=${PROTO_DIR}"
    "--cpp_out=${GENERATED_DIR}"
    "--grpc_out=${GENERATED_DIR}"
    "--plugin=protoc-gen-grpc=${GRPC_CPP_PLUGIN_BIN}"
    "${PROTO_FILE}"
  DEPENDS
    "${PROTO_FILE}"
  VERBATIM
)

add_library(engine_proto STATIC
  "${GENERATED_PROTO_SRC}"
  "${GENERATED_GRPC_SRC}"
)

target_include_directories(engine_proto PUBLIC "${GENERATED_DIR}")
target_link_libraries(engine_proto PUBLIC protobuf::libprotobuf gRPC::grpc++)

add_library(gb_mahjong_vendor STATIC
  "${GB_MAHJONG_VENDOR_MAHJONG_DIR}/tile.cpp"
  "${GB_MAHJONG_VENDOR_MAHJONG_DIR}/pack.cpp"
  "${GB_MAHJONG_VENDOR_MAHJONG_DIR}/handtiles.cpp"
  "${GB_MAHJONG_VENDOR_MAHJONG_DIR}/fan.cpp"
  "${GB_MAHJONG_VENDOR_CONSOLE_DIR}/console.cpp"
  "${GB_MAHJONG_VENDOR_CONSOLE_DIR}/print.cpp"
)

target_include_directories(gb_mahjong_vendor PUBLIC
  "${GB_MAHJONG_VENDOR_MAHJONG_DIR}"
  "${GB_MAHJONG_VENDOR_CONSOLE_DIR}"
)

add_library(rule_engine_core STATIC
  src/gb_mahjong_adapter.cc
  src/rule_engine_service.cc
)

target_include_directories(rule_engine_core PUBLIC
  "${CMAKE_CURRENT_SOURCE_DIR}/src"
  "${GENERATED_DIR}"
)
target_link_libraries(rule_engine_core PUBLIC
  engine_proto
  gb_mahjong_vendor
  protobuf::libprotobuf
  gRPC::grpc++
)

add_executable(gb_mahjong_rule_engine_server
  src/main.cc
)

target_link_libraries(gb_mahjong_rule_engine_server PRIVATE rule_engine_core gRPC::grpc++)
