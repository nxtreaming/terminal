from __future__ import annotations

import json
import random
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Dict, Iterable, List, Optional


DATASET_ALIASES = {
    "real_v8": Path("datasets/real_v8.json"),
    "real_v14": Path("datasets/real_v14_short.json"),
    "real_v14_short": Path("datasets/real_v14_short.json"),
}


@dataclass(frozen=True)
class DatasetTask:
    dataset: str
    path: Path
    task_id: str
    text: str
    raw: Dict[str, Any]

    def to_dict(self) -> Dict[str, Any]:
        return {
            "dataset": self.dataset,
            "path": str(self.path),
            "task_id": self.task_id,
            "text": self.text,
            "raw": self.raw,
        }


def resolve_dataset(name_or_path: str, cwd: Optional[Path] = None) -> Path:
    root = cwd or Path.cwd()
    if name_or_path in DATASET_ALIASES:
        return (root / DATASET_ALIASES[name_or_path]).resolve()
    path = Path(name_or_path).expanduser()
    if not path.is_absolute():
        path = root / path
    return path.resolve()


def load_dataset(name_or_path: str, cwd: Optional[Path] = None) -> List[DatasetTask]:
    path = resolve_dataset(name_or_path, cwd=cwd)
    data = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(data, list):
        raise ValueError(f"dataset must be a JSON list: {path}")
    dataset_name = _dataset_name(name_or_path, path)
    tasks: List[DatasetTask] = []
    for index, item in enumerate(data, start=1):
        if not isinstance(item, dict):
            raise ValueError(f"dataset item {index} is not an object in {path}")
        task_id = str(item.get("task_id") or index)
        text = str(item.get("confirmed_task") or item.get("task") or item.get("text") or "").strip()
        if not text:
            raise ValueError(f"dataset item {task_id} has no task text in {path}")
        tasks.append(DatasetTask(dataset=dataset_name, path=path, task_id=task_id, text=text, raw=item))
    return tasks


def select_tasks(
    tasks: List[DatasetTask],
    count: int = 1,
    seed: Optional[int] = None,
    task_ids: Optional[Iterable[str]] = None,
) -> List[DatasetTask]:
    if task_ids:
        wanted = {str(task_id) for task_id in task_ids}
        selected = [task for task in tasks if task.task_id in wanted]
        missing = wanted - {task.task_id for task in selected}
        if missing:
            raise ValueError(f"task id(s) not found: {', '.join(sorted(missing))}")
        return selected
    rng = random.Random(seed)
    if count >= len(tasks):
        selected = list(tasks)
        rng.shuffle(selected)
        return selected
    return rng.sample(tasks, count)


def build_dataset_prompt(task: DatasetTask, headless: bool = True) -> str:
    headless_text = "true" if headless else "false"
    return (
        "You are running a browser-use-terminal dataset task.\n"
        f"Dataset: {task.dataset}\n"
        f"Task ID: {task.task_id}\n\n"
        "Use the python tool and raw CDP/browser helpers freely. "
        f"When launching browser work, use headless {headless_text} unless the task requires visible Chrome. "
        "The current working directory is an isolated task workspace; save requested task files there, "
        "or use artifact_dir/save_artifact for supporting evidence. "
        "Attach screenshots after meaningful page transitions or before relying on visual state. "
        "Recover from broken helpers by using raw CDP, JavaScript, shell, requests, or local helper code. "
        "Respect any output constraints in the task. Finish by calling done with the final answer.\n\n"
        f"Task:\n{task.text}"
    )


def dataset_summary(tasks: List[DatasetTask]) -> Dict[str, Any]:
    by_dataset: Dict[str, int] = {}
    for task in tasks:
        by_dataset[task.dataset] = by_dataset.get(task.dataset, 0) + 1
    return {"count": len(tasks), "datasets": by_dataset}


def _dataset_name(name_or_path: str, path: Path) -> str:
    for alias, alias_path in DATASET_ALIASES.items():
        if path.name == alias_path.name:
            return alias
    return Path(name_or_path).stem
