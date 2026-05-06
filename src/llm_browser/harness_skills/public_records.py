from __future__ import annotations

from typing import Any, Dict, List, Tuple

from llm_browser.harness.api import HelperAPI
from llm_browser.tool.web_fetch import (
    _extract_cve_ids,
    _extract_fcc_grantee_codes,
    _search_cve_records,
    _search_fcc_grantee_records,
)


SKILL = {
    "name": "public_records",
    "description": "Targeted public-record search shortcuts such as CVE and FCC grantee lookups.",
    "exports": ["search_public_records", "search_cve_records", "search_fcc_grantee_records"],
}


def install(api: HelperAPI) -> Dict[str, Any]:
    def search_cve_records(query: str, max_results: int = 8) -> Dict[str, Any]:
        results, attempts = _public_record_candidates(query, limit=max_results, include_cve=True, include_fcc=False)
        return {"query": query, "results": results[:max_results], "attempts": attempts}

    def search_fcc_grantee_records(query: str, max_results: int = 8) -> Dict[str, Any]:
        results, attempts = _public_record_candidates(query, limit=max_results, include_cve=False, include_fcc=True)
        return {"query": query, "results": results[:max_results], "attempts": attempts}

    def search_public_records(query: str, max_results: int = 8) -> Dict[str, Any]:
        results, attempts = _public_record_candidates(query, limit=max_results, include_cve=True, include_fcc=True)
        return {"query": query, "results": results[:max_results], "attempts": attempts}

    return {
        "search_public_records": search_public_records,
        "search_cve_records": search_cve_records,
        "search_fcc_grantee_records": search_fcc_grantee_records,
    }


def _public_record_candidates(
    query: str,
    *,
    limit: int,
    include_cve: bool = True,
    include_fcc: bool = True,
) -> Tuple[List[Dict[str, str]], List[Dict[str, Any]]]:
    results: List[Dict[str, str]] = []
    attempts: List[Dict[str, Any]] = []
    if include_cve:
        cve_ids = _extract_cve_ids(query)
        if cve_ids:
            found, attempt = _search_cve_records(cve_ids, limit=limit)
            results.extend(found[: max(0, limit - len(results))])
            attempt["parsed"] = min(len(found), limit)
            attempts.append(attempt)
    if include_fcc and len(results) < limit:
        fcc_codes = _extract_fcc_grantee_codes(query)
        if fcc_codes:
            found, attempt = _search_fcc_grantee_records(fcc_codes, limit=limit - len(results))
            results.extend(found[: max(0, limit - len(results))])
            attempt["parsed"] = min(len(found), limit)
            attempts.append(attempt)
    return results, attempts
