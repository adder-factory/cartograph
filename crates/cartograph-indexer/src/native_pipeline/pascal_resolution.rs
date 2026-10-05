//! Pascal and Delphi runtime-library names.
//!
//! The RTL, VCL, FMX, and the common Delphi library units are never part of
//! the indexed project, yet their routines and types (`Format`, `Length`,
//! `Inc`, `FreeAndNil`, `TObject`, `TStringList`, …) are named unqualified in
//! almost every unit. A project routine that happens to share such a name must
//! not become their target through the project-wide fallback, so these names
//! resolve only through an exact same-file declaration or an explicit import
//! binding, and otherwise stay unresolved as external references. The lists
//! are the v1 contract's; Pascal identifiers are case-insensitive, so they
//! match regardless of spelling.
//!
//! A Pascal self-scope reference names the exact same-file target the
//! extractor proved by Pascal scope; when that exact lookup fails (an
//! overload the extractor could not tell apart) it never widens to another
//! lexical candidate of the same name.

use cartograph_domain::SourceLanguage;

use super::ReferenceDispatch;

/// Unit-scope prefixes of the Delphi runtime and component libraries.
const RUNTIME_UNIT_PREFIXES: [&str; 15] = [
    "System.",
    "Winapi.",
    "Vcl.",
    "Fmx.",
    "Data.",
    "Datasnap.",
    "Soap.",
    "Xml.",
    "Web.",
    "REST.",
    "FireDAC.",
    "IBX.",
    "IdHTTP",
    "IdTCP",
    "IdSSL",
];

/// Runtime units, intrinsic routines, and RTL types named without a unit.
const RUNTIME_NAMES: [&str; 87] = [
    "System",
    "SysUtils",
    "Classes",
    "Types",
    "Variants",
    "StrUtils",
    "Math",
    "DateUtils",
    "IOUtils",
    "Generics.Collections",
    "Generics.Defaults",
    "Rtti",
    "TypInfo",
    "SyncObjs",
    "RegularExpressions",
    "SysInit",
    "Windows",
    "Messages",
    "Graphics",
    "Controls",
    "Forms",
    "Dialogs",
    "StdCtrls",
    "ExtCtrls",
    "ComCtrls",
    "Menus",
    "ActnList",
    "WriteLn",
    "Write",
    "ReadLn",
    "Read",
    "Inc",
    "Dec",
    "Ord",
    "Chr",
    "Length",
    "SetLength",
    "High",
    "Low",
    "Assigned",
    "FreeAndNil",
    "Format",
    "IntToStr",
    "StrToInt",
    "FloatToStr",
    "StrToFloat",
    "Trim",
    "UpperCase",
    "LowerCase",
    "Pos",
    "Copy",
    "Delete",
    "Insert",
    "Now",
    "Date",
    "Time",
    "DateToStr",
    "StrToDate",
    "Raise",
    "Exit",
    "Break",
    "Continue",
    "Abort",
    "True",
    "False",
    "nil",
    "Self",
    "Result",
    "Create",
    "Destroy",
    "Free",
    "TObject",
    "TComponent",
    "TPersistent",
    "TInterfacedObject",
    "TList",
    "TStringList",
    "TStrings",
    "TStream",
    "TMemoryStream",
    "TFileStream",
    "Exception",
    "EAbort",
    "EConvertError",
    "EAccessViolation",
    "IInterface",
    "IUnknown",
];

/// Whether a Pascal reference names the runtime library rather than the
/// project: a runtime unit or intrinsic, or a name under a runtime unit scope.
pub(super) fn runtime_reference(language: &str, name: &str) -> bool {
    language == SourceLanguage::Pascal.as_str()
        && (RUNTIME_NAMES
            .iter()
            .any(|runtime| runtime.eq_ignore_ascii_case(name))
            || RUNTIME_UNIT_PREFIXES.iter().any(|prefix| {
                name.get(..prefix.len())
                    .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
            }))
}

/// Whether a Pascal self-scope reference resolves only by its exact same-file
/// qualified name.
pub(super) fn exact_scope_only(language: &str, dispatch: ReferenceDispatch) -> bool {
    language == SourceLanguage::Pascal.as_str() && dispatch == ReferenceDispatch::RustSelf
}
