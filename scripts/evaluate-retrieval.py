#!/usr/bin/env python3
"""Measure a labeled repository query set through one persistent native MCP process.

Use an isolated, already indexed and embedded source snapshot. No indexing,
model configuration changes, or service restarts are performed by this script.
"""

import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import selectors
import subprocess
import time


class Mcp:
    def __init__(self, binary, project, stderr):
        self.process = subprocess.Popen(
            [str(binary), "serve", "--mcp", "--profile", "full", "--project-path", str(project),
             "--no-startup-sync", "--no-auto-sync"],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=stderr,
        )
        self.selector = selectors.DefaultSelector()
        self.selector.register(self.process.stdout, selectors.EVENT_READ)
        self.buffer = b""
        self.sequence = 0

    def send(self, message):
        self.process.stdin.write(json.dumps(message).encode() + b"\n")
        self.process.stdin.flush()

    def request(self, method, params):
        self.sequence += 1
        request_id = self.sequence
        self.send({"jsonrpc": "2.0", "id": request_id, "method": method, "params": params})
        deadline = time.monotonic() + 90
        while True:
            while b"\n" in self.buffer:
                line, self.buffer = self.buffer.split(b"\n", 1)
                message = json.loads(line)
                if message.get("id") == request_id:
                    if "error" in message:
                        raise RuntimeError(f"MCP {method} failed: {message['error']}")
                    return message["result"]
            remaining = deadline - time.monotonic()
            if remaining <= 0 or not self.selector.select(remaining):
                raise TimeoutError(f"MCP {method} exceeded 90 seconds")
            chunk = os.read(self.process.stdout.fileno(), 65536)
            if not chunk:
                raise RuntimeError("MCP process closed its output")
            self.buffer += chunk
            if len(self.buffer) > 16 * 1024 * 1024:
                raise RuntimeError("MCP response exceeded the measurement bound")

    def tool(self, name, arguments):
        result = self.request("tools/call", {"name": name, "arguments": arguments})
        if result.get("isError"):
            raise RuntimeError(f"{name} returned an error")
        if isinstance(result.get("structuredContent"), dict):
            return result["structuredContent"]
        for content in result.get("content", []):
            if content.get("type") == "text":
                return json.loads(content["text"])
        raise RuntimeError(f"{name} returned no structured evidence")

    def close(self):
        self.process.stdin.close()
        try:
            self.process.wait(timeout=15)
        except subprocess.TimeoutExpired:
            self.process.terminate()
            try:
                self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=5)
        self.selector.close()
        self.process.stdout.close()


def items_from(result):
    evidence = result.get("evidence", result)
    if isinstance(evidence, dict) and "packet" in evidence:
        evidence = evidence["packet"]
    if not isinstance(evidence, dict):
        raise RuntimeError("unrecognized evidence envelope")
    return evidence.get("items", evidence.get("rows", [])), evidence


def identity(item):
    document = item.get("document", item)
    return {
        "path": document.get("path"),
        "symbol": document.get("qualified_name", document.get("name")),
        "symbol_kind": document.get("symbol_kind", document.get("symbolKind", document.get("kind"))),
    }


def correct(item, targets):
    actual = identity(item)
    return any(actual["path"] == target["path"] and actual["symbol"] == target["symbol"]
               for target in targets)


def percentile(values, quantile):
    ordered = sorted(values)
    return ordered[max(0, math.ceil(quantile * len(ordered)) - 1)] if ordered else None


def file_sha256(path):
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def summarize(observations):
    count = len(observations)
    if not count:
        return {"count": 0}
    ranks = [row["first_correct_rank"] for row in observations]
    elapsed = [row["elapsed_ms"] for row in observations]
    return {
        "count": count,
        "hit_at_5": sum(rank is not None and rank <= 5 for rank in ranks) / count,
        "mean_reciprocal_rank": sum(1 / rank for rank in ranks if rank is not None) / count,
        "returned_pool_target_recall": sum(rank is not None for rank in ranks) / count,
        "latency_p50_ms": percentile(elapsed, 0.5),
        "latency_p95_ms": percentile(elapsed, 0.95),
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--project", type=Path, required=True)
    parser.add_argument("--cases", type=Path, default=Path(__file__).resolve().parents[1] /
                        "crates/cartograph-agent/tests/fixtures/retrieval_quality/cases.json")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--label", required=True)
    parser.add_argument("--repeat", type=int, choices=range(1, 11), default=1)
    parser.add_argument("--limit", type=int, choices=range(5, 101), default=40)
    args = parser.parse_args()
    if args.output.exists():
        raise ValueError("measurement output already exists")
    with args.cases.open("rb") as source:
        manifest_bytes = source.read(256 * 1024 + 1)
    if len(manifest_bytes) > 256 * 1024:
        raise ValueError("case manifest is too large")
    manifest = json.loads(manifest_bytes)
    cases = manifest["cases"]
    if manifest["schema_version"] != 1 or not 1 <= len(cases) <= 256:
        raise ValueError("unsupported case manifest")
    if len({case["id"] for case in cases}) != len(cases):
        raise ValueError("duplicate case identities")
    for case in cases:
        if case["split"] not in {"development", "held_out"} or not case["targets"]:
            raise ValueError("case requires a split and explicit symbol/path targets")
        if not isinstance(case["query"], str) or not 1 <= len(case["query"].encode()) <= 4096:
            raise ValueError("case query is invalid")
    args.output.parent.mkdir(parents=True, exist_ok=True)
    report = {
        "label": args.label, "complete": False, "binary_sha256": file_sha256(args.binary),
        "runner_sha256": file_sha256(Path(__file__)),
        "cases_sha256": hashlib.sha256(manifest_bytes).hexdigest(),
        "method": {"transport": "persistent MCP stdio, sequential warm requests",
                   "limit": args.limit, "repeat": args.repeat, "correctness": "exact symbol and path",
                   "recall": "target present in the returned bounded pool; not corpus-wide recall",
                   "percentiles": "nearest rank", "models": "operator configured; no configuration edits"},
        "observations": [],
    }
    stderr_path = args.output.with_suffix(".stderr")
    with stderr_path.open("xb") as stderr:
        os.chmod(stderr_path, 0o600)
        client = Mcp(args.binary.resolve(), args.project.resolve(), stderr)
        try:
            report["server"] = client.request("initialize", {
                "protocolVersion": "2024-11-05", "capabilities": {},
                "clientInfo": {"name": "cartograph-retrieval-evaluation", "version": "1"},
            })
            client.send({"jsonrpc": "2.0", "method": "notifications/initialized"})
            client.request("tools/list", {})
            project = {"projectPath": str(args.project.resolve())}
            report["initial_status"] = client.tool("cartograph_status", project)
            status = report["initial_status"]["project"]
            if status["fresh"] is not True:
                raise ValueError("measurement requires a fresh source snapshot")
            generation = status["snapshot"]["current"]["generation_id"]
            # Check labels before observing ranks. Incorrect/moved identities invalidate a case.
            invalid = []
            report["target_coverage"] = {}
            for case in cases:
                matched = False
                for symbol in dict.fromkeys(target["symbol"] for target in case["targets"]):
                    rows, _ = items_from(client.tool("cartograph_find", {
                        **project, "by": "name", "mode": "exact", "query": symbol,
                        "limit": 100, "lowTokens": True,
                    }))
                    matched = matched or any(correct(row, case["targets"]) for row in rows)
                if not matched and not case.get("allow_unindexed", False):
                    invalid.append(case["id"])
                report["target_coverage"][case["id"]] = matched
            if invalid:
                raise ValueError(f"target labels absent from snapshot: {', '.join(invalid)}")
            for mode in ["bm25", "hybrid"]:
                client.tool("cartograph_find", {**project, "by": mode, "query": cases[0]["query"],
                                                "limit": args.limit, "lowTokens": True})
            for repetition in range(args.repeat):
                for case in cases:
                    for mode in ["bm25", "hybrid"]:
                        started = time.perf_counter()
                        result = client.tool("cartograph_find", {**project, "by": mode,
                            "query": case["query"], "limit": args.limit, "lowTokens": True})
                        elapsed = (time.perf_counter() - started) * 1000
                        items, evidence = items_from(result)
                        observed_generation = evidence.get("generationId", evidence.get("generation_id"))
                        if result.get("freshness") != "current" or (items and observed_generation != generation):
                            raise ValueError("measurement source or generation changed")
                        readiness = evidence.get("semanticReadiness", evidence.get("semantic_readiness"))
                        if mode == "hybrid" and readiness != "ready":
                            raise ValueError("hybrid measurement requires complete semantic readiness")
                        rank = next((index + 1 for index, item in enumerate(items)
                                     if correct(item, case["targets"])), None)
                        report["observations"].append({"case": case["id"], "split": case["split"],
                            "mode": mode, "repetition": repetition, "elapsed_ms": elapsed,
                            "first_correct_rank": rank, "returned_count": len(items),
                            "freshness": result.get("freshness"), "generation_id": observed_generation,
                            "semantic_readiness": readiness,
                            "rerank": evidence.get("rerank"), "fallback": evidence.get("fallback"),
                            "items": [identity(item) for item in items]})
                    print(f"{args.label}: {repetition + 1}/{args.repeat} {case['id']}", flush=True)
            report["final_status"] = client.tool("cartograph_status", project)
            final = report["final_status"]["project"]
            if final["fresh"] is not True or final["snapshot"]["current"]["generation_id"] != generation:
                raise ValueError("measurement ended on a changed source generation")
            report["complete"] = True
        finally:
            client.close()
            report["summary"] = {
                f"{mode}/{split}": summarize([row for row in report["observations"]
                    if row["mode"] == mode and (split == "all" or row["split"] == split)])
                for mode in ["bm25", "hybrid"] for split in ["all", "development", "held_out"]
            }
            with args.output.open("x") as output:
                os.chmod(args.output, 0o600)
                json.dump(report, output, indent=2)
                output.write("\n")
    print(json.dumps(report["summary"], indent=2))


if __name__ == "__main__":
    main()
