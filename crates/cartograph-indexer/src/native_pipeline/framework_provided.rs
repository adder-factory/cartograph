//! Compiler and virtual-module names remain explicit targetless boundaries.
//! This classification runs only after native bindings have had their turn.
use super::{
    ReferenceKind, ResolutionRequest, StageItemFailure, binding_matches_reference_name,
    javascript_modules,
};

pub(super) const PROVENANCE: &str = "native-framework-provided";
const VUE_MACROS: &[&str] = &[
    "defineProps",
    "defineEmits",
    "defineExpose",
    "defineOptions",
    "defineSlots",
    "defineModel",
    "withDefaults",
];
const NUXT_IMPORTS: &[&str] = &[
    "abortNavigation",
    "clearError",
    "clearNuxtState",
    "createError",
    "defineNuxtConfig",
    "defineNuxtPlugin",
    "defineNuxtRouteMiddleware",
    "definePageMeta",
    "navigateTo",
    "refreshNuxtData",
    "showError",
    "useAppConfig",
    "useAsyncData",
    "useCookie",
    "useError",
    "useFetch",
    "useHead",
    "useLazyAsyncData",
    "useLazyFetch",
    "useNuxtApp",
    "useRequestEvent",
    "useRequestFetch",
    "useRequestHeaders",
    "useRequestURL",
    "useRoute",
    "useRouter",
    "useRuntimeConfig",
    "useSeoMeta",
    "useServerSeoMeta",
    "useState",
];
const SVELTE_RUNES: &[&str] = &[
    "$props",
    "$state",
    "$state.raw",
    "$state.snapshot",
    "$derived",
    "$derived.by",
    "$effect",
    "$effect.pre",
    "$effect.root",
    "$effect.tracking",
    "$bindable",
    "$inspect",
    "$inspect.trace",
    "$host",
    "$snippet",
];
const VIRTUAL_MODULES: &[&str] = &[
    "#imports",
    "#components",
    "#app",
    "#build",
    "#head",
    "$app",
    "$env",
];

pub(super) fn provenance<Cancel>(
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<Option<&'static str>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if !javascript_modules::module_language(request.language)
        || request.import_bindings.fallback_blocked
    {
        return Ok(None);
    }
    let name = request.name;
    let provided = match request.kind {
        ReferenceKind::Imports => virtual_module(name),
        ReferenceKind::Calls | ReferenceKind::References => {
            VUE_MACROS.contains(&name)
                || NUXT_IMPORTS.contains(&name)
                || SVELTE_RUNES.contains(&name)
        }
        _ => false,
    };
    if !provided || has_other_binding(request, cancelled)? {
        return Ok(None);
    }
    Ok(Some(PROVENANCE))
}

fn virtual_module(name: &str) -> bool {
    VIRTUAL_MODULES.iter().any(|prefix| {
        name == *prefix
            || name
                .strip_prefix(prefix)
                .is_some_and(|tail| tail.starts_with('/'))
    })
}

fn has_other_binding<Cancel>(
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<bool, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    for binding in request.import_bindings.iter() {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if binding_matches_reference_name(binding, request.name)
            && !virtual_module(&binding.module_specifier)
        {
            return Ok(true);
        }
    }
    Ok(false)
}
