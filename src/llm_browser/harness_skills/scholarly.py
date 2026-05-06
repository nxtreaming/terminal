from __future__ import annotations

from typing import Any, Dict, List, Tuple

from llm_browser.harness.api import HelperAPI
from llm_browser.tool.web_fetch import (
    _query_looks_scholarly,
    _search_crossref_api,
    _search_pubmed_api,
    _search_wikipedia_api,
)


SKILL = {
    "name": "scholarly",
    "description": "Scholarly and reference search fallbacks through Wikipedia, PubMed, and Crossref APIs.",
    "exports": ["query_looks_scholarly", "search_scholarly"],
}


def install(api: HelperAPI) -> Dict[str, Any]:
    def query_looks_scholarly(query: str) -> bool:
        return _query_looks_scholarly(query)

    def search_scholarly(query: str, max_results: int = 8, timeout: float = 20.0) -> Dict[str, Any]:
        results, attempts = _scholarly_candidates(api, query, limit=max_results, timeout=timeout)
        return {"query": query, "results": results[:max_results], "attempts": attempts}

    return {"query_looks_scholarly": query_looks_scholarly, "search_scholarly": search_scholarly}


def _scholarly_candidates(
    api: HelperAPI,
    query: str,
    *,
    limit: int,
    timeout: float,
) -> Tuple[List[Dict[str, str]], List[Dict[str, Any]]]:
    results: List[Dict[str, str]] = []
    attempts: List[Dict[str, Any]] = []
    for source, searcher in (
        ("wikipedia_api", _search_wikipedia_api),
        ("pubmed_api", _search_pubmed_api),
        ("crossref_api", _search_crossref_api),
    ):
        if len(results) >= limit:
            break
        try:
            found, attempt = searcher(query, limit=limit - len(results), timeout=timeout)
            api.check_cancel()
            results.extend(found[: max(0, limit - len(results))])
            attempt["parsed"] = min(len(found), limit)
            attempts.append(attempt)
        except Exception as exc:
            attempts.append({"source": source, "error": str(exc)})
    return results, attempts
