#!/usr/bin/env python3
"""Regressions for the HTTP/native form test client."""
import importlib.util
from pathlib import Path
import unittest
from unittest.mock import Mock, patch

spec = importlib.util.spec_from_file_location("e2e_web", Path(__file__).with_name("e2e-web.py"))
web = importlib.util.module_from_spec(spec)
spec.loader.exec_module(web)


class Forms(unittest.TestCase):
    def test_explicit_owners_resolve_across_the_document_in_order(self):
        forms = web.parse_forms('''
<input form="new-entry-form" name="labels" value="before" checked type="checkbox">
<form id="other"><input form="new-entry-form" name="assignees" value="bob" checked type="checkbox">
<input name="local" value="other"><input form="missing" name="ignored" value="yes"></form>
<form id="new-entry-form" action="/issues"><input name="title" value="Hello"></form>
<aside><input form="new-entry-form" name="labels" value="after" checked type="checkbox">
<input form="new-entry-form" name="labels" value="unchecked" type="checkbox">
<input form="new-entry-form" name="labels" value="disabled" checked disabled type="checkbox">
<input form="new-entry-form" name="labels" checked type="checkbox"></aside>
''')
        self.assertEqual(forms[0].fields, [("local", "other")])
        self.assertEqual(forms[1].fields, [("labels", "before"), ("assignees", "bob"),
                                          ("title", "Hello"), ("labels", "after"), ("labels", "on")])

    def test_all_control_types_use_their_owner(self):
        form, other = web.parse_forms('''
<form id="entry"><textarea name="body">\nEnclosed</textarea>
<select name="choice"><option>First</option><option selected value="second">Second</option></select>
<input form="" name="ignored" value="yes"></form><form id="other"></form>
<textarea form="entry" name="external">\nOutside &amp; text</textarea>
<select form="entry" name="external-choice"><option>Fallback</option></select>
<input form="entry" type="file" name="picture">
<button form="entry" name="save" value="yes" formaction="/save">Save</button>
<input form="other" name="only" value="other">
''')
        self.assertEqual(form.fields, [("body", "Enclosed"), ("choice", "second"),
                                      ("external", "Outside & text"), ("external-choice", "Fallback")])
        self.assertEqual(form.files, ["picture"])
        self.assertEqual(form.buttons, [("save", "yes", "/save")])
        self.assertEqual(other.fields, [("only", "other")])


class Transport(unittest.TestCase):
    def test_https_requests_use_tls_and_https_default_port(self):
        for base, port, tls in [("https://forge.example", 443, True),
                                ("https://forge.example:8443", 8443, True),
                                ("http://forge.example", 80, False)]:
            with self.subTest(base=base), patch.object(web.http.client, "HTTPSConnection") as https, \
                    patch.object(web.http.client, "HTTPConnection") as http:
                selected, unused = (https, http) if tls else (http, https)
                raw = Mock(status=200, headers={})
                raw.read.return_value = b"ok"
                selected.return_value.getresponse.return_value = raw
                client = web.Client(base, None, None)
                client.cookies["session_id"] = "secret"
                response = client.request("POST", "/api/auth/login", body=b"password=secret")
                self.assertEqual(response.body, b"ok")
                selected.assert_called_once_with("forge.example", port, timeout=60)
                unused.assert_not_called()
                self.assertEqual(selected.return_value.request.call_args.kwargs["headers"]["Cookie"],
                                 "session_id=secret")
                selected.return_value.close.assert_called_once()

    def test_non_http_bases_are_rejected_before_a_request(self):
        for base in ["ftp://forge.example", "forge.example", "https:///missing-host"]:
            with self.subTest(base=base), self.assertRaises(SystemExit):
                web.Client(base, None, None)


if __name__ == "__main__":
    unittest.main()
