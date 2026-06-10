"""Nebula SDK (v1alpha1): talk to a local (or, later, remote) Nebula engine.

    from nebula_vm import NebulaClient
    nebula = NebulaClient()
    print(nebula.status()["vmState"])
    result = nebula.exec("uname", ["-a"])
    print(result["stdout"])

Stdlib-only on purpose: zero dependencies to embed anywhere.
"""

from __future__ import annotations

import json
import urllib.error
import urllib.request
from typing import Any, Optional

__all__ = ["NebulaClient", "NebulaError"]
__version__ = "0.1.0"


class NebulaError(RuntimeError):
    """Engine API error; .status carries the HTTP status when available."""

    def __init__(self, message: str, status: Optional[int] = None):
        super().__init__(message)
        self.status = status


class NebulaClient:
    def __init__(self, base_url: str = "http://127.0.0.1:7440", timeout: float = 30.0):
        self.base_url = base_url.rstrip("/")
        self.timeout = timeout

    def status(self) -> dict[str, Any]:
        """Engine + guest agent status."""
        return self._request("GET", "/v1alpha1/status")

    def stats(self) -> dict[str, Any]:
        """Live memory/balloon/footprint stats."""
        return self._request("GET", "/v1alpha1/stats")

    def exec(self, cmd: str, args: Optional[list[str]] = None, timeout_ms: int = 30_000) -> dict[str, Any]:
        """Run a command inside the Vessel; returns exit_code/stdout/stderr."""
        return self._request(
            "POST",
            "/v1alpha1/exec",
            {"cmd": cmd, "args": args or [], "timeout_ms": timeout_ms},
        )

    def containers(self) -> list[dict[str, Any]]:
        """List containers (Docker Engine API ContainerSummary shape)."""
        return self._request("GET", "/v1alpha1/containers")

    def is_running(self) -> bool:
        """True when the engine API is reachable."""
        try:
            self._request("GET", "/healthz")
            return True
        except Exception:
            return False

    def _request(self, method: str, path: str, body: Optional[dict] = None) -> Any:
        url = f"{self.base_url}{path}"
        data = json.dumps(body).encode() if body is not None else None
        req = urllib.request.Request(url, data=data, method=method)
        if data is not None:
            req.add_header("Content-Type", "application/json")
        try:
            with urllib.request.urlopen(req, timeout=self.timeout) as resp:
                return json.loads(resp.read())
        except urllib.error.HTTPError as e:
            try:
                detail = json.loads(e.read()).get("error", str(e))
            except Exception:
                detail = str(e)
            raise NebulaError(detail, status=e.code) from None
        except urllib.error.URLError as e:
            raise NebulaError(f"engine unreachable at {self.base_url}: {e.reason}") from None
