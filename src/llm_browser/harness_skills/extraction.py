from __future__ import annotations

import re
from typing import Any, Dict, List, Optional

from llm_browser.harness.api import HelperAPI
from llm_browser.harness_skills.research import make_fetch_text
from llm_browser.tool.web_fetch import (
    _extract_email_records,
    _extract_links,
    _extract_markdown_link_blocks,
    _html_to_readable_text,
    _normalize_email_domains,
)


SKILL = {
    "name": "extraction",
    "description": "Generic HTML/text extraction, sitemap parsing, email parsing, and link parsing helpers.",
    "exports": [
        "html_to_text",
        "extract_links",
        "extract_emails",
        "extract_markdown_link_blocks",
        "read_sitemap",
    ],
}


def install(api: HelperAPI) -> Dict[str, Any]:
    fetch_text = api.namespace.get("fetch_text")
    if not callable(fetch_text):
        fetch_text = make_fetch_text(api)

    def html_to_text(markup: str, max_chars: int = 30000, remove_chrome: bool = True) -> str:
        return _html_to_readable_text(str(markup or ""), max_chars=max_chars, remove_chrome=remove_chrome)

    def extract_links(text: str, pattern: Optional[str] = None, limit: int = 1000) -> List[str]:
        return _extract_links(str(text), pattern=pattern, limit=limit)

    def extract_markdown_link_blocks(
        text: str,
        url_pattern: Optional[str] = None,
        max_lines_after: int = 8,
        limit: int = 1000,
    ) -> List[Dict[str, Any]]:
        return _extract_markdown_link_blocks(
            str(text),
            url_pattern=url_pattern,
            max_lines_after=max_lines_after,
            limit=limit,
        )

    def extract_emails(
        text: str,
        domains: Optional[Any] = None,
        max_results: int = 200,
        include_context: bool = True,
    ) -> List[Dict[str, str]]:
        return _extract_email_records(
            str(text),
            domains=_normalize_email_domains(domains),
            max_results=max_results,
            include_context=include_context,
        )

    def read_sitemap(
        url: str,
        include: Optional[str] = None,
        exclude: Optional[str] = None,
        max_urls: int = 10000,
        timeout: float = 30.0,
        use_jina: Any = "auto",
    ) -> Dict[str, Any]:
        result = fetch_text(url, max_chars=2_000_000, use_jina=use_jina, timeout=timeout)
        text = str(result.get("text") or "")
        links = _extract_links(text, pattern=include, limit=max(max_urls * 2, max_urls))
        if exclude:
            exclude_re = re.compile(exclude)
            links = [link for link in links if not exclude_re.search(link)]
        return {
            "url": url,
            "source": result.get("source"),
            "status": result.get("status"),
            "chars": result.get("chars"),
            "truncated": result.get("truncated"),
            "links": links[:max_urls],
            "count": len(links),
        }

    return {
        "html_to_text": html_to_text,
        "extract_links": extract_links,
        "extract_emails": extract_emails,
        "extract_markdown_link_blocks": extract_markdown_link_blocks,
        "read_sitemap": read_sitemap,
    }
