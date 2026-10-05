//! Delphi/FireMonkey `.dfm`/`.fmx` form-file contracts ported from the v1 extraction suite.

mod dependency_ownership;

use std::fmt::Write as _;

use cartograph_domain::{FileParseStatus, ReferenceKind, SourceLanguage, SymbolId, SymbolKind};
use cartograph_extract::{
    DiagnosticCode, ExtractError, ExtractedFile, ExtractedSymbol, ImportBindingKind,
    NativeExtractor, SourceLimits, SourceSnapshot,
};

const SOURCE_LIMIT: usize = 1024 * 1024;
/// Canonical storage bound for a qualified name.
const MAX_CANONICAL_QUALIFIED_NAME_BYTES: usize = 2_048;

const MAIN_FORM: &str = r"object frmMain: TfrmMain
  Left = 0
  Top = 0
  Caption = 'Cartograph DFM Fixture'
  ClientHeight = 480
  ClientWidth = 640
  OnCreate = FormCreate
  OnDestroy = FormDestroy
  object pnlTop: TPanel
    Left = 0
    Top = 0
    Width = 640
    Height = 50
    object lblTitle: TLabel
      Left = 16
      Top = 16
      Caption = 'Authentication Service'
    end
    object btnLogin: TButton
      Left = 540
      Top = 12
      OnClick = btnLoginClick
    end
  end
  object pnlContent: TPanel
    Left = 0
    Top = 50
    object edtUsername: TEdit
      Left = 16
      Top = 16
      OnChange = edtUsernameChange
    end
    object edtPassword: TEdit
      Left = 16
      Top = 48
      OnKeyPress = edtPasswordKeyPress
    end
    object mmoLog: TMemo
      Left = 16
      Top = 88
    end
  end
  object pnlStatus: TStatusBar
    Left = 0
    Top = 440
    Panels = <
      item
        Width = 200
      end
      item
        Width = 200
      end>
  end
end
";

#[test]
fn components_carry_their_class_as_signature() {
    let extracted = extract(
        "src/Form1.dfm",
        "object Form1: TForm1\n  Left = 0\n  Top = 0\n  Caption = 'My Form'\n  object Button1: TButton\n    Left = 10\n    Top = 10\n    Caption = 'Click Me'\n  end\nend",
    );
    assert_eq!(extracted.language, SourceLanguage::Pascal);
    assert_eq!(extracted.parse_status, FileParseStatus::Parsed);
    assert!(
        extracted.diagnostics.is_empty(),
        "a form file is not Pascal source: {:?}",
        extracted.diagnostics
    );
    assert_eq!(components(&extracted), ["Form1", "Button1"]);
    let form = component(&extracted, "Form1");
    let button = component(&extracted, "Button1");
    assert_eq!(form.signature.as_deref(), Some("TForm1"));
    assert_eq!(button.signature.as_deref(), Some("TButton"));
    assert_eq!(button.qualified_name, "Form1::Button1");
    assert_eq!(button.span.start_line(), 5);
    assert_eq!(
        button.span.end_line(),
        9,
        "the component spans its object block"
    );
    assert!(contains(&extracted, &form.id, &button.id));
    let rendered = format!("{extracted:?}");
    assert!(
        !rendered.contains("Click Me") && !rendered.contains("My Form"),
        "property literals are never retained"
    );
}

#[test]
fn nested_components_form_a_containment_hierarchy() {
    let extracted = extract(
        "src/Form1.dfm",
        "object Form1: TForm1\n  object Panel1: TPanel\n    object Label1: TLabel\n      Caption = 'Hello'\n    end\n  end\nend",
    );
    assert_eq!(components(&extracted), ["Form1", "Panel1", "Label1"]);
    let panel = component(&extracted, "Panel1");
    let label = component(&extracted, "Label1");
    assert!(contains(&extracted, &panel.id, &label.id));
    assert!(!contains(
        &extracted,
        &component(&extracted, "Form1").id,
        &label.id
    ));
    assert_eq!(label.qualified_name, "Form1::Panel1::Label1");
}

#[test]
fn event_handlers_reference_methods_of_the_root_form_class() {
    let extracted = extract(
        "src/Form1.dfm",
        "object Form1: TForm1\n  OnCreate = FormCreate\n  OnDestroy = FormDestroy\n  OnlyDigits = True\n  OnHide = nil\n  object Button1: TButton\n    OnClick = Button1Click\n  end\n  object Button2: TButton\n    OnClick = Button1Click\n  end\nend",
    );
    let form = component(&extracted, "Form1");
    let button = component(&extracted, "Button1");
    let shared = component(&extracted, "Button2");
    let bindings = extracted
        .import_bindings
        .iter()
        .map(|binding| {
            (
                binding.kind,
                binding.module_specifier.as_str(),
                binding.imported_name.as_str(),
                binding.local_name.as_str(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        bindings,
        [
            (
                ImportBindingKind::Named,
                "./Form1.pas",
                "TForm1::FormCreate",
                "TForm1::FormCreate"
            ),
            (
                ImportBindingKind::Named,
                "./Form1.pas",
                "TForm1::FormDestroy",
                "TForm1::FormDestroy"
            ),
            (
                ImportBindingKind::Named,
                "./Form1.pas",
                "TForm1::Button1Click",
                "TForm1::Button1Click"
            ),
        ],
        "each handler binds once, through the unit the form file belongs to"
    );
    assert!(extracted.references.iter().any(|reference| {
        reference.owner.as_ref() == Some(&shared.id) && reference.name == "Button1Click"
    }));
    let handlers = extracted
        .references
        .iter()
        .map(|reference| {
            (
                reference.kind,
                reference.owner.clone(),
                reference.name.as_str(),
                reference.resolution_name.as_deref(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        handlers,
        [
            (
                ReferenceKind::References,
                Some(form.id.clone()),
                "FormCreate",
                Some("TForm1::FormCreate")
            ),
            (
                ReferenceKind::References,
                Some(form.id.clone()),
                "FormDestroy",
                Some("TForm1::FormDestroy")
            ),
            (
                ReferenceKind::References,
                Some(button.id.clone()),
                "Button1Click",
                Some("TForm1::Button1Click")
            ),
            (
                ReferenceKind::References,
                Some(shared.id.clone()),
                "Button1Click",
                Some("TForm1::Button1Click")
            ),
        ]
    );
}

#[test]
fn multi_line_values_and_collections_are_skipped() {
    let extracted = extract(
        "src/Form1.dfm",
        "object Form1: TForm1\n  SQL.Strings = (\n    'SELECT * FROM users'\n    'object Fake: TFake'\n    'WHERE active = 1')\n  object Button1: TButton\n    OnClick = Button1Click\n  end\nend",
    );
    assert_eq!(components(&extracted), ["Form1", "Button1"]);
    assert_eq!(handler_names(&extracted), ["Button1Click"]);
    assert!(!format!("{extracted:?}").contains("SELECT"));

    let collections = extract(
        "src/Form1.dfm",
        "object Form1: TForm1\n  object StatusBar1: TStatusBar\n    Panels = <\n      item\n        Width = 200\n      end\n      item\n        Width = 200\n      end>\n  end\nend",
    );
    assert_eq!(components(&collections), ["Form1", "StatusBar1"]);
    assert_eq!(collections.parse_status, FileParseStatus::Parsed);
}

#[test]
fn nested_collections_and_binary_data_do_not_corrupt_nesting() {
    let extracted = extract(
        "src/Grid.dfm",
        "object Form1: TForm1\n  object Grid1: TDBGrid\n    Columns = <\n      item\n        Expanded = False\n        Items = <\n          item\n            OnClick = NotAHandler\n          end>\n      end\n      item\n        Width = 10\n      end>\n    Glyph.Data = {\n      0A0B0C0D\n      end\n    }\n    OnDblClick = Grid1DblClick\n  end\n  object Button2: TButton\n  end\nend",
    );
    assert_eq!(components(&extracted), ["Form1", "Grid1", "Button2"]);
    let form = component(&extracted, "Form1");
    let grid = component(&extracted, "Grid1");
    let button = component(&extracted, "Button2");
    assert!(contains(&extracted, &form.id, &grid.id));
    assert!(
        contains(&extracted, &form.id, &button.id),
        "the collection's inner `end` lines must not pop the component stack"
    );
    assert_eq!(handler_names(&extracted), ["Grid1DblClick"]);
    assert_eq!(extracted.parse_status, FileParseStatus::Parsed);
}

#[test]
fn inherited_and_inline_blocks_are_components() {
    let extracted = extract(
        "src/Form2.dfm",
        "inherited Form2: TForm2\n  Caption = 'Inherited Form'\n  inline Frame1: TFrame1\n    inherited Button1: TButton [0]\n      OnClick = Button1Click\n    end\n  end\n  object Button2: TButton\n  end\nend",
    );
    assert_eq!(
        components(&extracted),
        ["Form2", "Frame1", "Button1", "Button2"]
    );
    assert_eq!(
        component(&extracted, "Button1").qualified_name,
        "Form2::Frame1::Button1"
    );
    let handler = extracted
        .references
        .iter()
        .find(|reference| reference.name == "Button1Click")
        .unwrap_or_else(|| panic!("handler missing: {:?}", extracted.references));
    assert_eq!(
        handler.resolution_name.as_deref(),
        Some("TForm2::Button1Click"),
        "the streaming root owns every handler in its form file"
    );
}

#[test]
fn main_form_fixture_matches_the_v1_contract() {
    for path in ["src/MainForm.dfm", "src/MainForm.fmx"] {
        let extracted = extract(path, MAIN_FORM);
        assert_eq!(
            components(&extracted),
            [
                "frmMain",
                "pnlTop",
                "lblTitle",
                "btnLogin",
                "pnlContent",
                "edtUsername",
                "edtPassword",
                "mmoLog",
                "pnlStatus",
            ],
            "{path}"
        );
        assert_eq!(
            handler_names(&extracted),
            [
                "FormCreate",
                "FormDestroy",
                "btnLoginClick",
                "edtUsernameChange",
                "edtPasswordKeyPress",
            ],
            "{path}"
        );
        assert!(
            extracted
                .references
                .iter()
                .all(|reference| reference.kind == ReferenceKind::References)
        );
    }
}

#[test]
fn unbalanced_forms_are_partial_and_extraction_is_cancellable() {
    let extracted = extract(
        "src/Broken.dfm",
        "object Form1: TForm1\n  object Button1: TButton\n    OnClick = Button1Click\n  end\n",
    );
    assert_eq!(extracted.parse_status, FileParseStatus::Partial);
    assert_eq!(components(&extracted), ["Form1", "Button1"]);
    let stray = extract("src/Stray.dfm", "end\nobject Form1: TForm1\nend\n");
    assert_eq!(stray.parse_status, FileParseStatus::Partial);

    let snapshot = SourceSnapshot::from_bytes("src/Main.dfm", MAIN_FORM.as_bytes(), limits())
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
fn deeply_nested_long_component_names_fit_the_canonical_bound() {
    const DEPTH: usize = 70;
    let names = (0..DEPTH)
        .map(|level| format!("{:x<40}", format!("Panel{level}_")))
        .collect::<Vec<_>>();
    let mut source = String::new();
    for (level, name) in names.iter().enumerate() {
        writeln!(source, "{}object {name}: TPanel", "  ".repeat(level))
            .unwrap_or_else(|error| panic!("could not build the fixture: {error}"));
    }
    writeln!(source, "{}OnClick = DeepClick", "  ".repeat(DEPTH))
        .unwrap_or_else(|error| panic!("could not build the fixture: {error}"));
    for level in (0..DEPTH).rev() {
        writeln!(source, "{}end", "  ".repeat(level))
            .unwrap_or_else(|error| panic!("could not build the fixture: {error}"));
    }
    let extracted = extract("src/Deep.dfm", &source);
    assert_eq!(extracted.parse_status, FileParseStatus::Parsed);
    assert_eq!(components(&extracted).len(), DEPTH);
    assert!(
        extracted
            .symbols
            .iter()
            .all(|symbol| symbol.qualified_name.len() <= MAX_CANONICAL_QUALIFIED_NAME_BYTES),
        "every component name must fit the canonical storage bound"
    );
    let distinct = extracted
        .symbols
        .iter()
        .map(|symbol| symbol.qualified_name.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        distinct.len(),
        DEPTH,
        "shortened names stay distinct per component"
    );
    assert!(
        extracted
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == DiagnosticCode::CanonicalNameTruncated),
        "a shortened name is reported: {:?}",
        extracted.diagnostics
    );
    let deepest = component(&extracted, &names[DEPTH - 1]);
    let parent = component(&extracted, &names[DEPTH - 2]);
    assert!(contains(&extracted, &parent.id, &deepest.id));
    let handler = extracted
        .references
        .iter()
        .find(|reference| reference.name == "DeepClick")
        .unwrap_or_else(|| panic!("handler missing: {:?}", extracted.references));
    assert_eq!(handler.owner.as_ref(), Some(&deepest.id));
    assert_eq!(
        handler.resolution_name.as_deref(),
        Some("TPanel::DeepClick")
    );
}

#[test]
fn unnamed_components_keep_the_hierarchy_and_handler_owners() {
    let extracted = extract(
        "src/Styled.fmx",
        "object Form1: TForm1\n  object Panel1: TPanel\n    object TLayout\n      Align = Client\n      OnResize = LayoutResize\n      object TRectangle\n      end\n    end\n    object B: TButton\n      OnClick = BClick\n    end\n  end\n  object C: TButton\n    OnClick = CClick\n  end\nend\n",
    );
    assert_eq!(extracted.parse_status, FileParseStatus::Parsed);
    assert_eq!(components(&extracted), ["Form1", "Panel1", "B", "C"]);
    let form = component(&extracted, "Form1");
    let panel = component(&extracted, "Panel1");
    let button = component(&extracted, "B");
    let sibling = component(&extracted, "C");
    assert!(
        contains(&extracted, &panel.id, &button.id),
        "an unnamed frame's `end` must not close its named parent"
    );
    assert!(contains(&extracted, &form.id, &sibling.id));
    assert_eq!(button.qualified_name, "Form1::Panel1::B");
    let owners = extracted
        .references
        .iter()
        .map(|reference| {
            (
                reference.name.as_str(),
                reference.owner.clone(),
                reference.resolution_name.as_deref(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        owners,
        [
            (
                "LayoutResize",
                Some(panel.id.clone()),
                Some("TForm1::LayoutResize")
            ),
            ("BClick", Some(button.id.clone()), Some("TForm1::BClick")),
            ("CClick", Some(sibling.id.clone()), Some("TForm1::CClick")),
        ],
        "handlers inside an unnamed frame belong to its nearest named component"
    );
}

#[test]
fn scalar_event_values_and_inline_binary_openers_are_not_handlers() {
    let extracted = extract(
        "src/Form1.dfm",
        "object Form1: TForm1\n  OnTop = True\n  OnLeft = False\n  OnShow = FormShow\n  Picture.Data = {0A0B\n    0C0D\n    0E0F}\n  Items.Strings = ('a'\n    'b')\n  object Button1: TButton\n  end\nend\n",
    );
    assert_eq!(handler_names(&extracted), ["FormShow"]);
    assert_eq!(components(&extracted), ["Form1", "Button1"]);
    assert_eq!(extracted.parse_status, FileParseStatus::Parsed);

    let unfinished = extract(
        "src/Form1.dfm",
        "object Form1: TForm1\n  Picture.Data = {0A0B\n    0C0D\nend\n",
    );
    assert_eq!(
        unfinished.parse_status,
        FileParseStatus::Partial,
        "binary data that never closes leaves the form unbalanced"
    );
}

#[test]
fn form_nesting_honors_the_configured_ast_depth() {
    const DEPTH: usize = 80;
    let mut source = String::new();
    for level in 0..DEPTH {
        writeln!(source, "object C{level}: TPanel")
            .unwrap_or_else(|error| panic!("could not build the fixture: {error}"));
    }
    for _ in 0..DEPTH {
        source.push_str("end\n");
    }
    let snapshot = SourceSnapshot::from_bytes("src/Nest.dfm", source.as_bytes(), limits())
        .unwrap_or_else(|error| panic!("snapshot failed: {error}"));
    let mut bounded = NativeExtractor::new(SourceLanguage::Pascal)
        .and_then(|extractor| extractor.with_maximum_ast_depth(64))
        .unwrap_or_else(|error| panic!("extractor failed: {error}"));
    let limited = bounded
        .extract(&snapshot)
        .unwrap_or_else(|error| panic!("bounded extraction failed: {error}"));
    assert_eq!(limited.parse_status, FileParseStatus::Partial);
    assert!(
        limited
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == DiagnosticCode::NestingLimitExceeded),
        "{:?}",
        limited.diagnostics
    );
    let complete = extract("src/Nest.dfm", &source);
    assert_eq!(components(&complete).len(), DEPTH);
}

#[test]
fn unsupported_component_headers_still_balance_their_frames() {
    let extracted = extract(
        "src/Form1.dfm",
        "object Form1: TForm1\n  object \u{dc}ber: TButton\n    OnClick = UberClick\n  end\n  object B: TButton\n    OnClick = BClick\n  end\nend\n",
    );
    assert_eq!(
        extracted.parse_status,
        FileParseStatus::Partial,
        "a component the scanner cannot represent leaves the form partial"
    );
    assert_eq!(components(&extracted), ["Form1", "B"]);
    let form = component(&extracted, "Form1");
    let button = component(&extracted, "B");
    assert!(
        contains(&extracted, &form.id, &button.id),
        "the unsupported frame's `end` must not close the form"
    );
    let handlers = extracted
        .references
        .iter()
        .map(|reference| {
            (
                reference.name.as_str(),
                reference.owner.clone(),
                reference.resolution_name.as_deref(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        handlers,
        [
            (
                "UberClick",
                Some(form.id.clone()),
                Some("TForm1::UberClick")
            ),
            ("BClick", Some(button.id.clone()), Some("TForm1::BClick")),
        ]
    );
}

#[test]
fn bare_frame_keywords_and_inline_collection_openers_keep_balance() {
    let bare = extract(
        "src/Form1.dfm",
        "object F: TForm1\n  object\n  end\n  object B: TButton\n    OnClick = Click\n  end\nend\n",
    );
    assert_eq!(bare.parse_status, FileParseStatus::Partial);
    assert_eq!(components(&bare), ["F", "B"]);
    let form = component(&bare, "F");
    let button = component(&bare, "B");
    assert!(contains(&bare, &form.id, &button.id));
    assert_eq!(handler_names(&bare), ["Click"]);
    assert_eq!(
        bare.references[0].resolution_name.as_deref(),
        Some("TForm1::Click"),
        "the bare frame's `end` must not make the button a second root"
    );

    let collection = extract(
        "src/Form1.dfm",
        "object F: TForm1\n  Items = <item\n    object Fake: TButton\n      OnClick = FakeClick\n    end\nend\n",
    );
    assert_eq!(
        collection.parse_status,
        FileParseStatus::Partial,
        "a collection opened with its first item never closes here"
    );
    assert_eq!(components(&collection), ["F"]);
    assert_eq!(handler_names(&collection), Vec::<&str>::new());

    let nested = extract(
        "src/Form1.dfm",
        "object F: TForm1
  Items = <item
    Nested = <item end>
    OnClick = FakeClick
  end>
  OnShow = FormShow
end
",
    );
    assert_eq!(nested.parse_status, FileParseStatus::Parsed);
    assert_eq!(
        handler_names(&nested),
        ["FormShow"],
        "a collection closed on its own line leaves the outer one open"
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

fn limits() -> SourceLimits {
    SourceLimits::new(SOURCE_LIMIT).unwrap_or_else(|error| panic!("source limits failed: {error}"))
}

fn components(extracted: &ExtractedFile) -> Vec<&str> {
    extracted
        .symbols
        .iter()
        .filter(|symbol| symbol.kind == SymbolKind::Component)
        .map(|symbol| symbol.name.as_str())
        .collect()
}

fn component<'file>(extracted: &'file ExtractedFile, name: &str) -> &'file ExtractedSymbol {
    extracted
        .symbols
        .iter()
        .find(|symbol| symbol.kind == SymbolKind::Component && symbol.name == name)
        .unwrap_or_else(|| panic!("missing component {name}: {:?}", extracted.symbols))
}

fn handler_names(extracted: &ExtractedFile) -> Vec<&str> {
    extracted
        .references
        .iter()
        .map(|reference| reference.name.as_str())
        .collect()
}

fn contains(extracted: &ExtractedFile, parent: &SymbolId, child: &SymbolId) -> bool {
    extracted
        .containments
        .iter()
        .any(|containment| &containment.parent == parent && &containment.child == child)
}
