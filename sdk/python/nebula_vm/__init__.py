"""Nebula SDK (v1alpha1): talk to a local (or remote, token-authed) Nebula
engine — full Nebula and Nebula-slim serve the same API.

    from nebula_vm import NebulaClient
    nebula = NebulaClient()                    # token: NEBULA_API_TOKEN if set
    print(nebula.status()["vmState"])
    print(nebula.exec("uname", ["-a"])["stdout"])

    # vessels: create, snapshot, fan out 8 live clones
    nebula.vessels.create("agent0")
    nebula.vessels.snapshot("agent0", "s1")
    nebula.vessels.branch("agent0", "fork", label="s1", count=8)

Stdlib-only on purpose: zero dependencies to embed anywhere.
"""

from __future__ import annotations

import json
import os
import urllib.error
import urllib.parse
import urllib.request
from typing import Any, Optional

__all__ = ["NebulaClient", "NebulaError", "Vessels"]
__version__ = "0.2.0"


class NebulaError(RuntimeError):
    """Engine API error; .status carries the HTTP status when available."""

    def __init__(self, message: str, status: Optional[int] = None):
        super().__init__(message)
        self.status = status


class NebulaClient:
    def __init__(
        self,
        base_url: str = "http://127.0.0.1:7440",
        timeout: float = 30.0,
        token: Optional[str] = None,
    ):
        self.base_url = base_url.rstrip("/")
        self.timeout = timeout
        self.token = token if token is not None else os.environ.get("NEBULA_API_TOKEN")
        #: Named-vessel lifecycle (microVMs with snapshots + live branching).
        self.vessels = Vessels(self)

    def status(self) -> dict[str, Any]:
        """Engine + guest agent status."""
        return self._request("GET", "/v1alpha1/status")

    def stats(self) -> dict[str, Any]:
        """Live memory/balloon/footprint stats."""
        return self._request("GET", "/v1alpha1/stats")

    def exec(self, cmd: str, args: Optional[list[str]] = None, timeout_ms: int = 30_000) -> dict[str, Any]:
        """Run a command inside the engine vessel; returns exit_code/stdout/stderr."""
        return self._request(
            "POST",
            "/v1alpha1/exec",
            {"cmd": cmd, "args": args or [], "timeout_ms": timeout_ms},
            timeout=timeout_ms / 1000 + 5,
        )

    def balloon(self, target_mib: int) -> dict[str, Any]:
        """Set the memory balloon target."""
        return self._request("POST", "/v1alpha1/balloon", {"target_mib": target_mib})

    def containers(self) -> list[dict[str, Any]]:
        """List containers (Docker Engine API ContainerSummary shape)."""
        return self._request("GET", "/v1alpha1/containers")

    def kubeconfig(self) -> str:
        """The standalone kubeconfig YAML (k3s on full Nebula; slim's TLS
        apiserver on slim). Feed it to any kubernetes client:
        ``kubernetes.config.load_kube_config_from_dict(yaml.safe_load(...))``.
        """
        return self._request_text("GET", "/v1alpha1/kubeconfig")

    def docker(self, method: str, path: str, body: Optional[dict] = None) -> Any:
        """Raw call against the engine's Docker API (`/docker` plane) —
        paths/payloads are the Docker Engine API verbatim, e.g.
        ``nebula.docker("GET", "/v1.43/containers/json?all=true")``."""
        return self._request(method, f"/docker{path}", body)

    def k8s(self, method: str, path: str, body: Optional[dict] = None) -> Any:
        """Raw call against the kubernetes apiserver (`/k8s` plane — slim
        only; k3s answers 501: use kubeconfig() with a real client)."""
        return self._request(method, f"/k8s{path}", body)

    def is_running(self) -> bool:
        """True when the engine API is reachable."""
        try:
            self._request("GET", "/healthz")
            return True
        except Exception:
            return False

    # -- plumbing ------------------------------------------------------------

    def _request(self, method: str, path: str, body: Optional[dict] = None,
                 timeout: Optional[float] = None) -> Any:
        return json.loads(self._request_text(method, path, body, timeout))

    def _request_text(self, method: str, path: str, body: Optional[dict] = None,
                      timeout: Optional[float] = None) -> str:
        url = f"{self.base_url}{path}"
        data = json.dumps(body).encode() if body is not None else None
        req = urllib.request.Request(url, data=data, method=method)
        if data is not None:
            req.add_header("Content-Type", "application/json")
        if self.token:
            req.add_header("Authorization", f"Bearer {self.token}")
        try:
            with urllib.request.urlopen(req, timeout=max(timeout or 0, self.timeout)) as resp:
                return resp.read().decode()
        except urllib.error.HTTPError as e:
            try:
                detail = json.loads(e.read()).get("error", str(e))
            except Exception:
                detail = str(e)
            raise NebulaError(detail, status=e.code) from None
        except urllib.error.URLError as e:
            raise NebulaError(f"engine unreachable at {self.base_url}: {e.reason}") from None


class Vessels:
    """``nebula.vessels.*`` — named microVMs with snapshots & live branching."""

    def __init__(self, client: NebulaClient):
        self._c = client

    def list(self) -> list[dict[str, Any]]:
        return self._c._request("GET", "/v1alpha1/vessels")

    def get(self, name: str) -> dict[str, Any]:
        return self._c._request("GET", f"/v1alpha1/vessels/{_enc(name)}")

    def create(self, name: str, *, cpus: int = 2, mem_mib: int = 2048, gpu: bool = False,
               data_gib: int = 16, backend: str = "krun",
               volumes: Optional[list[str]] = None,
               from_image: Optional[str] = None, rootfs_img: Optional[str] = None,
               rootfs_mb: int = 4096, no_start: bool = False) -> dict[str, Any]:
        """Create (and boot, unless no_start). ``from_image`` builds the
        rootfs from a docker image ref inside the engine (can take minutes);
        ``volumes`` are "name:GiB" strings mounted at /mnt/<name>."""
        body: dict[str, Any] = {
            "name": name, "cpus": cpus, "mem_mib": mem_mib, "gpu": gpu,
            "data_gib": data_gib, "backend": backend,
            "volumes": volumes or [], "rootfs_mb": rootfs_mb, "no_start": no_start,
        }
        if from_image is not None:
            body["from_image"] = from_image
        if rootfs_img is not None:
            body["rootfs_img"] = rootfs_img
        slow = 600 if from_image else 120
        return self._c._request("POST", "/v1alpha1/vessels", body, timeout=slow)

    def start(self, name: str) -> dict[str, Any]:
        return self._c._request("POST", f"/v1alpha1/vessels/{_enc(name)}/start", timeout=60)

    def stop(self, name: str) -> Any:
        return self._c._request("POST", f"/v1alpha1/vessels/{_enc(name)}/stop", timeout=60)

    def rm(self, name: str, force: bool = False) -> dict[str, Any]:
        """Remove a vessel; ``force`` stops it first when running."""
        q = "?force=true" if force else ""
        return self._c._request("DELETE", f"/v1alpha1/vessels/{_enc(name)}{q}", timeout=60)

    def exec(self, name: str, cmd: str, args: Optional[list[str]] = None,
             timeout_ms: int = 30_000) -> dict[str, Any]:
        return self._c._request(
            "POST",
            f"/v1alpha1/vessels/{_enc(name)}/exec",
            {"cmd": cmd, "args": args or [], "timeout_ms": timeout_ms},
            timeout=timeout_ms / 1000 + 5,
        )

    def console(self, name: str, tail: int = 16_384) -> str:
        """Last ``tail`` bytes of the vessel's boot/console log."""
        r = self._c._request("GET", f"/v1alpha1/vessels/{_enc(name)}/console?tail={tail}")
        return r["console"]

    def snapshot(self, name: str, label: str, mode: str = "auto") -> dict[str, Any]:
        """Take a snapshot. mode: "auto" (memory+disks when possible),
        "memory" (or fail), "disk"."""
        return self._c._request(
            "POST", f"/v1alpha1/vessels/{_enc(name)}/snapshots",
            {"label": label, "mode": mode}, timeout=300,
        )

    def snapshots(self, name: str) -> list[dict[str, Any]]:
        return self._c._request("GET", f"/v1alpha1/vessels/{_enc(name)}/snapshots")

    def snapshot_rm(self, name: str, label: str) -> dict[str, Any]:
        return self._c._request(
            "DELETE", f"/v1alpha1/vessels/{_enc(name)}/snapshots/{_enc(label)}"
        )

    def restore(self, name: str, label: str) -> dict[str, Any]:
        """Roll back to a snapshot — memory snapshots live-resume."""
        return self._c._request(
            "POST", f"/v1alpha1/vessels/{_enc(name)}/restore", {"label": label}, timeout=300,
        )

    def branch(self, name: str, new_name: str, *, label: Optional[str] = None,
               count: int = 1) -> dict[str, Any]:
        """Fan out clones (the tree-search primitive). With a memory-snapshot
        label every branch wakes MID-EXECUTION: RAM, processes and sockets
        intact, copy-on-write shared with the source on macOS/Linux."""
        body: dict[str, Any] = {"new_name": new_name, "count": count}
        if label is not None:
            body["label"] = label
        return self._c._request(
            "POST", f"/v1alpha1/vessels/{_enc(name)}/branch", body,
            timeout=120 + count * 30,
        )


def _enc(s: str) -> str:
    return urllib.parse.quote(s, safe="")
