from __future__ import annotations

import unittest
from pathlib import Path

from llm_browser.datasets import build_dataset_prompt, load_dataset, select_tasks


class DatasetTest(unittest.TestCase):
    def test_load_and_select_real_v8(self) -> None:
        tasks = load_dataset("real_v8", cwd=Path.cwd())

        self.assertGreaterEqual(len(tasks), 1)
        selected = select_tasks(tasks, count=2, seed=123)
        self.assertEqual(len(selected), 2)
        self.assertNotEqual(selected[0].task_id, selected[1].task_id)

    def test_prompt_wraps_task_without_hiding_original_text(self) -> None:
        task = select_tasks(load_dataset("real_v14_short", cwd=Path.cwd()), task_ids=["2"])[0]
        prompt = build_dataset_prompt(task, headless=True)

        self.assertIn("Dataset:", prompt)
        self.assertIn("Task ID: 2", prompt)
        self.assertIn(task.text[:80], prompt)
        self.assertIn("Attach screenshots", prompt)


if __name__ == "__main__":
    raise SystemExit(unittest.main())
