#!/usr/bin/env python3
"""Harness guards only; real CLI compatibility is checked by scripts/e2e.sh."""
import contextlib
import http.client
import io
import http.server
import importlib.util
import json
from pathlib import Path
import ssl
import tempfile
import unittest

import jsonschema

spec = importlib.util.spec_from_file_location("e2e_github", Path(__file__).with_name("e2e-github.py"))
github = importlib.util.module_from_spec(spec)
spec.loader.exec_module(github)


class GithubHarnessTests(unittest.TestCase):
    def test_failed_command_stays_failed_while_independent_checks_continue(self):
        client = github.Client.__new__(github.Client)
        client.failures = []
        client.passes = []
        client.diagnostics = []
        client.credentials = {"alice": "private-test-token"}
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            with client.check("gh repo view"):
                raise AssertionError("GraphQL: private-test-token field not implemented")
            with client.check("gh api REST"):
                pass
        self.assertEqual(client.failures, ["gh repo view"])
        self.assertIn("FAIL real-client gh repo view", output.getvalue())
        self.assertIn("PASS real-client gh api REST", output.getvalue())
        self.assertNotIn("private-test-token", output.getvalue())
        self.assertNotIn("private-test-token", str(client.diagnostics))

    def test_all_pinned_schemas_are_valid_and_references_resolve_without_network(self):
        components, registry = github.webhook_schemas()
        names = [name for name in components["schemas"] if name.startswith("webhook-")]
        self.assertGreater(len(names), 200)
        for name in names:
            with self.subTest(schema=name):
                jsonschema.Draft4Validator.check_schema(components["schemas"][name])
        validator = github.webhook_validator(components, registry, "issues", "opened")
        errors = list(validator.iter_errors({"action": "opened", "issue": {}, "repository": {}, "sender": {}}))
        self.assertTrue(errors)
        self.assertTrue(any(list(error.absolute_path) == ["issue"] for error in errors))
        self.assertTrue(any(list(error.absolute_path) == ["repository"] for error in errors))
        self.assertTrue(any(list(error.absolute_path) == ["sender"] for error in errors))

    def test_openapi_nullable_retains_required_fields_and_oneof(self):
        schema = github.openapi_nullable({"type": "object", "required": ["value"],
            "properties": {"value": {"type": "string", "enum": ["present"], "nullable": True}}})
        validator = jsonschema.Draft4Validator(schema)
        self.assertTrue(validator.is_valid({"value": None}))
        self.assertTrue(validator.is_valid({"value": "present"}))
        self.assertFalse(validator.is_valid({}))
        self.assertFalse(validator.is_valid({"value": 1}))
        overlap = github.openapi_nullable({"oneOf": [{"type": "integer"}, {"type": "number"}]})
        self.assertFalse(jsonschema.Draft4Validator(overlap).is_valid(1))

    def test_tls_tunnel_preserves_request_and_response_bytes(self):
        received = []
        request_body = bytes(range(256)) * 100
        response_body = request_body[::-1]

        class Endpoint(http.server.BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_POST(self):
                body = self.rfile.read(int(self.headers["Content-Length"]))
                received.append((self.path, self.headers["Authorization"], body))
                self.send_response(201)
                self.send_header("Content-Length", str(len(response_body)))
                self.send_header("X-Transport-Proof", "unchanged")
                self.end_headers()
                self.wfile.write(response_body)

        with tempfile.TemporaryDirectory() as temporary:
            cert, key = github.certificate(Path(temporary))
            upstream = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Endpoint)
            tunnel = http.server.ThreadingHTTPServer(("127.0.0.1", 0), github.Tunnel)
            tunnel.upstream = upstream.server_address
            tunnel.tls = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
            tunnel.tls.load_cert_chain(cert, key)
            with github.serving(upstream), github.serving(tunnel):
                context = ssl.create_default_context(cafile=str(cert))
                connection = http.client.HTTPSConnection("127.0.0.1", tunnel.server_port, context=context, timeout=10)
                connection.set_tunnel(github.HOST, 443)
                connection.request("POST", "/transport/proof?unchanged=yes", request_body,
                                   {"Authorization": "Bearer transport-fixture"})
                response = connection.getresponse()
                self.assertEqual(response.status, 201)
                self.assertEqual(response.getheader("X-Transport-Proof"), "unchanged")
                self.assertEqual(response.read(), response_body)
                connection.close()
            self.assertEqual(received, [("/transport/proof?unchanged=yes", "Bearer transport-fixture", request_body)])


if __name__ == "__main__":
    unittest.main()
