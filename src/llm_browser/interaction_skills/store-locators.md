# Store Locators

Use `load_skill("store_locators")` only when the task asks for store/branch/location lists and the website appears to use a locator API.

```python
load_skill("store_locators")
result = extract_store_locator_locations("https://brand.example/locations", save_to="stores.json")
```

This skill contains site-pattern recipes. It is intentionally separate from generic extraction because locator providers often expose special JSON endpoints.
