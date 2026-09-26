#!/usr/bin/env python3
import mmap
import os
from pathlib import Path
import plistlib
import re
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parent.parent
APP = Path(os.environ.get("AERO_APP_BUNDLE", ROOT / "target/debug/bundle/Aero.app"))
LIB_DIR = Path(os.environ.get("ZVEC_LIB_DIR", ROOT / "vendor/zvec/lib")).resolve()


@unittest.skipUnless(sys.platform == "darwin", "macOS bundle checks require Xcode tools")
class BundleTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.temp = tempfile.TemporaryDirectory(prefix="aero bundle checks ")
        cls.addClassCleanup(cls.temp.cleanup)
        cls.app = Path(cls.temp.name) / "Moved Aero.app"
        # ditto 保留 macOS 扩展属性，包括本机的本地构建标记。
        subprocess.run(["ditto", APP, cls.app], check=True)
        cls.contents = cls.app / "Contents"

    def test_bundled_library(self):
        lib = self.contents / "Frameworks/libzvec_c_api.dylib"
        self.assertTrue(lib.is_file(), "bundle must contain libzvec_c_api.dylib")
        self.assertFalse(lib.is_symlink())
        deps = subprocess.check_output(["otool", "-L", lib], text=True)
        self.assertIn("@rpath/libzvec_c_api.dylib", deps)
        for line in deps.splitlines()[1:]:
            dependency = line.strip().split(" (compatibility version", 1)[0]
            self.assertTrue(dependency.startswith(("@rpath/", "/usr/lib/", "/System/Library/")), dependency)

    def test_fresh_trimmed_dictionary(self):
        bundled = self.contents / "Resources/jieba_dict"
        for name in ("jieba.dict.utf8", "hmm_model.utf8"):
            self.assertTrue((bundled / name).is_file(), f"bundle must contain {name}")
            self.assertFalse((bundled / name).is_symlink())
        expected = Path(self.temp.name) / "expected dictionary"
        env = dict(os.environ, SRC_DIR=str(LIB_DIR / "data/jieba_dict"), LC_ALL="C")
        subprocess.run(["bash", ROOT / "scripts/trim-jieba-dict.sh", expected], env=env, check=True, capture_output=True)
        for name in ("jieba.dict.utf8", "hmm_model.utf8"):
            self.assertEqual((bundled / name).read_bytes(), (expected / name).read_bytes())
        lines = (bundled / "jieba.dict.utf8").read_text().splitlines()
        self.assertGreaterEqual(len(lines), 100_000)
        self.assertLess(len(lines), 101_000)
        self.assertTrue(all(len(line.split()) == 3 for line in lines))

    def test_executable_uses_only_bundle_rpath(self):
        binary = self.contents / "MacOS/aero"
        load_commands = subprocess.check_output(["otool", "-l", binary], text=True)
        paths = re.findall(r"^\s*path (.*?) \(offset \d+\)$", load_commands, re.MULTILINE)
        self.assertEqual(paths, ["@executable_path/../Frameworks"])
        deps = subprocess.check_output(["otool", "-L", binary], text=True)
        self.assertIn("@rpath/libzvec_c_api.dylib", deps)

    def test_relocated_bundle_signature(self):
        result = subprocess.run(["codesign", "--verify", "--deep", "--strict", self.app], capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_embedded_demo_vault_matches_the_repository(self):
        """示例库随可执行文件分发：每个 .dmg 带的都是仓库里当前的示例。"""
        notes = sorted((ROOT / "assets/demo-vault").rglob("*.md"))
        self.assertEqual(len(notes), 7, "demo vault note count drifted; update NOTES and this check together")
        with (self.contents / "MacOS/aero").open("rb") as executable:
            image = mmap.mmap(executable.fileno(), 0, access=mmap.ACCESS_READ)
        try:
            for note in notes:
                self.assertNotEqual(image.find(note.read_bytes()), -1, f"{note.name} is not embedded")
        finally:
            image.close()

    def test_branding_and_metadata(self):
        with (self.contents / "Info.plist").open("rb") as file:
            info = plistlib.load(file)
        self.assertEqual(info["CFBundleExecutable"], "aero")
        self.assertEqual(info["CFBundleVersion"], info["CFBundleShortVersionString"])
        self.assertEqual((self.contents / "Resources/Aero.icns").read_bytes(), (ROOT / "assets/branding/aero-icon-v1.icns").read_bytes())


@unittest.skipUnless(sys.platform == "darwin", "macOS packaging only")
class PreflightTests(unittest.TestCase):
    def test_missing_resources_fail_before_cargo(self):
        for missing in ("libzvec_c_api.dylib", "data/jieba_dict/jieba.dict.utf8", "data/jieba_dict/hmm_model.utf8"):
            with self.subTest(missing=missing), tempfile.TemporaryDirectory(prefix="aero missing resources ") as temp:
                root = Path(temp)
                lib = root / "custom zvec"
                for name in ("libzvec_c_api.dylib", "data/jieba_dict/jieba.dict.utf8", "data/jieba_dict/hmm_model.utf8"):
                    if name != missing:
                        resource = lib / name
                        resource.parent.mkdir(parents=True, exist_ok=True)
                        resource.touch()
                cargo = root / "cargo"
                cargo.write_text('#!/bin/sh\nprintf "cargo must not run\\n" >&2\nexit 97\n')
                cargo.chmod(0o755)
                env = dict(os.environ, PATH=f"{root}:{os.environ['PATH']}", ZVEC_LIB_DIR=str(lib))
                result = subprocess.run(["bash", ROOT / "scripts/bundle-macos.sh"], env=env, capture_output=True, text=True)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(f"Missing zvec resource: {lib / missing}", result.stderr)
                self.assertNotIn("cargo must not run", result.stderr)


@unittest.skipUnless(sys.platform == "darwin", "macOS packaging only")
class DmgTests(unittest.TestCase):
    """scripts/dmg-macos.sh：镜像里只有 Aero.app 与 Applications 快捷方式，签名在挂载后仍然有效。"""

    @classmethod
    def setUpClass(cls):
        cls.temp = tempfile.TemporaryDirectory(prefix="aero dmg checks ")
        cls.addClassCleanup(cls.temp.cleanup)
        cls.dmg = Path(cls.temp.name) / "out" / "Aero-test.dmg"
        subprocess.run(["bash", ROOT / "scripts/dmg-macos.sh", APP, cls.dmg], check=True, capture_output=True)

    def test_dmg_contents_and_signature(self):
        self.assertTrue(self.dmg.is_file())
        attach = subprocess.check_output(["hdiutil", "attach", "-nobrowse", "-readonly", "-plist", self.dmg])
        entities = plistlib.loads(attach)["system-entities"]
        mount = Path(next(e["mount-point"] for e in entities if "mount-point" in e))
        try:
            self.assertEqual(sorted(p.name for p in mount.iterdir() if not p.name.startswith(".")), ["Aero.app", "Applications"])
            self.assertEqual(os.readlink(mount / "Applications"), "/Applications")
            result = subprocess.run(["codesign", "--verify", "--deep", "--strict", mount / "Aero.app"], capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual((mount / "Aero.app/Contents/Info.plist").read_bytes(), (APP / "Contents/Info.plist").read_bytes())
        finally:
            subprocess.run(["hdiutil", "detach", "-quiet", mount], check=True)

    def test_rejects_bad_arguments(self):
        for args in ([], [str(APP)], [str(APP), str(self.dmg.with_suffix(".img"))], [str(Path(self.temp.name)), str(self.dmg)]):
            with self.subTest(args=args):
                result = subprocess.run(["bash", ROOT / "scripts/dmg-macos.sh", *args], capture_output=True, text=True)
                self.assertNotEqual(result.returncode, 0)


@unittest.skipUnless(sys.platform == "darwin", "macOS packaging only")
class NotarizePreflightTests(unittest.TestCase):
    """公证脚本在动手之前就把缺凭据 / ad-hoc 签名拦下来；发布脚本的 --notarize 同理，且不触发构建。"""

    def env(self, **extra):
        env = {k: v for k, v in os.environ.items() if not k.startswith("AERO_NOTARY_") and k != "AERO_SIGN_IDENTITY"}
        env.update(extra)
        return env

    def test_missing_credentials(self):
        result = subprocess.run(["bash", ROOT / "scripts/notarize-macos.sh", APP], env=self.env(), capture_output=True, text=True)
        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertIn("AERO_NOTARY_PROFILE", result.stderr)

    def test_adhoc_signature_is_rejected_before_submission(self):
        env = self.env(AERO_NOTARY_PROFILE="does-not-exist")
        result = subprocess.run(["bash", ROOT / "scripts/notarize-macos.sh", APP], env=env, capture_output=True, text=True)
        self.assertEqual(result.returncode, 3, result.stderr)
        self.assertIn("Developer ID", result.stderr)

    def test_release_notarize_requires_identity_and_credentials(self):
        with tempfile.TemporaryDirectory(prefix="aero release preflight ") as temp:
            cargo = Path(temp) / "cargo"
            cargo.write_text('#!/bin/sh\nprintf "cargo must not run\\n" >&2\nexit 97\n')
            cargo.chmod(0o755)
            path = f"{temp}:{os.environ['PATH']}"
            result = subprocess.run(["bash", ROOT / "scripts/release-macos.sh", "--notarize", temp], env=self.env(PATH=path), capture_output=True, text=True)
            self.assertEqual(result.returncode, 2, result.stderr)
            self.assertIn("AERO_SIGN_IDENTITY", result.stderr)
            result = subprocess.run(["bash", ROOT / "scripts/release-macos.sh", "--notarize", temp], env=self.env(PATH=path, AERO_SIGN_IDENTITY="Developer ID Application: Test"), capture_output=True, text=True)
            self.assertEqual(result.returncode, 2, result.stderr)
            self.assertIn("AERO_NOTARY_PROFILE", result.stderr)
            self.assertNotIn("cargo must not run", result.stderr)
            result = subprocess.run(["bash", ROOT / "scripts/release-macos.sh", "--bogus"], env=self.env(PATH=path), capture_output=True, text=True)
            self.assertEqual(result.returncode, 1)
            self.assertIn("Usage", result.stderr)


if __name__ == "__main__":
    unittest.main()
