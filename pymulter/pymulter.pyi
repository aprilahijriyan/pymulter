from typing import AsyncIterator, Dict, List, Optional, Tuple

class SizeLimit:
    def __init__(
        self,
        whole_stream: Optional[int] = None,
        per_field: Optional[int] = None,
        fields: Optional[Dict[str, int]] = None,
    ) -> None:
        self.whole_stream = whole_stream
        self.per_field = per_field
        self.fields = fields

class Constraint:
    def __init__(
        self,
        size_limit: Optional[SizeLimit] = None,
        allowed_fields: Optional[List[str]] = None,
    ) -> None:
        self.size_limit = size_limit
        self.allowed_fields = allowed_fields

class MultipartField:
    name: Optional[str]
    filename: Optional[str]
    content_type: Optional[str]
    headers: List[Tuple[str, str]]

    def __aiter__(self) -> AsyncIterator[bytes]: ...

class MultipartParser:
    def __init__(self, boundary: str, constraints: Optional[Constraint] = ...) -> None: ...
    async def feed(self, data: bytes) -> None: ...
    async def close(self) -> bool: ...
    async def next_field(self) -> MultipartField: ...

def parse_boundary(header: str) -> Optional[str]: ...
