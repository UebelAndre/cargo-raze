use std::path::PathBuf;
use structopt::StructOpt;
use anyhow::{anyhow, Result};
#[derive(StructOpt, Debug)]
#[structopt(name = "crate_uinverse_resolver")]
struct Opt {
    #[structopt(short, long)]
    repo_name: String,

    #[structopt(short, long, parse(from_os_str))]
    repo_path: PathBuf,

    #[structopt(short, long, parse(from_os_str))]
    manifest: Option<PathBuf>,

    #[structopt(short, long, parse(from_os_str))]
    lock_file: Option<PathBuf>,

    #[structopt(short, long, parse(from_os_str))]
    cargo_manifests: Option<Vec<PathBuf>>,

    #[structopt(short, long, parse(from_os_str))]
    carg_lock_file: Option<PathBuf>,

    #[structopt(short, long)]
    generate_lockfile: bool,
}

fn main() -> Result<()> {

    // Parse arguments
    let opt = Opt::from_args();

    let render_context = match detect_genmode(&opt)? {
        GenMode::Bazel => {
            // If there is a lockfile, 
            if opt.lock_file.is_some() {

            }
        },
        GenMode::BazelCargo => {

        },
        GenMode::Cargo => {
            
        },
    };

    // Render context

    // Write outputs

    Ok(())
}

enum GenMode {
    Bazel,
    BazelCargo,
    Cargo,
}

fn detect_genmode(opt: &Opt) -> Result<GenMode> {
    if opt.lock_file.is_some() {
        if opt.manifest.is_none() {
            return Err(anyhow!("Manifest info must be provided"));
        }

        if opt.carg_lock_file.is_some() {
            return Err(anyhow!("Bazel lockfiles and Cargo lockfiles are not compatible. Please specify only 1"));
        }

        if opt.cargo_manifests.is_some() {
            return Ok(GenMode::BazelCargo);
        }
        
        return Ok(GenMode::Bazel);
    }

    if opt.carg_lock_file.is_some() {
        if opt.lock_file.is_some() || opt.manifest.is_some() {
            return Err(anyhow!("When a Cargo.lock file is provided, Bazel mechanisms must be omitted"));
        }

        if let Some(manifests) = opt.cargo_manifests {
            if manifests.len() != 1 {
                return Err(anyhow!("When providing a `Cargo.lock` file, exactly 1 manifest must provided"));
            }
        }

        return Ok(GenMode::Cargo);
    }

    if opt.cargo_manifests.is_some() {
        return Ok(GenMode::BazelCargo);
    }

    if opt.manifest.is_some() {
        return Ok(GenMode::Bazel);
    }

    Err(anyhow!("No supported genmode for given arguments: {:?}", opt))
}

