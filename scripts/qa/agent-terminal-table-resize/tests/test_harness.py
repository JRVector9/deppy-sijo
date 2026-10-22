import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import analyze
import run


def table(width, padding=0):
    middle = width - 24
    border = "┌" + "─" * 11 + "┬" + "─" * middle + "┬" + "─" * 9 + "┐"
    lines = [border]
    for name, status, text in [("ROW_ALPHA", "ready", "한글 표"),
                               ("ROW_BETA", "waiting", "synthetic"),
                               ("ROW_OMEGA", "done", "원문과 복사 내용을 보존합니다.")]:
        lines.append(f"│ {name} │ {text} │ {status} │")
    lines.append(border.replace("┌", "└").replace("┬", "┴").replace("┐", "┘"))
    return "Fixture paragraph:\n" + "\n".join(x + " " * padding for x in lines)


class AnalyzerTests(unittest.TestCase):
    def case(self, root, widths=(80, 42, 120, 80), alt=False, history=False):
        rows = ["stage\tcols\trows\talt\tbytes"]
        for stage, (cols, width) in enumerate(zip((80, 42, 120, 80), widths)):
            rows.append(f"{stage}\t{cols}\t40\t{str(alt).lower()}\t10")
            source = "application-top" if alt and history else "history" if history else "stage"
            (root / f"{source}-{stage}-{cols}.txt").write_text(table(width, 13))
        (root / "metadata.tsv").write_text("\n".join(rows))
        return analyze.analyze_case(root, provider="grok", mode="minimal", history=history)

    def test_visible_frozen_table_is_not_resize_pass(self):
        with tempfile.TemporaryDirectory() as tmp:
            result = self.case(Path(tmp), (80, 42, 80, 80))
            self.assertEqual(result["visibility"], "PASS")
            self.assertEqual(result["resize"], "FAIL")
            self.assertEqual(result["exit_code"], 1)

    def test_matching_widths_with_cropped_history_are_not_resize_pass(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self.case(root, alt=True, history=True)
            path = root / "application-top-1-42.txt"
            path.write_text(path.read_text().split("└")[0])
            result = analyze.analyze_case(root, provider="grok", mode="minimal", history=True)
            self.assertEqual(result["resize"], "INVALID")
            self.assertEqual(result["exit_code"], 2)

    def test_missing_output_is_invalid(self):
        with tempfile.TemporaryDirectory() as tmp:
            self.assertEqual(analyze.analyze_case(Path(tmp), provider="codex", mode="inline", history=True)["exit_code"], 2)

    def test_alternate_history_never_falls_back_to_native(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self.case(root, alt=True, history=True)
            for path in root.glob("application-top-*.txt"):
                path.rename(root / path.name.replace("application-top", "history"))
            result = analyze.analyze_case(root, provider="grok", mode="minimal", history=True)
            self.assertEqual(result["validity"], "INVALID")
            self.assertEqual(result["exit_code"], 2)

    def test_cropped_boxed_table_is_invalid(self):
        result = analyze.inspect_snapshot(table(42).split("└")[0])
        self.assertFalse(result["complete"])

    def test_wrapped_identifiers_are_reconstructed_per_cell(self):
        text = "\n".join(["Fixture paragraph:", "┌────┬────────┬──────┐", "│ RO │ 한글   │ read │", "│ W_ │ 표     │ y    │", "│ AL │        │      │", "│ PH │        │      │", "│ A  │        │      │", "├────┼────────┼──────┤", "│ ROW_BETA │ synthetic │ waiting │", "├────┼────────┼──────┤", "│ ROW_OMEGA │ 원문과 복사 내용을 보존합니다. │ done │", "└────┴────────┴──────┘"])
        result = analyze.inspect_snapshot(text)
        self.assertTrue(result["semantic_complete"], result)

    def test_terminal_padding_and_korean_cell_width(self):
        result = analyze.inspect_snapshot(table(80, 30))
        self.assertEqual(result["border_width"], 80)
        self.assertEqual(analyze.cell_width("A한글e\u0301"), 6)
        self.assertTrue(result["semantic_complete"])

    def test_codex_narrow_semantics_use_cell_columns(self):
        text = (Path(__file__).parent / "fixtures/codex-inline-42.txt").read_text()
        result = analyze.inspect_snapshot(text)
        self.assertTrue(result["semantic_complete"], result)
        self.assertEqual(result["border_width"], 40)

    def test_visibility_uses_wrapped_table_cells_not_prose_marker(self):
        result = analyze.inspect_snapshot(table(80).replace("Fixture paragraph:", "Fixture\nparagraph:"))
        self.assertTrue(result["visibility"])

    def test_modal_confounds_resize_verdict(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self.case(root)
            path = root / "stage-2-120.txt"
            path.write_text(path.read_text() + "\nMake auto mode your default permission mode?")
            result = analyze.analyze_case(root, provider="grok", mode="minimal", history=False)
            self.assertEqual(result["validity"], "INVALID")
            self.assertEqual(result["exit_code"], 2)

    def test_clean_actual_claude_old_table_growth_fails(self):
        fixtures = Path(__file__).parent / "fixtures"
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self.case(root, history=True)
            for stage, cols in enumerate((80, 42, 120, 80)):
                text = (fixtures / f"claude-classic-history-{cols}.txt").read_text() if cols != 42 else "cropped history"
                (root / f"history-{stage}-{cols}.txt").write_text(text)
            result = analyze.analyze_case(root, provider="claude", mode="classic", history=True)
            self.assertEqual(result["resize"], "FAIL")
            self.assertEqual(result["validity"], "INVALID")
            self.assertEqual([s["border_width"] for s in result["stages"]], [76, None, 76, 76])
            self.assertFalse(any(s.get("modal") for s in result["stages"]))

    def test_actual_evidence_border_widths(self):
        fixtures = Path(__file__).parent / "fixtures"
        for filename, width, complete in [("grok-minimal-80.txt", 80, True), ("grok-fullscreen-42-cropped.txt", 23, False), ("codex-inline-120.txt", 118, True)]:
            with self.subTest(filename=filename):
                result = analyze.inspect_snapshot((fixtures / filename).read_text())
                self.assertEqual(result["border_width"], width)
                self.assertEqual(result["complete"], complete)


class RunnerTests(unittest.TestCase):
    def test_child_failure_exit_is_preserved(self):
        with tempfile.TemporaryDirectory() as tmp:
            result = run.run_child([sys.executable, "-c", "raise SystemExit(101)"], env={}, cwd=Path(tmp), log=Path(tmp) / "child.log", timeout=2)
            self.assertEqual(result["exit_code"], 101)

    def test_timeout_is_nonzero_and_child_is_reaped(self):
        with tempfile.TemporaryDirectory() as tmp:
            result = run.run_child([sys.executable, "-c", "import time; time.sleep(20)"], env={}, cwd=Path(tmp), log=Path(tmp) / "child.log", timeout=.1)
            self.assertEqual(result["exit_code"], 124)
            self.assertTrue(result["timed_out"])
            with self.assertRaises(ProcessLookupError):
                os.kill(result["pid"], 0)

    def test_timeout_closes_detached_pty_style_descendants(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            child_script = "import subprocess,sys,time; from pathlib import Path; p=subprocess.Popen([sys.executable,'-c','import time; time.sleep(20)'],start_new_session=True); Path('detached.pid').write_text(str(p.pid)); time.sleep(20)"
            result = run.run_child([sys.executable, "-c", child_script], env={}, cwd=root, log=root / "child.log", timeout=.3)
            pid = int((root / "detached.pid").read_text())
            try:
                state = subprocess.run(["/bin/ps", "-p", str(pid), "-o", "stat="], capture_output=True, text=True).stdout.strip()
                self.assertTrue(not state or state.startswith("Z"), f"detached child remains running: {pid} {state}")
                self.assertEqual(result["exit_code"], 124)
            finally:
                try:
                    os.kill(pid, 9)
                except ProcessLookupError:
                    pass

    def test_output_cap_is_nonzero_and_bounded(self):
        with tempfile.TemporaryDirectory() as tmp:
            log = Path(tmp) / "child.log"
            result = run.run_child([sys.executable, "-c", "import os; os.write(1, b'x' * 1000000)"], env={}, cwd=Path(tmp), log=log, timeout=2, output_limit=4096)
            self.assertNotEqual(result["exit_code"], 0)
            self.assertLessEqual(log.stat().st_size, 4096)

    def test_successful_responsive_table_passes_all_separate_checks(self):
        with tempfile.TemporaryDirectory() as tmp:
            result = AnalyzerTests().case(Path(tmp))
            self.assertEqual(result["validity"], "VALID")
            self.assertEqual(result["resize"], "PASS")
            self.assertEqual(result["semantics"], "PASS")
            self.assertEqual(result["exit_code"], 0)

    def test_fixture_env_does_not_inherit_credentials_or_repurpose_home(self):
        source = {"HOME": "/real/home", "PATH": "/usr/bin:/bin", "ANTHROPIC_AUTH_TOKEN": "secret", "OPENAI_API_KEY": "secret", "XAI_API_KEY": "secret", "SSH_AUTH_SOCK": "secret"}
        with tempfile.TemporaryDirectory() as tmp:
            env = run.fixture_env(Path(tmp), source)
        self.assertNotIn("ANTHROPIC_AUTH_TOKEN", env)
        self.assertNotIn("OPENAI_API_KEY", env)
        self.assertNotIn("SSH_AUTH_SOCK", env)
        self.assertEqual(env["HOME"], "/real/home")
        self.assertEqual(env["XAI_API_KEY"], "offline-fixture-not-a-real-key")


if __name__ == "__main__":
    unittest.main()
