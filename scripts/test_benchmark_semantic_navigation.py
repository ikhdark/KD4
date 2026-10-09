"""Accuracy-oracle regressions; the live LSP benchmark remains opt-in."""
import copy
import unittest

from scripts import benchmark_semantic_navigation as navigation


class NavigationAccuracyTests(unittest.TestCase):
    def test_utf16_positions_and_repeated_occurrences(self):
        # The fixture prefix contains one astral character (two UTF-16 units).
        self.assertEqual(navigation.source_range(4, "target"), {
            "start": {"line": 4, "character": 46},
            "end": {"line": 4, "character": 52},
        })
        self.assertEqual(navigation.source_range(4, "target", 1)["start"]["character"], 56)

    def test_locations_preserve_file_columns_and_multiplicity(self):
        span = {"start": {"line": 1, "character": 2}, "end": {"line": 1, "character": 5}}
        location = {"uri": "file:///fixture/a.rs", "range": span}
        link = {"targetUri": location["uri"], "targetSelectionRange": span}
        self.assertEqual(navigation.locations([location]), navigation.locations([link]))
        for other in [
            {**location, "uri": "file:///fixture/b.rs"},
            {**location, "range": {**span, "end": {"line": 1, "character": 6}}},
        ]:
            self.assertNotEqual(navigation.locations([location]), navigation.locations([other]))
        self.assertEqual(len(navigation.locations([location, location])), 2)

    def test_accuracy_rejects_wrong_targets_and_missing_same_line_calls(self):
        uri = "file:///fixture/lib.rs"

        def location(line, word, occurrence=0):
            return {"uri": uri, "range": navigation.source_range(line, word, occurrence)}

        def edge(direction, target, *calls):
            return {direction: location(*target), "fromRanges": [navigation.source_range(*call) for call in calls]}

        # Written fixture relationships: caller calls target twice, the trait
        # implementation calls it once, and caller invokes that implementation.
        results = [[location(3, "target")], [location(2, "target"), location(4, "target"), location(4, "target", 1)],
                   [location(2, "Value")], [location(0, "Value")], [location(3, "target")], [location(4, "caller")]]
        incoming = [edge("from", (2, "execute"), (2, "target")),
                    edge("from", (4, "caller"), (4, "target"), (4, "target", 1))]
        outgoing = [edge("to", (2, "execute"), (4, "execute")),
                    edge("to", (3, "target"), (4, "target"), (4, "target", 1))]
        self.assertTrue(all(navigation.navigation_checks(uri, results, incoming, outgoing)))
        wrong = copy.deepcopy(results)
        wrong[1].pop()
        self.assertFalse(navigation.navigation_checks(uri, wrong, incoming, outgoing)[1])
        wrong = copy.deepcopy(outgoing)
        wrong[1]["to"]["uri"] = "file:///shadow/lib.rs"
        self.assertFalse(navigation.navigation_checks(uri, results, incoming, wrong)[7])
        wrong = copy.deepcopy(incoming)
        wrong[1]["fromRanges"].pop()
        self.assertFalse(navigation.navigation_checks(uri, results, wrong, outgoing)[6])


if __name__ == "__main__":
    unittest.main()
