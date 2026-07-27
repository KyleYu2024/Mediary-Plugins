// SPDX-License-Identifier: GPL-3.0-only

#[path = "pt_flow/mod.rs"]
mod pt_flow;

#[tokio::main]
async fn main() {
    if let Err(error) = pt_flow::run().await {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
