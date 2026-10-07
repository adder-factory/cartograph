use super::*;
use crate::{
    NativeExtractor, SourceLimits,
    framework::{FrameworkBuilder, FrameworkInput, FrameworkRouteInput},
};
use cartograph_domain::{SourceLanguage, SymbolKind};
use std::{cell::Cell, collections::BTreeSet, fmt::Write};

#[test]
fn flat_angular_routes_build_ownership_once_and_charge_linear_registration_work()
-> Result<(), Box<dyn std::error::Error>> {
    const ROUTE_COUNT: usize = 2_000;
    const PATH_PREFIX: &str = "{path: '";
    let mut source = String::new();
    for position in 0..ROUTE_COUNT {
        writeln!(source, "class C{position} {{}}")?;
    }
    source.push_str("const routes: Routes = [");
    let mut sites = Vec::new();
    for position in 0..ROUTE_COUNT {
        let path = format!("r{position}");
        let start = source.len() + PATH_PREFIX.len();
        sites.push((path.clone(), start));
        write!(source, "{PATH_PREFIX}{path}', component: C{position}}},")?;
    }
    source.push_str("];");
    let snapshot = SourceSnapshot::from_bytes(
        "src/routes.ts",
        source.as_bytes(),
        SourceLimits::new(source.len())?,
    )?;
    let mut file = NativeExtractor::new(SourceLanguage::TypeScript)?
        .extract(&snapshot)
        .map_err(|error| format!("flat route extraction: {error}"))?;
    assert_eq!(
        file.symbols
            .iter()
            .filter(|symbol| {
                symbol.kind == SymbolKind::Route && symbol.qualified_name.contains("::angular::")
            })
            .count(),
        ROUTE_COUNT
    );
    // Recreate the pre-framework source facts, rather than admitting a second
    // complete route generation against the same file's output budget.
    file.symbols
        .retain(|symbol| symbol.kind != SymbolKind::Route);
    let source_ids = file
        .symbols
        .iter()
        .map(|symbol| symbol.id.clone())
        .collect::<BTreeSet<_>>();
    file.containments
        .retain(|edge| source_ids.contains(&edge.parent) && source_ids.contains(&edge.child));
    file.references.retain(|reference| {
        reference
            .owner
            .as_ref()
            .is_none_or(|owner| source_ids.contains(owner))
    });
    let polls = Cell::new(0_usize);
    let mut cancelled = || {
        polls.set(polls.get() + 1);
        false
    };
    let mut builder = FrameworkBuilder::new(FrameworkInput::new(&snapshot, file), &mut cancelled)
        .map_err(|error| format!("ownership construction: {error}"))?;
    assert!(builder.bridge.original_ownership.is_some());
    let work_before = builder.bridge.work;
    let polls_before = polls.get();
    builder.bridge.index_owners()?;
    assert_eq!(builder.bridge.work, work_before);
    assert_eq!(polls.get(), polls_before);
    for (path, start) in sites {
        builder
            .add_route(FrameworkRouteInput {
                method: "GET",
                path: &path,
                start,
                end: start + path.len(),
                command: false,
                handler: None,
            })
            .map_err(|error| format!("indexed registration: {error}"))?;
    }
    assert_eq!(work_before - builder.bridge.work, ROUTE_COUNT);
    assert_eq!(polls.get() - polls_before, ROUTE_COUNT);
    Ok(())
}
