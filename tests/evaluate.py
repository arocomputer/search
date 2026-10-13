"""Pin retrieval-evaluation scoring rather than framework smoke behavior."""

import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location("evaluate", Path(__file__).parents[1] / "scripts/evaluate.py")
evaluate = importlib.util.module_from_spec(spec)
spec.loader.exec_module(evaluate)


class EvaluationTests(unittest.TestCase):
    def test_overlapping_labels_do_not_inflate_useful_characters(self):
        self.assertEqual(evaluate.span_characters("abcdef", ["abcd", "cdef", "abcd"]), 6)
        self.assertEqual(evaluate.span_characters("ababa", ["aba"]), 5)

    def test_opaque_query_bytes_remain_distinct(self):
        self.assertNotEqual(evaluate.identity("https://example.com/?q=%FF"),
                            evaluate.identity("https://example.com/?q=%FE"))

    def test_one_redirect_destination_cannot_recall_two_sources(self):
        a, b = "https://a.example/a", "https://b.example/b"
        case = {"id": "x", "relevant": [{"url": a}, {"url": b}]}
        packet = {"sources": [
            {"url": a, "final_url": b, "passages": [{"text": "useful"}]},
            {"url": b, "passages": [{"text": "useful"}]},
        ]}
        score = evaluate.score(case, packet, 2)
        self.assertEqual(score["recall_at_k"], 0.5)
        self.assertEqual(score["distinct_hosts"], 1)

    def test_nonadjacent_passages_do_not_invent_a_labeled_span(self):
        case = {"id": "x", "relevant": [{"url": "https://example.com/a", "spans": ["one\n\ntwo"]}]}
        packet = {"sources": [{"url": "https://example.com/a", "passages": [{"text": "one"}, {"text": "two"}]}]}
        score = evaluate.score(case, packet, 1)
        self.assertEqual(score["labeled_useful_characters"], 0)
        self.assertEqual(score["returned_characters"], 6)

    def test_failed_or_empty_sources_do_not_count_as_recalled_evidence(self):
        case = {"id": "x", "relevant": [{"url": "https://example.com/a", "spans": ["useful"]}]}
        packet = {"sources": [{"url": "https://example.com/a", "error": "HTTP 404", "passages": []}]}
        self.assertEqual(evaluate.score(case, packet, 5)["recall_at_k"], 0)

    def test_duplicate_urls_cannot_inflate_recall(self):
        case = {"id": "x", "relevant": [{"url": "https://example.com/a", "spans": ["useful"]}]}
        source = {"url": "https://example.com/a?utm_source=test", "passages": [{"text": "useful filler"}]}
        score = evaluate.score(case, {"sources": [source, source]}, 5)
        self.assertEqual(score["recall_at_k"], 1)
        self.assertEqual(score["distinct_hosts"], 1)
        self.assertAlmostEqual(score["labeled_context_efficiency"], 6 / 13)

    def test_a_meaningful_query_is_part_of_source_identity(self):
        self.assertNotEqual(evaluate.identity("https://example.com/a?ref=dev"),
                            evaluate.identity("https://example.com/a?ref=main"))

    def test_distinct_paths_are_not_credited_as_the_labeled_source(self):
        self.assertNotEqual(evaluate.identity("https://example.com/a"),
                            evaluate.identity("https://example.com/a/"))
        self.assertEqual(evaluate.identity("https://example.com"),
                         evaluate.identity("https://example.com/"))

    def test_retry_failures_do_not_consume_evidence_ranks(self):
        case = {"id": "x", "relevant": [{"url": "https://example.com/a", "spans": ["useful"]}]}
        packet = {"sources": [
            {"url": "https://example.com/dead", "error": "HTTP 404", "passages": []},
            {"url": "https://example.com/empty", "passages": [{"text": " "}]},
            {"url": "https://example.com/old", "final_url": "https://example.com/a",
             "passages": [{"text": "useful"}]},
        ]}
        self.assertEqual(evaluate.score(case, packet, 1)["recall_at_k"], 1)


if __name__ == "__main__":
    unittest.main()
