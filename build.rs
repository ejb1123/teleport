fn main() {
    println!("cargo:rerun-if-changed=src/ffmpeg_codec.c");
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
