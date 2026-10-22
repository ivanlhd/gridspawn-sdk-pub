//! The checked-in header must equal what cbindgen generates from src/lib.rs.
//! Regenerate with: GS_UPDATE_HEADER=1 cargo test --test header

#[test]
fn the_header_is_up_to_date() {
    let dir = env!("CARGO_MANIFEST_DIR");
    let config =
        cbindgen::Config::from_file(format!("{dir}/cbindgen.toml")).expect("cbindgen.toml");
    let bindings = cbindgen::Builder::new()
        .with_crate(dir)
        .with_config(config)
        .generate()
        .expect("cbindgen");
    let mut generated = Vec::new();
    bindings.write(&mut generated);
    let path = format!("{dir}/include/gridspawn_server.h");
    if std::env::var_os("GS_UPDATE_HEADER").is_some() {
        std::fs::create_dir_all(format!("{dir}/include")).unwrap();
        std::fs::write(&path, &generated).unwrap();
    }
    let current = std::fs::read(&path).expect(
        "include/gridspawn_server.h is missing: run GS_UPDATE_HEADER=1 cargo test --test header",
    );
    assert!(
        current == generated,
        "include/gridspawn_server.h is stale: run GS_UPDATE_HEADER=1 cargo test --test header"
    );
}
