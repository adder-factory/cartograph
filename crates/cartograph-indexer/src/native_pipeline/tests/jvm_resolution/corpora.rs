use super::*;

#[test]
fn frozen_v1_correspondences_resolve_to_the_declared_import_target() {
    const CORPUS: &str = "src/main/java/com/acme/shop/service/OrderService.java";
    let facts = generation(&[
        (
            CORPUS,
            include_str!(
                "../../../../../cartograph-extract/tests/fixtures/v1_parity/java/src/main/java/com/acme/shop/service/OrderService.java"
            ),
        ),
        (
            "service/FooConverter.java",
            include_str!(
                "../../../../../cartograph-extract/tests/fixtures/v1_parity/java/src/main/java/com/acme/shop/service/converter/FooConverter.java"
            ),
        ),
        (
            "dao/FooConverter.java",
            include_str!(
                "../../../../../cartograph-extract/tests/fixtures/v1_parity/java/src/main/java/com/acme/shop/dao/converter/FooConverter.java"
            ),
        ),
    ]);
    let caller = capability_symbol(&facts, CORPUS, "com.acme.shop.service::OrderService::find");
    targets(
        CapabilityReferenceQuery::new(&facts, caller)
            .named("fooConverter.convert", ReferenceKind::Calls),
        capability_symbol(
            &facts,
            "service/FooConverter.java",
            "com.acme.shop.service.converter::FooConverter::convert",
        ),
        "native-dynamic-dispatch",
    );
    let field = capability_symbol(
        &facts,
        CORPUS,
        "com.acme.shop.service::OrderService::fooConverter",
    );
    targets(
        CapabilityReferenceQuery::new(&facts, field).named("FooConverter", ReferenceKind::TypeOf),
        capability_symbol(
            &facts,
            "service/FooConverter.java",
            "com.acme.shop.service.converter::FooConverter",
        ),
        "native-jvm-explicit-import",
    );
    assert!(
        CapabilityReferenceQuery::new(&facts, caller)
            .named("cache.get", ReferenceKind::Calls)
            .target_symbol_id
            .is_none()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn packages_and_member_calls_are_invariant_for_all_worker_counts() {
    let fixtures = [
        (
            "src/Java.java",
            "package p; public class Java { public static void go() {} }",
        ),
        (
            "src/Kotlin.kt",
            "package q\nimport p.Java as J\nclass Kotlin { fun run() { J.go() }; fun make() = J() }",
        ),
        (
            "src/Other.java",
            "package other; public class Java { public static void go() {} }",
        ),
    ];
    let directory = super::super::tempdir().unwrap_or_else(|error| panic!("fixture: {error}"));
    super::super::fs::create_dir_all(directory.path().join("src"))
        .unwrap_or_else(|error| panic!("fixture directory: {error}"));
    for (path, source) in fixtures {
        super::super::fs::write(directory.path().join(path), source)
            .unwrap_or_else(|error| panic!("fixture write: {error}"));
    }
    let serial = super::super::build(directory.path(), 1).await;
    let caller = capability_symbol(serial.facts(), "src/Kotlin.kt", "q::Kotlin::run");
    targets(
        CapabilityReferenceQuery::new(serial.facts(), caller).named("J.go", ReferenceKind::Calls),
        capability_symbol(serial.facts(), "src/Java.java", "p::Java::go"),
        "native-qualified-member",
    );
    for workers in [2, 4, 8, 16] {
        let parallel = super::super::build(directory.path(), workers).await;
        assert_eq!(
            serial.facts().digest(),
            parallel.facts().digest(),
            "{workers} workers"
        );
        assert_eq!(serial.report(), parallel.report());
    }
}
