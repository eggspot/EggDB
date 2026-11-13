cmake_minimum_required(VERSION 3.22)
project(eggdb_jni C)

set(EGGDB_ROOT ${CMAKE_CURRENT_SOURCE_DIR}/../../../../..)

add_library(eggdb_capi SHARED IMPORTED)
set_target_properties(eggdb_capi PROPERTIES
    IMPORTED_LOCATION ${CMAKE_CURRENT_SOURCE_DIR}/../../../build/rust-libs/${ANDROID_ABI}/libeggdb_capi.so)

add_library(eggdb_jni SHARED ${EGGDB_ROOT}/drivers/kotlin/jni/eggdb_jni.c)
target_include_directories(eggdb_jni PRIVATE ${EGGDB_ROOT}/crates/eggdb-capi/include)
target_compile_options(eggdb_jni PRIVATE -Wall -Werror)
target_link_libraries(eggdb_jni eggdb_capi)
