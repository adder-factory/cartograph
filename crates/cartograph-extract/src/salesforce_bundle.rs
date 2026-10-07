//! Exact bundle paths shared by extraction and resolution.

/// The role established by a conventional Salesforce bundle filename.
#[derive(Clone, Copy, Eq, PartialEq)]
pub enum SalesforceBundleKind {
    /// A bundle's component or application markup.
    AuraMarkup,
    /// The bundle's client controller object.
    AuraController,
    /// The bundle's helper object.
    AuraHelper,
    /// The bundle's renderer object.
    AuraRenderer,
    /// The bundle's component script.
    LwcScript,
    /// The bundle's HTML template.
    LwcTemplate,
}

/// A path-proven bundle, retaining its package directory and case-sensitive name.
#[derive(Clone, Copy)]
pub struct SalesforceBundle<'path> {
    /// Exact directory containing this bundle's files.
    pub directory: &'path str,
    /// Case-sensitive directory and component name.
    pub name: &'path str,
    /// Conventional role of the filename.
    pub kind: SalesforceBundleKind,
}

/// Recognize only `aura/name/name<Role>.js` and `lwc/name/name.<js|ts|html>`.
#[must_use]
pub fn salesforce_bundle(path: &str) -> Option<SalesforceBundle<'_>> {
    let (directory, filename) = path.rsplit_once('/')?;
    let (parent, name) = directory.rsplit_once('/')?;
    if name.is_empty()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return None;
    }
    let (stem, extension) = filename.rsplit_once('.')?;
    let suffix = stem.strip_prefix(name)?;
    let kind = match (parent.rsplit('/').next()?, suffix, extension) {
        ("aura", "", "cmp" | "app") => SalesforceBundleKind::AuraMarkup,
        ("aura", "Controller", "js") => SalesforceBundleKind::AuraController,
        ("aura", "Helper", "js") => SalesforceBundleKind::AuraHelper,
        ("aura", "Renderer", "js") => SalesforceBundleKind::AuraRenderer,
        ("lwc", "", "js" | "ts") => SalesforceBundleKind::LwcScript,
        ("lwc", "", "html") => SalesforceBundleKind::LwcTemplate,
        _ => return None,
    };
    Some(SalesforceBundle {
        directory,
        name,
        kind,
    })
}
