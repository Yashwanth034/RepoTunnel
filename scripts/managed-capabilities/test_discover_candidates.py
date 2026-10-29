import unittest

import discover_candidates as discovery


class DiscoveryTests(unittest.TestCase):
    def test_simple_node_ranges_cover_rea_contract(self):
        expression = "^22.19.0 || ^24.11.0 || >=26.0.0"
        for good in ["22.19.0", "22.22.1", "24.11.0", "26.0.0", "28.2.1"]:
            self.assertTrue(discovery.satisfies_simple_node_range(good, expression), good)
        for bad in ["20.20.2", "22.18.9", "23.9.0", "24.10.9", "25.0.0"]:
            self.assertFalse(discovery.satisfies_simple_node_range(bad, expression), bad)

    def test_unknown_engine_syntax_fails_closed(self):
        with self.assertRaises(ValueError):
            discovery.satisfies_simple_node_range("22.19.0", ">=22 <23")

    def test_semver_parser_is_strict(self):
        self.assertEqual(discovery.parse_version("v22.19.0"), (22, 19, 0))
        for bad in ["22.19", "latest", "22.19.0-rc.1", ""]:
            with self.assertRaises(ValueError):
                discovery.parse_version(bad)


if __name__ == "__main__":
    unittest.main()
