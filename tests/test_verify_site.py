"""Deploy smoke contract: a stale edge converges; a real mismatch fails loudly."""

from importlib.machinery import SourceFileLoader
from importlib.util import module_from_spec, spec_from_loader
from pathlib import Path
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / "scripts" / "verify-site"
loader = SourceFileLoader("verify_site", str(SCRIPT))
verify = module_from_spec(spec_from_loader("verify_site", loader))
loader.exec_module(verify)

NEW = b"<a href=download/v0.1.3/cantrip>"
OLD = b"<a href=download/v0.1.2/cantrip>"


class Clock:
    def __init__(self):
        self.now = 0.0

    def __call__(self):
        return self.now

    def sleep(self, seconds):
        self.now += seconds


def run(responses, timeout=180, interval=5):
    clock = Clock()
    served = iter(responses)
    calls = []

    def get(url):
        calls.append(clock.now)
        response = next(served)
        if isinstance(response, Exception):
            raise response
        return response

    try:
        verify.wait_for_page(NEW, "https://site.test/", timeout, interval, get, clock, clock.sleep)
        return calls, None
    except Exception as error:
        return calls, error


class SiteConvergence(unittest.TestCase):
    def test_stale_edge_then_exact_build_passes(self):
        calls, error = run([OLD, OLD, NEW])
        self.assertIsNone(error)
        self.assertEqual(calls, [0, 5, 10])

    def test_edge_that_never_converges_fails_at_the_bound_naming_the_difference(self):
        calls, error = run([OLD] * 100, timeout=12, interval=5)
        self.assertIsInstance(error, verify.Mismatch)
        self.assertEqual(calls, [0, 5, 10, 12])
        self.assertIn("byte 23", str(error))
        self.assertIn("v0.1.2", str(error))
        self.assertIn("v0.1.3", str(error))

    def test_http_error_fails_immediately_without_waiting(self):
        calls, error = run([RuntimeError("https://site.test/: HTTP 503"), NEW])
        self.assertIn("HTTP 503", str(error))
        self.assertEqual(calls, [0])

    def test_length_only_difference_is_reported(self):
        clock = Clock()
        with self.assertRaises(verify.Mismatch) as caught:
            verify.wait_for_page(NEW, "u", 0, 5, lambda _: NEW + b"x", clock, clock.sleep)
        self.assertIn(f"byte {len(NEW) + 1}", str(caught.exception))


if __name__ == "__main__":
    unittest.main()
