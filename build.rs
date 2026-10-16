fn main() {
    println!("cargo:rerun-if-changed=src/ffmpeg_codec.c");
    println!("cargo:rerun-if-changed=src/cuda_gl.c");
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
    // Linux: zero-copy presentation of CUDA-decoded frames in OpenGL. Header
    // only at build time; libcuda and GL are resolved at runtime.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux") {
        let headers = pkg_config::Config::new()
            .cargo_metadata(false)
            .probe("ffnvcodec")
            .expect("nv-codec-headers (ffnvcodec) are required on Linux");
        build.file("src/cuda_gl.c").includes(headers.include_paths);
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
}
