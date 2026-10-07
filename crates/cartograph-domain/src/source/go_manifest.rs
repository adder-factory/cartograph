use super::SourceLanguage;

pub(super) fn language(path: &str) -> Option<SourceLanguage> {
    (path.rsplit('/').next() == Some("go.mod")).then_some(SourceLanguage::Go)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_manifests_are_native_additions_and_do_not_enter_v1_import() {
        for path in ["go.mod", "nested/go.mod"] {
            assert_eq!(SourceLanguage::detect(path, None), Some(SourceLanguage::Go));
            assert!(SourceLanguage::is_native_candidate_path(path));
            assert!(!SourceLanguage::is_v1_candidate_path(path));
        }
        assert_eq!(SourceLanguage::detect("not-go.mod", None), None);
    }
}
