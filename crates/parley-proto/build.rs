fn main() -> Result<(), Box<dyn std::error::Error>> {
    let protoc_path = protoc_bin_vendored::protoc_bin_path()?;
    unsafe { std::env::set_var("PROTOC", protoc_path.display().to_string()) };
    println!("cargo:rerun-if-changed=../../proto/");

    prost_build::Config::new().compile_protos(
        &[
            "../../proto/handshake.proto",
            "../../proto/auth.proto",
            "../../proto/session.proto",
        ],
        &["../../proto"],
    )?;

    Ok(())
}
