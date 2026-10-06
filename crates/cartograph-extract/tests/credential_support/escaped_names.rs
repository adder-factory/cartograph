//! Escaped-name fixtures compiled only by targets that exercise them.

pub const ESCAPED_NAME_CASES: &[(&str, &str)] = &[
    (
        "named.ts",
        r#"import { "sk_l\u0069ve_FAKE1234567890abcdef" as x } from "./safe";"#,
    ),
    (
        "alias.ts",
        r#"const x=1; export { x as "sk_l\u0069ve_FAKE1234567890abcdef" };"#,
    ),
    (
        "exported.ts",
        r#"export { "sk_l\u0069ve_FAKE1234567890abcdef" as x } from "./safe";"#,
    ),
    (
        "namespace.ts",
        r#"export * as "sk_l\u0069ve_FAKE1234567890abcdef" from "./safe";"#,
    ),
    (
        "destructured.js",
        r#"const { "sk_l\u0069ve_FAKE1234567890abcdef": x } = require("./safe");"#,
    ),
    (
        "literal_type_alias.ts",
        r#"type Safe = "sk_l\u0069ve_FAKE1234567890abcdef";"#,
    ),
    (
        "package.lisp",
        r#"(defpackage "sk_l\ive_FAKE1234567890abcdef")"#,
    ),
    (
        "escaped.lisp",
        r"(defun |sk_l\ive_FAKE1234567890abcdef| () nil)",
    ),
    (
        "designator.lisp",
        r"(defpackage sk_l\ive_FAKE1234567890abcdef)",
    ),
    (
        "application/controllers/Users.php",
        r#"<?php class Users extends CI_Controller { function show() { $this->load->model("glp\x61t-aaaaaaaaaaaaaaaaaaaa", "users"); $this->users->find(); } }"#,
    ),
    (
        "application/controllers/Aliases.php",
        r#"<?php class Aliases extends CI_Controller { function show() { $this->load->model("user_model", "glp\x61t-aaaaaaaaaaaaaaaaaaaa"); $this->users->find(); } }"#,
    ),
    (
        "escaped.graphql",
        r#""sk_l\u0069ve_FAKE1234567890abcdef" type User { id: ID }"#,
    ),
];
