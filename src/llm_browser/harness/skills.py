from __future__ import annotations

import importlib
import os
from pathlib import Path
from types import SimpleNamespace
from typing import Any, Dict, List, Optional

from llm_browser.browser.instructions import BROWSER_HELP_PLAYBOOK
from llm_browser.harness.api import HelperAPI
from llm_browser.harness.helpers import PRIMARY_CORE_HELPERS


PYTHON_SKILLS: Dict[str, str] = {
    "downloads": "llm_browser.harness_skills.downloads",
    "cookies": "llm_browser.harness_skills.cookies",
    "artifacts": "llm_browser.harness_skills.artifacts",
    "cloud_artifacts": "llm_browser.harness_skills.cloud_artifacts",
    "research": "llm_browser.harness_skills.research",
    "search": "llm_browser.harness_skills.search",
    "public_records": "llm_browser.harness_skills.public_records",
    "scholarly": "llm_browser.harness_skills.scholarly",
    "extraction": "llm_browser.harness_skills.extraction",
    "store_locators": "llm_browser.harness_skills.store_locators",
    "dom_tools": "llm_browser.harness_skills.dom_tools",
    "cookie_banners": "llm_browser.harness_skills.cookie_banners",
    "uploads": "llm_browser.harness_skills.uploads",
    "tracing": "llm_browser.harness_skills.tracing",
    "harnesless_compat": "llm_browser.harness_skills.harnesless_compat",
}

DEFAULT_AUTOLOAD_SKILLS = "core"


def install_skill_loader(api: HelperAPI) -> Dict[str, Any]:
    def list_skills() -> List[Dict[str, Any]]:
        items: List[Dict[str, Any]] = []
        for name in sorted(PYTHON_SKILLS):
            meta = _skill_meta(name)
            items.append(
                {
                    "name": name,
                    "kind": "python",
                    "description": meta.get("description", ""),
                    "exports": list(meta.get("exports", [])),
                }
            )
        for name, path in _interaction_skill_paths().items():
            items.append({"name": name, "kind": "interaction", "path": str(path)})
        return items

    def loaded_skills() -> List[str]:
        return sorted((api.namespace.get("_loaded_browser_skills") or {}).keys())

    def load_skill(name: str, inject: bool = True) -> Any:
        return _load_python_skill(api, name, inject=inject)

    def read_skill(name: str) -> str:
        return _read_skill(name)

    def help_browser(topic: Optional[str] = None) -> str:
        if topic:
            return _read_skill(topic)
        python_skills = ", ".join(sorted(PYTHON_SKILLS))
        interaction = ", ".join(sorted(_interaction_skill_paths()))
        return (
            "Browser Python harness\n\n"
            "Default workflow:\n"
            "  new_tab('https://example.com')\n"
            "  capture_screenshot()\n"
            "  click_at_xy(410, 520)\n"
            "  wait_for_network_idle()\n"
            "  capture_screenshot()\n\n"
            + BROWSER_HELP_PLAYBOOK.rstrip()
            + "\n\nCore helpers:\n  "
            + ", ".join(PRIMARY_CORE_HELPERS + ["load_skill", "list_skills", "read_skill", "loaded_skills", "help_browser"])
            + "\n\nPython skills are opt-in. Use list_skills() for metadata and load_skill(name) before calling exports.\n  "
            + (python_skills or "(none)")
            + "\n\nInteraction skills are readable playbooks, not default behavior. Use read_skill(name).\n  "
            + (interaction or "(none)")
            + "\n\nKeep task-specific routines in agent_helpers.py. Use raw cdp(...) when a helper is too narrow."
        )

    exports = {
        "load_skill": load_skill,
        "list_skills": list_skills,
        "read_skill": read_skill,
        "loaded_skills": loaded_skills,
        "help_browser": help_browser,
    }
    api.namespace.update(exports)
    return exports


def autoload_skills(api: HelperAPI) -> None:
    requested = os.environ.get("LLM_BROWSER_AUTOLOAD_SKILLS", DEFAULT_AUTOLOAD_SKILLS).strip()
    if _env_truthy("LLM_BROWSER_LEGACY_AUTOLOAD_SKILLS"):
        requested = "all"
    loaded = set((api.namespace.get("_loaded_browser_skills") or {}).keys())
    names = _parse_autoload(requested)
    names.update(loaded)
    for name in sorted(names):
        _load_python_skill(api, name, inject=True)


def _parse_autoload(value: str) -> set[str]:
    if not value or value.lower() in {"0", "false", "no", "none", "core", "off"}:
        return set()
    if value.lower() in {"all", "legacy", "compat", "compatibility"}:
        return set(PYTHON_SKILLS)
    return {item.strip() for item in value.split(",") if item.strip()}


def _env_truthy(name: str) -> bool:
    value = os.environ.get(name)
    return bool(value and value.strip().lower() in {"1", "true", "yes", "on"})


def _load_python_skill(api: HelperAPI, name: str, inject: bool = True) -> Any:
    normalized = _normalize_skill_name(name)
    if normalized not in PYTHON_SKILLS:
        available = ", ".join(sorted(PYTHON_SKILLS))
        raise ValueError(f"Unknown browser skill {name!r}. Available Python skills: {available}")
    module = importlib.import_module(PYTHON_SKILLS[normalized])
    install = getattr(module, "install", None)
    if not callable(install):
        raise RuntimeError(f"Browser skill {normalized!r} does not define install(api)")
    exports = install(api)
    if not isinstance(exports, dict):
        raise RuntimeError(f"Browser skill {normalized!r} install(api) must return a dict")
    meta = dict(getattr(module, "SKILL", {}) or {})
    meta.setdefault("name", normalized)
    meta.setdefault("description", "")
    meta["exports"] = sorted(exports)
    meta["kind"] = "python"
    if not inject:
        return SimpleNamespace(**exports, __skill__=meta)

    loaded = dict(api.namespace.get("_loaded_browser_skills") or {})
    already_loaded = normalized in loaded
    api.namespace.update(exports)
    loaded[normalized] = meta
    api.namespace["_loaded_browser_skills"] = loaded
    _refresh_helper_modules(api.namespace)
    return {
        "name": normalized,
        "description": meta.get("description", ""),
        "exports": meta["exports"],
        "already_loaded": already_loaded,
    }


def _skill_meta(name: str) -> Dict[str, Any]:
    module = importlib.import_module(PYTHON_SKILLS[name])
    meta = dict(getattr(module, "SKILL", {}) or {})
    meta.setdefault("name", name)
    meta.setdefault("description", "")
    meta.setdefault("exports", [])
    return meta


def _refresh_helper_modules(namespace: Dict[str, Any]) -> None:
    try:
        from llm_browser.tool.browser_exports import install_browser_helpers_module
    except Exception:
        return
    install_browser_helpers_module(namespace)


def _read_skill(name: str) -> str:
    normalized = _normalize_skill_name(name)
    interaction_name = _normalize_interaction_skill_name(name)
    chunks: List[str] = []
    if normalized in PYTHON_SKILLS:
        meta = _skill_meta(normalized)
        chunks.append(
            "# "
            + normalized
            + "\n\n"
            + str(meta.get("description") or "")
            + "\n\nExports:\n"
            + "\n".join(f"- {item}" for item in meta.get("exports", []))
        )
        docs = str(meta.get("docs") or "").strip()
        if docs:
            chunks.append(docs)
    interaction = _interaction_skill_paths().get(interaction_name)
    if interaction is not None:
        chunks.append(interaction.read_text(encoding="utf-8"))
    if chunks:
        return "\n\n".join(chunks)
    available = sorted(set(PYTHON_SKILLS) | set(_interaction_skill_paths()))
    raise ValueError(f"Unknown browser skill {name!r}. Available skills: {', '.join(available)}")


def _normalize_skill_name(name: str) -> str:
    raw = str(name).strip().lower()
    if raw.endswith(".md"):
        raw = raw[:-3]
    dashed = raw.replace("_", "-")
    underscored = raw.replace("-", "_")
    if underscored in PYTHON_SKILLS:
        return underscored
    if dashed in _interaction_skill_paths():
        return dashed
    return underscored


def _normalize_interaction_skill_name(name: str) -> str:
    raw = str(name).strip().lower()
    if raw.endswith(".md"):
        raw = raw[:-3]
    return raw.replace("_", "-")


def _interaction_skill_paths() -> Dict[str, Path]:
    root = Path(__file__).resolve().parents[1] / "interaction_skills"
    if not root.exists():
        return {}
    return {path.stem.lower(): path for path in sorted(root.glob("*.md"))}
