# Sourced by the package wrapper after foreign LD_LIBRARY_PATH/LD_PRELOAD
# overrides have been cleared. NixOS owns its graphics stack; proprietary
# NVIDIA userspace must match the loaded kernel driver, so leave both alone.
if [ ! -d /run/opengl-driver/lib ] && [ ! -d /sys/module/nvidia ]; then
    # GLVND's GLX loader needs to find the vendor library as well as DRI drivers.
    # Only this package's Mesa directory is admitted, never the caller's path.
    export LD_LIBRARY_PATH="@mesa@/lib"
    if [ -z "${LIBGL_DRIVERS_PATH+x}" ]; then
        export LIBGL_DRIVERS_PATH="@mesa@/lib/dri"
    fi
    if [ -z "${__EGL_VENDOR_LIBRARY_FILENAMES+x}" ] && [ -z "${__EGL_VENDOR_LIBRARY_DIRS+x}" ]; then
        export __EGL_VENDOR_LIBRARY_FILENAMES="@mesa@/share/glvnd/egl_vendor.d/50_mesa.json"
    fi
fi
