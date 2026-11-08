---
"wry": patch
---

On Android, fix crashes in the file chooser when the camera or picker returns `RESULT_OK` without data, when the chooser intent has no MIME type, or when an extension filter maps to no known MIME type.
