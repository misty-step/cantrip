"""Native bundle boundaries: real Mach-O dependency relocation and fail-closed signing."""

import json
import os
from pathlib import Path
import platform
import plistlib
import runpy
import shutil
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
