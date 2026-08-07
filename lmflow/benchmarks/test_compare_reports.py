import unittest

from compare_reports import compare


class CompareReportsTest(unittest.TestCase):
    def test_changed_added_and_removed_results(self):
        report = compare(
            {"language": "cpp", "results": [
                {"name": "a", "packets_per_second": 100, "nanoseconds_per_packet": 10},
                {"name": "removed", "packets_per_second": 1, "nanoseconds_per_packet": 1},
            ]},
            {"language": "python", "results": [
                {"name": "a", "packets_per_second": 125, "nanoseconds_per_packet": 8},
                {"name": "added", "packets_per_second": 2, "nanoseconds_per_packet": 2},
            ]},
        )
        self.assertEqual(report["baseline_language"], "cpp")
        self.assertEqual(report["candidate_language"], "python")
        self.assertEqual(report["results"][0]["packets_per_second_delta_percent"], 25.0)
        self.assertEqual(report["results"][1], {"name": "added", "status": "added"})
        self.assertEqual(report["results"][2], {"name": "removed", "status": "removed"})


if __name__ == "__main__":
    unittest.main()
