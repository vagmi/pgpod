//! Rootless Podman client for pgpod.
//!
//! This crate is the only place `podman_api` types appear. Everything it
//! exposes is a pgpod type, which is what makes the mitigation in
//! `adrs/00-project-setup.md` §1 real: `podman-api` targets libpod 4.3
//! while hosts run 5.7 (Ubuntu 26.04) through 6.x, and if the crate has to
//! be replaced, the blast radius is this directory.

mod client;
mod container;
mod error;
mod info;
mod socket;
mod volume;

pub use client::PodmanClient;
pub use container::{Container, ContainerProbe, ContainerSpec, Mount};
pub use error::{Error, Result};
pub use info::PodmanInfo;
pub use socket::{
    ENV_OVERRIDE as PODMAN_SOCKET_ENV, SocketSource, default_socket_path, discover, socket_uri,
};
pub use volume::VolumeInfo;
