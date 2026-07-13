project(CFE_BESOM_IO C)

add_cfe_app(besom_io fsw/src/besom_io_app.c)
target_include_directories(besom_io PUBLIC fsw/inc)
