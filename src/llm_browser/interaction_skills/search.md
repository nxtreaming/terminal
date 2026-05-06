# Search

Use `load_skill("search")` when browser interaction is not necessary and a search result page is enough.

```python
load_skill("search")
result = search_web("site:example.com pricing", max_results=5)
```

For static pages after finding URLs, switch to `load_skill("research")` and `fetch_text()` / `fetch_many_text()`.
