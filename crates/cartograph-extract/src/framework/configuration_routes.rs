//! Symfony route-directory admission and implicit route paths.

use super::{ConfigurationRoute, MAX_ROUTE_BYTES};

pub(super) fn is_route_file(path: &str) -> bool {
    let (directory, filename) = path.rsplit_once('/').unwrap_or(("", path));
    let Some((stem, extension)) = filename.rsplit_once('.') else {
        return false;
    };
    if !matches!(extension, "yml" | "yaml") {
        return false;
    }
    if stem == "routes" || stem == "routing" || stem.ends_with(".routing") {
        return true;
    }
    let directory = directory
        .strip_prefix("config/")
        .or_else(|| directory.rsplit_once("/config/").map(|(_, suffix)| suffix));
    match directory {
        Some(suffix) => suffix
            .split('/')
            .next()
            .is_some_and(|segment| segment.starts_with("routes")),
        None => directory_is_config(path) && stem.starts_with("routes"),
    }
}

fn directory_is_config(path: &str) -> bool {
    path.rsplit_once('/')
        .is_some_and(|(directory, _)| directory == "config" || directory.ends_with("/config"))
}

pub(super) fn with_default_path(mut route: ConfigurationRoute) -> Option<ConfigurationRoute> {
    if route.path.is_empty() && route.target.is_some() && route.key.len() < MAX_ROUTE_BYTES {
        route.path = format!("/{}", route.key);
    }
    (!route.key.is_empty() && !route.path.is_empty() && route.path.len() <= MAX_ROUTE_BYTES)
        .then_some(route)
}
