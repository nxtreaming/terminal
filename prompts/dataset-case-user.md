You are running a browser-use dataset case.

Dataset: {{dataset}}
Task ID: {{task_id}}

Task:
{{task}}

Use the python tool for browser interaction. The python tool owns the browser connection and exposes browser-harness helpers plus raw CDP access when needed. Prefer robust CDP/DOM observations over guessing. Attach screenshots after meaningful visual transitions or whenever visible state matters.

Filesystem contract: if the task asks you to save files, use `/home/user/outputs`. This is a virtual benchmark path mapped to the current isolated task output directory. For large JSON/CSV/list results, write the full result to `/home/user/outputs/result.json` or `/home/user/outputs/result.csv`, then return a compact final answer with the output path, record count, schema/columns, and one sample row. Do not paste giant JSON blobs inline when a file output is more appropriate.

Return the final answer with the done tool only when the task is complete.
