//! NuGet: package sources from `NuGet.Config`, the V3 protocol, versions, and the
//! package view of a whole solution (installed, updates, consolidation).

pub mod client;
pub mod config;
pub mod solution_packages;
pub mod version;

pub use client::{Fetch, NuGetClient, PackageInfo};
pub use config::{Credentials, PackageSource, sources_for};
