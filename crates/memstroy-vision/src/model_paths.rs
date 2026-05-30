//! Resolve bundled ONNX model paths for dev trees and installed bundles.
//!
//! Client installers ship `models/u2netp.onnx` next to `bin/` (see
//! `scripts/package-client.ps1`). Development trees keep the same file
//! under `assets/models/` at the workspace root.

use std::path::{Path, PathBuf};

const U2NETP_NAME: &str = "u2netp.onnx";

/// Preferred path to the U²-Netp ONNX weights.
///
/// Search order:
/// 1. `{install_root}/models/u2netp.onnx` — `{exe}/../models/` when the
///    binary lives in `bin/` (Windows / Linux client bundle).
/// 2. `assets/models/u2netp.onnx` — workspace dev layout (`cargo run`).
/// 3. `{exe_dir}/models/u2netp.onnx` — fallback if the model is colocated
///    with the executable.
///
/// When nothing exists on disk, returns the dev-tree path so error
/// messages stay actionable.
pub fn resolve_u2netp_model_path() -> PathBuf {
    for candidate in u2netp_candidates() {
        if candidate.exists() {
            return candidate;
        }
    }
    dev_u2netp_path()
}

/// All locations checked by [`resolve_u2netp_model_path`], in order.
pub fn u2netp_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(bin_dir) = exe.parent() {
            if let Some(install_root) = bin_dir.parent() {
                out.push(install_root.join("models").join(U2NETP_NAME));
            }
            out.push(bin_dir.join("models").join(U2NETP_NAME));
        }
    }
    out.push(dev_u2netp_path());
    out
}

pub fn dev_u2netp_path() -> PathBuf {
    PathBuf::from("assets/models").join(U2NETP_NAME)
}

/// Human-readable hint when the model file is missing.
pub fn u2netp_missing_message(tried: &Path) -> String {
    format!(
        "U²-Netp model not found at {}. Expected assets/models/u2netp.onnx in dev \
         or models/u2netp.onnx next to the installed bin/ directory.",
        tried.display()
    )
}
