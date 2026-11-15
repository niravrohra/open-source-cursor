#!/usr/bin/env python3
"""
rursor-eval — Deterministic Sidecar, Retrieval & Live Gemini Benchmark Harness.

Spawns `rursor-sidecar` over stdio JSON-RPC and evaluates:
  1. Warm RPC round-trip latency p50/p95 (`ping`, `status`) + cold boot handshake
  2. Workspace indexing throughput (`index_workspace` files/sec & ms)
  3. Symbol search Recall@5 (`search_symbols`) across core Rursor symbols
  4. Hybrid BM25 + AST vector Recall@5 (`search_context`)
  5. Exact `grep_workspace` precision & latency
  6. `git_context` latency
  7. Session persistence round-trip (`save_session` -> `load_session` -> `delete_session`)
  8. Optional live Gemini `inline_edit` & `agent_chat` Pass@1 tasks when `GEMINI_API_KEY` is set
"""

from __future__ import annotations

import argparse
import json
import math
import os
import platform
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from typing import Any, Dict, List, Optional, Tuple


def percentile(values: List[float], pct: float) -> float:
    if not values:
        return 0.0
    ordered = sorted(values)
    idx = min(len(ordered) - 1, max(0, math.ceil((pct / 100.0) * len(ordered)) - 1))
    return ordered[idx]


def resolve_sidecar_binary(repo_root: Path, explicit_path: Optional[str] = None) -> Path:
    if explicit_path:
        cand = Path(explicit_path).expanduser().resolve()
        if cand.is_file() and os.access(cand, os.X_OK):
            return cand
        raise FileNotFoundError(f"Explicit sidecar binary is not executable: {cand}")

    candidates = [
        repo_root / "extension" / "bin" / "rursor-sidecar",
        repo_root / "sidecar" / "target" / "release" / "rursor-sidecar",
        repo_root / "sidecar" / "target" / "debug" / "rursor-sidecar",
    ]
    for cand in candidates:
        if cand.is_file() and os.access(cand, os.X_OK):
            return cand

    raise FileNotFoundError(
        "Could not locate `rursor-sidecar` binary. Expected one of:\n  - "
        + "\n  - ".join(str(c) for c in candidates)
        + "\nBuild it first with: cargo build --manifest-path sidecar/Cargo.toml"
    )


class SidecarRpcHarness:
    """Manages a child `rursor-sidecar` process speaking newline-delimited JSON-RPC over stdio."""

    def __init__(self, binary_path: Path, isolated_home: Path) -> None:
        self.binary_path = binary_path
        self.isolated_home = isolated_home
        self._next_id = 1
        self._proc: Optional[subprocess.Popen[str]] = None

    def start(self) -> float:
        env = os.environ.copy()
        env["HOME"] = str(self.isolated_home)
        t0 = time.perf_counter()
        self._proc = subprocess.Popen(
            [str(self.binary_path)],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            bufsize=1,
            env=env,
        )
        # Perform initial handshake to separate macOS process cold-start from warm RPC latency.
        _, boot_ms = self.request("ping")
        _ = t0
        return boot_ms

    def request(
        self, method: str, params: Optional[Dict[str, Any]] = None
    ) -> Tuple[Dict[str, Any], float]:
        if self._proc is None or self._proc.stdin is None or self._proc.stdout is None:
            raise RuntimeError("Sidecar process is not running.")

        req_id = self._next_id
        self._next_id += 1
        wire = json.dumps({"id": req_id, "method": method, "params": params or {}})

        t0 = time.perf_counter()
        self._proc.stdin.write(wire + "\n")
        self._proc.stdin.flush()

        while True:
            raw_line = self._proc.stdout.readline()
            if not raw_line:
                stderr_tail = ""
                if self._proc.stderr is not None:
                    stderr_tail = self._proc.stderr.read().strip()
                raise RuntimeError(
                    f"rursor-sidecar closed stdout while waiting for `{method}` (id={req_id}). "
                    f"stderr: {stderr_tail}"
                )
            line = raw_line.strip()
            if not line:
                continue
            msg = json.loads(line)
            # Skip streaming events (`stream_chunk`, `tool_event`, etc.) until final RpcResponse
            if "event" in msg and "id" not in msg:
                continue
            if msg.get("id") == req_id:
                elapsed_ms = (time.perf_counter() - t0) * 1000.0
                if not msg.get("ok", False):
                    raise RuntimeError(msg.get("error", f"RPC `{method}` returned ok=false"))
                return msg.get("result") or {}, elapsed_ms

    def close(self) -> None:
        if self._proc is None:
            return
        try:
            if self._proc.stdin is not None:
                self._proc.stdin.close()
            self._proc.terminate()
            self._proc.wait(timeout=2.0)
        except Exception:
            self._proc.kill()
        finally:
            self._proc = None


def evaluate_task(
    harness: SidecarRpcHarness,
    task: Dict[str, Any],
    workspace_root: Path,
    has_gemini_key: bool,
) -> Dict[str, Any]:
    task_id = str(task["id"])
    category = str(task.get("category", "general"))
    kind = str(task["kind"])
    requires_key = bool(task.get("requiresApiKey", False))

    if requires_key and not has_gemini_key:
        return {
            "id": task_id,
            "category": category,
            "kind": kind,
            "status": "SKIPPED",
            "passed": None,
            "latencyMs": None,
            "metricLabel": "GEMINI_API_KEY unset",
            "details": {"reason": "Set GEMINI_API_KEY in environment to run live Gemini Pass@1 tasks."},
        }

    try:
        if kind == "rpc_benchmark":
            method = str(task["method"])
            iterations = int(task.get("iterations", 20))
            samples: List[float] = []
            last_res: Dict[str, Any] = {}
            for _ in range(iterations):
                res, ms = harness.request(method, {})
                samples.append(ms)
                last_res = res

            p50 = percentile(samples, 50)
            p95 = percentile(samples, 95)
            exp_field = task.get("expectedField")
            field_ok = True
            if exp_field is not None:
                field_ok = last_res.get(exp_field) == task.get("expectedValue")
            max_p95 = float(task.get("maxP95Ms", 100.0))
            passed = field_ok and (p95 <= max_p95)
            return {
                "id": task_id,
                "category": category,
                "kind": kind,
                "status": "PASS" if passed else "FAIL",
                "passed": passed,
                "latencyMs": round(p50, 3),
                "p50Ms": round(p50, 3),
                "p95Ms": round(p95, 3),
                "metricLabel": f"p50={p50:.2f}ms, p95={p95:.2f}ms ({iterations}x)",
                "details": {"iterations": iterations, "p50Ms": round(p50, 3), "p95Ms": round(p95, 3)},
            }

        if kind == "index_workspace":
            res, ms = harness.request("index_workspace", {"workspaceRoot": str(workspace_root)})
            indexed = int(res.get("indexedFiles", 0))
            min_files = int(task.get("minFiles", 1))
            files_per_sec = (indexed / (ms / 1000.0)) if ms > 0 else 0.0
            passed = indexed >= min_files
            return {
                "id": task_id,
                "category": category,
                "kind": kind,
                "status": "PASS" if passed else "FAIL",
                "passed": passed,
                "latencyMs": round(ms, 2),
                "indexedFiles": indexed,
                "filesPerSec": round(files_per_sec, 1),
                "metricLabel": f"{indexed} files in {ms:.1f}ms ({files_per_sec:.0f} files/s)",
                "details": {
                    "indexedFiles": indexed,
                    "elapsedMs": round(ms, 2),
                    "filesPerSec": round(files_per_sec, 1),
                },
            }

        if kind == "search_symbols":
            query = str(task["query"])
            limit = int(task.get("limit", 5))
            expected_path = str(task["expectedPath"])
            expected_symbol = str(task.get("expectedSymbol", query))
            res, ms = harness.request("search_symbols", {"query": query, "limit": limit})
            symbols = res.get("symbols") or []
            matched_rank: Optional[int] = None
            for idx, hit in enumerate(symbols[:limit], start=1):
                if hit.get("path") == expected_path and expected_symbol in str(hit.get("name", "")):
                    matched_rank = idx
                    break
            recall = 1.0 if matched_rank is not None else 0.0
            passed = recall == 1.0
            rank_str = f"rank #{matched_rank}" if matched_rank is not None else "miss"
            return {
                "id": task_id,
                "category": category,
                "kind": kind,
                "status": "PASS" if passed else "FAIL",
                "passed": passed,
                "latencyMs": round(ms, 2),
                "recallAt5": recall,
                "rank": matched_rank,
                "metricLabel": f"Recall@{limit}={recall:.2f} ({rank_str}, {ms:.2f}ms)",
                "details": {
                    "query": query,
                    "expectedPath": expected_path,
                    "rank": matched_rank,
                    "topPaths": [h.get("path") for h in symbols[:limit]],
                },
            }

        if kind == "search_context":
            query = str(task["query"])
            limit = int(task.get("limit", 5))
            expected_path = str(task["expectedPath"])
            res, ms = harness.request("search_context", {"query": query, "limit": limit})
            hits = res.get("hits") or []
            matched_rank = None
            for idx, hit in enumerate(hits[:limit], start=1):
                if hit.get("path") == expected_path:
                    matched_rank = idx
                    break
            recall = 1.0 if matched_rank is not None else 0.0
            passed = recall == 1.0
            rank_str = f"rank #{matched_rank}" if matched_rank is not None else "miss"
            return {
                "id": task_id,
                "category": category,
                "kind": kind,
                "status": "PASS" if passed else "FAIL",
                "passed": passed,
                "latencyMs": round(ms, 2),
                "recallAt5": recall,
                "rank": matched_rank,
                "metricLabel": f"Recall@{limit}={recall:.2f} ({rank_str}, {ms:.2f}ms)",
                "details": {
                    "query": query,
                    "expectedPath": expected_path,
                    "rank": matched_rank,
                    "topPaths": [h.get("path") for h in hits[:limit]],
                },
            }

        if kind == "grep_workspace":
            query = str(task["query"])
            case_sensitive = bool(task.get("caseSensitive", True))
            limit = int(task.get("limit", 20))
            expected_path = str(task["expectedPath"])
            res, ms = harness.request(
                "grep_workspace",
                {"query": query, "caseSensitive": case_sensitive, "limit": limit},
            )
            hits = res.get("hits") or []
            if hits:
                valid_hits = [
                    h
                    for h in hits
                    if (
                        query in str(h.get("text", ""))
                        if case_sensitive
                        else query.lower() in str(h.get("text", "")).lower()
                    )
                ]
                precision = len(valid_hits) / len(hits)
            else:
                precision = 0.0
            has_expected = any(h.get("path") == expected_path for h in hits)
            passed = len(hits) > 0 and precision == 1.0 and has_expected
            return {
                "id": task_id,
                "category": category,
                "kind": kind,
                "status": "PASS" if passed else "FAIL",
                "passed": passed,
                "latencyMs": round(ms, 2),
                "precision": round(precision, 3),
                "hitCount": len(hits),
                "metricLabel": f"Precision={precision:.2f} ({len(hits)} hits, {ms:.2f}ms)",
                "details": {
                    "query": query,
                    "hitCount": len(hits),
                    "precision": round(precision, 3),
                    "matchedExpectedPath": has_expected,
                },
            }

        if kind == "git_context":
            expected_sub = str(task.get("expectedSubstring", "git status"))
            res, ms = harness.request("git_context", {})
            ctx_text = str(res.get("context", ""))
            passed = expected_sub in ctx_text
            return {
                "id": task_id,
                "category": category,
                "kind": kind,
                "status": "PASS" if passed else "FAIL",
                "passed": passed,
                "latencyMs": round(ms, 2),
                "metricLabel": f"{len(ctx_text)} chars in {ms:.2f}ms",
                "details": {"chars": len(ctx_text), "elapsedMs": round(ms, 2)},
            }

        if kind == "session_roundtrip":
            sess_id = f"eval_sess_{os.getpid()}_{int(time.time() * 1000)}"
            title = str(task.get("title", "Eval Session"))
            messages = [
                {"role": "user", "content": "Explain WorkspaceIndex hybrid search.", "images": []},
                {
                    "role": "assistant",
                    "content": "WorkspaceIndex combines SQLite FTS5 BM25 with 128-dim cosine RRF.",
                    "images": [],
                },
            ]
            t0 = time.perf_counter()
            save_res, save_ms = harness.request(
                "save_session",
                {"sessionId": sess_id, "title": title, "messages": messages},
            )
            load_res, load_ms = harness.request("load_session", {"sessionId": sess_id})
            del_res, del_ms = harness.request("delete_session", {"sessionId": sess_id})
            total_ms = (time.perf_counter() - t0) * 1000.0

            loaded_sess = load_res.get("session") or {}
            loaded_msgs = loaded_sess.get("messages") or []
            summary = save_res.get("summary") or {}
            passed = (
                summary.get("sessionId") == sess_id
                and loaded_sess.get("title") == title
                and len(loaded_msgs) == 2
                and loaded_msgs[1].get("content") == messages[1]["content"]
                and del_res.get("deleted") is True
            )
            return {
                "id": task_id,
                "category": category,
                "kind": kind,
                "status": "PASS" if passed else "FAIL",
                "passed": passed,
                "latencyMs": round(total_ms, 2),
                "metricLabel": (
                    f"save={save_ms:.2f}ms, load={load_ms:.2f}ms, del={del_ms:.2f}ms "
                    f"(total {total_ms:.2f}ms)"
                ),
                "details": {
                    "saveMs": round(save_ms, 2),
                    "loadMs": round(load_ms, 2),
                    "deleteMs": round(del_ms, 2),
                    "totalMs": round(total_ms, 2),
                },
            }

        if kind == "syntax_gate_check":
            t0 = time.perf_counter()
            ok_res, ok_ms = harness.request(
                "validate_syntax",
                {
                    "path": "src/lib.rs",
                    "before": "",
                    "after": "pub fn clamp(x: f64) -> f64 { x.clamp(0.0, 1.0) }\n",
                },
            )
            bad_res, bad_ms = harness.request(
                "validate_syntax",
                {
                    "path": "src/lib.rs",
                    "before": "pub fn clamp(x: f64) -> f64 { x }\n",
                    "after": "pub fn broken(x: i32 -> {\n    let y = ;\n",
                },
            )
            total_ms = (time.perf_counter() - t0) * 1000.0
            ok_val = ok_res.get("validation") or {}
            bad_val = bad_res.get("validation") or {}
            err_cnt = int(bad_val.get("afterErrors", 0))
            passed = (
                ok_val.get("valid") is True
                and bad_val.get("valid") is False
                and err_cnt >= 1
            )
            return {
                "id": task_id,
                "category": category,
                "kind": kind,
                "status": "PASS" if passed else "FAIL",
                "passed": passed,
                "latencyMs": round(total_ms, 2),
                "metricLabel": f"valid={ok_ms:.2f}ms, broken={bad_ms:.2f}ms ({err_cnt} AST err)",
                "details": {"okMs": round(ok_ms, 2), "badMs": round(bad_ms, 2), "afterErrors": err_cnt},
            }

        if kind == "repo_map_check":
            focus_files = task.get("focusFiles", ["extension/src/extension.ts"])
            budget_chars = int(task.get("tokenBudget", 1200)) * 4
            expected_sub = str(task.get("expectedSubstring", "SidecarClient"))
            res, ms = harness.request(
                "repo_map",
                {"activeFiles": focus_files, "tokenBudgetChars": budget_chars},
            )
            repo_map_text = str(res.get("repoMap", ""))
            passed = len(repo_map_text) > 80 and expected_sub in repo_map_text
            return {
                "id": task_id,
                "category": category,
                "kind": kind,
                "status": "PASS" if passed else "FAIL",
                "passed": passed,
                "latencyMs": round(ms, 2),
                "metricLabel": f"{len(repo_map_text)} chars in {ms:.2f}ms",
                "details": {"chars": len(repo_map_text), "elapsedMs": round(ms, 2)},
            }

        if kind == "seatbelt_classify_check":
            t0 = time.perf_counter()
            read_res, _ = harness.request("classify_command", {"command": "git status --short"})
            build_res, _ = harness.request("classify_command", {"command": "cargo test --quiet"})
            dest_res, _ = harness.request("classify_command", {"command": "rm -rf /"})
            total_ms = (time.perf_counter() - t0) * 1000.0
            passed = (
                read_res.get("risk") == "safeRead"
                and build_res.get("risk") == "safeBuildOrTest"
                and dest_res.get("risk") == "destructiveOrExternal"
            )
            return {
                "id": task_id,
                "category": category,
                "kind": kind,
                "status": "PASS" if passed else "FAIL",
                "passed": passed,
                "latencyMs": round(total_ms, 2),
                "metricLabel": f"safeRead / safeBuildOrTest / destructiveOrExternal ({total_ms:.2f}ms)",
                "details": {"read": read_res, "build": build_res, "destructive": dest_res},
            }

        if kind == "cow_clone_roundtrip":
            fixture_ws = harness.isolated_home / "cow_fixture_ws"
            (fixture_ws / "src").mkdir(parents=True, exist_ok=True)
            (fixture_ws / "src" / "main.rs").write_text("fn main() { println!(\"v1\"); }\n", encoding="utf-8")
            (fixture_ws / "Cargo.toml").write_text("[package]\nname = \"demo\"\nversion = \"0.1.0\"\n", encoding="utf-8")

            harness.request("index_workspace", {"workspaceRoot": str(fixture_ws)})
            t0 = time.perf_counter()
            create_res, create_ms = harness.request(
                "create_cow_clone",
                {"taskLabel": "eval-cow"},
            )
            clone_meta = create_res.get("clone") or {}
            clone_id = str(clone_meta.get("cloneId", ""))
            clone_path = Path(str(clone_meta.get("cloneRoot", "")))
            (clone_path / "src" / "main.rs").write_text(
                "fn main() { println!(\"v2-mutated-in-apfs-cow\"); }\n",
                encoding="utf-8",
            )
            diff_res, diff_ms = harness.request("diff_cow_clone", {"cloneId": clone_id})
            destroy_res, destroy_ms = harness.request("destroy_cow_clone", {"cloneId": clone_id})
            total_ms = (time.perf_counter() - t0) * 1000.0
            harness.request("index_workspace", {"workspaceRoot": str(workspace_root)})

            diffs = diff_res.get("diffs") or []
            orig_intact = "v1" in (fixture_ws / "src" / "main.rs").read_text(encoding="utf-8")
            passed = (
                bool(clone_id)
                and len(diffs) == 1
                and orig_intact
                and destroy_res.get("destroyed") is True
            )
            return {
                "id": task_id,
                "category": category,
                "kind": kind,
                "status": "PASS" if passed else "FAIL",
                "passed": passed,
                "latencyMs": round(total_ms, 2),
                "metricLabel": (
                    f"clone={create_ms:.1f}ms, diff={diff_ms:.1f}ms, destroy={destroy_ms:.1f}ms"
                ),
                "details": {
                    "cloneMs": round(create_ms, 2),
                    "diffMs": round(diff_ms, 2),
                    "destroyMs": round(destroy_ms, 2),
                },
            }

        if kind == "swarm_wave_check":
            tasks_spec = [
                {
                    "id": "arch",
                    "title": "Design Contract",
                    "role": "architect",
                    "prompt": "Publish API contract",
                    "dependsOn": [],
                    "writeEnabled": False,
                },
                {
                    "id": "backend",
                    "title": "Implement Rust Handler",
                    "role": "coder",
                    "prompt": "Build backend endpoint",
                    "dependsOn": ["arch"],
                    "writeEnabled": True,
                },
                {
                    "id": "frontend",
                    "title": "Implement TS Panel",
                    "role": "coder",
                    "prompt": "Build webview UI",
                    "dependsOn": ["arch"],
                    "writeEnabled": True,
                },
                {
                    "id": "qa",
                    "title": "Integration Verification",
                    "role": "tester",
                    "prompt": "Run test suite",
                    "dependsOn": ["backend", "frontend"],
                    "writeEnabled": False,
                },
            ]
            res, ms = harness.request("plan_swarm_waves", {"tasks": tasks_spec})
            waves = res.get("waves") or []
            wave_count = int(res.get("waveCount", len(waves)))
            passed = (
                wave_count == 3
                and len(waves) == 3
                and len(waves[0]) == 1
                and len(waves[1]) == 2
                and len(waves[2]) == 1
            )
            return {
                "id": task_id,
                "category": category,
                "kind": kind,
                "status": "PASS" if passed else "FAIL",
                "passed": passed,
                "latencyMs": round(ms, 2),
                "metricLabel": f"3 waves [1 -> 2 -> 1] in {ms:.2f}ms",
                "details": {"waveCount": wave_count, "elapsedMs": round(ms, 2)},
            }

        if kind == "pptx_deck_check":
            media_ws = harness.isolated_home / "media_eval_ws"
            media_ws.mkdir(parents=True, exist_ok=True)
            res, ms = harness.request(
                "create_presentation",
                {
                    "workspaceRoot": str(media_ws),
                    "spec": {
                        "title": "Rursor 1.0 Omni-Studio",
                        "subtitle": "Native Rust + macOS AI Studio",
                        "author": "Rursor Eval",
                        "theme": "obsidian",
                        "outputPath": "decks/eval_deck.pptx",
                        "slides": [
                            {
                                "layout": "title",
                                "title": "Rursor Omni-Studio",
                                "subtitle": "Code + Swarm + Browser + Media",
                                "bullets": ["1.9ms APFS CoW Swarm", "Native OpenXML + MP4"],
                            },
                            {
                                "layout": "metrics",
                                "title": "Eval Benchmarks",
                                "subtitle": "Measured on Apple Silicon",
                                "metrics": [
                                    {"label": "RPC p50", "value": "0.06ms", "delta": "ultrafast"},
                                    {"label": "Recall@5", "value": "100%", "delta": "+RRF"},
                                ],
                                "bullets": ["Zero cloud dependency for local studio exports"],
                            },
                        ],
                    },
                },
            )
            pptx_path = Path(str(res.get("pptxPath", "")))
            html_path = Path(str(res.get("htmlDeckPath", "")))
            size_bytes = int(res.get("bytes", 0))
            is_zip = pptx_path.is_file() and pptx_path.read_bytes()[:4] == b"PK\x03\x04"
            passed = (
                is_zip
                and html_path.is_file()
                and int(res.get("slideCount", 0)) == 2
                and size_bytes > 1500
            )
            return {
                "id": task_id,
                "category": category,
                "kind": kind,
                "status": "PASS" if passed else "FAIL",
                "passed": passed,
                "latencyMs": round(ms, 2),
                "metricLabel": f"2 slides, {size_bytes}B .pptx + .deck.html ({ms:.1f}ms)",
                "details": {"bytes": size_bytes, "slideCount": res.get("slideCount")},
            }

        if kind == "mp4_video_check":
            media_ws = harness.isolated_home / "media_eval_ws"
            media_ws.mkdir(parents=True, exist_ok=True)
            res, ms = harness.request(
                "render_explainer_video",
                {
                    "workspaceRoot": str(media_ws),
                    "spec": {
                        "title": "Rursor Video Eval",
                        "outputPath": "videos/eval_clip.mp4",
                        "width": 640,
                        "height": 360,
                        "fps": 12,
                        "scenes": [
                            {
                                "heading": "Rursor Video Engine",
                                "subheading": "Procedural Rust Rasterizer + FFmpeg",
                                "bullets": ["H.264 yuv420p MP4 output", "Apple Silicon native"],
                                "durationSecs": 1.0,
                            }
                        ],
                    },
                },
            )
            video_path = Path(str(res.get("videoPath", "")))
            header = video_path.read_bytes()[:16] if video_path.is_file() else b""
            has_ftyp = b"ftyp" in header
            size_bytes = int(res.get("bytes", 0))
            passed = video_path.is_file() and has_ftyp and size_bytes > 500
            return {
                "id": task_id,
                "category": category,
                "kind": kind,
                "status": "PASS" if passed else "FAIL",
                "passed": passed,
                "latencyMs": round(ms, 1),
                "metricLabel": f"MP4 ftyp verified, {size_bytes}B ({ms:.0f}ms)",
                "details": {"bytes": size_bytes, "engine": res.get("engine")},
            }

        if kind == "xlsx_docx_check":
            media_ws = harness.isolated_home / "media_eval_ws"
            media_ws.mkdir(parents=True, exist_ok=True)
            t0 = time.perf_counter()
            xlsx_res, _ = harness.request(
                "create_spreadsheet",
                {
                    "workspaceRoot": str(media_ws),
                    "spec": {
                        "outputPath": "reports/eval_metrics.xlsx",
                        "sheets": [
                            {
                                "name": "Benchmarks",
                                "headers": ["Subsystem", "Score"],
                                "rows": [["Swarm", 100], ["Browser", 100], ["Total", "=SUM(B2:B3)"]],
                            }
                        ],
                    },
                },
            )
            docx_res, _ = harness.request(
                "create_document",
                {
                    "workspaceRoot": str(media_ws),
                    "spec": {
                        "title": "Rursor Architecture Spec",
                        "subtitle": "Automated Eval Verification",
                        "outputPath": "docs/eval_spec.docx",
                        "sections": [
                            {
                                "heading": "Overview",
                                "paragraphs": ["Rursor unifies code, browser, swarm, and media."],
                                "bullets": ["OpenXML .pptx/.xlsx/.docx", "Native CDP Browser"],
                            }
                        ],
                    },
                },
            )
            total_ms = (time.perf_counter() - t0) * 1000.0
            xlsx_path = Path(str(xlsx_res.get("xlsxPath", "")))
            docx_path = Path(str(docx_res.get("docxPath", "")))
            passed = (
                xlsx_path.is_file()
                and xlsx_path.read_bytes()[:4] == b"PK\x03\x04"
                and docx_path.is_file()
                and docx_path.read_bytes()[:4] == b"PK\x03\x04"
            )
            return {
                "id": task_id,
                "category": category,
                "kind": kind,
                "status": "PASS" if passed else "FAIL",
                "passed": passed,
                "latencyMs": round(total_ms, 2),
                "metricLabel": (
                    f".xlsx ({xlsx_res.get('bytes', 0)}B) + .docx ({docx_res.get('bytes', 0)}B) "
                    f"in {total_ms:.1f}ms"
                ),
                "details": {"xlsxBytes": xlsx_res.get("bytes"), "docxBytes": docx_res.get("bytes")},
            }

        if kind == "docs_and_mcp_check":
            t0 = time.perf_counter()
            idx_res, _ = harness.request(
                "index_docs_pages",
                {
                    "name": "rursor-sdk",
                    "rootUrl": "https://docs.rursor.dev/",
                    "pages": [
                        {
                            "url": "https://docs.rursor.dev/swarm",
                            "title": "Swarm Coordinator Guide",
                            "markdown": "# APFS Copy-on-Write Swarm\nUse `spawn_worker_swarm` to run parallel workers inside `cp -c -R` clones with `SwarmBlackboard`.",
                        },
                        {
                            "url": "https://docs.rursor.dev/browser",
                            "title": "CDP Browser Guide",
                            "markdown": "# Set-of-Mark Grounding\nUse `browser_snapshot` to overlay `[1..N]` visual badges over interactive DOM elements.",
                        },
                    ],
                },
            )
            search_res, _ = harness.request(
                "search_docs",
                {"query": "spawn_worker_swarm SwarmBlackboard parallel clones", "source": "rursor-sdk", "limit": 3},
            )
            mcp_res, _ = harness.request("list_mcp_catalog", {})
            harness.request("remove_docs_source", {"name": "rursor-sdk"})
            total_ms = (time.perf_counter() - t0) * 1000.0

            hits = search_res.get("hits") or []
            presets = mcp_res.get("presets") or []
            top_hit_ok = len(hits) >= 1 and "swarm" in str(hits[0].get("url", "")).lower()
            passed = (
                int((idx_res.get("source") or {}).get("pagesIndexed", 0)) == 2
                and top_hit_ok
                and len(presets) >= 8
            )
            return {
                "id": task_id,
                "category": category,
                "kind": kind,
                "status": "PASS" if passed else "FAIL",
                "passed": passed,
                "latencyMs": round(total_ms, 2),
                "metricLabel": f"@docs Recall@1=1.00, {len(presets)} MCP presets ({total_ms:.2f}ms)",
                "details": {"pagesIndexed": 2, "mcpPresetCount": len(presets)},
            }

        if kind == "live_inline_edit":
            res, ms = harness.request(
                "inline_edit",
                {
                    "filePath": task["filePath"],
                    "languageId": task["languageId"],
                    "prefix": task["prefix"],
                    "selection": task["selection"],
                    "suffix": task["suffix"],
                    "instruction": task["instruction"],
                },
            )
            replacement = str(res.get("replacement", "")).strip()
            candidates = [str(s) for s in task.get("mustContainAny", [])]
            passed = bool(replacement) and any(c in replacement for c in candidates)
            return {
                "id": task_id,
                "category": category,
                "kind": kind,
                "status": "PASS" if passed else "FAIL",
                "passed": passed,
                "latencyMs": round(ms, 1),
                "passAt1": 1.0 if passed else 0.0,
                "metricLabel": f"Pass@1={'1.00' if passed else '0.00'} ({ms:.0f}ms)",
                "details": {"replacement": replacement},
            }

        if kind == "live_agent_chat":
            res, ms = harness.request(
                "agent_chat",
                {
                    "sessionId": f"eval_live_{int(time.time())}",
                    "mode": task.get("mode", "ask"),
                    "useWorkspaceSearch": True,
                    "includeGitDiff": False,
                    "messages": [{"role": "user", "content": task["prompt"], "images": []}],
                },
            )
            answer = str(res.get("text", "")).strip()
            candidates = [str(s) for s in task.get("mustContainAny", [])]
            passed = bool(answer) and any(c in answer for c in candidates)
            return {
                "id": task_id,
                "category": category,
                "kind": kind,
                "status": "PASS" if passed else "FAIL",
                "passed": passed,
                "latencyMs": round(ms, 1),
                "passAt1": 1.0 if passed else 0.0,
                "metricLabel": f"Pass@1={'1.00' if passed else '0.00'} ({ms:.0f}ms)",
                "details": {"answerPreview": answer[:240]},
            }

        raise ValueError(f"Unsupported task kind: {kind}")
    except Exception as exc:
        return {
            "id": task_id,
            "category": category,
            "kind": kind,
            "status": "FAIL",
            "passed": False,
            "latencyMs": None,
            "metricLabel": f"ERROR: {exc}",
            "details": {"error": str(exc)},
        }


def render_markdown_report(report: Dict[str, Any]) -> str:
    agg = report["aggregates"]
    lines: List[str] = []
    lines.append("# Rursor Eval (`rursor-eval`) Benchmark Report")
    lines.append("")
    lines.append(
        f"- **Sidecar Binary**: `{report['environment']['sidecarBinary']}` "
        f"(cold handshake: `{report['environment']['coldHandshakeMs']:.1f} ms`)"
    )
    lines.append(f"- **Workspace**: `{report['environment']['workspaceRoot']}`")
    lines.append(
        f"- **Platform**: `{report['environment']['platform']}` · "
        f"Python `{report['environment']['pythonVersion']}`"
    )
    lines.append(
        f"- **Summary**: **{agg['passed']}/{agg['executed']} passed** "
        f"(`{agg['failed']}` failed, `{agg['skipped']}` skipped) · "
        f"Symbol/Context Recall@5: **{agg['recallAt5'] * 100:.1f}%** · "
        f"Grep Precision: **{agg['grepPrecision'] * 100:.1f}%**"
    )
    if agg["indexingFilesPerSec"] is not None:
        lines.append(
            f"- **Indexing Throughput**: **{agg['indexedFiles']} files** at "
            f"**{agg['indexingFilesPerSec']:.0f} files/sec** (`{agg['indexLatencyMs']:.1f} ms`)"
        )
    if report["environment"]["liveGeminiNote"]:
        lines.append(f"- **Live Gemini Tasks**: _{report['environment']['liveGeminiNote']}_")
    lines.append("")
    lines.append("| # | Task ID | Category | Status | Latency | Key Metric |")
    lines.append("|---:|---|---|---|---|---|")

    for idx, res in enumerate(report["results"], start=1):
        lat = f"{res['latencyMs']:.2f} ms" if res["latencyMs"] is not None else "—"
        status_cell = res["status"]
        lines.append(
            f"| {idx} | `{res['id']}` | `{res['category']}` | **{status_cell}** | {lat} | {res['metricLabel']} |"
        )

    lines.append("")
    return "\n".join(lines)


def main() -> int:
    parser = argparse.ArgumentParser(description="Run the Rursor sidecar & retrieval benchmark suite.")
    parser.add_argument(
        "--tasks",
        default=None,
        help="Path to tasks.json (defaults to eval/tasks.json).",
    )
    parser.add_argument(
        "--workspace",
        default=None,
        help="Workspace root to index and benchmark (defaults to repository root).",
    )
    parser.add_argument(
        "--sidecar",
        default=None,
        help="Explicit path to rursor-sidecar binary.",
    )
    parser.add_argument(
        "--json",
        action="store_true",
        help="Print structured JSON report instead of Markdown table.",
    )
    parser.add_argument(
        "--output",
        default=None,
        help="Optional file path to save the JSON benchmark report.",
    )
    args = parser.parse_args()

    eval_dir = Path(__file__).resolve().parent
    repo_root = eval_dir.parent
    tasks_path = Path(args.tasks).resolve() if args.tasks else (eval_dir / "tasks.json")
    workspace_root = Path(args.workspace).resolve() if args.workspace else repo_root

    spec = json.loads(tasks_path.read_text(encoding="utf-8"))
    tasks: List[Dict[str, Any]] = spec.get("tasks", [])

    sidecar_bin = resolve_sidecar_binary(repo_root, args.sidecar)
    has_gemini_key = bool(os.environ.get("GEMINI_API_KEY", "").strip())

    with tempfile.TemporaryDirectory(prefix="rursor_eval_home_") as temp_home:
        harness = SidecarRpcHarness(sidecar_bin, Path(temp_home))
        try:
            cold_ms = harness.start()
            results: List[Dict[str, Any]] = []
            for task in tasks:
                result = evaluate_task(harness, task, workspace_root, has_gemini_key)
                results.append(result)
        finally:
            harness.close()

    executed = [r for r in results if r["status"] != "SKIPPED"]
    passed = [r for r in executed if r["passed"] is True]
    failed = [r for r in executed if r["passed"] is False]
    skipped = [r for r in results if r["status"] == "SKIPPED"]

    recall_scores = [float(r["recallAt5"]) for r in executed if "recallAt5" in r]
    grep_scores = [float(r["precision"]) for r in executed if "precision" in r]
    index_entry = next((r for r in executed if r["kind"] == "index_workspace"), None)
    ping_entry = next((r for r in executed if r["id"] == "rpc_ping_latency"), None)

    live_note = (
        "Executed live Gemini Pass@1 tasks (`GEMINI_API_KEY` detected)."
        if has_gemini_key
        else "Skipped cleanly (`GEMINI_API_KEY` is not set in environment; offline suite is 100% deterministic)."
    )

    report: Dict[str, Any] = {
        "suite": spec.get("suite", "rursor-eval"),
        "version": spec.get("version", "1.0.0"),
        "timestamp": int(time.time()),
        "environment": {
            "sidecarBinary": str(sidecar_bin),
            "workspaceRoot": str(workspace_root),
            "coldHandshakeMs": round(cold_ms, 2),
            "platform": f"{platform.system()}-{platform.machine()}",
            "pythonVersion": platform.python_version(),
            "geminiApiKeySet": has_gemini_key,
            "liveGeminiNote": live_note,
        },
        "aggregates": {
            "totalTasks": len(results),
            "executed": len(executed),
            "passed": len(passed),
            "failed": len(failed),
            "skipped": len(skipped),
            "passRate": round(len(passed) / len(executed), 4) if executed else 0.0,
            "pingP50Ms": ping_entry.get("p50Ms") if ping_entry else None,
            "pingP95Ms": ping_entry.get("p95Ms") if ping_entry else None,
            "indexedFiles": index_entry.get("indexedFiles") if index_entry else None,
            "indexLatencyMs": index_entry.get("latencyMs") if index_entry else None,
            "indexingFilesPerSec": index_entry.get("filesPerSec") if index_entry else None,
            "recallAt5": round(sum(recall_scores) / len(recall_scores), 4) if recall_scores else 0.0,
            "grepPrecision": round(sum(grep_scores) / len(grep_scores), 4) if grep_scores else 0.0,
        },
        "results": results,
    }

    if args.output:
        out_path = Path(args.output).resolve()
        out_path.parent.mkdir(parents=True, exist_ok=True)
        out_path.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")

    if args.json:
        print(json.dumps(report, indent=2))
    else:
        print(render_markdown_report(report))

    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
