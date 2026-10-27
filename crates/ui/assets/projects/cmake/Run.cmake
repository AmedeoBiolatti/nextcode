cmake_minimum_required(VERSION 3.20)

get_filename_component(project_root "${CMAKE_CURRENT_LIST_DIR}/.." ABSOLUTE)
set(build_dir "${project_root}/build")

execute_process(
    COMMAND "${CMAKE_COMMAND}" -S "${project_root}" -B "${build_dir}" -DCMAKE_BUILD_TYPE=Debug
    COMMAND_ERROR_IS_FATAL ANY
)
execute_process(
    COMMAND "${CMAKE_COMMAND}" --build "${build_dir}" --config Debug
    COMMAND_ERROR_IS_FATAL ANY
)

if(RUN_APP)
    set(executable "${build_dir}/bin/app")
    if(CMAKE_HOST_WIN32)
        string(APPEND executable ".exe")
    endif()
    execute_process(
        COMMAND "${executable}"
        WORKING_DIRECTORY "${project_root}"
        COMMAND_ERROR_IS_FATAL ANY
    )
endif()
