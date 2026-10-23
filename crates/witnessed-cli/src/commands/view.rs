use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde_json::Value;
use witnessed_supervisor::Layout;

use super::bundle::{prepare_dir, write_dir};
use super::verify::Source;
use crate::bundle::assemble::{Edits, Include, assemble, write_bundle};
use crate::state::{Need, open_needing};

#[derive(clap::Args)]
pub struct ViewArgs {
    #[arg(
        default_value = "last",
        help = "Bundle directory or zip file, or a session id, a unique prefix of it, or `last`"
    )]
    pub target: String,
    #[arg(long, help = "Print the dashboard path instead of opening it in a browser")]
    pub no_open: bool,
}

pub(crate) fn view(layout: &Layout, args: &ViewArgs) -> Result<i32> {
    let as_dir = Path::new(&args.target);
    let looks_like_path = args.target.contains('/') || args.target.ends_with(".zip") || args.target.starts_with('.');
    if looks_like_path && !as_dir.exists() {
        bail!("{} does not exist; pass a bundle directory, a zip file or a session id", as_dir.display());
    }
    let index = if as_dir.join("manifest.json").is_file() {
        as_dir.join("index.html")
    } else if as_dir.is_file() {
        let mut src = Source::open(as_dir)?;
        let manifest: Value = serde_json::from_slice(&src.read("manifest.json").context("reading manifest.json")?)
            .context("manifest.json is not JSON")?;
        let Some(id) = manifest["session"]["id"].as_str().filter(|id| !id.is_empty() && !id.contains('/')) else {
            bail!("{} has no session id in its manifest", as_dir.display());
        };
        let dir = layout.view_dir(id);
        prepare_dir(&dir, true)?;
        fs::write(dir.join("index.html"), src.read("index.html").context("reading index.html")?)
            .context("writing index.html")?;
        dir.join("index.html")
    } else {
        let (store, s, _) = open_needing(layout, &args.target, Need::Finished("the dashboard"))?;
        let dir = layout.view_dir(&s.id);
        prepare_dir(&dir, true)?;
        let edits = Edits::default();
        let assembled = assemble(&store, &s, Include::ALL, &edits)?;
        let (files, _) = write_bundle(&store, &s, &dir.display().to_string(), assembled, &edits.patterns)?;
        write_dir(&dir, &files)?;
        dir.join("index.html")
    };
    if !index.is_file() {
        bail!("{} has no index.html", as_dir.display());
    }
    println!("{}", index.display());
    if !args.no_open {
        let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
        match std::process::Command::new(opener).arg(&index).status() {
            Ok(st) if st.success() => {}
            _ => eprintln!("witnessed: could not run {opener}; open the file above in a browser"),
        }
    }
    Ok(0)
}
