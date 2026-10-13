#!/usr/bin/env python3
"""Score labeled retrieval packets, or collect them from a configured binary.

Labels are explicit source URLs and verbatim useful spans, not model judgments.
The span metric is conservative labeled-character coverage, not answer quality.
Use saved packets for reproducibility; live results are dated observations.
"""

import argparse
import json
from pathlib import Path
import subprocess
import sys
import time
from urllib.parse import unquote_plus, urlsplit, urlunsplit


def identity(raw):
    """Normalize harmless URL variants without discarding meaningful queries."""
    url = urlsplit(raw)
    query = []
    for part in url.query.split("&"):
        key = unquote_plus(part.split("=", 1)[0]).lower()
        if not key.startswith("utm_") and key not in ("fbclid", "gclid"):
            query.append(part)
    return urlunsplit((url.scheme.lower(), url.netloc.lower(),
                       url.path or "/", "&".join(query), ""))


def span_characters(text, spans):
    """Count the union of labeled verbatim spans, avoiding overlapping credit."""
    intervals = []
    for span in spans:
        if not span:
            continue
        start = 0
        while (start := text.find(span, start)) >= 0:
            intervals.append((start, start + len(span)))
            start += 1
    end = 0
    total = 0
    for left, right in sorted(intervals):
        total += max(0, right - max(left, end))
        end = max(end, right)
    return total


def score(case, packet, k):
    """Measure useful evidence, including honest fetch and engine failures."""
    labels = {identity(label["url"]): label.get("spans", []) for label in case["relevant"]}
    if not labels:
        raise ValueError("each case needs at least one labeled relevant source")
    # Failed attempts remain in an honest packet, but do not occupy evidence
    # ranking slots. Score k successfully returned sources, not k attempts.
    sources = [source for source in packet.get("sources", [])
               if not source.get("error")
               and any(passage.get("text", "").strip()
                       for passage in source.get("passages", []))][:k]
    useful = returned = 0
    found = set()
    seen_destinations = set()
    hosts = set()
    for source in sources:
        passages = [passage["text"] for passage in source.get("passages", [])]
        destination = source.get("final_url") or source["url"]
        final_key = identity(destination)
        requested_key = identity(source["url"])
        key = final_key if final_key in labels else requested_key
        hosts.add(urlsplit(destination).hostname)
        returned += sum(len(text) for text in passages)
        if key in labels:
            if final_key not in seen_destinations:
                found.add(key)
            useful += sum(span_characters(text, labels[key]) for text in passages)
            seen_destinations.add(final_key)
    return {
        "id": case["id"],
        "recall_at_k": len(found) / len(labels),
        "labeled_context_efficiency": useful / returned if returned else 0,
        "returned_characters": returned,
        "labeled_useful_characters": useful,
        "distinct_hosts": len(hosts),
        "read": packet.get("coverage", {}).get("read", 0),
        "failed": packet.get("coverage", {}).get("failed", 0),
        "engine_failures": sum(engine["status"] != "ok" for engine in packet.get("engines", [])),
        "duration_ms": packet.get("duration_ms", 0),
    }


def collect(binary, config, case):
    """Run a real configured engine; no credentials are logged or copied."""
    args = [binary]
    if config:
        args += ["-config", config]
    args += ["research", case["queries"][0]]
    for query in case["queries"][1:]:
        args += ["-query", query]
    args += ["-sources", str(case.get("sources", 5)), "-json"]
    result = subprocess.run(args, capture_output=True, text=True, timeout=120, check=True)
    return json.loads(result.stdout)


def main():
    """Produce a machine-readable dated score report and optional raw packets."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("labels", help="JSON array of labeled cases")
    source = parser.add_mutually_exclusive_group(required=True)
    source.add_argument("--packets", help="JSON object keyed by case ID")
    source.add_argument("--binary", help="search binary for live collection")
    parser.add_argument("--config", help="trusted settings path, for --binary only")
    parser.add_argument("--save-packets", help="save captured packets for replay")
    parser.add_argument("-k", type=int, default=5)
    args = parser.parse_args()
    if args.k < 1:
        parser.error("k must be positive")
    cases = json.loads(Path(args.labels).read_text())
    if not cases or len({case["id"] for case in cases}) != len(cases):
        parser.error("labels must contain cases with unique IDs")
    packets = json.loads(Path(args.packets).read_text()) if args.packets else {
        case["id"]: collect(args.binary, args.config, case) for case in cases
    }
    scores = [score(case, packets[case["id"]], args.k) for case in cases]
    if args.save_packets:
        Path(args.save_packets).write_text(json.dumps(packets, indent=2) + "\n")
    print(json.dumps({
        "evaluated_at": int(time.time()), "mode": "replay" if args.packets else "live",
        "k": args.k, "cases": scores,
        "mean_recall_at_k": sum(item["recall_at_k"] for item in scores) / len(scores),
        "mean_labeled_context_efficiency": sum(item["labeled_context_efficiency"] for item in scores) / len(scores),
    }, indent=2))


if __name__ == "__main__":
    try:
        main()
    except (ValueError, KeyError, OSError, subprocess.SubprocessError):
        print("evaluation failed: check labels, packets and engine configuration", file=sys.stderr)
        sys.exit(1)
