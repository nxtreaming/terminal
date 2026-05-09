from __future__ import annotations

import re
import time
from typing import Any, Dict, List
from urllib.parse import quote_plus

from llm_browser.harness.api import HelperAPI
from llm_browser.harness_skills.scholarly import _scholarly_candidates
from llm_browser.tool.web_fetch import (
    _browser_headers,
    _jina_reader_url,
    _looks_like_external_result_url,
    _normalize_search_url,
    _parse_bing_results,
    _parse_brave_results,
    _parse_duckduckgo_results,
    _parse_generic_search_results,
    _parse_markdown_links,
    _query_looks_scholarly,
)


SKILL = {
    "name": "search",
    "description": "General web search across search result pages, with optional scholarly fallbacks.",
    "exports": ["search_web"],
}


def install(api: HelperAPI) -> Dict[str, Any]:
    def search_web(
        query: str,
        max_results: int = 8,
        timeout: float = 20.0,
        save_raw: Any = "auto",
        include_specialized: Any = "auto",
    ) -> Dict[str, Any]:
        try:
            import requests
        except Exception as exc:
            raise RuntimeError("requests is not installed") from exc

        raw_mode = str(save_raw).lower()
        save_raw_always = save_raw is True or raw_mode in {"1", "true", "yes", "always", "all"}
        save_raw_auto = raw_mode in {"auto", "failed", "empty"}
        urls = [
            ("bing", f"https://www.bing.com/search?q={quote_plus(query)}"),
            ("duckduckgo_html", f"https://html.duckduckgo.com/html/?q={quote_plus(query)}"),
            ("duckduckgo_lite", f"https://lite.duckduckgo.com/lite/?q={quote_plus(query)}"),
            ("brave", f"https://search.brave.com/search?q={quote_plus(query)}"),
            ("google_reader", _jina_reader_url(f"https://www.google.com/search?q={quote_plus(query)}")),
            ("bing_reader", _jina_reader_url(f"https://www.bing.com/search?q={quote_plus(query)}")),
        ]
        results: List[Dict[str, str]] = []
        attempts: List[Dict[str, Any]] = []
        seen_urls: set[str] = set()

        def add_results(candidates: List[Dict[str, str]]) -> List[Dict[str, str]]:
            added: List[Dict[str, str]] = []
            for candidate in candidates:
                url = _normalize_search_url(candidate.get("url", ""))
                if not url or url in seen_urls or not _looks_like_external_result_url(url):
                    continue
                seen_urls.add(url)
                item = dict(candidate)
                item["url"] = url
                results.append(item)
                added.append(item)
                if len(results) >= max_results:
                    break
            return added

        def save_search_page(source: str, text: str) -> str:
            search_dir = api.cwd / "search_pages"
            search_dir.mkdir(parents=True, exist_ok=True)
            slug = re.sub(r"[^a-zA-Z0-9_.-]+", "-", query).strip("-")[:80] or "query"
            path = search_dir / f"{int(time.time() * 1000)}-{source}-{slug}.html"
            path.write_text(text, encoding="utf-8", errors="replace")
            return str(path)

        for source, search_url in urls:
            if len(results) >= max_results:
                break
            try:
                api.check_cancel()
                source_timeout = min(timeout, 12.0 if source.endswith("_reader") else 6.0)
                response = requests.get(search_url, headers=_browser_headers(), timeout=source_timeout)
                api.check_cancel()
                text = response.text
                if source == "bing":
                    parsed = _parse_bing_results(text, limit=max_results - len(results))
                elif source.startswith("duckduckgo"):
                    parsed = _parse_duckduckgo_results(text, limit=max_results - len(results))
                elif source == "brave":
                    parsed = _parse_brave_results(text, limit=max_results - len(results))
                elif source.endswith("_reader"):
                    parsed = _parse_markdown_links(text, limit=max_results - len(results), source=source)
                else:
                    parsed = _parse_generic_search_results(text, source=source, limit=max_results - len(results))
                added = add_results(parsed)
                attempt: Dict[str, Any] = {
                    "source": source,
                    "status": response.status_code,
                    "url": response.url,
                    "chars": len(text),
                    "parsed": len(added),
                }
                if save_raw_always or (save_raw_auto and not added):
                    attempt["raw_path"] = save_search_page(source, text)
                attempts.append(attempt)
            except Exception as exc:
                attempts.append({"source": source, "url": search_url, "error": str(exc)})
            if len(results) >= max_results:
                break

        specialized_mode = str(include_specialized).lower()
        specialized_enabled = (
            include_specialized is True
            or specialized_mode in {"1", "true", "yes", "always"}
            or (specialized_mode == "auto" and _query_looks_scholarly(query))
        )
        if specialized_enabled and len(results) < max_results:
            scholarly_results, scholarly_attempts = _scholarly_candidates(
                api,
                query,
                limit=max_results - len(results),
                timeout=timeout,
            )
            before = len(results)
            add_results(scholarly_results)
            added_count = len(results) - before
            remaining_added = added_count
            for attempt in scholarly_attempts:
                parsed = int(attempt.get("parsed") or 0)
                assigned = min(parsed, remaining_added) if remaining_added > 0 else 0
                attempt["parsed"] = assigned
                remaining_added -= assigned
                attempts.append(attempt)

        return {"query": query, "results": results[:max_results], "attempts": attempts}

    return {"search_web": search_web}
