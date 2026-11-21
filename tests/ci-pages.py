#!/usr/bin/env python3
"""Regression tests for verifying downloads after a Pages deployment."""
import importlib.util
import io
import hashlib
import json
from pathlib import Path
import ssl
import unittest
from unittest.mock import call, patch
from urllib.error import HTTPError, URLError

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("pages", ROOT / "scripts/ci/verify-pages.py")
pages = importlib.util.module_from_spec(spec)
spec.loader.exec_module(pages)
URL = "https://example.test/asagiri/downloads/latest/macos-arm64.txt"


class Response(io.BytesIO):
    status = 200


def failure(code, headers=None):
    return HTTPError(URL, code, "temporary test response", headers or {}, io.BytesIO(b"unavailable"))


class FetchTests(unittest.TestCase):
    def test_other_methods_are_never_requested(self):
        with patch.object(pages, "urlopen") as request:
            with self.assertRaises(ValueError):
                pages.fetch(URL, method="POST")
        request.assert_not_called()

    def test_a_transient_get_failure_returns_the_verified_response_body(self):
        error = failure(503)
        with patch.object(pages, "urlopen", side_effect=[error, Response(b"release feed")]) as request, \
                patch.object(pages.time, "sleep") as sleep:
            self.assertEqual(pages.fetch(URL), b"release feed")
        self.assertTrue(error.fp.closed)
        self.assertEqual(request.call_count, 2)
        self.assertIsNot(request.call_args_list[0].args[0], request.call_args_list[1].args[0])
        self.assertEqual(request.call_args.args[0].get_method(), "GET")
        sleep.assert_called_once_with(2)

    def test_head_retries_without_reading_a_release_asset(self):
        response = Response()
        with patch.object(response, "read", side_effect=AssertionError("HEAD must not download an asset")), \
                patch.object(pages, "urlopen", side_effect=[failure(502), response]) as request, \
                patch.object(pages.time, "sleep"):
            self.assertIsNone(pages.available(URL))
        self.assertTrue(response.closed)
        self.assertTrue(all(item.args[0].get_method() == "HEAD" for item in request.call_args_list))

    def test_retries_are_bounded_and_exhaustion_still_fails(self):
        errors = [failure(503) for _ in range(4)]
        with patch.object(pages, "urlopen", side_effect=errors) as request, \
                patch.object(pages.time, "sleep") as sleep:
            with self.assertRaises(HTTPError) as caught:
                pages.fetch(URL)
        self.assertIs(caught.exception, errors[-1])
        self.assertEqual(request.call_count, 4)
        self.assertEqual(sleep.call_args_list, [call(2), call(4), call(8)])
        self.assertTrue(all(error.fp.closed for error in errors))

    def test_permanent_http_errors_fail_without_retrying(self):
        for code in (401, 403, 404, 410, 501):
            with self.subTest(code=code), \
                    patch.object(pages, "urlopen", side_effect=failure(code)) as request, \
                    patch.object(pages.time, "sleep") as sleep:
                with self.assertRaises(HTTPError):
                    pages.fetch(URL)
                request.assert_called_once()
                sleep.assert_not_called()

    def test_connection_failures_and_timeouts_can_recover(self):
        for error in (TimeoutError("timed out"), ConnectionResetError("connection reset"),
                      URLError(TimeoutError("connect timed out"))):
            with self.subTest(error=type(error).__name__), \
                    patch.object(pages, "urlopen", side_effect=[error, Response(b"page")]), \
                    patch.object(pages.time, "sleep") as sleep:
                self.assertEqual(pages.fetch(URL), b"page")
                sleep.assert_called_once_with(2)

    def test_certificate_verification_errors_fail_immediately(self):
        error = URLError(ssl.SSLCertVerificationError("certificate verification failed"))
        with patch.object(pages, "urlopen", side_effect=error) as request, \
                patch.object(pages.time, "sleep") as sleep:
            with self.assertRaises(URLError):
                pages.fetch(URL)
        request.assert_called_once()
        sleep.assert_not_called()

    def test_retry_after_is_honored_with_a_bounded_delay(self):
        for header, delay in (("10", 10), ("999999", 30), ("-1", 2), ("invalid", 2)):
            with self.subTest(header=header), \
                    patch.object(pages, "urlopen", side_effect=[
                        failure(503, {"Retry-After": header}), Response(b"ready"),
                    ]), patch.object(pages.time, "sleep") as sleep:
                self.assertEqual(pages.fetch(URL), b"ready")
                sleep.assert_called_once_with(delay)

    def test_partial_content_is_not_a_valid_complete_asset(self):
        response = Response(b"partial")
        response.status = 206
        with patch.object(pages, "urlopen", return_value=response) as request, \
                patch.object(pages.time, "sleep") as sleep:
            with self.assertRaisesRegex(RuntimeError, "HTTP 206"):
                pages.fetch(URL)
        request.assert_called_once()
        sleep.assert_not_called()


class DownloadConsistencyTests(unittest.TestCase):
    def test_verifier_rejects_stale_publication_with_matching_commit(self):
        commit = "a" * 40
        args = ["verify-pages.py", "https://example.test/asagiri/", commit,
                "--publication", "100-2", "--timeout", "0"]
        stale = json.dumps({"commit": commit, "publication": "100-1"}).encode()
        with patch("sys.argv", args), patch.object(pages, "fetch", return_value=stale) as fetch:
            with self.assertRaisesRegex(SystemExit, "publication '100-2'"):
                pages.main()
        fetch.assert_called_once_with(
            f"https://example.test/asagiri/deployment.json?commit={commit}&publication=100-2"
        )

    def test_mixed_publications_report_both_versions(self):
        key = (ROOT / "packaging/keys/asagiri-rpm.asc").read_bytes()
        for indexed, served in (("1.2.3", "1.2.4"), ("1.2.4", "1.2.3")):
            with self.subTest(indexed=indexed, served=served):
                prefix = f"https://github.com/AndreKurait/asagiri/releases/download/v{indexed}/Asagiri-{indexed}-arm64"
                release = {
                    "version": indexed, "commit": "a" * 40, "tag": "v" + indexed,
                    "release_url": f"https://github.com/AndreKurait/asagiri/releases/tag/v{indexed}",
                    "downloads": {"macos-arm64": {
                        "url": prefix + ".dmg", "archive_url": prefix + ".tar.gz",
                        "sha256": "b" * 64, "archive_sha256": "c" * 64,
                    }},
                }
                index = {"repository": "AndreKurait/asagiri", "channels": {
                    "latest": release, "stable": None, "preview": None,
                }}
                base = "https://example.test/asagiri/"
                responses = {
                    base + "install.sh": f"key_sha256={hashlib.sha256(key).hexdigest()}".encode(),
                    base + "keys/asagiri-rpm.asc": key,
                    base + "releases.json": json.dumps(index).encode(),
                    URL: f"version {served}\n".encode(),
                }
                with patch.object(pages, "fetch", side_effect=responses.__getitem__):
                    with self.assertRaises(SystemExit) as caught:
                        pages.verify_downloads(base)
                message = str(caught.exception)
                self.assertIn("latest/arm64", message)
                self.assertIn(f"Expected 'version {indexed}", message)
                self.assertIn(f"observed 'version {served}", message)


if __name__ == "__main__":
    unittest.main()
