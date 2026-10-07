pub(super) fn member_language(language: &str) -> bool {
    super::super::javascript_family_name(language)
        || matches!(language, "arkts" | "astro" | "vue" | "svelte")
}
