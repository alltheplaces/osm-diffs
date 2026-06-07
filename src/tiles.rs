use crate::{TileLayer, make_progress_bar};
use anyhow::{Ok, Result};
use indicatif::MultiProgress;
use serde::Deserialize;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub fn render_tiles(
    layers: &[TileLayer],
    progress: &MultiProgress,
    workdir: &Path,
) -> Result<PathBuf> {
    let out_path = workdir.join("diffed-places.pmtiles");
    if out_path.exists() {
        return Ok(out_path);
    }
    let mut tmp_path = PathBuf::from(&out_path);
    tmp_path.add_extension("tmp");

    let progress_bar = make_progress_bar(progress, "tiles.render", 100, "percent");

    let mut cmd = make_tippecanoe_command(layers, workdir, &tmp_path)?;
    let mut child = cmd.stderr(Stdio::piped()).spawn()?;
    let stderr = child.stderr.take().expect("Failed to capture stderr");
    let stderr_reader = BufReader::new(stderr);
    for line in stderr_reader.lines() {
        let line = line?;
        if let Some(progress) = parse_progress(&line) {
            progress_bar.set_position((progress + 0.5) as u64);
        } else {
            eprintln!("{}", line);
        }
    }

    if !child.wait()?.success() {
        anyhow::bail!("tippecanoe failed");
    }

    std::fs::rename(&tmp_path, &out_path)?;
    progress_bar.finish();
    Ok(out_path)
}

fn make_tippecanoe_command(
    layers: &[TileLayer],
    workdir: &Path,
    out_path: &Path,
) -> Result<Command> {
    let mut cmd = Command::new("tippecanoe");
    // Clear all environment variables, so we don't leak secrets to an untrusted subprocess.
    cmd.env_clear()
        .env("PATH", env!("PATH"))
        .arg("--json-progress")
        .arg("--read-parallel")
        .arg("--force")
        .arg("--temporary-directory")
        .arg(std::path::absolute(workdir)?)
        .arg("-zg")
        .arg("-r")
        .arg("1.2")
        .arg("--drop-densest-as-needed")
        .arg("--extend-zooms-if-still-dropping");
    for layer in layers.iter() {
        // Suppress empty layers; Tippecanoe fails if any input file is empty.
        let metadata = std::fs::metadata(&layer.path)?;
        if metadata.len() > 0 {
            cmd.arg(format!(
                "--named-layer={}:{}",
                &layer.name,
                &layer.path.display()
            ));
        } else {
            eprintln!("dropping layer {:?}", layer);
        }
    }
    cmd.arg("--output").arg(out_path);
    Ok(cmd)
}

/// Structure of the JSON record that tippecanoe writes to stderr
/// when it gets invoked with --json-progress.
#[derive(Deserialize)]
struct Progress {
    progress: f64,
}

fn parse_progress(line: &str) -> Option<f64> {
    let p: Progress = serde_json::from_str(line).ok()?;
    if p.progress >= 0.0 && p.progress <= 100.0 {
        Some(p.progress)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_make_tippecanoe_command() {
        let layers = [
            // Empty input file, expected to be suppressed from arguments.
            make_test_tile_layer("Empty", "empty.jsonl"),
            make_test_tile_layer("Shops", "diffed-shops.jsonl"),
        ];
        let workdir = PathBuf::from("test-workdir");
        let out = PathBuf::from("test-output.pmtiles");
        let cmd = make_tippecanoe_command(&layers, &workdir, &out).expect("should not fail");
        assert_eq!(cmd.get_program(), "tippecanoe");
        let args: Vec<String> = cmd
            .get_args()
            .map(|s| s.to_string_lossy().into_owned())
            .collect();
        let shop_layer_arg = format!("--named-layer=Shops:{}", &layers[1].path.display());
        let workdir_abs = std::path::absolute(&workdir).expect("absolute path of workdir");
        assert_eq!(
            args,
            &[
                "--json-progress",
                "--read-parallel",
                "--force",
                "--temporary-directory",
                workdir_abs.to_str().expect("workdir.to_str"),
                "-zg",
                "-r",
                "1.2",
                "--drop-densest-as-needed",
                "--extend-zooms-if-still-dropping",
                &shop_layer_arg,
                "--output",
                "test-output.pmtiles"
            ]
        );
    }

    fn make_test_tile_layer(name: &str, filename: &str) -> TileLayer {
        let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        path.push("tests");
        path.push("test_data");
        path.push(filename);
        TileLayer {
            name: String::from(name),
            path,
        }
    }

    #[test]
    fn test_parse_progress() {
        assert_eq!(parse_progress("{\"progress\":23.5}"), Some(23.5));
        assert_eq!(parse_progress("{\"progress\":-23.5}"), None);
        assert_eq!(parse_progress("{\"progress\":123.4}"), None);
        assert_eq!(parse_progress("warning: test message"), None);
    }
}
