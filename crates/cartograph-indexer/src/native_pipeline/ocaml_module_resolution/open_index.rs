//! Compile root-open namespaces and activation positions once per source file.
use super::super::{ResolveBudget, SymbolId};
use super::{
    FileEvidence, FileId, HashMap, RESOLUTION_MAP_NODE_ALLOWANCE, ResolutionCandidate,
    ResolutionIndex, StageItemFailure, SymbolKind, size_of, try_clone_text, usize_to_u64,
};

type Members<'index> = HashMap<&'index str, Vec<&'index ResolutionCandidate>>;

pub(super) struct Namespaces<'index> {
    files: HashMap<&'index FileId, Members<'index>>,
}

impl<'index> Namespaces<'index> {
    pub(super) fn build<Cancel>(
        index: &'index ResolutionIndex,
        budget: &mut ResolveBudget,
        cancelled: &mut Cancel,
    ) -> Result<Self, StageItemFailure>
    where
        Cancel: FnMut() -> bool,
    {
        let mut namespaces = Self {
            files: HashMap::new(),
        };
        for (key, bucket) in &index.candidates {
            for candidate in bucket.as_slice() {
                if cancelled() {
                    return Err(StageItemFailure);
                }
                if key == &candidate.qualified_name
                    && matches!(candidate.kind, SymbolKind::Module | SymbolKind::Function)
                    && index
                        .languages
                        .ocaml_modules
                        .files
                        .contains_key(&candidate.file_id)
                {
                    namespaces.record(candidate, budget)?;
                }
            }
        }
        Ok(namespaces)
    }

    fn record(
        &mut self,
        candidate: &'index ResolutionCandidate,
        budget: &mut ResolveBudget,
    ) -> Result<(), StageItemFailure> {
        let prefix = candidate
            .qualified_name
            .rsplit_once('.')
            .map_or("", |(prefix, _)| prefix);
        let members = self.namespace((&candidate.file_id, prefix), budget)?;
        budget.charge(usize_to_u64(size_of::<&ResolutionCandidate>()))?;
        members.try_reserve(1).map_err(|_| StageItemFailure)?;
        members.push(candidate);
        if candidate.kind == SymbolKind::Module {
            self.namespace((&candidate.file_id, &candidate.qualified_name), budget)?;
        }
        Ok(())
    }

    fn namespace(
        &mut self,
        key: (&'index FileId, &'index str),
        budget: &mut ResolveBudget,
    ) -> Result<&mut Vec<&'index ResolutionCandidate>, StageItemFailure> {
        if !self.files.contains_key(key.0) {
            budget.charge(
                RESOLUTION_MAP_NODE_ALLOWANCE + usize_to_u64(size_of::<(&FileId, Members<'_>)>()),
            )?;
            self.files.try_reserve(1).map_err(|_| StageItemFailure)?;
            self.files.insert(key.0, HashMap::new());
        }
        let file = self.files.get_mut(key.0).ok_or(StageItemFailure)?;
        if !file.contains_key(key.1) {
            budget.charge(
                RESOLUTION_MAP_NODE_ALLOWANCE
                    + usize_to_u64(size_of::<(&str, Vec<&ResolutionCandidate>)>()),
            )?;
            file.try_reserve(1).map_err(|_| StageItemFailure)?;
            file.insert(key.1, Vec::new());
        }
        file.get_mut(key.1).ok_or(StageItemFailure)
    }

    fn get(&self, key: (&FileId, &str)) -> Option<&[&'index ResolutionCandidate]> {
        self.files
            .get(key.0)
            .and_then(|file| file.get(key.1))
            .map(Vec::as_slice)
    }
}

#[derive(Default)]
pub(super) struct OpenIndex {
    functions: HashMap<String, Vec<FunctionEvent>>,
    modules: HashMap<String, Vec<u64>>,
    values: HashMap<String, u64>,
    unknown_at: Option<u64>,
}

struct FunctionEvent {
    position: u64,
    symbol_id: SymbolId,
    ambiguous: bool,
}

impl OpenIndex {
    pub(super) fn prepare<'index, Cancel>(
        index: &'index ResolutionIndex,
        file: (&'index FileId, &'index FileEvidence),
        work: (&Namespaces<'index>, &mut ResolveBudget, &mut Cancel),
    ) -> Result<Self, StageItemFailure>
    where
        Cancel: FnMut() -> bool,
    {
        let (namespaces, budget, cancelled) = work;
        let mut prepared = Self::default();
        let opens = prepared.collect_opens(index, (file.0, file.1, budget, cancelled))?;
        prepared.record_values(index, (&opens, budget, cancelled))?;
        for ((target, prefix), position) in opens {
            if cancelled() {
                return Err(StageItemFailure);
            }
            let members = namespaces.get((target, prefix));
            if members.is_none() && !prefix.is_empty() {
                prepared.record_unknown(position);
                continue;
            }
            for member in members.unwrap_or_default() {
                if cancelled() {
                    return Err(StageItemFailure);
                }
                prepared.record_member((file.0, member, position), budget)?;
            }
        }
        prepared.finish(cancelled)?;
        prepared.fence_open_qualifiers(file.1, cancelled)?;
        Ok(prepared)
    }

    fn record_values<'index, Cancel>(
        &mut self,
        index: &'index ResolutionIndex,
        work: (
            &HashMap<(&'index FileId, &str), u64>,
            &mut ResolveBudget,
            &mut Cancel,
        ),
    ) -> Result<(), StageItemFailure>
    where
        Cancel: FnMut() -> bool,
    {
        let (opens, budget, cancelled) = work;
        let files = value_open_files(opens, budget, cancelled)?;
        for (file, position) in files {
            if cancelled() {
                return Err(StageItemFailure);
            }
            let evidence = index
                .languages
                .ocaml_modules
                .files
                .get(file)
                .ok_or(StageItemFailure)?;
            for name in &evidence.value_fences {
                if cancelled() {
                    return Err(StageItemFailure);
                }
                if name == "*" {
                    self.record_unknown(position);
                } else {
                    self.record_value((name, position), budget)?;
                }
            }
        }
        Ok(())
    }

    fn record_value(
        &mut self,
        event: (&str, u64),
        budget: &mut ResolveBudget,
    ) -> Result<(), StageItemFailure> {
        if let Some(prior) = self.values.get_mut(event.0) {
            *prior = (*prior).min(event.1);
            return Ok(());
        }
        budget.charge(
            RESOLUTION_MAP_NODE_ALLOWANCE
                + usize_to_u64(size_of::<(String, u64)>() + event.0.len()),
        )?;
        self.values.try_reserve(1).map_err(|_| StageItemFailure)?;
        self.values.insert(try_clone_text(event.0)?, event.1);
        Ok(())
    }

    fn fence_open_qualifiers<Cancel>(
        &mut self,
        evidence: &FileEvidence,
        cancelled: &mut Cancel,
    ) -> Result<(), StageItemFailure>
    where
        Cancel: FnMut() -> bool,
    {
        for (name, position) in &evidence.opens {
            if cancelled() {
                return Err(StageItemFailure);
            }
            let root = name.split('.').next().unwrap_or_default();
            if evidence.fences.contains(root) || self.shadowed(root, *position) {
                self.record_unknown(*position);
            }
        }
        Ok(())
    }

    fn collect_opens<'index, Cancel>(
        &mut self,
        index: &'index ResolutionIndex,
        file: (
            &'index FileId,
            &'index FileEvidence,
            &mut ResolveBudget,
            &mut Cancel,
        ),
    ) -> Result<HashMap<(&'index FileId, &'index str), u64>, StageItemFailure>
    where
        Cancel: FnMut() -> bool,
    {
        let (file_id, evidence, budget, cancelled) = file;
        let mut opens = HashMap::<(&FileId, &str), u64>::new();
        for (module, position) in &evidence.opens {
            if cancelled() {
                return Err(StageItemFailure);
            }
            let Some(key) = super::scope(index, (file_id, module, *position), cancelled)? else {
                self.record_unknown(*position);
                continue;
            };
            if let Some(prior) = opens.get_mut(&key) {
                *prior = (*prior).min(*position);
                continue;
            }
            budget.charge(
                RESOLUTION_MAP_NODE_ALLOWANCE + usize_to_u64(size_of::<((&FileId, &str), u64)>()),
            )?;
            opens.try_reserve(1).map_err(|_| StageItemFailure)?;
            opens.insert(key, *position);
        }
        Ok(opens)
    }

    fn record_unknown(&mut self, position: u64) {
        self.unknown_at = Some(
            self.unknown_at
                .map_or(position, |prior| prior.min(position)),
        );
    }

    fn record_member(
        &mut self,
        query: (&FileId, &ResolutionCandidate, u64),
        budget: &mut ResolveBudget,
    ) -> Result<(), StageItemFailure> {
        let (source, candidate, position) = query;
        let name = candidate
            .qualified_name
            .rsplit('.')
            .next()
            .unwrap_or_default();
        if candidate.kind == SymbolKind::Module {
            return self.record_module((name, position), budget);
        }
        self.record_function(
            (
                name,
                candidate,
                if source == &candidate.file_id {
                    position.max(candidate.declaration_span.0)
                } else {
                    position
                },
            ),
            budget,
        )
    }

    fn record_module(
        &mut self,
        event: (&str, u64),
        budget: &mut ResolveBudget,
    ) -> Result<(), StageItemFailure> {
        budget.charge(
            RESOLUTION_MAP_NODE_ALLOWANCE
                + usize_to_u64(size_of::<(String, Vec<u64>)>() + event.0.len()),
        )?;
        self.modules.try_reserve(1).map_err(|_| StageItemFailure)?;
        let positions = self.modules.entry(try_clone_text(event.0)?).or_default();
        positions.try_reserve(1).map_err(|_| StageItemFailure)?;
        positions.push(event.1);
        Ok(())
    }

    fn record_function(
        &mut self,
        event: (&str, &ResolutionCandidate, u64),
        budget: &mut ResolveBudget,
    ) -> Result<(), StageItemFailure> {
        budget.charge(
            RESOLUTION_MAP_NODE_ALLOWANCE
                + usize_to_u64(
                    size_of::<FunctionEvent>() + event.0.len() + event.1.symbol_id.as_str().len(),
                ),
        )?;
        self.functions
            .try_reserve(1)
            .map_err(|_| StageItemFailure)?;
        let events = self.functions.entry(try_clone_text(event.0)?).or_default();
        events.try_reserve(1).map_err(|_| StageItemFailure)?;
        events.push(FunctionEvent {
            position: event.2,
            symbol_id: event.1.symbol_id.clone(),
            ambiguous: false,
        });
        Ok(())
    }

    fn finish<Cancel>(&mut self, cancelled: &mut Cancel) -> Result<(), StageItemFailure>
    where
        Cancel: FnMut() -> bool,
    {
        for positions in self.modules.values_mut() {
            if cancelled() {
                return Err(StageItemFailure);
            }
            positions.sort_unstable();
            positions.dedup();
        }
        for events in self.functions.values_mut() {
            if cancelled() {
                return Err(StageItemFailure);
            }
            events.sort_unstable_by(|left, right| {
                left.position
                    .cmp(&right.position)
                    .then(left.symbol_id.cmp(&right.symbol_id))
            });
            let Some((first, rest)) = events.split_first_mut() else {
                continue;
            };
            let mut ambiguous = false;
            for event in rest {
                if cancelled() {
                    return Err(StageItemFailure);
                }
                ambiguous |= event.symbol_id != first.symbol_id;
                event.ambiguous = ambiguous;
            }
        }
        Ok(())
    }

    pub(super) fn shadowed(&self, name: &str, position: u64) -> bool {
        self.unknown_before(position)
            || self
                .modules
                .get(name)
                .and_then(|positions| positions.first())
                .is_some_and(|first| *first <= position)
    }

    pub(super) fn function(&self, name: &str, position: u64) -> Option<&SymbolId> {
        if self.unknown_before(position)
            || self
                .values
                .get(name)
                .is_some_and(|shadow| *shadow <= position)
        {
            return None;
        }
        let events = self.functions.get(name)?;
        let event = events
            .partition_point(|event| event.position <= position)
            .checked_sub(1)?;
        events
            .get(event)
            .filter(|event| !event.ambiguous)
            .map(|event| &event.symbol_id)
    }

    fn unknown_before(&self, position: u64) -> bool {
        self.unknown_at.is_some_and(|unknown| unknown <= position)
    }
}

fn value_open_files<'index, Cancel>(
    opens: &HashMap<(&'index FileId, &str), u64>,
    budget: &mut ResolveBudget,
    cancelled: &mut Cancel,
) -> Result<HashMap<&'index FileId, u64>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let mut files = HashMap::<&FileId, u64>::new();
    for ((file, _), position) in opens {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if let Some(prior) = files.get_mut(file) {
            *prior = (*prior).min(*position);
            continue;
        }
        budget.charge(RESOLUTION_MAP_NODE_ALLOWANCE + usize_to_u64(size_of::<(&FileId, u64)>()))?;
        files.try_reserve(1).map_err(|_| StageItemFailure)?;
        files.insert(file, *position);
    }
    Ok(files)
}
