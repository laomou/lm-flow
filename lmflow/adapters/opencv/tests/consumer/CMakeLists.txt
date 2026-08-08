cmake_minimum_required(VERSION 3.18)
project(lmflow_opencv_consumer LANGUAGES CXX)

find_package(lmflow REQUIRED COMPONENTS opencv)

add_executable(opencv_sdk_smoke main.cc)
target_link_libraries(opencv_sdk_smoke PRIVATE lmflow::opencv)
