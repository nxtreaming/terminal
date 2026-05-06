# Public Records

Use `load_skill("public_records")` when the query is clearly about structured public-record identifiers.

```python
load_skill("public_records")
search_cve_records("CVE-2020-8166")
search_fcc_grantee_records("2ACAH BCE FCC grantee code")
```

These are shortcuts for known public databases. For ordinary web search, use `load_skill("search")`.
