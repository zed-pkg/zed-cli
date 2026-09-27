use std::fs;
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result, bail};

use super::{ReleasePlan, render_human};

fn escape(value: &str) -> String {
    return value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;");
}

pub(super) fn write_report(plan: &ReleasePlan, unchecked: &[String], output: &Path) -> Result<()> {
    match fs::symlink_metadata(output) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            bail!("refusing to write a release report through a symbolic link");
        }
        Ok(_) => bail!("refusing to overwrite existing report {}", output.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("inspect report destination"),
    }
    let parent = output
        .parent()
        .context("report destination must have a parent")?;
    fs::create_dir_all(parent).context("create report directory")?;
    let document = format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; base-uri 'none'; form-action 'none'\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>Release plan</title></head><body><h1>Release plan</h1>\
         <p>{}</p><pre>{}</pre><h2>Unchecked inputs</h2><pre>{}</pre></body></html>\n",
        escape(&plan.release_set),
        escape(&render_human(plan)),
        escape(&unchecked.join("\n")),
    );
    let staged = tempfile::NamedTempFile::new_in(parent).context("stage release report")?;
    staged
        .as_file()
        .write_all(document.as_bytes())
        .context("write release report")?;
    staged.as_file().sync_all().context("sync release report")?;
    // The preflight check provides a useful diagnostic; no-clobber publication
    // also refuses files or symlinks created after that check.
    staged
        .persist_noclobber(output)
        .map_err(|error| error.error)
        .context("publish release report without overwriting an existing path")?;
    return Ok(());
}
