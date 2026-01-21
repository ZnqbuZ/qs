fn main() -> Result<(), Box<dyn std::error::Error>> {
    let proto_files = [
        "src/proto/common.proto",
    ];

    for proto_file in proto_files {
        println!("cargo:rerun-if-changed={proto_file}");
    }

    let mut config = prost_build::Config::new();
    config
        .protoc_arg("--experimental_allow_proto3_optional")
        .type_attribute(".common", "#[derive(serde::Serialize, serde::Deserialize)]")
        .btree_map(["."])
        .skip_debug([".common.Ipv4Addr", ".common.Ipv6Addr"]);;

    config.compile_protos(&proto_files, &["src/proto/"])?;

    Ok(())
}