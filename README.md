# pymulter

`pymulter` is a Python binding for the Rust library [multer](https://github.com/rwf2/multer).

> **Note:**  
> This module was created with the help of AI tools to bridge between Python and Rust.  
> I'm not a Rust developer, so feedback and contributions are appreciated!

## Features

- **Async Streaming Multipart Parsing**: Efficiently parse multipart form data in a streaming fashion, suitable for large file uploads.
- **Field Constraints**: Restrict which fields are accepted using `allowed_fields`.
- **Size Limits**: Enforce limits on the whole stream, per field, or per field name.
- **Typed API**: Exposes clear Python classes for size limits, constraints, parser, and fields.
- **Header Parsing Utility**: Extract boundary from Content-Type headers.

## Installation

Install from PyPI (recommended):

```bash
pip install pymulter
# or
uv add pymulter
```

For development (editable mode), use:

```bash
pip install maturin
maturin develop
```

## Usage Example

> **NOTE:**  
> `pymulter` is designed for advanced use cases where you need to process `multipart/form-data` uploads in an async, streaming fashion.  
>
> - You must use `await` with all async methods (`feed`, `close`, `next_field`, and iterating over fields).
> - You are responsible for extracting the boundary from the `Content-Type` header using `pymulter.parse_boundary`.
> - Data should be fed to the parser in bytes, and you can call `feed` multiple times as you receive data (e.g., from a network stream).
> - After feeding all data, call `await parser.close()` before iterating fields.
> - Each field is an async iterator yielding chunks of bytes.
> - Use constraints (`Constraint`, `SizeLimit`) to restrict allowed fields or enforce size limits as needed.
>
> See the example below for a typical usage pattern.

```python
import pymulter

# Extract boundary from Content-Type header
type_header = "multipart/form-data; boundary=----WebKitFormBoundary7MA4YWxkTrZu0gW"
boundary = pymulter.parse_boundary(type_header)

# Create a parser (optionally with constraints)
parser = pymulter.MultipartParser(boundary)

# Feed data (can be called multiple times for streaming)
await parser.feed(b"--boundary...multipart body bytes...")
await parser.close()

# Iterate over fields
while True:
    field = await parser.next_field()
    if field is None:
        break
    print("Field name:", field.name)
    print("Filename:", field.filename)
    print("Content-Type:", field.content_type)
    print("Headers:", field.headers)
    data = b""
    async for chunk in field:
        data += chunk
    print("Data:", data)
```

## Testing

Install test dependencies and run tests:

```bash
maturin develop -E tests
pytest
```
