# Stripped at publish time. `test_*.py` is unittest's default discovery
# pattern, so this file is the canonical Python test spelling.
import unittest

from zed_poly import greet


class GreetTest(unittest.TestCase):
    def test_greet(self):
        self.assertEqual(greet("zed"), "hello, zed")
