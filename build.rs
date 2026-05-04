use clap::CommandFactory;
use std::path::PathBuf;

include!("src/cli.rs");

fn main() -> std::io::Result<()> {
    println!("cargo:rerun-if-changed=src/cli.rs");
    println!("cargo:rerun-if-changed=build.rs");

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let out = manifest_dir.join("man");
    std::fs::create_dir_all(&out)?;

    let cmd = Args::command().name("git-cleanup");
    let man = clap_mangen::Man::new(cmd);
    let mut buf = Vec::<u8>::new();
    man.render(&mut buf)?;
    std::fs::write(out.join("git-cleanup.1"), buf)?;

    Ok(())
}
