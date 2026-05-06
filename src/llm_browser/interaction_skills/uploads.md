# Uploads

Use `load_skill("uploads")` for standard file inputs.

```python
load_skill("uploads")
upload_file("input[type=file]", "invoice.pdf")
```

For custom drag/drop uploaders, first try coordinate clicks and screenshots. If the page hides the real file input, use `js(...)` or raw `cdp(...)` to find the backing input.
