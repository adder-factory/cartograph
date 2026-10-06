//! Pascal/Delphi source extraction contracts ported from the v1 extraction suite.

mod dependency_ownership;

use std::fmt::Write as _;

use cartograph_domain::{FileParseStatus, ReferenceKind, SourceLanguage, SymbolKind, Visibility};
use cartograph_extract::{
    ExtractError, ExtractedFile, ExtractedReference, ExtractedSymbol, ImportBindingKind,
    NativeExtractor, RUST_SELF_RECEIVER_RESOLUTION_PREFIX, SourceLimits, SourceSnapshot,
};

const SOURCE_LIMIT: usize = 1024 * 1024;

#[test]
fn external_ancestor_members_prevent_exact_unit_homonym_resolution() {
    let extracted = extract(
        "src/Child.pas",
        r"unit Child;
interface
uses Base;
type
  TChild = class(TBase)
    procedure Run;
  end;
  TGrandchild = class(TChild)
    procedure RunAgain;
  end;
  TPlain = class
    procedure Run;
  end;
procedure Ping;
implementation
procedure Ping;
begin end;
procedure TChild.Run;
begin Ping; end;
procedure TGrandchild.RunAgain;
begin Ping; end;
procedure TPlain.Run;
begin Ping; end;
end.",
    );
    for name in ["TChild::Run", "TGrandchild::RunAgain"] {
        let run = member(&extracted, SymbolKind::Method, name);
        assert_eq!(
            call(&extracted, &run.id, "Ping").resolution_name,
            Some(scoped("Ping"))
        );
    }
    let plain = member(&extracted, SymbolKind::Method, "TPlain::Run");
    assert_eq!(
        call(&extracted, &plain.id, "Ping").resolution_name,
        Some(scoped("self::Ping"))
    );
}

const UAUTH: &str = r"unit UAuth;

interface

uses
  System.SysUtils,
  System.Classes;

type
  ITokenValidator = interface
    ['{11111111-1111-1111-1111-111111111111}']
    function Validate(const AToken: string): Boolean;
  end;

  TAuthService = class(TInterfacedObject, ITokenValidator)
  private
    FToken: string;
    FLoginCount: Integer;
    procedure IncLoginCount;
  protected
    function GetToken: string;
  public
    constructor Create;
    destructor Destroy; override;
    function Validate(const AToken: string): Boolean;
    function Login(const AUser, APass: string): string;
    property Token: string read GetToken;
    property LoginCount: Integer read FLoginCount;
  end;

implementation

constructor TAuthService.Create;
begin
  inherited Create;
  FToken := '';
  FLoginCount := 0;
end;

destructor TAuthService.Destroy;
begin
  FToken := '';
  inherited Destroy;
end;

procedure TAuthService.IncLoginCount;
begin
  Inc(FLoginCount);
end;

function TAuthService.GetToken: string;
begin
  Result := FToken;
end;

function TAuthService.Validate(const AToken: string): Boolean;
begin
  Result := AToken <> '';
end;

function TAuthService.Login(const AUser, APass: string): string;
begin
  IncLoginCount;
  if Validate(AUser + ':' + APass) then
  begin
    FToken := AUser;
    Result := 'ok';
  end
  else
    Result := '';
end;

end.
";

const UTYPES: &str = r"unit UTypes;

interface

uses
  System.SysUtils;

const
  C_MAX_RETRIES = 3;
  C_DEFAULT_NAME = 'Guest';

type
  TUserRole = (urAdmin, urEditor, urViewer);

  TPoint2D = record
    X: Double;
    Y: Double;
  end;

  TUserName = string;

  TUserInfo = class
  public
    type
      TAddress = record
        Street: string;
        City: string;
        Zip: string;
      end;
  private
    FName: TUserName;
    FRole: TUserRole;
    FAddress: TAddress;
  public
    constructor Create(const AName: TUserName; ARole: TUserRole);
    function GetDisplayName: string;
    class function CreateAdmin(const AName: TUserName): TUserInfo; static;
    property Name: TUserName read FName write FName;
    property Role: TUserRole read FRole;
    property Address: TAddress read FAddress write FAddress;
  end;

implementation

constructor TUserInfo.Create(const AName: TUserName; ARole: TUserRole);
begin
  FName := AName;
  FRole := ARole;
end;

function TUserInfo.GetDisplayName: string;
begin
  if FRole = urAdmin then
    Result := '[Admin] ' + FName
  else
    Result := FName;
end;

class function TUserInfo.CreateAdmin(const AName: TUserName): TUserInfo;
begin
  Result := TUserInfo.Create(AName, urAdmin);
end;

end.
";

#[test]
fn pascal_extensions_route_to_the_dedicated_family() {
    for path in [
        "src/UAuth.pas",
        "src/App.dpr",
        "src/Package.dpk",
        "src/App.lpr",
        "src/MainForm.dfm",
        "src/MainForm.fmx",
    ] {
        assert_eq!(
            SourceLanguage::for_normalized_path(path),
            Some(SourceLanguage::Pascal),
            "{path}"
        );
    }
    assert_eq!(
        format!(
            "{:?}",
            cartograph_extract::LanguageSpec::for_language(SourceLanguage::Pascal).strategy()
        ),
        "PascalFamily"
    );
}

#[test]
fn units_programs_and_nameless_programs_are_modules() {
    let unit = extract(
        "src/MyUnit.pas",
        "unit MyUnit;\ninterface\nimplementation\nend.",
    );
    let module = symbol(&unit, SymbolKind::Module, "MyUnit");
    assert_eq!(module.qualified_name, "MyUnit");
    assert!(module.export.exported, "a unit is importable");

    let program = extract("src/MyApp.dpr", "program MyApp;\nbegin\nend.");
    let module = symbol(&program, SymbolKind::Module, "MyApp");
    assert!(
        !module.export.exported,
        "a program cannot be used by a unit"
    );

    let nameless = extract("src/Console.dpr", "program;\nuses SysUtils;\nbegin\nend.");
    let _ = symbol(&nameless, SymbolKind::Module, "Console");
    assert_eq!(
        nameless
            .symbols
            .iter()
            .filter(|symbol| symbol.kind == SymbolKind::Module)
            .count(),
        1
    );
}

#[test]
fn uses_clauses_emit_imports_references_and_unit_bindings() {
    let extracted = extract(
        "src/Test.pas",
        "unit Test;\ninterface\nuses\n  System.SysUtils,\n  System.Classes;\nimplementation\nuses UAuth;\nend.",
    );
    let imports = names_of(&extracted, SymbolKind::Import);
    assert_eq!(
        imports,
        ["System.SysUtils", "System.Classes", "UAuth"],
        "each unit is one import symbol"
    );
    for unit in ["System.SysUtils", "System.Classes", "UAuth"] {
        assert!(
            extracted.references.iter().any(|reference| {
                reference.kind == ReferenceKind::Imports
                    && reference.name == unit
                    && reference.owner.is_none()
            }),
            "missing imports reference {unit}: {:?}",
            extracted.references
        );
        assert!(
            extracted.import_bindings.iter().any(|binding| {
                binding.kind == ImportBindingKind::Namespace
                    && binding.module_specifier == unit
                    && binding.imported_name == "*"
                    && binding.local_name == unit
            }),
            "missing unit binding {unit}: {:?}",
            extracted.import_bindings
        );
    }
    assert!(
        extracted
            .import_bindings
            .iter()
            .all(|binding| binding.local_name != "*"),
        "a wildcard local would make every reference ambiguous"
    );
}

#[test]
fn classes_records_interfaces_and_inheritance_are_typed() {
    let extracted = extract(
        "src/Test.pas",
        r"unit Test;
interface
type
  TFwd = class;
  TMyClass = class
  public
    procedure DoSomething;
  end;
  TChild = class(TParent)
  end;
  TService = class(TInterfacedObject, ILogger)
  end;
  TPoint = record
    X: Double;
    Y: Double;
  end;
  ILogger = interface(IInterface)
    procedure Log(const AMsg: string);
  end;
  TFwd = class(TObject)
  end;
implementation
end.",
    );
    for name in ["TMyClass", "TChild", "TService", "TPoint"] {
        let class = symbol(&extracted, SymbolKind::Class, name);
        assert_eq!(class.qualified_name, name, "a unit is not a qualifier");
    }
    assert_eq!(
        extracted
            .symbols
            .iter()
            .filter(|symbol| symbol.name == "TFwd")
            .count(),
        1,
        "a forward class declaration is not a second definition"
    );
    let logger = symbol(&extracted, SymbolKind::Interface, "ILogger");
    assert_eq!(
        reference_names(&extracted, ReferenceKind::Extends, &logger.id),
        ["IInterface"]
    );
    let child = symbol(&extracted, SymbolKind::Class, "TChild");
    assert_eq!(
        reference_names(&extracted, ReferenceKind::Extends, &child.id),
        ["TParent"]
    );
    let service = symbol(&extracted, SymbolKind::Class, "TService");
    assert_eq!(
        reference_names(&extracted, ReferenceKind::Extends, &service.id),
        ["TInterfacedObject"]
    );
    assert_eq!(
        reference_names(&extracted, ReferenceKind::Implements, &service.id),
        ["ILogger"]
    );
    let point = symbol(&extracted, SymbolKind::Class, "TPoint");
    for field in ["X", "Y"] {
        let field = symbol(&extracted, SymbolKind::Field, field);
        assert!(contains(&extracted, &point.id, &field.id));
        assert!(
            reference_names(&extracted, ReferenceKind::TypeOf, &field.id).is_empty(),
            "Double is a builtin type"
        );
    }
    let log = symbol(&extracted, SymbolKind::Method, "Log");
    assert_eq!(log.qualified_name, "ILogger::Log");
    assert!(log.implementation.declaration_only);
    assert!(
        !extracted
            .symbols
            .iter()
            .any(|symbol| symbol.kind == SymbolKind::Function),
        "no declaration in this unit is a free routine: {:?}",
        extracted.symbols
    );
}

#[test]
fn methods_fields_and_properties_carry_section_visibility() {
    let extracted = extract(
        "src/Test.pas",
        r"unit Test;
interface
type
  TMyClass = class
  private
    FValue: Integer;
    FLeft, FRight: TNode;
  strict protected
    procedure Hidden;
  public
    constructor Create;
    function GetValue: Integer;
    property Name: string read FName write FName;
  published
    property Value: Integer read FValue;
  end;
  THelper = class
  public
    class function Create: THelper; static;
  end;
implementation
end.",
    );
    let class = symbol(&extracted, SymbolKind::Class, "TMyClass");
    let methods = extracted
        .symbols
        .iter()
        .filter(|symbol| {
            symbol.kind == SymbolKind::Method && symbol.qualified_name.starts_with("TMyClass::")
        })
        .collect::<Vec<_>>();
    assert_eq!(methods.len(), 3, "{methods:?}");
    for name in ["Create", "GetValue"] {
        let method = member(&extracted, SymbolKind::Method, &format!("TMyClass::{name}"));
        assert_eq!(method.visibility, Some(Visibility::Public));
        assert!(method.export.exported);
        assert!(contains(&extracted, &class.id, &method.id));
    }
    let hidden = member(&extracted, SymbolKind::Method, "TMyClass::Hidden");
    assert_eq!(hidden.visibility, Some(Visibility::Protected));
    assert!(!hidden.export.exported);
    let value = member(&extracted, SymbolKind::Field, "TMyClass::FValue");
    assert_eq!(value.visibility, Some(Visibility::Private));
    assert!(!value.export.exported);
    for name in ["FLeft", "FRight"] {
        let field = member(&extracted, SymbolKind::Field, &format!("TMyClass::{name}"));
        assert_eq!(
            reference_names(&extracted, ReferenceKind::TypeOf, &field.id),
            ["TNode"]
        );
    }
    let name = member(&extracted, SymbolKind::Property, "TMyClass::Name");
    assert_eq!(name.visibility, Some(Visibility::Public));
    let published = member(&extracted, SymbolKind::Property, "TMyClass::Value");
    assert_eq!(published.visibility, Some(Visibility::Public));
    let accessor = extracted
        .references
        .iter()
        .find(|reference| {
            reference.owner.as_ref() == Some(&published.id)
                && reference.kind == ReferenceKind::References
        })
        .unwrap_or_else(|| panic!("property accessor reference missing"));
    assert_eq!(accessor.name, "FValue");
    assert_eq!(accessor.resolution_name, Some(scoped("TMyClass::FValue")));
    let get_value = member(&extracted, SymbolKind::Method, "TMyClass::GetValue");
    assert!(!get_value.execution.static_member);
    let factory = member(&extracted, SymbolKind::Method, "THelper::Create");
    assert!(factory.execution.static_member);
}

#[test]
fn enums_constants_and_type_aliases_never_store_literals() {
    let extracted = extract(
        "src/Test.pas",
        r"unit Test;
interface
const
  MAX_RETRIES = 3;
  APP_NAME = 'MyApp';
  DEFAULT_COLOR = clGreen;
type
  TColor = (clRed, clGreen, clBlue);
  TUserName = string;
  TNodeRef = TNode;
implementation
end.",
    );
    let color = symbol(&extracted, SymbolKind::Enum, "TColor");
    let members = extracted
        .symbols
        .iter()
        .filter(|symbol| symbol.kind == SymbolKind::EnumMember)
        .collect::<Vec<_>>();
    assert_eq!(
        members
            .iter()
            .map(|member| member.name.as_str())
            .collect::<Vec<_>>(),
        ["clRed", "clGreen", "clBlue"]
    );
    assert!(
        members
            .iter()
            .all(|member| contains(&extracted, &color.id, &member.id))
    );
    assert_eq!(
        names_of(&extracted, SymbolKind::Constant),
        ["MAX_RETRIES", "APP_NAME", "DEFAULT_COLOR"]
    );
    for name in ["MAX_RETRIES", "APP_NAME"] {
        assert_eq!(
            symbol(&extracted, SymbolKind::Constant, name).signature,
            None,
            "literal initializers are never retained"
        );
    }
    assert_eq!(
        symbol(&extracted, SymbolKind::Constant, "DEFAULT_COLOR")
            .signature
            .as_deref(),
        Some("= clGreen")
    );
    let _ = symbol(&extracted, SymbolKind::TypeAlias, "TUserName");
    let alias = symbol(&extracted, SymbolKind::TypeAlias, "TNodeRef");
    assert_eq!(
        reference_names(&extracted, ReferenceKind::TypeOf, &alias.id),
        ["TNode"]
    );
    let rendered = format!("{extracted:?}");
    assert!(!rendered.contains("MyApp"), "string literal leaked");
}

#[test]
fn implementation_bodies_attach_to_their_declarations() {
    let extracted = extract(
        "src/Test.pas",
        r"unit Test;
interface
type
  TObj = class
  public
    procedure DoWork;
    procedure Idle;
  end;
implementation
procedure TObj.DoWork;
begin
  WriteLn('hello');
end;
end.",
    );
    let class = symbol(&extracted, SymbolKind::Class, "TObj");
    let do_work = member(&extracted, SymbolKind::Method, "TObj::DoWork");
    assert!(contains(&extracted, &class.id, &do_work.id));
    assert!(!do_work.implementation.declaration_only);
    assert_eq!(
        (do_work.span.start_line(), do_work.span.end_line()),
        (10, 13),
        "a paired routine spans its implementation"
    );
    let idle = member(&extracted, SymbolKind::Method, "TObj::Idle");
    assert_eq!(
        (idle.span.start_line(), idle.span.end_line()),
        (7, 7),
        "an unimplemented routine stays at its declaration"
    );
    assert!(do_work.body_search_text.contains("WriteLn"));
    assert!(do_work.health.cyclomatic >= 1);
    assert_eq!(
        reference_names(&extracted, ReferenceKind::Calls, &do_work.id),
        ["WriteLn"]
    );
    assert!(
        member(&extracted, SymbolKind::Method, "TObj::Idle")
            .implementation
            .declaration_only
    );
    assert_eq!(
        extracted
            .symbols
            .iter()
            .filter(|symbol| symbol.name == "DoWork" || symbol.name == "TObj")
            .count(),
        2,
        "an implementation is not a second symbol: {:?}",
        extracted.symbols
    );
}

#[test]
fn uauth_fixture_matches_the_v1_contract() {
    let extracted = extract("src/UAuth.pas", UAUTH);
    assert_eq!(extracted.parse_status, FileParseStatus::Parsed);
    assert!(
        extracted.diagnostics.is_empty(),
        "{:?}",
        extracted.diagnostics
    );
    let _ = symbol(&extracted, SymbolKind::Module, "UAuth");
    assert_eq!(names_of(&extracted, SymbolKind::Import).len(), 2);
    let _ = symbol(&extracted, SymbolKind::Interface, "ITokenValidator");
    let service = symbol(&extracted, SymbolKind::Class, "TAuthService");
    let methods = names_of(&extracted, SymbolKind::Method);
    assert!(methods.len() >= 6, "{methods:?}");
    for name in ["Create", "Destroy", "Login", "IncLoginCount"] {
        assert!(methods.contains(&name), "{name} missing from {methods:?}");
    }
    let fields = extracted
        .symbols
        .iter()
        .filter(|symbol| symbol.kind == SymbolKind::Field)
        .collect::<Vec<_>>();
    assert_eq!(fields.len(), 2);
    assert!(
        fields
            .iter()
            .all(|field| field.visibility == Some(Visibility::Private))
    );
    assert_eq!(
        names_of(&extracted, SymbolKind::Property),
        ["Token", "LoginCount"]
    );
    assert_eq!(
        reference_names(&extracted, ReferenceKind::Extends, &service.id),
        ["TInterfacedObject"]
    );
    assert_eq!(
        reference_names(&extracted, ReferenceKind::Implements, &service.id),
        ["ITokenValidator"]
    );
    let calls = extracted
        .references
        .iter()
        .filter(|reference| reference.kind == ReferenceKind::Calls)
        .map(|reference| reference.name.as_str())
        .collect::<Vec<_>>();
    for name in ["Inc", "Validate", "IncLoginCount"] {
        assert!(calls.contains(&name), "{name} missing from {calls:?}");
    }
    assert!(
        !calls.contains(&"Create") && !calls.contains(&"Destroy"),
        "inherited calls target the ancestor and are not attributed: {calls:?}"
    );
    let login = member(&extracted, SymbolKind::Method, "TAuthService::Login");
    let validate = call(&extracted, &login.id, "Validate");
    assert_eq!(
        validate.resolution_name,
        Some(scoped("TAuthService::Validate")),
        "implicit Self binds to the declared member"
    );
    let bare = call(&extracted, &login.id, "IncLoginCount");
    assert_eq!(
        bare.resolution_name,
        Some(scoped("TAuthService::IncLoginCount"))
    );
    let increment = member(
        &extracted,
        SymbolKind::Method,
        "TAuthService::IncLoginCount",
    );
    assert_eq!(
        call(&extracted, &increment.id, "Inc").resolution_name,
        None,
        "with no same-file homonym, an external ancestor keeps ordinary name resolution"
    );
    let validator = member(&extracted, SymbolKind::Method, "ITokenValidator::Validate");
    assert!(validator.implementation.declaration_only);
    let typed = member(&extracted, SymbolKind::Method, "TAuthService::Validate");
    assert!(!typed.implementation.declaration_only);
    assert_eq!(
        reference_names(&extracted, ReferenceKind::Returns, &typed.id),
        Vec::<&str>::new(),
        "Boolean is a builtin"
    );
    assert_eq!(
        typed.signature.as_deref(),
        Some("(const AToken: string): Boolean")
    );
}

#[test]
fn utypes_fixture_matches_the_v1_contract() {
    let extracted = extract("src/UTypes.pas", UTYPES);
    assert_eq!(extracted.parse_status, FileParseStatus::Parsed);
    let role = symbol(&extracted, SymbolKind::Enum, "TUserRole");
    assert_eq!(role.qualified_name, "TUserRole");
    assert_eq!(
        names_of(&extracted, SymbolKind::EnumMember),
        ["urAdmin", "urEditor", "urViewer"]
    );
    assert_eq!(
        names_of(&extracted, SymbolKind::Constant),
        ["C_MAX_RETRIES", "C_DEFAULT_NAME"]
    );
    assert!(names_of(&extracted, SymbolKind::TypeAlias).contains(&"TUserName"));
    let classes = names_of(&extracted, SymbolKind::Class);
    for name in ["TPoint2D", "TUserInfo", "TAddress"] {
        assert!(classes.contains(&name), "{name} missing from {classes:?}");
    }
    let address = symbol(&extracted, SymbolKind::Class, "TAddress");
    assert_eq!(address.qualified_name, "TUserInfo::TAddress");
    assert_eq!(address.visibility, Some(Visibility::Public));
    let _ = member(&extracted, SymbolKind::Field, "TUserInfo::TAddress::Street");
    let fields = names_of(&extracted, SymbolKind::Field);
    for name in ["X", "Y"] {
        assert!(fields.contains(&name));
    }
    let admin = member(&extracted, SymbolKind::Method, "TUserInfo::CreateAdmin");
    assert!(admin.execution.static_member);
    assert_eq!(
        reference_names(&extracted, ReferenceKind::Returns, &admin.id),
        ["TUserInfo"]
    );
    assert_eq!(
        reference_names(&extracted, ReferenceKind::TypeOf, &admin.id),
        ["TUserName"]
    );
    let construct = call(&extracted, &admin.id, "TUserInfo.Create");
    assert_eq!(
        construct.resolution_name,
        Some(scoped("TUserInfo::Create")),
        "a type-qualified call names the in-file member"
    );
    let role_property = member(&extracted, SymbolKind::Property, "TUserInfo::Role");
    assert_eq!(
        reference_names(&extracted, ReferenceKind::TypeOf, &role_property.id),
        ["TUserRole"]
    );
}

#[test]
fn implicit_self_respects_shadowing_with_blocks_and_ancestors() {
    let extracted = extract(
        "src/Scope.pas",
        r"unit Scope;
interface
type
  TBase = class
  public
    procedure Shared;
  end;
  TChild = class(TBase)
  public
    procedure Run(Step: Integer);
    procedure Step;
    procedure Inner;
    procedure Local;
    procedure Missing;
  end;
implementation
procedure TBase.Shared;
begin
end;
procedure TChild.Run(Step: Integer);
var
  Local: TProc;
  procedure Inner;
  begin
    Shared;
  end;
begin
  Step;
  Inner;
  Local;
  Shared;
  Self.Missing;
  Self.Step;
  Unknown;
  with Other do
    Shared;
  Run(1);
end;
end.",
    );
    let run = member(&extracted, SymbolKind::Method, "TChild::Run");
    let resolutions = extracted
        .references
        .iter()
        .filter(|reference| {
            reference.owner.as_ref() == Some(&run.id) && reference.kind == ReferenceKind::Calls
        })
        .map(|reference| (reference.name.as_str(), reference.resolution_name.clone()))
        .collect::<Vec<_>>();
    assert_eq!(
        resolutions,
        [
            ("Step", Some(scoped("Step"))),
            ("Inner", Some(scoped("TChild::Run::Inner"))),
            ("Local", Some(scoped("Local"))),
            ("Shared", Some(scoped("TBase::Shared"))),
            ("Self.Missing", Some(scoped("TChild::Missing"))),
            ("Self.Step", Some(scoped("TChild::Step"))),
            ("Unknown", None),
            ("Shared", Some(scoped("Shared"))),
            ("Run", Some(scoped("TChild::Run"))),
        ],
        "nested routines and recursion bind in-file; parameters and locals are procedural values; \
         explicit Self bypasses locals; `with` bodies never widen to a global guess"
    );
    let inner = member(&extracted, SymbolKind::Function, "TChild::Run::Inner");
    assert!(contains(&extracted, &run.id, &inner.id));
    assert_eq!(
        call(&extracted, &inner.id, "Shared").resolution_name,
        Some(scoped("TBase::Shared")),
        "nested routines keep the enclosing method's Self"
    );
}

#[test]
fn overloads_pair_with_matching_implementations() {
    let extracted = extract(
        "src/Overloads.pas",
        r"unit Overloads;
interface
type
  TCodec = class
  public
    procedure Put(Value: Integer); overload;
    procedure Put(const Value: string); overload;
  end;
implementation
procedure TCodec.Put(const Value: string);
begin
  PutText(Value);
end;
procedure TCodec.Put(Value: Integer);
begin
  PutNumber(Value);
end;
end.",
    );
    let overloads = extracted
        .symbols
        .iter()
        .filter(|symbol| symbol.qualified_name == "TCodec::Put")
        .collect::<Vec<_>>();
    assert_eq!(overloads.len(), 2);
    assert_eq!(
        reference_names(&extracted, ReferenceKind::Calls, &overloads[0].id),
        ["PutNumber"]
    );
    assert_eq!(
        reference_names(&extracted, ReferenceKind::Calls, &overloads[1].id),
        ["PutText"]
    );
}

#[test]
fn overload_pairing_ignores_defaults_and_order_and_never_guesses() {
    let extracted = extract(
        "src/Codec.pas",
        r"unit Codec;
interface
type
  TCodec = class
  public
    procedure Put(Value: Integer; Width: Integer = 4); overload;
    procedure Put(const Value: string); overload;
    procedure Flush;
    function Size: Integer;
    procedure Write;
  end;
implementation
procedure TCodec.Put(const Value: string);
begin
  PutText(Value);
end;
procedure TCodec.Put(Value: Integer; Width: Integer);
begin
  PutNumber(Value);
  Put(Value);
end;
procedure TCodec.Flush;
begin
end;
function TCodec.Size;
begin
  Result := 0;
end;
procedure TCodec.Write(Extra: Integer);
begin
end;
end.",
    );
    let put = extracted
        .symbols
        .iter()
        .filter(|symbol| {
            symbol.qualified_name == "TCodec::Put" && symbol.kind == SymbolKind::Method
        })
        .collect::<Vec<_>>();
    assert_eq!(put.len(), 2);
    assert_eq!(
        reference_names(&extracted, ReferenceKind::Calls, &put[0].id),
        ["PutNumber", "Put"],
        "a default value omitted from the implementation still pairs"
    );
    assert_eq!(
        reference_names(&extracted, ReferenceKind::Calls, &put[1].id),
        ["PutText"]
    );
    assert_eq!(
        call(&extracted, &put[0].id, "Put").resolution_name,
        Some(scoped("Put")),
        "an overloaded member is a member, but not one exact target"
    );
    assert!(
        !member(&extracted, SymbolKind::Method, "TCodec::Flush")
            .implementation
            .declaration_only
    );
    assert!(
        !member(&extracted, SymbolKind::Method, "TCodec::Size")
            .implementation
            .declaration_only,
        "an abbreviated header pairs with its only declaration"
    );
    let writes = extracted
        .symbols
        .iter()
        .filter(|symbol| symbol.qualified_name == "TCodec::Write")
        .collect::<Vec<_>>();
    assert_eq!(
        writes.len(),
        2,
        "a mismatched header is not attached to a guess"
    );
    assert!(
        writes
            .iter()
            .any(|symbol| symbol.implementation.declaration_only)
    );
}

#[test]
fn nested_homonyms_and_computed_receivers_bind_by_pascal_scope() {
    let extracted = extract(
        "src/Scopes.pas",
        r"unit Scopes;
interface
type
  TBase = class
  strict private
    procedure Hidden;
  public
    procedure Visible;
  end;
  TChild = class(TBase)
  public
    procedure Run;
  end;
procedure Work;
procedure Outer;
implementation
procedure Work;
begin
end;
procedure Outer;
  procedure Work;
  begin
  end;
begin
  Work;
end;
procedure TBase.Hidden;
begin
end;
procedure TBase.Visible;
begin
end;
procedure TChild.Run;
begin
  Hidden;
  Visible;
  Items[0].Run;
  Build(1).Finish(2);
end;
end.",
    );
    let outer = symbol(&extracted, SymbolKind::Function, "Outer");
    assert_eq!(
        call(&extracted, &outer.id, "Work").resolution_name,
        Some(scoped("Outer::Work")),
        "a nested routine shadows the unit-level homonym"
    );
    let run = member(&extracted, SymbolKind::Method, "TChild::Run");
    assert_eq!(
        call(&extracted, &run.id, "Hidden").resolution_name,
        None,
        "an ancestor's strict private member is invisible to descendants"
    );
    assert_eq!(
        call(&extracted, &run.id, "Visible").resolution_name,
        Some(scoped("TBase::Visible"))
    );
    assert_eq!(
        call(&extracted, &run.id, "Run").resolution_name,
        Some(scoped("Run")),
        "a member of a computed receiver is not a global routine"
    );
    assert_eq!(call(&extracted, &run.id, "Build").resolution_name, None);
    assert_eq!(
        call(&extracted, &run.id, "Finish").resolution_name,
        Some(scoped("Finish"))
    );
}

#[test]
fn class_helpers_bind_self_to_the_helped_type_without_inheriting_it() {
    let extracted = extract(
        "src/Helpers.pas",
        r"unit Helpers;
interface
type
  TText = class
  public
    function Length: Integer;
  end;
  TTextHelper = class helper for TText
  public
    function IsEmpty: Boolean;
  end;
implementation
function TText.Length: Integer;
begin
  Result := 0;
end;
function TTextHelper.IsEmpty: Boolean;
begin
  Result := Length = 0;
  Length;
end;
end.",
    );
    let helper = symbol(&extracted, SymbolKind::Class, "TTextHelper");
    assert!(
        extracted.references.iter().all(|reference| {
            reference.owner.as_ref() != Some(&helper.id)
                || !matches!(
                    reference.kind,
                    ReferenceKind::Extends | ReferenceKind::Implements
                )
        }),
        "the helped type is not an ancestor"
    );
    let is_empty = member(&extracted, SymbolKind::Method, "TTextHelper::IsEmpty");
    assert_eq!(
        call(&extracted, &is_empty.id, "Length").resolution_name,
        Some(scoped("TText::Length")),
        "a helper's Self is the helped type"
    );
}

#[test]
fn pascal_literals_never_reach_signatures() {
    let extracted = extract(
        "src/Literals.pas",
        r"unit Literals;
interface
const
  MASK = $FF;
  ENABLED = True;
  MASK_COPY = MASK;
function Pad(const Value: string[$FF]): Integer;
function Join(const Left: string; Right: TPart): TPart;
implementation
end.",
    );
    assert_eq!(
        symbol(&extracted, SymbolKind::Constant, "MASK").signature,
        None
    );
    assert_eq!(
        symbol(&extracted, SymbolKind::Constant, "ENABLED").signature,
        None
    );
    assert_eq!(
        symbol(&extracted, SymbolKind::Constant, "MASK_COPY")
            .signature
            .as_deref(),
        Some("= MASK")
    );
    assert_eq!(
        symbol(&extracted, SymbolKind::Function, "Pad").signature,
        None
    );
    assert_eq!(
        symbol(&extracted, SymbolKind::Function, "Join")
            .signature
            .as_deref(),
        Some("(const Left: string; Right: TPart): TPart")
    );
    assert!(!format!("{extracted:?}").contains("$FF"));

    // `alias` is a keyword in this grammar; error recovery attaches the next
    // declaration's value to ENABLED, which must not become its signature.
    let recovered = extract(
        "src/Recovered.pas",
        "unit Recovered;\ninterface\nconst\n  ENABLED = True;\n  ALIAS = MASK;\nimplementation\nend.",
    );
    assert_eq!(
        symbol(&recovered, SymbolKind::Constant, "ENABLED").signature,
        None
    );
}

#[test]
fn package_and_project_files_degrade_without_inventing_units() {
    let package = extract(
        "src/Pkg.dpk",
        "package Pkg;\nrequires\n  rtl, vcl;\ncontains\n  UMain in 'UMain.pas';\nend.\n",
    );
    assert_eq!(package.parse_status, FileParseStatus::Partial);
    assert!(
        package
            .symbols
            .iter()
            .all(|symbol| symbol.kind != SymbolKind::Import),
        "the grammar has no package rule; nothing is guessed: {:?}",
        package.symbols
    );
    let project = extract(
        "src/App.dpr",
        "program App;\nuses\n  UMain in 'UMain.pas',\n  SysUtils;\nbegin\nend.\n",
    );
    assert_eq!(project.parse_status, FileParseStatus::Partial);
    assert_eq!(
        names_of(&project, SymbolKind::Import),
        ["UMain", "SysUtils"]
    );
    assert!(!format!("{project:?}").contains("UMain.pas"));
}

#[test]
fn routines_without_declarations_and_program_blocks_keep_their_calls() {
    let extracted = extract(
        "src/App.dpr",
        r"program App;
uses
  Unit1,
  SysUtils;
procedure Helper(A: TWidget);
begin
  Writeln(A);
end;
begin
  Helper(Twice(1));
  Obj.Run.Next(2);
  Exit;
end.",
    );
    let app = symbol(&extracted, SymbolKind::Module, "App");
    let helper = symbol(&extracted, SymbolKind::Function, "Helper");
    assert!(!helper.implementation.declaration_only);
    assert!(!helper.export.exported);
    assert_eq!(
        reference_names(&extracted, ReferenceKind::Calls, &helper.id),
        ["Writeln"]
    );
    assert_eq!(
        reference_names(&extracted, ReferenceKind::TypeOf, &helper.id),
        ["TWidget"]
    );
    assert_eq!(
        reference_names(&extracted, ReferenceKind::Calls, &app.id),
        ["Helper", "Twice", "Obj.Run.Next"],
        "main-block calls belong to the program; Exit is control flow"
    );
}

#[test]
fn interface_routines_pair_with_implementation_bodies() {
    let extracted = extract(
        "src/Impl.pas",
        r"unit Impl;
interface
procedure PublicProc(A: Integer);
implementation
procedure PrivateProc;
begin
  PublicProc(1);
end;
procedure PublicProc(A: Integer);
begin
  PrivateProc;
end;
initialization
  PublicProc(2);
end.",
    );
    let public = symbol(&extracted, SymbolKind::Function, "PublicProc");
    assert!(public.export.exported);
    assert!(!public.implementation.declaration_only);
    assert_eq!(
        extracted
            .symbols
            .iter()
            .filter(|symbol| symbol.name == "PublicProc")
            .count(),
        1
    );
    let private = symbol(&extracted, SymbolKind::Function, "PrivateProc");
    assert!(!private.export.exported);
    assert_eq!(
        reference_names(&extracted, ReferenceKind::Calls, &private.id),
        ["PublicProc"]
    );
    assert_eq!(
        reference_names(&extracted, ReferenceKind::Calls, &public.id),
        ["PrivateProc"]
    );
    let module = symbol(&extracted, SymbolKind::Module, "Impl");
    assert_eq!(
        reference_names(&extracted, ReferenceKind::Calls, &module.id),
        ["PublicProc"]
    );
}

#[test]
fn malformed_pascal_is_partial_but_keeps_recoverable_structure() {
    let extracted = extract(
        "src/Broken.pas",
        "unit Broken;\ninterface\ntype\n  TOk = class\n  end;\nimplementation\nprocedure Broken(; begin end;\nend.",
    );
    assert_eq!(extracted.parse_status, FileParseStatus::Partial);
    assert!(
        !extracted.diagnostics.is_empty(),
        "a recovered parse still reports its syntax errors"
    );
    let _ = symbol(&extracted, SymbolKind::Module, "Broken");
    let _ = symbol(&extracted, SymbolKind::Class, "TOk");
}

#[test]
fn inner_values_shadow_outer_routines_and_unit_receivers() {
    let extracted = extract(
        "src/Shadow.pas",
        r"unit Shadow;
interface
uses UAuth;
procedure Outer;
implementation
procedure Outer;
  procedure Work;
  begin
  end;
  procedure Inner(Work: TProc; UAuth: TMock);
  begin
    Work();
    UAuth.Run;
  end;
begin
  Work;
  Inner(nil, nil);
end;
end.",
    );
    let inner = member(&extracted, SymbolKind::Function, "Outer::Inner");
    assert_eq!(
        call(&extracted, &inner.id, "Work").resolution_name,
        Some(scoped("Work")),
        "a parameter shadows the enclosing routine's nested routine"
    );
    assert_eq!(
        call(&extracted, &inner.id, "UAuth.Run").resolution_name,
        Some(scoped("UAuth.Run")),
        "a parameter named like a used unit shadows the unit"
    );
    let outer = symbol(&extracted, SymbolKind::Function, "Outer");
    assert_eq!(
        call(&extracted, &outer.id, "Work").resolution_name,
        Some(scoped("Outer::Work"))
    );
    assert_eq!(
        call(&extracted, &outer.id, "Inner").resolution_name,
        Some(scoped("Outer::Inner"))
    );
}

#[test]
fn unit_member_paths_normalize_and_overloaded_routines_stay_ambiguous() {
    let extracted = extract(
        "src/Paths.pas",
        r"unit Paths;
interface
uses UAuth, System.SysUtils;
procedure Put(Value: Integer); overload;
procedure Put(const Value: string); overload;
procedure Run;
implementation
procedure Put(const Value: string);
begin
end;
procedure Run;
begin
  Put(1);
  UAuth.TAuthService.Instance;
  UAuth.Hash(1);
  System.SysUtils.TFormatSettings.Create;
  Missing.TThing.Make;
end;
end.",
    );
    let run = symbol(&extracted, SymbolKind::Function, "Run");
    assert_eq!(
        call(&extracted, &run.id, "Put").resolution_name,
        Some(scoped("Put")),
        "an unimplemented overload must not lose to the implemented one"
    );
    assert_eq!(
        call(&extracted, &run.id, "UAuth.TAuthService.Instance").resolution_name,
        Some("UAuth.TAuthService::Instance".to_owned())
    );
    assert_eq!(
        call(&extracted, &run.id, "UAuth.Hash").resolution_name,
        None
    );
    assert_eq!(
        call(
            &extracted,
            &run.id,
            "System.SysUtils.TFormatSettings.Create"
        )
        .resolution_name,
        Some("System.SysUtils.TFormatSettings::Create".to_owned())
    );
    assert_eq!(
        call(&extracted, &run.id, "Missing.TThing.Make").resolution_name,
        None,
        "only units this file uses are rewritten"
    );
}

#[test]
fn signatures_drop_comments_and_escaped_identifiers_keep_their_names() {
    let extracted = extract(
        "src/Escaped.dpr",
        "program Escaped;\nprocedure Run(A: Integer {sk_live_REVIEW_SENTINEL_00000000}; B: TValue);\nbegin\nend;\nprocedure &type;\nbegin\nend;\nbegin\n  &type;\nend.\n",
    );
    let run = symbol(&extracted, SymbolKind::Function, "Run");
    assert_eq!(
        run.signature.as_deref(),
        Some("(A: Integer ; B: TValue)"),
        "comments are not syntax and are never copied"
    );
    assert!(!format!("{extracted:?}").contains("REVIEW_SENTINEL"));
    let escaped = symbol(&extracted, SymbolKind::Function, "type");
    let program = symbol(&extracted, SymbolKind::Module, "Escaped");
    assert_eq!(
        reference_names(&extracted, ReferenceKind::Calls, &program.id),
        ["type"]
    );
    assert!(!escaped.implementation.declaration_only);
}

#[test]
fn deeply_nested_routines_keep_scope_state_linear() {
    const DEPTH: usize = 180;
    let names = (0..DEPTH)
        .map(|level| format!("{:_<250}", format!("R{level}")))
        .collect::<Vec<_>>();
    let mut source = String::from("program Deep;\n");
    for name in &names {
        writeln!(source, "procedure {name};")
            .unwrap_or_else(|error| panic!("could not build the fixture: {error}"));
    }
    writeln!(source, "begin\n  {};\nend;", names[0])
        .unwrap_or_else(|error| panic!("could not build the fixture: {error}"));
    for _ in 1..DEPTH {
        source.push_str("begin\nend;\n");
    }
    source.push_str("begin\nend.\n");
    let extracted = extract("src/Deep.dpr", &source);
    assert_eq!(names_of(&extracted, SymbolKind::Function).len(), DEPTH);
    let deepest = extracted
        .symbols
        .iter()
        .rfind(|symbol| symbol.kind == SymbolKind::Function)
        .unwrap_or_else(|| panic!("missing deepest routine"));
    let outermost = symbol(&extracted, SymbolKind::Function, &names[0]);
    assert_eq!(
        call(&extracted, &deepest.id, &names[0]).resolution_name,
        Some(scoped(&format!("self::{}", outermost.qualified_name))),
        "the outermost routine stays visible through every scope"
    );
}

#[test]
fn implementation_overloads_inline_locals_and_escaped_types_follow_pascal_scope() {
    let extracted = extract(
        "src/Mixed.pas",
        r"unit Mixed;
interface
uses UAuth;
type
  T = Integer;
procedure Run(A: &T);
implementation
procedure Put(Value: Integer); overload; forward;
procedure Put(const Value: string); overload;
begin
end;
procedure Run(A: T);
begin
  Put(1);
  var UAuth: TMock := nil;
  UAuth.Hash(1);
end;
end.",
    );
    let runs = extracted
        .symbols
        .iter()
        .filter(|symbol| symbol.name == "Run")
        .collect::<Vec<_>>();
    assert_eq!(runs.len(), 1, "`&T` and `T` are one type: {runs:?}");
    assert!(!runs[0].implementation.declaration_only);
    assert_eq!(
        call(&extracted, &runs[0].id, "Put").resolution_name,
        Some(scoped("Put")),
        "a forward overload plus an implemented one is still ambiguous"
    );
    assert_eq!(
        call(&extracted, &runs[0].id, "UAuth.Hash").resolution_name,
        Some(scoped("UAuth.Hash")),
        "an inline local named like a used unit shadows the unit"
    );
}

#[test]
fn grouped_parameters_with_large_types_stay_within_the_working_budget() {
    const PARAMETERS: usize = 5_000;
    let names = (0..PARAMETERS)
        .map(|index| format!("P{index}"))
        .collect::<Vec<_>>()
        .join(",");
    let arguments = (0..PARAMETERS)
        .map(|index| format!("T{index}"))
        .collect::<Vec<_>>()
        .join(",");
    let source = format!(
        "program P;\nprocedure Run({names}: TTuple<{arguments}>);\nbegin\nend;\nbegin\n  Run;\nend.\n"
    );
    let extracted = extract("src/Wide.dpr", &source);
    let run = symbol(&extracted, SymbolKind::Function, "Run");
    assert_eq!(run.signature, None, "an oversized signature is dropped");
    assert_eq!(
        reference_names(&extracted, ReferenceKind::TypeOf, &run.id),
        ["TTuple"]
    );
}

#[test]
fn pascal_extraction_is_deterministic_and_cancellable() {
    let limits = limits();
    let snapshot = SourceSnapshot::from_bytes("src/UAuth.pas", UAUTH.as_bytes(), limits)
        .unwrap_or_else(|error| panic!("snapshot failed: {error}"));
    let mut extractor = NativeExtractor::new(SourceLanguage::Pascal)
        .unwrap_or_else(|error| panic!("extractor failed: {error}"));
    let first = extractor
        .extract(&snapshot)
        .unwrap_or_else(|error| panic!("first extraction failed: {error}"));
    let second = extractor
        .extract(&snapshot)
        .unwrap_or_else(|error| panic!("second extraction failed: {error}"));
    assert_eq!(first, second);
    assert_eq!(
        extractor.extract_with_cancellation(&snapshot, || true),
        Err(ExtractError::Cancelled)
    );
}

#[test]
fn grouped_declarations_with_thousands_of_names_extract_every_name() {
    // Realistic member-name width keeps the file inside the shared per-file
    // output budget, which (as for every language) rejects a symbol per few
    // source bytes. Each grouped name used to rescan the whole declaration,
    // so this group took minutes; it is now one pass per name.
    const NAMES: usize = 4_000;
    let names = (0..NAMES)
        .map(|index| format!("GroupedDeclarationMember{index:08}"))
        .collect::<Vec<_>>()
        .join(", ");
    let source = format!(
        "unit Wide;\ninterface\ntype\n  TWide = record\n    {names}: TItem;\n  end;\nvar\n  {names}: TItem;\nimplementation\nend.\n"
    );
    let extracted = extract("src/Wide.pas", &source);
    let fields = names_of(&extracted, SymbolKind::Field);
    let variables = names_of(&extracted, SymbolKind::Variable);
    assert_eq!(fields.len(), NAMES);
    assert_eq!(variables.len(), NAMES);
    let last = member(
        &extracted,
        SymbolKind::Field,
        &format!("TWide::GroupedDeclarationMember{:08}", NAMES - 1),
    );
    assert_eq!(
        reference_names(&extracted, ReferenceKind::TypeOf, &last.id),
        ["TItem"],
        "every grouped name keeps the shared declared type"
    );
    let first = member(
        &extracted,
        SymbolKind::Field,
        "TWide::GroupedDeclarationMember00000000",
    );
    assert_ne!(
        first.structural_digest, last.structural_digest,
        "each grouped name has its own structural root"
    );
}

#[test]
fn nested_routines_call_their_enclosing_routine_by_exact_same_file_name() {
    let extracted = extract(
        "src/Nested.dpr",
        r"program Nested;
procedure Outer;
  procedure Inner;
  begin
    outer;
  end;
begin
  Inner;
  Outer;
end;
begin
end.",
    );
    let outer = symbol(&extracted, SymbolKind::Function, "Outer");
    let inner = symbol(&extracted, SymbolKind::Function, "Inner");
    assert_eq!(
        call(&extracted, &inner.id, "outer").resolution_name,
        Some(scoped("self::Outer")),
        "a top-level enclosing routine binds by its declared same-file name"
    );
    assert_eq!(
        call(&extracted, &outer.id, "Outer").resolution_name,
        Some(scoped("self::Outer"))
    );
    assert_eq!(
        call(&extracted, &outer.id, "Inner").resolution_name,
        Some(scoped("Outer::Inner"))
    );
}

#[test]
fn abbreviated_headers_scope_member_shadowing_and_cyclic_ancestry_stay_uncertain() {
    let extracted = extract(
        "src/Worker.pas",
        r"unit Worker;
interface
uses Settings;
type
  TProc = procedure;
  TWorker = class
  private
    procedure Callback;
    function GetSettings: TObject;
  public
    procedure Run(Callback: TProc);
    property Settings: TObject read GetSettings;
  end;
  TLoopA = class(TLoopB)
    procedure Go;
  end;
  TLoopB = class(TLoopA)
  end;
implementation
procedure TWorker.Callback;
begin
end;
function TWorker.GetSettings: TObject;
begin
  Result := nil;
end;
procedure TWorker.Run;
begin
  Callback;
  Settings.Load;
  Self.Settings.Load;
end;
procedure TLoopA.Go;
begin
  Missing;
end;
end.",
    );
    let run = member(&extracted, SymbolKind::Method, "TWorker::Run");
    assert!(!run.implementation.declaration_only);
    assert_eq!(
        call(&extracted, &run.id, "Callback").resolution_name,
        Some(scoped("Callback")),
        "the declared parameter shadows the member in an abbreviated header"
    );
    assert_eq!(
        call(&extracted, &run.id, "Settings.Load").resolution_name,
        Some(scoped("Settings.Load")),
        "a member named like a used unit shadows the unit"
    );
    assert_eq!(
        call(&extracted, &run.id, "Self.Settings.Load").resolution_name,
        Some(scoped("Self.Settings.Load")),
        "a member of Self is a value of unknown type"
    );
    let go = member(&extracted, SymbolKind::Method, "TLoopA::Go");
    assert_eq!(
        call(&extracted, &go.id, "Missing").resolution_name,
        Some(scoped("Missing")),
        "a cyclic in-file ancestry never falls back to a global"
    );
}

#[test]
fn local_forward_declarations_hide_members_until_their_bodies_are_known() {
    let extracted = extract(
        "src/Forward.pas",
        r"unit Forward;
interface
type
  TOwner = class
    procedure Later;
    procedure Run;
  end;
implementation
procedure TOwner.Later;
begin
end;
procedure TOwner.Run;
  procedure Later; forward;
  procedure Early;
  begin
    Later;
  end;
  procedure Later;
  begin
  end;
begin
  Later;
end;
end.",
    );
    let early = member(&extracted, SymbolKind::Function, "TOwner::Run::Early");
    assert_eq!(
        call(&extracted, &early.id, "Later").resolution_name,
        Some(scoped("Later")),
        "a forward-declared local routine is not the class member"
    );
    let run = member(&extracted, SymbolKind::Method, "TOwner::Run");
    assert_eq!(
        call(&extracted, &run.id, "Later").resolution_name,
        Some(scoped("TOwner::Run::Later"))
    );
}

#[test]
fn overload_identity_keeps_token_boundaries() {
    let extracted = extract(
        "src/Tokens.pas",
        r"unit Tokens;
interface
type
  TItem = Integer;
  arrayofTItem = Integer;
procedure Put(A: array of TItem); overload;
procedure Put(A: arrayofTItem); overload;
implementation
procedure Put(A: arrayofTItem);
begin
  Second;
end;
procedure Put(A: array of TItem);
begin
  First;
end;
end.",
    );
    let puts = extracted
        .symbols
        .iter()
        .filter(|symbol| symbol.name == "Put")
        .collect::<Vec<_>>();
    assert_eq!(puts.len(), 2, "{}", inventory(&extracted));
    assert_eq!(
        reference_names(&extracted, ReferenceKind::Calls, &puts[0].id),
        ["First"]
    );
    assert_eq!(
        reference_names(&extracted, ReferenceKind::Calls, &puts[1].id),
        ["Second"]
    );
}

#[test]
fn unicode_identifier_fragments_and_generic_parameters_are_not_facts() {
    let extracted = extract(
        "src/Gen.pas",
        "unit Gen;\ninterface\ntype\n  T\u{dc}ber = class\n  end;\n  TBox<T> = class\n    FItem: T;\n    FOther: TOther;\n    procedure Put(const AItem: T);\n    function Get<U>(const A: U): T;\n  end;\n  TArr<V> = array of V;\nimplementation\nprocedure TBox<T>.Put(const AItem: T);\nbegin\nend;\nfunction TBox<T>.Get<U>(const A: U): T;\nbegin\nend;\nend.\n",
    );
    assert!(
        !extracted.symbols.iter().any(|symbol| symbol.name == "ber"),
        "a fragment of an identifier the grammar cannot lex is not a symbol: {}",
        inventory(&extracted)
    );
    let type_names = extracted
        .references
        .iter()
        .filter(|reference| {
            matches!(
                reference.kind,
                ReferenceKind::TypeOf | ReferenceKind::Returns
            )
        })
        .map(|reference| reference.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        type_names,
        ["TOther"],
        "generic parameters in scope never become type references"
    );
}

#[test]
fn overloaded_recursion_stays_uncertain_and_unit_routines_bind_case_insensitively() {
    let extracted = extract(
        "src/Over.pas",
        r"unit Over;
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
  end;
  procedure Inner(const S: string); overload;
  begin
    Inner(1);
  end;
begin
  Inner(2);
end;
end.",
    );
    let put = extracted
        .symbols
        .iter()
        .find(|symbol| symbol.name == "Put")
        .unwrap_or_else(|| panic!("missing Put: {}", inventory(&extracted)));
    assert_eq!(
        call(&extracted, &put.id, "Put").resolution_name,
        Some(scoped("Put")),
        "an overloaded routine's own name cannot pick an overload"
    );
    assert_eq!(
        call(&extracted, &put.id, "format").resolution_name,
        Some(scoped("self::Format")),
        "a unit-level routine binds by its declared spelling"
    );
    let outer = symbol(&extracted, SymbolKind::Function, "Outer");
    assert_eq!(
        call(&extracted, &outer.id, "Inner").resolution_name,
        Some(scoped("Inner")),
        "local overloads name no single nested routine"
    );
    let second = extracted
        .symbols
        .iter()
        .rfind(|symbol| symbol.qualified_name == "Outer::Inner")
        .unwrap_or_else(|| panic!("missing Inner: {}", inventory(&extracted)));
    assert_eq!(
        call(&extracted, &second.id, "Inner").resolution_name,
        Some(scoped("Inner"))
    );
}

#[test]
fn small_grouped_declarations_keep_type_sensitive_structural_roots() {
    let digest = |declared: &str| {
        let extracted = extract(
            "src/Grouped.pas",
            &format!("unit Grouped;\ninterface\nvar\n  A, B: {declared};\nimplementation\nend.\n"),
        );
        symbol(&extracted, SymbolKind::Variable, "A")
            .structural_digest
            .clone()
    };
    assert_ne!(
        digest("Integer"),
        digest("string"),
        "a changed declared type changes every small group member's digest"
    );
}

#[test]
fn members_past_the_canonical_bound_bind_by_their_stored_name() {
    const LEVELS: usize = 8;
    let types = (0..LEVELS)
        .map(|level| format!("{:T<250}", format!("TLevel{level}")))
        .collect::<Vec<_>>();
    let method = format!("{:M<100}", "Method");
    let mut source = String::from("unit Long;\ninterface\ntype\n");
    for (level, name) in types.iter().enumerate() {
        let indent = "  ".repeat(level + 1);
        writeln!(source, "{indent}{name} = class")
            .unwrap_or_else(|error| panic!("could not build the fixture: {error}"));
        if level + 1 < LEVELS {
            writeln!(source, "{indent}type")
                .unwrap_or_else(|error| panic!("could not build the fixture: {error}"));
        }
    }
    let inner = "  ".repeat(LEVELS + 1);
    writeln!(source, "{inner}procedure {method};\n{inner}procedure Run;")
        .unwrap_or_else(|error| panic!("could not build the fixture: {error}"));
    for level in (0..LEVELS).rev() {
        writeln!(source, "{}end;", "  ".repeat(level + 1))
            .unwrap_or_else(|error| panic!("could not build the fixture: {error}"));
    }
    let path = types.join(".");
    writeln!(
        source,
        "implementation\nprocedure {path}.{method};\nbegin\nend;\nprocedure {path}.Run;\nbegin\n  {method};\nend;\nend."
    )
    .unwrap_or_else(|error| panic!("could not build the fixture: {error}"));
    let extracted = extract("src/Long.pas", &source);
    let target = symbol(&extracted, SymbolKind::Method, &method);
    let raw = format!("{}::{method}", types.join("::"));
    assert!(raw.len() > 2_048);
    assert_ne!(target.qualified_name, raw, "the stored name is shortened");
    let run = symbol(&extracted, SymbolKind::Method, "Run");
    assert_eq!(
        call(&extracted, &run.id, &method).resolution_name,
        Some(scoped(&target.qualified_name)),
        "an implicit Self call names the stored member"
    );
}

#[test]
fn local_overloads_stay_uncertain_whatever_their_spelling_and_self_named_scopes_keep_their_qualifier()
 {
    let extracted = extract(
        "src/Local.dpr",
        r"program Local;
procedure Outer;
  procedure Put(I: Integer); overload; forward;
  procedure PUT(S: string); overload; forward;
  procedure Put(I: Integer);
  begin
  end;
  procedure Early;
  begin
    Put('x');
  end;
  procedure PUT(S: string);
  begin
  end;
begin
  Early;
end;
procedure &self;
  procedure Inner;
  begin
  end;
begin
  Inner;
end;
begin
end.",
    );
    let early = member(&extracted, SymbolKind::Function, "Outer::Early");
    assert_eq!(
        call(&extracted, &early.id, "Put").resolution_name,
        Some(scoped("Put")),
        "two local overloads differing only in spelling name no single routine"
    );
    let own = symbol(&extracted, SymbolKind::Function, "self");
    let inner = member(&extracted, SymbolKind::Function, "self::Inner");
    assert_eq!(
        call(&extracted, &own.id, "Inner").resolution_name,
        Some(scoped(&format!("self::{}", inner.qualified_name))),
        "a routine spelled `self` keeps its qualifier under the module prefix"
    );
}

#[test]
fn wide_generic_scopes_filter_parameters_without_copying_them() {
    const PARAMETERS: usize = 3_000;
    let parameters = (0..PARAMETERS)
        .map(|index| format!("P{index}"))
        .collect::<Vec<_>>()
        .join(", ");
    let mut source = format!("unit Bag;\ninterface\ntype\n  TBag<{parameters}> = class\n");
    for index in 0..PARAMETERS {
        writeln!(
            source,
            "    procedure Put{index}(const A: P{}; const B: TItem);",
            PARAMETERS - 1
        )
        .unwrap_or_else(|error| panic!("could not build the fixture: {error}"));
    }
    source.push_str("  end;\nimplementation\nend.\n");
    let extracted = extract("src/Bag.pas", &source);
    let type_names = extracted
        .references
        .iter()
        .filter(|reference| reference.kind == ReferenceKind::TypeOf)
        .map(|reference| reference.name.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        type_names.into_iter().collect::<Vec<_>>(),
        ["TItem"],
        "the class's generic parameters are never type references"
    );
    assert_eq!(names_of(&extracted, SymbolKind::Method).len(), PARAMETERS);
}

#[test]
fn local_routines_become_visible_where_they_are_declared() {
    let extracted = extract(
        "src/Scope.dpr",
        r"program Scope;
procedure Put;
begin
end;
procedure Outer;
  procedure Early;
  begin
    Put;
  end;
  procedure Put(I: Integer); overload;
  begin
  end;
  procedure PUT(S: string); overload;
  begin
  end;
  procedure Later;
  begin
    Put(1);
  end;
begin
  Early;
end;
procedure Mixed;
  procedure Put(I: Integer); overload; forward;
  procedure PUT(S: string); overload;
  begin
  end;
  procedure Early;
  begin
    Put(1);
  end;
begin
  Early;
end;
begin
end.",
    );
    let early = member(&extracted, SymbolKind::Function, "Outer::Early");
    assert_eq!(
        call(&extracted, &early.id, "Put").resolution_name,
        Some(scoped("self::Put")),
        "local overloads declared later do not hide the visible global routine"
    );
    let later = member(&extracted, SymbolKind::Function, "Outer::Later");
    assert_eq!(
        call(&extracted, &later.id, "Put").resolution_name,
        Some(scoped("Put")),
        "once declared, the local overloads name no single routine"
    );
    let mixed = member(&extracted, SymbolKind::Function, "Mixed::Early");
    assert_eq!(
        call(&extracted, &mixed.id, "Put").resolution_name,
        Some(scoped("Put")),
        "an unmatched forward overload and another overload's body stay ambiguous"
    );
}

fn extract(path: &str, source: &str) -> ExtractedFile {
    let snapshot = SourceSnapshot::from_bytes(path, source.as_bytes(), limits())
        .unwrap_or_else(|error| panic!("snapshot failed for {path}: {error}"));
    let mut extractor = NativeExtractor::new(snapshot.language())
        .unwrap_or_else(|error: ExtractError| panic!("extractor failed for {path}: {error}"));
    extractor
        .extract(&snapshot)
        .unwrap_or_else(|error| panic!("extraction failed for {path}: {error}"))
}

/// The self-scope lookup name the indexer resolves by exact same-file
/// qualified name (and records as unresolved without a `::` qualifier).
fn scoped(target: &str) -> String {
    format!("{RUST_SELF_RECEIVER_RESOLUTION_PREFIX}{target}")
}

fn limits() -> SourceLimits {
    SourceLimits::new(SOURCE_LIMIT).unwrap_or_else(|error| panic!("source limits failed: {error}"))
}

fn symbol<'file>(
    extracted: &'file ExtractedFile,
    kind: SymbolKind,
    name: &str,
) -> &'file ExtractedSymbol {
    extracted
        .symbols
        .iter()
        .find(|symbol| symbol.kind == kind && symbol.name == name)
        .unwrap_or_else(|| {
            panic!(
                "missing {kind:?} {name}; extracted: {}",
                inventory(extracted)
            )
        })
}

fn member<'file>(
    extracted: &'file ExtractedFile,
    kind: SymbolKind,
    qualified_name: &str,
) -> &'file ExtractedSymbol {
    extracted
        .symbols
        .iter()
        .find(|symbol| symbol.kind == kind && symbol.qualified_name == qualified_name)
        .unwrap_or_else(|| {
            panic!(
                "missing {kind:?} {qualified_name}; extracted: {}",
                inventory(extracted)
            )
        })
}

fn inventory(extracted: &ExtractedFile) -> String {
    extracted
        .symbols
        .iter()
        .map(|symbol| format!("{:?} {}", symbol.kind, symbol.qualified_name))
        .collect::<Vec<_>>()
        .join(", ")
}

fn names_of(extracted: &ExtractedFile, kind: SymbolKind) -> Vec<&str> {
    extracted
        .symbols
        .iter()
        .filter(|symbol| symbol.kind == kind)
        .map(|symbol| symbol.name.as_str())
        .collect()
}

fn reference_names<'file>(
    extracted: &'file ExtractedFile,
    kind: ReferenceKind,
    owner: &cartograph_domain::SymbolId,
) -> Vec<&'file str> {
    extracted
        .references
        .iter()
        .filter(|reference| reference.kind == kind && reference.owner.as_ref() == Some(owner))
        .map(|reference| reference.name.as_str())
        .collect()
}

fn call<'file>(
    extracted: &'file ExtractedFile,
    owner: &cartograph_domain::SymbolId,
    name: &str,
) -> &'file ExtractedReference {
    extracted
        .references
        .iter()
        .find(|reference| {
            reference.kind == ReferenceKind::Calls
                && reference.owner.as_ref() == Some(owner)
                && reference.name == name
        })
        .unwrap_or_else(|| panic!("missing call {name}: {:?}", extracted.references))
}

fn contains(
    extracted: &ExtractedFile,
    parent: &cartograph_domain::SymbolId,
    child: &cartograph_domain::SymbolId,
) -> bool {
    extracted
        .containments
        .iter()
        .any(|containment| &containment.parent == parent && &containment.child == child)
}
