// SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
// SPDX-License-Identifier: MIT

//! Build tool to generate Rust wrappers for protocol buffers.

use std::io::Result;

fn main() -> Result<()> {
    prost_build::compile_protos(&["src/tables/feature.proto"], &["src/"])?;
    Ok(())
}
