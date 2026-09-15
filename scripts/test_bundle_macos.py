#!/usr/bin/env python3
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


if __name__ == "__main__":
    unittest.main()
