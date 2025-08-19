/// To generate updated proto objects:
/// run `RUSTFLAGS="--cfg genproto" cargo build`
fn main() -> std::io::Result<()> {
    #[cfg(genproto)]
    generate_protos()?;
    Ok(())
}

#[cfg(genproto)]
fn generate_protos() -> std::io::Result<()> {
    tonic_build::configure()
        .build_server(false)
        .build_client(true)
        .out_dir("src/generated")
        .build_transport(true)
        .compile_protos(
            &[
                "proto/ark/v1/service.proto",
                "proto/ark/v1/wallet.proto",
                "proto/ark/v1/indexer.proto",
                "proto/ark/v1/types.proto",
                "proto/ark/v1/admin.proto",
            ],
            &["proto"],
        )?;

    Ok(())
}
