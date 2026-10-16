fn main() {
    println!("cargo:rerun-if-changed=src/ffmpeg_codec.c");
    for file in [
        "src/cuda_gl.c",
        "src/vaapi_egl.c",
        "src/gl_nv12.c",
        "src/gl_nv12.h",
        "src/dmabuf_encode.c",
        "src/dmabuf_encode.h",
    ] {
        println!("cargo:rerun-if-changed={file}");
    }
    let linux = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux");
    let mut build = cc::Build::new();
    build
        .file("src/ffmpeg_codec.c")
        .opt_level(2)
        .flag_if_supported("-std=c11");
    for library in ["libavcodec", "libavutil", "libswscale"] {
        let library = pkg_config::Config::new()
            .cargo_metadata(false)
            .probe(library)
            .expect("FFmpeg development libraries are required");
        build.includes(library.include_paths);
    }
    // Linux: zero-copy presentation of CUDA- and VA-API-decoded frames in
    // OpenGL, and zero-copy DMA-BUF capture into NVENC / VA-API encoders.
    // libcuda, libEGL and GL are resolved at runtime; libva is FFmpeg's own
    // VA-API dependency.
    if linux {
        let headers = pkg_config::Config::new()
            .cargo_metadata(false)
            .probe("ffnvcodec")
            .expect("nv-codec-headers (ffnvcodec) are required on Linux");
        let va = pkg_config::Config::new()
            .cargo_metadata(false)
            .probe("libva")
            .expect("libva is required on Linux");
        build
            .files([
                "src/gl_nv12.c",
                "src/cuda_gl.c",
                "src/vaapi_egl.c",
                "src/dmabuf_encode.c",
            ])
            .includes(headers.include_paths)
            .includes(va.include_paths);
        println!("cargo:rustc-link-lib=dl");
    }
    build
        .warnings(true)
        .warnings_into_errors(true)
        .compile("teleport_ffmpeg");
    // The shim must precede its dynamic dependencies with --as-needed linkers.
    for library in ["libavcodec", "libavutil", "libswscale"] {
        pkg_config::Config::new()
            .probe(library)
            .expect("FFmpeg library disappeared");
    }
    if linux {
        // libva for the shim; GStreamer's DMA-BUF memory accessors for capture.
        for library in ["libva", "gstreamer-allocators-1.0"] {
            pkg_config::Config::new()
                .probe(library)
                .unwrap_or_else(|error| panic!("{library} is required on Linux: {error}"));
        }
    }
}
