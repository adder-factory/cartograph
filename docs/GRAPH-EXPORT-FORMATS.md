# Graph export formats

[Documentation home](README.md) · [Project overview](../README.md) ·
[CLI reference](CLI-REFERENCE.md) · [Architecture](v2/ARCHITECTURE.md)

The browser visual-graph viewer is not part of v2, but graph data and diagram
interchange remain first-class. `cartograph export` reads one current-generation
snapshot, applies filters, caps nodes, and removes edges whose endpoints are not
in the exported set.

**On this page:** [Usage](#usage) · [Versioned JSON](#versioned-json) ·
[Cytoscape JSON](#cytoscape-json) · [DOT and Mermaid](#dot-and-mermaid) ·
[Limits and filters](#limits-and-filters) · [SCIP interchange](#scip-interchange)

## Usage

```sh
cartograph export --format json --limit 1000
cartograph export --format dot --kind class,method --edge-kind calls --out graph.dot
cartograph export --format mermaid --file src/billing --out billing.mmd
cartograph export --format cytoscape --language typescript --out graph.cy.json
```

> [!IMPORTANT]
> `export` requires a fresh current generation and has no `--allow-stale`
> override: a stale index fails with "synchronize it with `cartograph index`
> and retry". Index the project first.

## Versioned JSON

JSON and Cytoscape exports use `formatVersion: 1`. Optional fields may be added
within a version; renaming/removing fields or changing their meaning requires a
new version. Only the JSON format carries `generationId` and `files`.

```text
{
  formatVersion: 1,
  generationId: string,
  filters: {
    kinds: string[],
    edgeKinds: string[],
    languages: string[],
    filePrefix?: string
  },
  stats: {
    totalNodes: number,
    totalEdges: number,
    exportedNodes: number,
    exportedEdges: number,
    exportedFiles: number,
    truncatedNodes: number
  },
  nodes: GraphExportNode[],
  edges: GraphExportEdge[],
  files: GraphExportFile[]
}
```

| Record | Fields |
| --- | --- |
| Node | `id`, `kind`, `name`, `qualifiedName`, `signature`, `filePath`, `language`, `startLine`, `endLine` |
| Edge | `source`, `target`, `kind`, numeric `confidence`, `provenance`, and represented `siteCount` |
| File | `path`, `language`, `nodeCount` |

## Cytoscape JSON

Cytoscape output wraps the same node data under `elements.nodes[].data` and
adds deterministic edge IDs/labels under `elements.edges[].data`. Its shape
differs from the JSON format:

```text
{
  formatVersion: 1,
  metadata: {
    filters: { kinds, edgeKinds, languages, filePrefix? },
    stats: { totalNodes, totalEdges, exportedNodes, exportedEdges, exportedFiles, truncatedNodes }
  },
  elements: {
    nodes: [ { data: GraphExportNode } ],
    edges: [ { data: { id, source, target, kind, label, confidence, provenance, site_count } } ]
  }
}
```

- There is no `generationId` and no `files` list.
- `filters` and `stats` sit under `metadata`, so truncation is
  `metadata.stats.truncatedNodes`.
- Edge data uses the snake_case `site_count`, not `siteCount`.

## DOT and Mermaid

DOT and Mermaid are presentation formats; their labels may improve without
changing the selected membership semantics.

## Limits and filters

The default cap is 1,000 nodes and the accepted maximum is 50,000. Filters are
exact comma-separated kind/edge/language values, with a normalized
project-relative file prefix. `--kind` accepts every stored symbol kind
(including `union`) and `--edge-kind` every stored edge kind, in their
underscore spellings (for example `type_of` or `field_access`); an unsupported
value fails with the allowed list. `stats.truncatedNodes` (Cytoscape:
`metadata.stats.truncatedNodes`) and the JSON generation ID make partial/stale
downstream interpretation explicit.

## SCIP interchange

For standardized code-intelligence interchange, use `cartograph admin
scip-export`/`scip-import`. SCIP retains definitions, occurrences,
documentation, relationships, and Cartograph's forward-compatible exact typed
edge/site-count extension.
