

import pytest
import pymulter


# ---------- Test parse_boundary ----------
def test_parse_boundary():
    header = "multipart/form-data; boundary=----WebKitFormBoundary7MA4YWxkTrZu0gW"
    boundary = pymulter.parse_boundary(header)
    assert boundary == "----WebKitFormBoundary7MA4YWxkTrZu0gW"


# ---------- Test SizeLimitWrapper ----------
def test_size_limit_wrapper():
    sl = pymulter.SizeLimit(whole_stream=1000, per_field=100, fields={"file": 500})
    assert sl.whole_stream == 1000
    assert sl.per_field == 100
    assert sl.fields is not None
    assert sl.fields["file"] == 500


# ---------- Test ConstraintWrapper ----------
def test_constraint_wrapper():
    sl = pymulter.SizeLimit(whole_stream=1000)
    c = pymulter.Constraint(size_limit=sl, allowed_fields=["file", "desc"])
    assert c
    assert c.size_limit
    assert c.allowed_fields
    assert c.size_limit.whole_stream == 1000
    assert "file" in c.allowed_fields
    assert "desc" in c.allowed_fields


# ---------- Test MultipartParser and MultipartField (integration) ----------


def make_multipart_body(boundary, fields) -> bytes:
    # fields: list of (name, filename, content_type, value)
    lines: list[str] = []
    for name, filename, content_type, value in fields:
        lines.append(f"--{boundary}")
        disp = f'form-data; name="{name}"'
        if filename:
            disp += f'; filename="{filename}"'
        lines.append(f"Content-Disposition: {disp}")
        if content_type:
            lines.append(f"Content-Type: {content_type}")
        lines.append("")
        # Pastikan value bertipe str
        if isinstance(value, bytes):
            value = value.decode()
        lines.append(value)
    lines.append(f"--{boundary}--")
    lines.append("")
    return "\r\n".join(lines).encode()


@pytest.mark.asyncio
async def test_multipart_parser_and_field():
    boundary = "testboundary"
    fields = [
        ("file", "hello.txt", "text/plain", "Hello, world!"),
        ("desc", None, None, "A file upload"),
    ]
    body = make_multipart_body(boundary, fields)
    parser = pymulter.MultipartParser(boundary)
    # Feed in two chunks to test streaming
    await parser.feed(body[:20])
    await parser.feed(body[20:])
    await parser.close()

    # Ambil field pertama
    field = await parser.next_field()
    assert field.name == "file"
    assert field.filename == "hello.txt"
    assert field.content_type == "text/plain"
    assert (
        "Content-Disposition",
        'form-data; name="file"; filename="hello.txt"',
    ) in field.headers or any("file" in h[1] for h in field.headers)

    # Baca seluruh chunk
    data = b""
    async for chunk in field:
        data += bytes(chunk)
    assert data == b"Hello, world!"

    # Ambil field kedua
    field2 = await parser.next_field()
    assert field2.name == "desc"
    assert field2.filename is None
    assert field2.content_type is None
    data2 = b""
    async for chunk in field2:
        data2 += bytes(chunk)
    assert data2 == b"A file upload"

    # Habis, next_field harus nya None
    field3 = await parser.next_field()
    assert field3 is None


@pytest.mark.asyncio
async def test_multipart_parser_large_file():
    boundary = "largeboundary"
    # Buat data besar, misal 2MB
    large_content = b"x" * (2 * 1024 * 1024)
    fields = [
        ("bigfile", "big.txt", "application/octet-stream", large_content),
    ]
    body = make_multipart_body(boundary, fields)
    parser = pymulter.MultipartParser(boundary)
    # Feed in small chunks (simulate streaming)
    chunk_size = 65536
    for i in range(0, len(body), chunk_size):
        await parser.feed(body[i : i + chunk_size])
    await parser.close()

    field = await parser.next_field()
    assert field.name == "bigfile"
    assert field.filename == "big.txt"
    assert field.content_type == "application/octet-stream"

    # Baca seluruh chunk
    data = b""
    async for chunk in field:
        data += bytes(chunk)
    assert data == large_content

    # Habis, next_field harus nya None
    field2 = await parser.next_field()
    assert field2 is None

# Test parser with constraint (allowed_fields and size_limit)
@pytest.mark.asyncio
async def test_multipart_parser_with_constraint():
    boundary = "myboundary"
    # 2 fields, one allowed, one not
    fields = [
        ("allowed", "a.txt", "text/plain", b"abc"),
        ("notallowed", "b.txt", "text/plain", b"def"),
    ]
    body = make_multipart_body(boundary, fields)
    # Only allow "allowed" field
    constraint = pymulter.Constraint(allowed_fields=["allowed"])
    parser = pymulter.MultipartParser(boundary, constraints=constraint)
    await parser.feed(body)
    await parser.close()

    # First field is allowed
    field = await parser.next_field()
    assert field.name == "allowed"
    data = b""
    async for chunk in field:
        data += bytes(chunk)
    assert data == b"abc"

    # Second field is not allowed, should raise error
    with pytest.raises(RuntimeError, match='unknown field received: "notallowed"'):
        await parser.next_field()

@pytest.mark.asyncio
async def test_multipart_parser_with_size_limit():
    boundary = "sizebound"
    # 2 fields, one within limit, one exceeding
    fields = [
        ("f1", "f1.txt", "text/plain", b"12345"),
        ("f2", "f2.txt", "text/plain", b"1234567890"),
    ]
    body = make_multipart_body(boundary, fields)
    # Set per_field limit to 6 bytes
    size_limit = pymulter.SizeLimit(per_field=6)
    constraint = pymulter.Constraint(size_limit=size_limit)
    parser = pymulter.MultipartParser(boundary, constraints=constraint)
    await parser.feed(body)
    await parser.close()

    # First field is within limit
    field = await parser.next_field()
    assert field.name == "f1"
    data = b""
    async for chunk in field:
        data += bytes(chunk)
    assert data == b"12345"

    field = await parser.next_field()
    assert field.name == "f2"
    data = b""
    with pytest.raises(RuntimeError, match='field "f2" exceeded the size limit: 6 bytes'):
        async for chunk in field:
            data += bytes(chunk)
