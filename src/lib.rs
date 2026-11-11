//! Teamspace library: the app, kept in a lib so fuzz targets and the thin
//! `teamspace` binary can both link it. Everything is private except the entry
//! point and the opt-in `fuzzing` seam.

mod app;
mod cli;
mod data;
mod features;
#[cfg(feature = "fuzzing")]
#[doc(hidden)]
pub mod fuzz;
mod integrations;
mod shared;
#[cfg(test)]
mod testing;
mod ui;

pub use cli::main_entry;
