//! Pascal units, `uses` imports, implicit `Self` calls, and Delphi form
//! event handlers resolve through the existing compilation-unit, lexical,
//! and project resolution paths.

use std::{fmt::Write as _, fs};

use tempfile::tempdir;

use super::{
    CanonicalGenerationFacts, CapabilityReferenceQuery, DYNAMIC_DISPATCH_UNRESOLVED_PROVENANCE,
    EXACT_PROJECT_PROVENANCE, EXACT_SAME_FILE_PROVENANCE, EXTERNAL_REFERENCE_UNRESOLVED_PROVENANCE,
    IMPORT_BINDING_PROVENANCE, MODULE_IMPORT_PROVENANCE, ReferenceInput, ReferenceKind, SymbolKind,
    UNRESOLVED_IMPORT_PROVENANCE, build, build_capability_generation, capability_symbol,
};

/// Worker counts the native benchmark supports.
const WORKER_COUNTS: [usize; 5] = [1, 2, 4, 8, 16];
/// Canonical storage bound for a qualified name.
const MAXIMUM_QUALIFIED_NAME_BYTES: usize = 2_048;

const AUTH_UNIT: &str = r"unit UAuth;
interface
type
  TAuthService = class
  public
    function Validate(const AToken: string): Boolean;
    function Login(const AUser: string): string;
    class function Instance: TAuthService;
  end;
function Hash(Value: Integer): Integer;
implementation
function TAuthService.Validate(const AToken: string): Boolean;
begin
  Result := AToken <> '';
end;
function TAuthService.Login(const AUser: string): string;
begin
  if Validate(AUser) then
    Result := AUser;
end;
class function TAuthService.Instance: TAuthService;
begin
  Result := nil;
end;
function Hash(Value: Integer): Integer;
begin
  Result := Value;
end;
end.
";

const MAIN_UNIT: &str = r"unit UMain;
interface
uses
  SysUtils, UAuth;
type
  TfrmMain = class(TForm)
    btnLogin: TButton;
    procedure btnLoginClick(Sender: TObject);
    procedure FormCreate(Sender: TObject);
  end;
procedure Shadowed(UAuth: TMock);
implementation
procedure TfrmMain.btnLoginClick(Sender: TObject);
begin
  Hash(1);
  UAuth.Hash(2);
  SysUtils.FreeAndNil(Sender);
  FormCreate(Sender);
  UAuth.TAuthService.Instance;
end;
procedure Shadowed(UAuth: TMock);
begin
  UAuth.Hash(3);
end;
procedure TfrmMain.FormCreate(Sender: TObject);
begin
end;
end.
";

const MAIN_FORM: &str = r"object frmMain: TfrmMain
  OnCreate = FormCreate
  object btnLogin: TButton
    Caption = 'Login'
    OnClick = btnLoginClick
  end
end
";

fn fixtures() -> [(&'static str, &'static str); 6] {
    [
        ("src/UAuth.pas", AUTH_UNIT),
        ("src/UMain.pas", MAIN_UNIT),
        ("src/UMain.dfm", MAIN_FORM),
        (
            "first/UDup.pas",
            "unit UDup;\ninterface\nimplementation\nend.\n",
        ),
        (
            "second/UDup.pas",
            "unit UDup;\ninterface\nimplementation\nend.\n",
        ),
        (
            "src/UConsumer.pas",
            "unit UConsumer;\ninterface\nuses UDup;\nimplementation\nend.\n",
        ),
    ]
}

#[test]
fn pascal_uses_resolve_to_declared_units_and_ambiguous_units_fail_closed() {
    let facts = build_capability_generation(&fixtures(), false);
    let reversed = build_capability_generation(&fixtures(), true);
    assert_eq!(
        facts.digest(),
        reversed.digest(),
        "input order changed facts"
    );

    let auth_file = file_symbol(&facts, "src/UAuth.pas");
    let uses_auth = import_reference(&facts, "src/UMain.pas", "UAuth");
    assert_eq!(uses_auth.target_symbol_id.as_ref(), Some(&auth_file));
    assert_eq!(uses_auth.resolution_provenance, MODULE_IMPORT_PROVENANCE);

    let uses_runtime = import_reference(&facts, "src/UMain.pas", "SysUtils");
    assert!(uses_runtime.target_symbol_id.is_none());
    assert_eq!(
        uses_runtime.resolution_provenance,
        UNRESOLVED_IMPORT_PROVENANCE
    );

    let duplicate = import_reference(&facts, "src/UConsumer.pas", "UDup");
    assert!(
        duplicate.target_symbol_id.is_none(),
        "two files declare unit UDup; the import must not pick one"
    );
    assert_eq!(
        duplicate.resolution_provenance,
        UNRESOLVED_IMPORT_PROVENANCE
    );
}

#[test]
fn pascal_calls_resolve_through_units_implicit_self_and_project_names() {
    let facts = build_capability_generation(&fixtures(), false);
    let hash = capability_symbol(&facts, "src/UAuth.pas", "Hash");
    let click = capability_symbol(&facts, "src/UMain.pas", "TfrmMain::btnLoginClick");
    let create = capability_symbol(&facts, "src/UMain.pas", "TfrmMain::FormCreate");

    let unqualified =
        CapabilityReferenceQuery::new(&facts, click).named("Hash", ReferenceKind::Calls);
    assert_eq!(unqualified.target_symbol_id.as_ref(), Some(&hash.symbol_id));
    assert_eq!(unqualified.resolution_provenance, EXACT_PROJECT_PROVENANCE);

    let qualified =
        CapabilityReferenceQuery::new(&facts, click).named("UAuth.Hash", ReferenceKind::Calls);
    assert_eq!(qualified.target_symbol_id.as_ref(), Some(&hash.symbol_id));
    assert_eq!(qualified.resolution_provenance, IMPORT_BINDING_PROVENANCE);

    let runtime = CapabilityReferenceQuery::new(&facts, click)
        .named("SysUtils.FreeAndNil", ReferenceKind::Calls);
    assert!(runtime.target_symbol_id.is_none());
    assert_eq!(
        runtime.resolution_provenance,
        EXTERNAL_REFERENCE_UNRESOLVED_PROVENANCE
    );

    let sibling =
        CapabilityReferenceQuery::new(&facts, click).named("FormCreate", ReferenceKind::Calls);
    assert_eq!(sibling.target_symbol_id.as_ref(), Some(&create.symbol_id));
    assert_eq!(sibling.resolution_provenance, EXACT_SAME_FILE_PROVENANCE);

    let instance = capability_symbol(&facts, "src/UAuth.pas", "TAuthService::Instance");
    let member_path = CapabilityReferenceQuery::new(&facts, click)
        .named("UAuth.TAuthService.Instance", ReferenceKind::Calls);
    assert_eq!(
        member_path.target_symbol_id.as_ref(),
        Some(&instance.symbol_id),
        "a unit-qualified member path resolves through the unit"
    );
    assert_eq!(member_path.resolution_provenance, IMPORT_BINDING_PROVENANCE);

    let shadowed = capability_symbol(&facts, "src/UMain.pas", "Shadowed");
    let parameter =
        CapabilityReferenceQuery::new(&facts, shadowed).named("UAuth.Hash", ReferenceKind::Calls);
    assert!(
        parameter.target_symbol_id.is_none(),
        "a parameter named like a used unit is not the unit"
    );

    let login = capability_symbol(&facts, "src/UAuth.pas", "TAuthService::Login");
    let validate = capability_symbol(&facts, "src/UAuth.pas", "TAuthService::Validate");
    let member =
        CapabilityReferenceQuery::new(&facts, login).named("Validate", ReferenceKind::Calls);
    assert_eq!(member.target_symbol_id.as_ref(), Some(&validate.symbol_id));
    assert_eq!(validate.symbol_kind, SymbolKind::Method.as_str());
    assert!(
        !validate.declaration_only,
        "the implementation body is attached"
    );
}

#[test]
fn delphi_form_handlers_resolve_to_the_form_class_methods() {
    let facts = build_capability_generation(&fixtures(), false);
    let form = capability_symbol(&facts, "src/UMain.dfm", "frmMain");
    let button = capability_symbol(&facts, "src/UMain.dfm", "frmMain::btnLogin");
    assert_eq!(form.symbol_kind, SymbolKind::Component.as_str());
    let click = capability_symbol(&facts, "src/UMain.pas", "TfrmMain::btnLoginClick");
    let create = capability_symbol(&facts, "src/UMain.pas", "TfrmMain::FormCreate");

    let on_click = CapabilityReferenceQuery::new(&facts, button)
        .named("btnLoginClick", ReferenceKind::References);
    assert_eq!(on_click.target_symbol_id.as_ref(), Some(&click.symbol_id));
    let on_create =
        CapabilityReferenceQuery::new(&facts, form).named("FormCreate", ReferenceKind::References);
    assert_eq!(on_create.target_symbol_id.as_ref(), Some(&create.symbol_id));
}

const DECOY_UNIT: &str = r"unit UDecoy;
interface
procedure Shared;
procedure Run;
procedure Work;
implementation
procedure Shared;
begin
end;
procedure Run;
begin
end;
procedure Work;
begin
end;
end.
";

const SCOPED_UNIT: &str = r"unit UScoped;
interface
type
  TWorker = class
  public
    procedure Shared;
    procedure Step;
  end;
procedure Work;
implementation
procedure TWorker.Shared;
begin
end;
procedure TWorker.Step;
begin
  with Target do
    Shared;
  Items[0].Run;
end;
procedure Work;
begin
  Work;
end;
end.
";

#[test]
fn receiver_uncertain_pascal_calls_never_bind_to_unrelated_unit_globals() {
    let facts = build_capability_generation(
        &[
            ("src/UDecoy.pas", DECOY_UNIT),
            ("src/UScoped.pas", SCOPED_UNIT),
        ],
        false,
    );
    let step = capability_symbol(&facts, "src/UScoped.pas", "TWorker::Step");
    for name in ["Shared", "Run"] {
        let call = CapabilityReferenceQuery::new(&facts, step).named(name, ReferenceKind::Calls);
        assert!(
            call.target_symbol_id.is_none(),
            "{name} depends on a receiver and must not bind to UDecoy.{name}"
        );
        assert_eq!(
            call.resolution_provenance,
            DYNAMIC_DISPATCH_UNRESOLVED_PROVENANCE
        );
    }
    let work = capability_symbol(&facts, "src/UScoped.pas", "Work");
    let recursion = CapabilityReferenceQuery::new(&facts, work).named("Work", ReferenceKind::Calls);
    assert_eq!(
        recursion.target_symbol_id.as_ref(),
        Some(&work.symbol_id),
        "recursion binds to the routine itself, never to another unit's Work"
    );
    assert_eq!(recursion.resolution_provenance, EXACT_SAME_FILE_PROVENANCE);
}

#[test]
fn nested_routines_bind_their_enclosing_routine_in_the_same_file() {
    let facts = build_capability_generation(
        &[
            ("src/UDecoy.pas", DECOY_UNIT),
            (
                "src/Nested.dpr",
                "program Nested;\nprocedure Run;\n  procedure Inner;\n  begin\n    run;\n  end;\nbegin\n  Inner;\nend;\nbegin\n  Run;\nend.\n",
            ),
        ],
        false,
    );
    let run = capability_symbol(&facts, "src/Nested.dpr", "Run");
    let inner = capability_symbol(&facts, "src/Nested.dpr", "Run::Inner");
    let enclosing = CapabilityReferenceQuery::new(&facts, inner).named("run", ReferenceKind::Calls);
    assert_eq!(
        enclosing.target_symbol_id.as_ref(),
        Some(&run.symbol_id),
        "the enclosing routine binds by its declared spelling, not to UDecoy.Run"
    );
    assert_eq!(enclosing.resolution_provenance, EXACT_SAME_FILE_PROVENANCE);
}

const STRINGS_UNIT: &str = r"unit UStrings;
interface
function Format(const Value: string): string;
function Length(const Value: string): Integer;
procedure Inc(var Value: Integer);
function IntToStr(Value: Integer): string;
procedure FreeAndNil(var Value);
function Create: TObject;
implementation
function Format(const Value: string): string;
begin
  Result := Value;
end;
function Length(const Value: string): Integer;
begin
  Result := 0;
end;
procedure Inc(var Value: Integer);
begin
end;
function IntToStr(Value: Integer): string;
begin
  Result := '';
end;
procedure FreeAndNil(var Value);
begin
end;
function Create: TObject;
begin
  Result := nil;
end;
end.
";

const RUNTIME_CALLER_UNIT: &str = r"unit UOther;
interface
procedure Run(var S: string; var N: Integer);
implementation
procedure Run(var S: string; var N: Integer);
begin
  S := Format('%d');
  N := Length(S);
  Inc(N);
  inc(N);
  S := IntToStr(N);
  FreeAndNil(S);
  Create;
  System.SysUtils.Format(S);
end;
end.
";

const LOCAL_RUNTIME_UNIT: &str = r"unit ULocal;
interface
uses UStrings;
function Trim(const Value: string): string;
procedure Run;
implementation
function Trim(const Value: string): string;
begin
  Result := Value;
end;
procedure Run;
begin
  Trim('a');
  UStrings.Length('b');
end;
end.
";

#[test]
fn pascal_runtime_intrinsics_never_bind_to_project_homonyms() {
    let facts = build_capability_generation(
        &[
            ("src/UStrings.pas", STRINGS_UNIT),
            ("src/UOther.pas", RUNTIME_CALLER_UNIT),
            ("src/ULocal.pas", LOCAL_RUNTIME_UNIT),
        ],
        false,
    );
    let run = capability_symbol(&facts, "src/UOther.pas", "Run");
    for name in [
        "Format",
        "Length",
        "Inc",
        "inc",
        "IntToStr",
        "FreeAndNil",
        "Create",
        "System.SysUtils.Format",
    ] {
        let call = CapabilityReferenceQuery::new(&facts, run).named(name, ReferenceKind::Calls);
        assert_eq!(
            (
                call.target_symbol_id.as_ref(),
                call.resolution_provenance.as_str()
            ),
            (None, EXTERNAL_REFERENCE_UNRESOLVED_PROVENANCE),
            "{name} is a runtime intrinsic, not UStrings.{name}"
        );
    }

    let local = capability_symbol(&facts, "src/ULocal.pas", "Run");
    let local_trim = capability_symbol(&facts, "src/ULocal.pas", "Trim");
    let own = CapabilityReferenceQuery::new(&facts, local).named("Trim", ReferenceKind::Calls);
    assert_eq!(
        own.target_symbol_id.as_ref(),
        Some(&local_trim.symbol_id),
        "a same-file routine named like an intrinsic still binds"
    );
    assert_eq!(own.resolution_provenance, EXACT_SAME_FILE_PROVENANCE);
    let length = capability_symbol(&facts, "src/UStrings.pas", "Length");
    let qualified =
        CapabilityReferenceQuery::new(&facts, local).named("UStrings.Length", ReferenceKind::Calls);
    assert_eq!(
        qualified.target_symbol_id.as_ref(),
        Some(&length.symbol_id),
        "an explicit unit qualifier still binds through the import"
    );
    assert_eq!(qualified.resolution_provenance, IMPORT_BINDING_PROVENANCE);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deeply_nested_form_components_publish_bounded_names() {
    const DEPTH: usize = 70;
    let directory = tempdir().unwrap_or_else(|error| panic!("could not create fixture: {error}"));
    let mut form = String::new();
    for level in 0..DEPTH {
        writeln!(
            form,
            "{}object {:x<40}: TPanel",
            "  ".repeat(level),
            format!("Panel{level}_")
        )
        .unwrap_or_else(|error| panic!("could not build the form: {error}"));
    }
    for level in (0..DEPTH).rev() {
        writeln!(form, "{}end", "  ".repeat(level))
            .unwrap_or_else(|error| panic!("could not build the form: {error}"));
    }
    fs::write(directory.path().join("Deep.dfm"), form)
        .unwrap_or_else(|error| panic!("could not write the form: {error}"));
    let generation = build(directory.path(), 1).await;
    let components = generation
        .facts()
        .symbols()
        .iter()
        .filter(|symbol| symbol.symbol_kind == SymbolKind::Component.as_str())
        .collect::<Vec<_>>();
    assert_eq!(components.len(), DEPTH);
    assert!(
        components
            .iter()
            .all(|symbol| symbol.qualified_name.len() <= MAXIMUM_QUALIFIED_NAME_BYTES),
        "every stored component name fits the canonical bound"
    );
}

#[test]
fn form_handlers_bind_only_through_the_owning_unit() {
    let form_unit = "unit UForm;\ninterface\ntype\n  TfrmDup = class(TForm)\n    procedure DoClick(Sender: TObject);\n  end;\nimplementation\nprocedure TfrmDup.DoClick(Sender: TObject);\nbegin\nend;\nend.\n";
    let facts = build_capability_generation(
        &[
            ("first/UForm.pas", form_unit),
            ("second/UForm.pas", form_unit),
            (
                "first/UForm.dfm",
                "object frmDup: TfrmDup\n  OnClick = DoClick\nend\n",
            ),
            (
                "orphan/UOrphan.pas",
                "unit UOrphan;\ninterface\ntype\n  TfrmOrphan = class(TForm)\n  end;\nimplementation\nend.\n",
            ),
            (
                "orphan/UOrphan.dfm",
                "object frmOrphan: TfrmOrphan\n  OnClick = Missing\nend\n",
            ),
            (
                "decoy/UDecoyForm.pas",
                "unit UDecoyForm;\ninterface\ntype\n  TfrmOrphan = class(TForm)\n    procedure Missing(Sender: TObject);\n  end;\nimplementation\nprocedure TfrmOrphan.Missing(Sender: TObject);\nbegin\nend;\nend.\n",
            ),
        ],
        false,
    );
    let form = capability_symbol(&facts, "first/UForm.dfm", "frmDup");
    let owner = capability_symbol(&facts, "first/UForm.pas", "TfrmDup::DoClick");
    let handler =
        CapabilityReferenceQuery::new(&facts, form).named("DoClick", ReferenceKind::References);
    assert_eq!(
        handler.target_symbol_id.as_ref(),
        Some(&owner.symbol_id),
        "the form file binds to the unit beside it, not the same-named copy"
    );
    assert_eq!(handler.resolution_provenance, IMPORT_BINDING_PROVENANCE);

    let orphan = capability_symbol(&facts, "orphan/UOrphan.dfm", "frmOrphan");
    let missing =
        CapabilityReferenceQuery::new(&facts, orphan).named("Missing", ReferenceKind::References);
    assert!(
        missing.target_symbol_id.is_none(),
        "a handler the owning unit lacks never binds to another unit's class"
    );
    assert_eq!(missing.resolution_provenance, UNRESOLVED_IMPORT_PROVENANCE);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pascal_generations_are_identical_across_worker_counts() {
    let directory = tempdir().unwrap_or_else(|error| panic!("could not create fixture: {error}"));
    for (path, source) in fixtures() {
        let target = directory.path().join(path);
        fs::create_dir_all(
            target
                .parent()
                .unwrap_or_else(|| panic!("fixture had no parent: {path}")),
        )
        .unwrap_or_else(|error| panic!("could not create {path} parent: {error}"));
        fs::write(target, source).unwrap_or_else(|error| panic!("could not write {path}: {error}"));
    }
    let mut digests = Vec::new();
    for workers in WORKER_COUNTS {
        let generation = build(directory.path(), workers).await;
        digests.push(generation.facts().digest().clone());
    }
    assert!(
        digests.windows(2).all(|pair| pair[0] == pair[1]),
        "worker count changed the Pascal generation: {digests:?}"
    );
}

fn file_symbol(facts: &CanonicalGenerationFacts, path: &str) -> cartograph_domain::SymbolId {
    let file_id = &facts
        .files()
        .iter()
        .find(|file| file.normalized_path == path)
        .unwrap_or_else(|| panic!("missing file {path}"))
        .file_id;
    facts
        .symbols()
        .iter()
        .find(|symbol| {
            &symbol.file_id == file_id && symbol.symbol_kind == SymbolKind::File.as_str()
        })
        .map_or_else(
            || panic!("missing file symbol {path}"),
            |symbol| symbol.symbol_id.clone(),
        )
}

fn import_reference<'facts>(
    facts: &'facts CanonicalGenerationFacts,
    path: &str,
    unit: &str,
) -> &'facts ReferenceInput {
    let file_id = &facts
        .files()
        .iter()
        .find(|file| file.normalized_path == path)
        .unwrap_or_else(|| panic!("missing file {path}"))
        .file_id;
    facts
        .references()
        .iter()
        .find(|reference| {
            &reference.file_id == file_id
                && reference.reference_kind == ReferenceKind::Imports.as_str()
                && reference.reference_name == unit
        })
        .unwrap_or_else(|| panic!("missing import {unit} in {path}"))
}

const OVERLOADED_UNIT: &str = r"unit UOver;
interface
procedure Put(I: Integer); overload;
procedure Put(const S: string); overload;
function Format(const Value: string): string;
implementation
procedure Put(I: Integer);
begin
  Put(1);
  format('x');
end;
procedure Put(const S: string);
begin
end;
function Format(const Value: string): string;
begin
  Result := Value;
end;
procedure Outer;
  procedure Inner(I: Integer); overload;
  begin
    Inner(0);
  end;
  procedure Inner(const S: string); overload;
  begin
    Inner(1);
  end;
begin
  Inner(2);
end;
end.
";

#[test]
fn overloaded_recursion_never_binds_and_same_file_routines_bind_case_insensitively() {
    let facts = build_capability_generation(
        &[
            (
                "src/UGlobals.pas",
                "unit UGlobals;\ninterface\nprocedure Put;\nprocedure Inner;\nimplementation\nprocedure Put;\nbegin\nend;\nprocedure Inner;\nbegin\nend;\nend.\n",
            ),
            ("src/UOver.pas", OVERLOADED_UNIT),
        ],
        false,
    );
    let file_id = &facts
        .files()
        .iter()
        .find(|file| file.normalized_path == "src/UOver.pas")
        .unwrap_or_else(|| panic!("missing UOver.pas"))
        .file_id;
    let calls = |name: &str| {
        facts
            .references()
            .iter()
            .filter(|reference| {
                &reference.file_id == file_id
                    && reference.reference_name == name
                    && reference.reference_kind == ReferenceKind::Calls.as_str()
            })
            .collect::<Vec<_>>()
    };
    let overloaded = [calls("Put"), calls("Inner")].concat();
    assert_eq!(overloaded.len(), 4);
    for call in overloaded {
        assert_eq!(
            call.target_symbol_id, None,
            "a call that cannot pick an overload never binds one ({})",
            call.reference_name
        );
    }
    let format = capability_symbol(&facts, "src/UOver.pas", "Format");
    let lowercase = calls("format");
    assert_eq!(lowercase.len(), 1);
    assert_eq!(
        lowercase[0].target_symbol_id.as_ref(),
        Some(&format.symbol_id)
    );
    assert_eq!(
        lowercase[0].resolution_provenance,
        EXACT_SAME_FILE_PROVENANCE
    );
}
