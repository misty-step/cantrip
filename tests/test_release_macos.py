"""Native bundle boundaries: real Mach-O dependency relocation and fail-closed signing."""

import hashlib
import json
import os
from pathlib import Path
import platform
import plistlib
import runpy
import shutil
import ssl
import subprocess
import sys
import tempfile
import unittest
from unittest import mock
import zipfile


ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts" / "package-macos"
PACKAGE = runpy.run_path(str(SCRIPT))
VERIFY = runpy.run_path(str(ROOT / "scripts" / "verify-macos"))
PUBLISH = runpy.run_path(str(ROOT / "scripts" / "publish-macos"))
TEAM_ID = PACKAGE["EXPECTED_TEAM_ID"]
VERSION = "0.1.0"


class MacSigningPrerequisiteContracts(unittest.TestCase):
    def invoke(self, directory, *options):
        output = Path(directory) / "must-not-exist"
        result = subprocess.run(
            [sys.executable, str(SCRIPT), "--target", "aarch64-apple-darwin", "--output", str(output), *options],
            capture_output=True, text=True, timeout=10,
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(output.exists(), "rejected signing inputs exposed an application bundle")
        return result.stderr

    def test_production_is_the_default_and_has_no_unsigned_or_unnotarized_fallback(self):
        with tempfile.TemporaryDirectory(prefix="cantrip-signing-contract-") as directory:
            refused = self.invoke(directory)
            for option in ("--sign-identity", "--team-id", "--notary-profile"):
                self.assertIn(option, refused)
            refused = self.invoke(
                directory, "--sign-identity", "Developer ID Application: Fixture (ABCDEFGHIJ)",
                "--team-id", "ABCDEFGHIJ",
            )
            self.assertIn("--notary-profile", refused)

    def test_development_signing_cannot_be_misrepresented_as_production(self):
        with tempfile.TemporaryDirectory(prefix="cantrip-signing-contract-") as directory:
            for development in (("--ad-hoc",), ("--dev-sign-identity", "A" * 40)):
                for option, value in (
                    ("--sign-identity", "Developer ID Application: Fixture (ABCDEFGHIJ)"),
                    ("--team-id", "ABCDEFGHIJ"), ("--notary-profile", "fixture"),
                ):
                    with self.subTest(development=development, option=option):
                        self.assertIn("cannot be combined", self.invoke(directory, *development, option, value))

    def test_local_identity_inputs_fail_closed_before_building(self):
        with tempfile.TemporaryDirectory(prefix="cantrip-signing-contract-") as directory:
            self.assertIn("cannot be combined", self.invoke(directory, "--ad-hoc", "--dev-sign-identity", "A" * 40))
            self.assertIn("requires --dev-sign-identity", self.invoke(directory, "--keychain", "fixture.keychain-db"))
            self.assertIn("requires --dev-sign-identity", self.invoke(directory, "--ad-hoc", "--keychain", "fixture.keychain-db"))
            for invalid in ("-", "Cantrip Development", "A" * 39, "G" * 40):
                with self.subTest(identity=invalid):
                    self.assertIn("full forty-character SHA-1", self.invoke(directory, "--dev-sign-identity", invalid))


class MacProductionReleaseContracts(unittest.TestCase):
    def test_preflight_rejects_development_without_building(self):
        for options in (["--ad-hoc"], ["--dev-sign-identity", "A" * 40]):
            result = subprocess.run(
                [sys.executable, str(SCRIPT), "--preflight", *options],
                capture_output=True, text=True, timeout=10,
            )
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("accepts only production", result.stderr)

    def test_identity_selection_rejects_development_foreign_and_ambiguous_certificates(self):
        identity = PACKAGE["developer_identity"]
        for names in (
            ["Cantrip Development (local)"],
            ["Developer ID Application: Fixture (OTHERTEAM1)"],
            [f"Developer ID Application: One ({TEAM_ID})", f"Developer ID Application: Two ({TEAM_ID})"],
        ):
            listing = "".join(f' {index}) {str(index) * 40} "{name}"\n' for index, name in enumerate(names, 1))
            result = subprocess.CompletedProcess([], 0, listing.encode(), b"")
            with mock.patch.dict(identity.__globals__, {"run": mock.Mock(return_value=result)}):
                with self.assertRaisesRegex(PACKAGE["ReleaseError"], "exactly one valid Developer ID"):
                    identity(TEAM_ID)

    def test_preflight_emits_only_public_receipt_and_redacts_profile_responses(self):
        identity = {
            "name": f"Developer ID Application: Fixture ({TEAM_ID})", "team_id": TEAM_ID,
            "sha1": "A" * 40, "expires_utc": "2030-01-01T00:00:00+00:00",
        }
        preflight = PACKAGE["production_preflight"]
        failed = False
        calls = []
        def native(command, **_):
            calls.append(command)
            if command[0:3] == ["xcrun", "notarytool", "history"]:
                return subprocess.CompletedProcess(command, 1 if failed else 0, b"PROFILE_BODY_MUST_NOT_LEAK", b"PROFILE_ERROR_MUST_NOT_LEAK")
            return subprocess.CompletedProcess(command, 0, b"/public/tool/path\n", b"")
        with mock.patch.object(sys, "platform", "darwin"), mock.patch.dict(
            preflight.__globals__, {"run": native, "developer_identity": mock.Mock(return_value=identity)},
        ), mock.patch.object(shutil, "which", return_value="/public/tool/path"):
            pending = preflight(TEAM_ID, None)
            self.assertEqual(pending["identity"], identity)
            self.assertEqual(pending["notary_profile_state"], "not-configured")
            self.assertIsNone(pending["notary_profile_authenticated"])
            self.assertFalse(any("history" in command for command in calls))
            receipt = preflight(TEAM_ID, "fixture-profile")
            self.assertEqual(receipt["identity"], identity)
            self.assertTrue(receipt["notary_profile_authenticated"])
            self.assertNotIn("PROFILE_", json.dumps(receipt))
            failed = True
            with self.assertRaises(PACKAGE["ReleaseError"]) as error:
                preflight(TEAM_ID, "fixture-profile")
            self.assertNotIn("PROFILE_", str(error.exception))
        self.assertTrue(any("--keychain" in command and PACKAGE["LOGIN_KEYCHAIN"] in command for command in calls))
        self.assertFalse(any("--password" in command or "store-credentials" in command for command in calls))

    def test_identity_receipt_binds_public_certificate_digest_and_expiry(self):
        der = b"public-certificate-parser-fixture"
        digest = hashlib.sha1(der).hexdigest().upper()
        expiry = "Jan  1 00:00:00 2030 GMT"
        def native(command, **options):
            if command[1] == "find-identity":
                stdout = f' 1) {digest} "Developer ID Application: Fixture ({TEAM_ID})"\n'.encode()
            elif command[1] == "find-certificate":
                stdout = (
                    ssl.DER_cert_to_PEM_cert(b"old-same-name-certificate-without-local-private-key")
                    + ssl.DER_cert_to_PEM_cert(der)
                ).encode()
            else:
                self.assertEqual(command[:2], ["openssl", "x509"])
                self.assertEqual(options["input"], der)
                stdout = f"notAfter={expiry}\n".encode()
            return subprocess.CompletedProcess(command, 0, stdout, b"")
        with mock.patch.dict(PACKAGE["developer_identity"].__globals__, {"run": native}):
            receipt = PACKAGE["developer_identity"](TEAM_ID, digest.lower())
            self.assertEqual(receipt["sha1"], digest)
            self.assertEqual(receipt["expires_utc"], "2030-01-01T00:00:00+00:00")
            with self.assertRaisesRegex(PACKAGE["ReleaseError"], "names are not selectors"):
                PACKAGE["developer_identity"](TEAM_ID, f"Developer ID Application: Fixture ({TEAM_ID})")
            with self.assertRaisesRegex(PACKAGE["ReleaseError"], "expected Apple team"):
                PACKAGE["developer_identity"]("OTHERTEAM1", digest)
            expiry = "Jan  1 00:00:00 2020 GMT"
            with self.assertRaisesRegex(PACKAGE["ReleaseError"], "expired"):
                PACKAGE["developer_identity"](TEAM_ID, digest)


    def test_released_source_checks_real_clean_tag_and_github_tag_before_publication(self):
        with tempfile.TemporaryDirectory(prefix="cantrip-mac-publish-") as directory:
            root = Path(directory).resolve()
            def git(*arguments):
                return subprocess.run(
                    ["git", "-C", str(root), *arguments], capture_output=True, text=True,
                    check=True, timeout=10,
                ).stdout.strip()
            git("init", "-q")
            git("config", "user.name", "Cantrip contract")
            git("config", "user.email", "contract@example.invalid")
            (root / "Cargo.toml").write_text(
                '[package]\nname = "cantrip"\nversion.workspace = true\n'
                '[workspace.package]\nversion = "1.2.3"\n'
            )
            git("add", "Cargo.toml")
            git("-c", "core.hooksPath=/dev/null", "commit", "-qm", "fixture")
            git("tag", "v1.2.3")
            revision = git("rev-parse", "HEAD")
            calls = []
            remote_sha = revision
            draft = False
            def github(command, **_):
                calls.append(command)
                self.assertEqual(command[0], "gh")
                if command[1] == "api":
                    response = {"object": {"type": "commit", "sha": remote_sha}}
                else:
                    self.assertEqual(command[1:3], ["release", "view"])
                    response = {"tagName": "v1.2.3", "isDraft": draft, "isPrerelease": False, "url": "https://example.invalid", "assets": []}
                return subprocess.CompletedProcess(command, 0, json.dumps(response).encode(), b"")
            with mock.patch.dict(PUBLISH["released_source"].__globals__, {"ROOT": root, "run": github}):
                measured, version, _ = PUBLISH["released_source"]("v1.2.3")
                self.assertEqual((measured, version), (revision, "1.2.3"))
                calls.clear()
                (root / "untracked").write_text("dirty")
                with self.assertRaisesRegex(PUBLISH["ReleaseError"], "dirty"):
                    PUBLISH["released_source"]("v1.2.3")
                self.assertEqual(calls, [], "dirty checkout reached GitHub")
                (root / "untracked").unlink()
                remote_sha = "b" * 40
                with self.assertRaisesRegex(PUBLISH["ReleaseError"], "does not match"):
                    PUBLISH["released_source"]("v1.2.3")
                remote_sha = revision
                draft = True
                with self.assertRaisesRegex(PUBLISH["ReleaseError"], "existing published stable"):
                    PUBLISH["released_source"]("v1.2.3")
            self.assertFalse(any("upload" in command for command in calls))

    def test_mac_upload_names_cannot_clobber_linux_or_the_other_architecture(self):
        arm = set(PUBLISH["asset_names"]("1.2.3", "aarch64-apple-darwin"))
        intel = set(PUBLISH["asset_names"]("1.2.3", "x86_64-apple-darwin"))
        self.assertEqual(len(arm), 3)
        self.assertFalse(arm & intel)
        self.assertFalse((arm | intel) & {"SHA256SUMS", "manifest.json", "release.json", "provenance.json"})


class MacSmokeDiagnosticsContracts(unittest.TestCase):
    def test_failure_names_command_and_bounds_stderr_without_disclosing_transcript_stdout(self):
        with tempfile.TemporaryDirectory(prefix="cantrip-smoke-error-") as directory:
            root = Path(directory)
            command = root / "failed-command.py"
            command.write_text(
                'import sys\n'
                'print("TRANSCRIPT_MUST_NOT_BE_LOGGED")\n'
                'sys.stderr.write("OMITTED_PREFIX" + "x" * 5000 + "\\\\nDIAGNOSTIC_TAIL\\\\n")\n'
                'sys.exit(17)\n'
            )
            with self.assertRaises(RuntimeError) as raised:
                VERIFY["run"]([sys.executable, command, "models", "pull"], env=os.environ, cwd=root)
            diagnostic = str(raised.exception)
            self.assertIn("failed-command.py models pull", diagnostic)
            self.assertIn("failed (17)", diagnostic)
            self.assertIn("DIAGNOSTIC_TAIL", diagnostic)
            self.assertNotIn("OMITTED_PREFIX", diagnostic)
            self.assertNotIn("TRANSCRIPT_MUST_NOT_BE_LOGGED", diagnostic)
            self.assertLessEqual(len(diagnostic.split("stderr tail:\n", 1)[1]), 4096)

            # Expected refusal paths still receive their result rather than raising.
            result = VERIFY["run"]([sys.executable, command], env=os.environ, cwd=root, check=False)
            self.assertEqual(result.returncode, 17)
            self.assertIn(b"TRANSCRIPT_MUST_NOT_BE_LOGGED", result.stdout)


class MacCaptureRefusalContracts(unittest.TestCase):
    def test_permission_smoke_never_starts_an_undetermined_owner(self):
        for authorization, expected_error in (
            ("not-determined", None),
            ("authorized", "capture-source-missing"),
            ("denied", "microphone-denied"),
            ("restricted", "microphone-restricted"),
        ):
            with self.subTest(authorization=authorization), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                commands = []
                status = {"state": "idle", "microphone": {"authorization": authorization}}

                def run(command, **_):
                    commands.append(command[1:])
                    if command[1] == "start":
                        self.assertIsNotNone(expected_error, "undetermined access must never receive Start")
                        return subprocess.CompletedProcess(command, 1, b"", expected_error.encode())
                    self.assertEqual(command[1:], ["status", "--json"])
                    return subprocess.CompletedProcess(command, 0, json.dumps(status).encode(), b"")

                with mock.patch.dict(VERIFY["capture_refusal"].__globals__, {"run": run}):
                    result = VERIFY["capture_refusal"](root / "cantrip", root, {}, status)
                self.assertEqual(
                    commands,
                    ([["start", "--postproc", "raw"]] if expected_error else []) + [["status", "--json"]],
                )
                self.assertEqual(result["status"], "passed" if expected_error else "not-run")
                self.assertFalse(result["live_audio_created"])
                if expected_error:
                    self.assertEqual(result["guard"], expected_error)


@unittest.skipUnless(
    sys.platform == "darwin" and platform.machine() in ("arm64", "x86_64")
    and all(shutil.which(tool) for tool in ("xcrun", "otool", "lipo", "install_name_tool", "codesign", "ditto")),
    "native packaging contracts need a Mac and Xcode command-line tools",
)
class NativeMacBundleContracts(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="cantrip-native-package-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name).resolve()
        self.architecture = platform.machine()
        self.target = next(target for target, architecture in PACKAGE["TARGETS"].items() if architecture == self.architecture)
        self.original = self.root / "native-build"
        self.original.mkdir()
        self.libraries = self.original / "runtime"
        self.libraries.mkdir()
        self.binary = self.original / "cantrip"
        self.app = self.root / "Cantrip.app"
        self.compiler = ["xcrun", "clang", "-arch", self.architecture, "-mmacosx-version-min=13.0", "-Wl,-headerpad_max_install_names"]
        (self.original / "model.c").write_text(f'const char *model_version(void) {{ return "{VERSION}"; }}\n')
        (self.original / "engine.c").write_text('extern const char *model_version(void);\nconst char *engine_version(void) { return model_version(); }\n')
        (self.original / "main.c").write_text(
            '#include <stdio.h>\nextern const char *engine_version(void);\n'
            'int main(void) { printf("cantrip %s\\n", engine_version()); return 0; }\n'
        )
        self.compile(
            "-dynamiclib", self.original / "model.c", "-Wl,-install_name,@rpath/libmodel.dylib",
            "-o", self.libraries / "libmodel.dylib",
        )
        self.compile(
            "-dynamiclib", self.original / "engine.c", "-L", self.libraries, "-lmodel",
            "-Wl,-install_name,@rpath/libengine.dylib", "-Wl,-rpath,@loader_path",
            "-o", self.libraries / "libengine.dylib",
        )
        self.compile(
            self.original / "main.c", "-L", self.libraries, "-lengine",
            f"-Wl,-rpath,{self.libraries}", "-o", self.binary,
        )

    def compile(self, *arguments):
        subprocess.run(list(map(str, [*self.compiler, *arguments])), check=True, capture_output=True, timeout=60)

    def bundle(self):
        return PACKAGE["create_bundle"](
            self.binary, self.app, self.target, VERSION, lambda path: (ROOT / path).read_bytes(),
        )

    def test_relocated_bundle_runs_without_original_native_dependencies_or_build_tools(self):
        info, entitlements, libraries, _ = self.bundle()
        self.assertEqual(info["CFBundleShortVersionString"], VERSION)
        self.assertEqual(info["CFBundleVersion"], VERSION)
        self.assertEqual(info["CFBundleIdentifier"], "com.misty-step.cantrip")
        self.assertTrue(info["NSMicrophoneUsageDescription"])
        self.assertTrue(info["LSUIElement"])
        self.assertEqual(entitlements, {"com.apple.security.device.audio-input": True})
        entitlement_file = self.root / "entitlements.plist"
        entitlement_file.write_bytes(plistlib.dumps(entitlements))
        PACKAGE["sign_bundle"](self.app, libraries, entitlement_file, "-", None, development=True)
        shutil.rmtree(self.original)
        environment = {
            key: value for key, value in os.environ.items()
            if not key.startswith("DYLD_") and key != "ORT_DYLIB_PATH"
        }
        environment["PATH"] = "/usr/bin:/bin:/usr/sbin:/sbin"
        executable = self.app / "Contents" / "MacOS" / "cantrip"
        result = subprocess.run([str(executable)], cwd=self.root, env=environment, capture_output=True, text=True, timeout=10)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), f"cantrip {VERSION}")
        archive = self.root / "native-development.zip"
        PACKAGE["zip_bundle"](self.app, archive)
        relocated = self.root / "unpacked"
        with zipfile.ZipFile(archive) as zipped:
            self.assertIn("Cantrip.app/Contents/MacOS/cantrip", zipped.namelist())
        subprocess.run(["ditto", "-x", "-k", str(archive), str(relocated)], check=True, capture_output=True, timeout=30)
        subprocess.run(["codesign", "--verify", "--deep", "--strict", str(relocated / "Cantrip.app")], check=True, capture_output=True, timeout=30)
        result = subprocess.run([str(relocated / "Cantrip.app" / "Contents" / "MacOS" / "cantrip")], cwd=relocated, env=environment, capture_output=True, text=True, timeout=10)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), f"cantrip {VERSION}")

    def test_missing_transitive_native_library_is_not_silently_externalized(self):
        (self.libraries / "libmodel.dylib").unlink()
        with self.assertRaisesRegex(PACKAGE["ReleaseError"], "cannot contain native runtime dependency"):
            self.bundle()

    def test_architecture_mismatch_reaches_the_native_binary_guard(self):
        other = "x86_64" if self.architecture == "arm64" else "arm64"
        with self.assertRaisesRegex(PACKAGE["ReleaseError"], "wrong Mach-O architecture"):
            PACKAGE["inspect_native"](self.binary, other, "13.0", executable=True)

    def test_newer_native_runtime_cannot_claim_an_older_app_baseline(self):
        newer = self.original / "newer"
        subprocess.run([
            "xcrun", "clang", "-arch", self.architecture, "-mmacosx-version-min=14.0",
            "-x", "c", "-", "-o", str(newer),
        ], input=b"int main(void) { return 0; }\n", check=True, capture_output=True, timeout=60)
        with self.assertRaisesRegex(PACKAGE["ReleaseError"], "newer than the app baseline"):
            PACKAGE["inspect_native"](newer, self.architecture, "13.0", executable=True)


if __name__ == "__main__":
    unittest.main()
