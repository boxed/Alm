//! The development server shared by `alm reactor` and `alm make --live`.
//!
//! Both commands serve compiled Elm over loopback and push updates when the
//! sources change; they differ only in what they route. The HTTP layer, the
//! pages, and the watch-rebuild-notify loop live here so there is one of each.

pub mod http;
pub mod live;
pub mod pages;
