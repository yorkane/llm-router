#!/usr/bin/env python3
"""llm-watcher: keep local LLM inference services automatically inside an llm-router pool.

Discovers OpenAI-compatible workers (vLLM / sglang / llama.cpp / anything serving
/v1/models) on this host and reconciles them into a running llm-router (smg) through
its dynamic worker API:

    GET    /workers        -> current pool
    POST   /workers        -> queue an AddWorker job   (202 Accepted)
    DELETE /workers/{id}   -> queue a RemoveWorker job (202 Accepted)

The router already health-checks its workers, so a temporarily dead service stays in
the pool and is simply marked unhealthy. This daemon covers what the router cannot:
a service that appears (a new model started on some port) or goes away for good
(container removed / permanently dead), which previously meant editing --worker-urls
and restarting the router.

Safety properties, in order of importance:
  * Never adds the router itself (self-loop guard: /server_info fingerprint + own port).
  * Only accepts a candidate when GET /v1/models returns real OpenAI JSON (data[].id);
    many services answer an unknown path with 200 + HTML and must not become workers.
  * Only deletes workers it created itself (tracked in a local ledger). Workers coming
    from --worker-urls or added by a human are snapshotted as protected on first run
    and never touched.
  * Never removes the last worker of a model, and only removes after a grace period.

Standard library only: needs a bare python3 (>= 3.8), no pip, no CUDA, no Python image.
"""

from __future__ import annotations

import argparse
import http.client
import json
import logging
import os
import re
import signal
import socket
import sys
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass, field
from typing import Dict, List, Optional, Sequence, Set, Tuple

LOG = logging.getLogger("llm-watcher")

MANAGED_LABEL = "llm-watcher"
ROUTER_FINGERPRINT_KEYS = ("router_manager", "workers_count", "routers_count")
ENV_PREFIX = "LLM_WATCHER_"


def _env(*names, default=None):
    """First non-empty environment value, so a container can be configured by env alone.

    CLI flags still win: argparse keeps these as defaults, and an explicit flag simply
    overrides the default. Plain names (ROUTER_URL) are accepted next to prefixed ones
    because those are what people already write in a compose file.
    """
    for name in names:
        for candidate in (ENV_PREFIX + name, name):
            value = os.environ.get(candidate)
            if value is not None and value.strip():
                return value.strip()
    return default


def _env_float(name, default):
    raw = _env(name)
    if raw is None:
        return default
    try:
        return float(raw)
    except ValueError:
        LOG.warning("ignoring %s=%r (not a number)", ENV_PREFIX + name, raw)
        return default


def _env_int(name, default):
    raw = _env_float(name, float(default))
    return int(raw)


def _env_bool(name, default):
    raw = _env(name)
    if raw is None:
        return default
    return raw.lower() in ("1", "true", "yes", "on")


def parse_model_map(spec):
    """Parse `orig1:new1,orig2:new2` into {orig: new}. Empty values drop nothing."""
    out = {}
    for part in filter(None, (s.strip() for s in re.split(r"[,;\n]", spec or ""))):
        orig, sep, new = part.partition(":")
        if not sep or not orig.strip() or not new.strip():
            LOG.warning("ignoring bad model-map entry %r (want original:new)", part)
            continue
        out[orig.strip()] = new.strip()
    return out


def parse_targets(spec):
    """Split a worker-URL list from env/comma/space separated text."""
    if not spec:
        return []
    return [part for part in re.split(r"[,;\s]+", spec.strip()) if part]

# Infrastructure ports that are never worth probing (ssh/dns/redis/node_exporter/...).
# Deliberately short: the probe itself is the strict gate, this only trims noise, and
# inference servers do sometimes sit on busy-web-default ports like 8080 or 3000.
DEFAULT_DENY_PORTS = {
    22, 25, 53, 111, 135, 139, 445, 631, 1433, 1521, 2049, 3306, 3389,
    5432, 5900, 6379, 6443, 9100, 9400, 11211, 27017,
}


class UnixHTTPConnection(http.client.HTTPConnection):
    """http.client connection that talks to the docker daemon over its unix socket."""

    def __init__(self, socket_path: str, timeout: float = 5.0):
        super().__init__("localhost", timeout=timeout)
        self._socket_path = socket_path

    def connect(self):
        sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        sock.settimeout(self.timeout or 5.0)
        try:
            sock.connect(self._socket_path)
        except Exception:
            sock.close()   # do not leak the fd when the daemon is unreachable
            raise
        self.sock = sock


def http_json(url, timeout=3.0, method="GET", body=None, headers=None):
    """Fetch url -> (status, json_or_None, raw_body). Never raises.

    The raw body is returned so callers can tell "answered with HTML" apart from
    "connection refused": the first must be rejected, the second merely retried.
    """
    data = None
    hdrs = {"Accept": "application/json"}
    if body is not None:
        data = json.dumps(body).encode()
        hdrs["Content-Type"] = "application/json"
    if headers:
        hdrs.update(headers)
    req = urllib.request.Request(url, data=data, headers=hdrs, method=method)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            return _parse(resp.status, resp.read(1 << 20))
    except urllib.error.HTTPError as exc:
        try:
            raw = exc.read(1 << 20)
        except Exception:
            raw = b""
        return _parse(exc.code, raw)
    except Exception:
        return 0, None, None


def _parse(status, raw):
    try:
        return status, json.loads(raw.decode("utf-8", "replace")), raw
    except Exception:
        return status, None, raw


def unix_http_json(socket_path, path, timeout=5.0):
    conn = UnixHTTPConnection(socket_path, timeout=timeout)
    try:
        conn.request("GET", path)
        resp = conn.getresponse()
        raw = resp.read(16 << 20)
        return resp.status, json.loads(raw.decode("utf-8", "replace"))
    except Exception:
        return 0, None
    finally:
        try:
            conn.close()
        except Exception:
            pass


def normalize_url(url):
    """Canonical worker key: scheme://host[:port], no trailing slash, lowercase host."""
    if "://" not in url:
        url = "http://" + url
    parsed = urllib.parse.urlsplit(url)
    scheme = (parsed.scheme or "http").lower()
    host = (parsed.hostname or "").lower()
    port = parsed.port
    if port and not (scheme == "http" and port == 80) and not (scheme == "https" and port == 443):
        netloc = "%s:%d" % ("[" + host + "]" if ":" in host and not host.startswith("[") else host, port)
    else:
        netloc = "[" + host + "]" if ":" in host and not host.startswith("[") else host
    return "%s://%s" % (scheme, netloc)


@dataclass
class Candidate:
    url: str
    source: str
    label: str = ""
    instance_key: Optional[str] = None


@dataclass
class WorkerInfo:
    url: str
    models: List[str]
    engine: str
    has_health: bool
    label: str = ""


def listening_sockets():
    """Listening TCP sockets as (bind_address, port) read from /proc/net/tcp*.

    This is what catches host-network containers (their ports are published by the
    kernel, not by docker-proxy) and native processes such as a bare llama.cpp server.
    """
    out: List[Tuple[str, int]] = []
    seen: Set[Tuple[str, int]] = set()
    for proc, is_v6 in (("/proc/net/tcp", False), ("/proc/net/tcp6", True)):
        try:
            with open(proc, "r", encoding="ascii") as fh:
                lines = fh.readlines()[1:]
        except OSError:
            continue
        for line in lines:
            cols = line.split()
            if len(cols) < 4 or cols[3] != "0A":  # 0A == TCP_LISTEN
                continue
            hex_ip, _, hex_port = cols[1].partition(":")
            try:
                port = int(hex_port, 16)
            except ValueError:
                continue
            if not is_v6:
                octets = [int(hex_ip[i:i + 2], 16) for i in range(0, 8, 2)]
                host = ".".join(str(o) for o in reversed(octets))
            elif hex_ip == "0" * 32:
                host = "::"
            elif hex_ip == "0" * 31 + "1":
                host = "::1"
            elif hex_ip[:24] == "0" * 22 + "ffff":  # v4-mapped address
                octets = [int(hex_ip[24 + i:26 + i], 16) for i in range(0, 8, 2)]
                host = ".".join(str(o) for o in reversed(octets))
            else:
                words = [hex_ip[i:i + 4] for i in range(0, 32, 4)]
                words = [w[2:] + w[0:2] for w in words]
                host = ":".join(w.lstrip("0") or "0" for w in words)
            if (host, port) in seen:
                continue
            seen.add((host, port))
            out.append((host, port))
    return out


def docker_candidates(socket_path, include_container_ips=True):
    """Candidates from running containers, plus {published_port: container_name}.

    Published ports win over container-internal IPs: one instance must map to exactly
    one worker URL, two URLs for the same instance would split the prefix cache.
    """
    cands: List[Candidate] = []
    port_names: Dict[int, str] = {}
    status, data = unix_http_json(
        socket_path,
        "/containers/json?filters=" + urllib.parse.quote(json.dumps({"status": ["running"]})),
    )
    if status != 200 or not isinstance(data, list):
        return cands, port_names
    for ctr in data:
        name = (ctr.get("Names") or [ctr.get("Id", "container")])[0].lstrip("/")
        host_net = (ctr.get("HostConfig") or {}).get("NetworkMode") == "host"
        published: Set[int] = set()
        mapped_private: Set[int] = set()
        for binding in ctr.get("Ports") or []:
            public = binding.get("PublicPort")
            if not public:
                continue
            published.add(int(public))
            if binding.get("PrivatePort"):
                mapped_private.add(int(binding["PrivatePort"]))
            port_names[int(public)] = name
            cands.append(Candidate(
                url=normalize_url("http://127.0.0.1:%d" % int(public)),
                source="docker",
                label=name,
                instance_key=ctr.get("Id"),
            ))
        if host_net or not include_container_ips:
            # Host-network containers share the host stack: /proc/net/tcp already covers
            # their listening ports and their "container IP" is a host address anyway.
            continue
        for net in (ctr.get("NetworkSettings") or {}).get("Networks", {}).values():
            ip = net.get("IPAddress")
            if not ip:
                continue
            for port in net.get("Ports") or {}:
                try:
                    cport = int(str(port).split("/")[0])
                except ValueError:
                    continue
                # Also skip a container port the host already publishes: reaching one
                # instance through both its published port and its container IP would
                # register it twice and split the prefix cache between the two URLs.
                if cport in published or cport in mapped_private:
                    continue
                cands.append(Candidate(
                    url=normalize_url("http://%s:%d" % (ip, cport)),
                    source="docker-net",
                    label=name,
                    instance_key=ctr.get("Id"),
                ))
    return cands, port_names


def local_candidates(deny):
    cands: List[Candidate] = []
    for host, port in listening_sockets():
        if port in deny:
            continue
        if host in ("0.0.0.0", "::"):
            targets = ["127.0.0.1"]
        elif ":" in host:
            targets = ["[" + host + "]"]
        else:
            targets = [host]
        for tgt in targets:
            cands.append(Candidate(url=normalize_url("http://%s:%d" % (tgt, port)), source="proc"))
    return cands


def probe_worker(url, timeout=3.0, require_health=False, max_models=0):
    """Return WorkerInfo when url really is an OpenAI-compatible inference server."""
    status, payload, raw = http_json(url + "/v1/models", timeout=timeout)
    if status < 200 or status >= 400 or raw is None:
        return None
    ids: List[str] = []
    if isinstance(payload, dict) and isinstance(payload.get("data"), list):
        ids = [
            str(item["id"])
            for item in payload["data"]
            if isinstance(item, dict) and isinstance(item.get("id"), (str, int))
        ]
    if not ids:
        return None  # HTML/JSON service that is not an OpenAI model endpoint

    # A server advertising dozens of models is an aggregator or another proxy, not a
    # worker: routing through it adds a hop and can loop back into this very router.
    if max_models and len(ids) > max_models:
        LOG.debug("Skipping %s: advertises %d models (> --max-models %d), looks like a proxy",
                  url, len(ids), max_models)
        return None

    # Self-loop guard: another llm-router also serves /v1/models.
    st_info, sinfo, _ = http_json(url + "/server_info", timeout=timeout)
    if 200 <= st_info < 400 and isinstance(sinfo, dict) and any(k in sinfo for k in ROUTER_FINGERPRINT_KEYS):
        LOG.debug("Skipping %s: it is a router, not a worker", url)
        return None

    engine = "openai"
    st_gsi, gsi, _r = http_json(url + "/get_server_info", timeout=timeout)
    st_props, props, _r = http_json(url + "/props", timeout=timeout)
    st_met, _j, met_raw = http_json(url + "/metrics", timeout=timeout)
    met_text = (met_raw or b"").decode("utf-8", "replace")
    if 200 <= st_gsi < 400 and isinstance(gsi, dict) and ("disaggregation_mode" in gsi or "tp_size" in gsi or "model_path" in gsi):
        engine = "sglang"
    elif 200 <= st_info < 400 and isinstance(sinfo, dict) and "model_path" in sinfo:
        engine = "sglang"
    elif "vllm:" in met_text:
        engine = "vllm"
    elif "llamacpp:" in met_text:
        engine = "llama.cpp"
    elif 200 <= st_props < 400 and isinstance(props, dict) and ("build_commit" in props or "build_number" in props or "webui" in props):
        engine = "llama.cpp"

    st_health, _hp, _hr = http_json(url + "/health", timeout=timeout)
    has_health = 200 <= st_health < 400
    if require_health and not has_health:
        LOG.debug("Skipping %s: no usable /health endpoint", url)
        return None
    return WorkerInfo(url=url, models=ids, engine=engine, has_health=has_health)


class RouterClient:
    def __init__(self, base_url, api_key=None, timeout=5.0):
        self.base = normalize_url(base_url)
        self.timeout = timeout
        self.headers: Dict[str, str] = {}
        if api_key:
            self.headers["Authorization"] = "Bearer " + api_key

    @property
    def port(self):
        try:
            return urllib.parse.urlsplit(self.base).port
        except ValueError:
            return None

    def health(self):
        status, _p, raw = http_json(self.base + "/health", timeout=self.timeout)
        return 200 <= status < 400 and raw is not None

    def server_info(self):
        # Self-report; routers_count tells IGW (per-model) from single-router mode.
        status, payload, _raw = http_json(self.base + "/server_info", timeout=self.timeout)
        return payload if 200 <= status < 400 and isinstance(payload, dict) else None

    def list_workers(self):
        status, payload, _raw = http_json(self.base + "/workers", timeout=self.timeout, headers=self.headers)
        if status != 200 or not isinstance(payload, dict):
            raise RuntimeError("GET /workers failed (HTTP %s)" % status)
        out: Dict[str, dict] = {}
        for item in payload.get("workers") or []:
            if isinstance(item, dict) and item.get("url"):
                out[normalize_url(str(item["url"]))] = item
        return out

    def add(self, url, model_id, extra):
        body = {"url": url, "model_id": model_id, "worker_type": "regular"}
        body.update(extra)
        status, payload, raw = http_json(
            self.base + "/workers", timeout=self.timeout, method="POST", body=body, headers=self.headers
        )
        if status not in (200, 201, 202):
            return False, self._detail(payload, raw, status), None
        worker_id = payload.get("worker_id") if isinstance(payload, dict) else None
        return True, "", (str(worker_id) if worker_id else None)

    def delete(self, worker_id):
        status, payload, raw = http_json(
            self.base + "/workers/" + urllib.parse.quote(worker_id),
            timeout=self.timeout, method="DELETE", headers=self.headers,
        )
        if status not in (200, 202, 204):
            return False, self._detail(payload, raw, status)
        return True, ""

    def job_status(self, worker_id):
        status, payload, _raw = http_json(
            self.base + "/workers/" + urllib.parse.quote(worker_id), timeout=self.timeout, headers=self.headers
        )
        if status != 200 or not isinstance(payload, dict):
            return "unknown"
        job = payload.get("job_status")
        if isinstance(job, dict):
            return str(job.get("status") or "unknown")
        return "registered" if payload.get("url") else "unknown"

    @staticmethod
    def _detail(payload, raw, status):
        if isinstance(payload, dict):
            return str(payload.get("error") or payload)
        if raw:
            return raw.decode("utf-8", "replace")[:200]
        return "HTTP %s" % status


class Ledger:
    """Persisted memory of what this daemon owns and what it must never touch."""

    def __init__(self, path, read_only=False):
        self.path = path
        self.read_only = read_only
        self.protected: Set[str] = set()
        self.owned: Dict[str, dict] = {}
        self.missing_since: Dict[str, float] = {}
        self.warned: Set[str] = set()
        self.model_map: Dict[str, str] = {}
        self._load()

    def _load(self):
        try:
            with open(self.path, "r", encoding="utf-8") as fh:
                data = json.load(fh)
        except FileNotFoundError:
            return
        except Exception as exc:
            LOG.warning("ledger %s unreadable (%s); starting fresh", self.path, exc)
            return
        self.protected = {normalize_url(str(u)) for u in data.get("protected", [])}
        self.owned = {normalize_url(str(k)): v for k, v in (data.get("owned") or {}).items()}
        self.missing_since = {normalize_url(str(k)): float(v) for k, v in (data.get("missing_since") or {}).items()}
        self.model_map = {str(k): str(v) for k, v in (data.get("model_map") or {}).items()}
        LOG.info("ledger loaded: %d owned, %d protected", len(self.owned), len(self.protected))

    def save(self):
        if self.read_only:      # --dry-run must leave no trace, not even a ledger
            return
        tmp = self.path + ".tmp"
        payload = {
            "version": 1,
            "protected": sorted(self.protected),
            "owned": self.owned,
            "missing_since": self.missing_since,
            "model_map": getattr(self, "model_map", {}),
            "updated_at": int(time.time()),
        }
        os.makedirs(os.path.dirname(os.path.abspath(self.path)) or ".", exist_ok=True)
        with open(tmp, "w", encoding="utf-8") as fh:
            json.dump(payload, fh, indent=2, sort_keys=True)
            fh.write("\n")
        os.replace(tmp, self.path)


@dataclass
class Config:
    router: str = "http://127.0.0.1:8800"
    router_api_key: Optional[str] = None
    worker_api_key: Optional[str] = None
    state_dir: str = "/data/tmp/llm-watcher"
    docker_socket: str = "/var/run/docker.sock"
    interval: float = 15.0
    probe_timeout: float = 3.0
    workers: int = 16
    deny_ports: Set[int] = field(default_factory=set)
    allow_ports: Set[int] = field(default_factory=set)
    extra_targets: List[str] = field(default_factory=list)
    exclude: List[str] = field(default_factory=list)
    scan_proc: bool = True
    scan_docker: bool = True
    scan_container_ips: bool = True
    require_health: bool = False
    max_models: int = 8
    allow_remove: bool = True
    remove_grace: float = 300.0
    keep_last_per_model: bool = True
    fix_model_drift: bool = False
    short_model_names: bool = False
    model_map: Dict[str, str] = field(default_factory=dict)
    add_confirm_timeout: float = 180.0
    dry_run: bool = False
    health_check_interval_secs: int = 15
    health_check_timeout_secs: int = 5
    health_failure_threshold: int = 3
    health_success_threshold: int = 2
    metrics_port: Optional[int] = None
    once: bool = False


class Reconciler:
    def __init__(self, cfg: Config):
        self.cfg = cfg
        self.router = RouterClient(cfg.router, cfg.router_api_key)
        if not cfg.dry_run:
            os.makedirs(cfg.state_dir, exist_ok=True)
        self.ledger = Ledger(os.path.join(cfg.state_dir, "ledger.json"), read_only=cfg.dry_run)
        self._exclude = [re.compile(p) for p in cfg.exclude]
        self._pending: Dict[str, dict] = {}
        self.stats = {"reconciles": 0, "adds": 0, "removes": 0, "discovered": 0, "last_error": ""}

    def model_map(self) -> Dict[str, str]:
        """Current original-id -> public-id renames."""
        return dict(self.cfg.model_map)

    def set_model_map(self, mapping: Dict[str, object]) -> Dict[str, str]:
        """Merge renames at runtime (POST /model-map). An empty new id deletes an entry."""
        merged = self.cfg.model_map
        for orig, new in (mapping or {}).items():
            orig = str(orig).strip()
            if not orig:
                continue
            if new is None or not str(new).strip():
                merged.pop(orig, None)
            else:
                merged[orig] = str(new).strip()
        self.ledger.model_map = dict(merged)
        self.ledger.save()
        return dict(merged)

    def collect(self):
        cfg = self.cfg
        cands: List[Candidate] = []
        for target in cfg.extra_targets:
            cands.append(Candidate(url=normalize_url(target), source="cli", label="static"))
        port_names: Dict[int, str] = {}
        if cfg.scan_docker and cfg.docker_socket and os.path.exists(cfg.docker_socket):
            try:
                docker_cands, port_names = docker_candidates(cfg.docker_socket, cfg.scan_container_ips)
                cands.extend(docker_cands)
            except Exception as exc:
                LOG.debug("docker scan failed: %s", exc)
        if cfg.scan_proc:
            deny = cfg.deny_ports | (set() if cfg.allow_ports else DEFAULT_DENY_PORTS)
            for cand in local_candidates(deny):
                port = urllib.parse.urlsplit(cand.url).port
                if port in port_names:
                    cand.label = port_names[port]
                cands.append(cand)
        for port in sorted(cfg.allow_ports):
            cands.append(Candidate(url=normalize_url("http://127.0.0.1:%d" % port), source="allow-list"))

        order = {"docker": 0, "cli": 1, "docker-net": 2, "proc": 3, "allow-list": 4}
        cands.sort(key=lambda c: (order.get(c.source, 9), c.url))
        unique: Dict[str, Candidate] = {}
        for cand in cands:
            unique.setdefault(cand.url, cand)
        return list(unique.values())

    def probe_all(self, cands):
        cfg = self.cfg
        router_port = self.router.port
        infos: List[WorkerInfo] = []
        by_instance: Dict[str, WorkerInfo] = {}

        def work(item):
            parts = urllib.parse.urlsplit(item.url)
            if router_port is not None and parts.port == router_port and parts.hostname in ("127.0.0.1", "localhost", "::1"):
                return None  # never point the router at itself
            if any(pat.search(item.url) for pat in self._exclude):
                return None
            info = probe_worker(item.url, timeout=cfg.probe_timeout, require_health=cfg.require_health,
                                max_models=cfg.max_models)
            if info is None:
                return None
            info.label = item.label or info.engine
            return item, info

        with ThreadPoolExecutor(max_workers=max(1, cfg.workers)) as pool:
            for result in pool.map(work, cands):
                if result is None:
                    continue
                item, info = result
                if item.instance_key:
                    first = by_instance.get(item.instance_key)
                    if first is not None:
                        LOG.debug("Skipping %s: %s already covers this instance", info.url, first.url)
                        continue
                    by_instance[item.instance_key] = info
                infos.append(info)
        return infos

    def reconcile(self):
        cfg = self.cfg
        self.stats["reconciles"] += 1
        try:
            actual = self.router.list_workers()
        except Exception as exc:
            self.stats["last_error"] = str(exc)
            LOG.warning("router unreachable: %s", exc)
            return
        self.stats["last_error"] = ""

        # First contact: anything already in the pool was configured by someone else
        # (--worker-urls or a human), so protect it permanently.
        if not self.ledger.protected and not self.ledger.owned:
            self.ledger.protected = set(actual)
            if self.ledger.protected:
                LOG.info("protecting %d pre-existing worker(s): %s",
                         len(self.ledger.protected), ", ".join(sorted(self.ledger.protected)))
            self.ledger.save()

        discovered = self.probe_all(self.collect())
        self.stats["discovered"] = len(discovered)
        desired: Dict[str, WorkerInfo] = {}
        for info in discovered:
            if info.url not in self.ledger.protected:
                desired[info.url] = info

        pending = self._reap_pending(actual)

        for url, info in sorted(desired.items()):
            if url not in actual and url not in pending:
                self._add(info)

        # model_map (or --short-model-names) changed: recycle owned workers so the
        # next pass re-adds them under the new public id. Protected workers stay.
        for url, info in sorted(desired.items()):
            entry = self.ledger.owned.get(url)
            if not entry or url in self._pending or url not in actual:
                continue
            want = self._model_name(info.models[0])
            have = str(actual[url].get("model_id") or "")
            if entry.get("model_id") != want and have != want:
                LOG.info("Rename %s: registered %r, want %r; re-registering", url, have, want)
                self._remove(url, entry, 0.0)

        for url, entry in sorted(list(self.ledger.owned.items())):
            if url in desired:
                self.ledger.missing_since.pop(url, None)
                self._check_drift(url, entry, actual)
                continue
            if url not in actual:
                self.ledger.owned.pop(url, None)
                self.ledger.missing_since.pop(url, None)
                LOG.info("Dropped %s from ledger (no longer in router pool)", url)
                continue
            first_missing = self.ledger.missing_since.setdefault(url, time.time())
            age = time.time() - first_missing
            if not cfg.allow_remove:
                if age > cfg.remove_grace and url not in self.ledger.warned:
                    self.ledger.warned.add(url)
                    LOG.warning("%s disappeared %.0fs ago but removal is disabled (--allow-remove false)", url, age)
                continue
            if age < cfg.remove_grace:
                continue
            if cfg.keep_last_per_model and self._is_last_for_model(str(entry.get("model_id") or ""), url, actual):
                if url not in self.ledger.warned:
                    self.ledger.warned.add(url)
                    LOG.warning("%s gone %.0fs but it is the last worker of model '%s'; keeping it",
                                url, age, entry.get("model_id"))
                continue
            self._remove(url, entry, age)

        self.ledger.save()
        self._check_model_isolation(actual)

    # Measured against smg: in single-router mode the worker pick ignores the requested
    # model (router.rs sets effective_model_id = None unless IGW). Once the pool holds two
    # models, a request naming model A gets served by a worker running model B and still
    # answers 200: 10/10 requests for the local model went to a remote instance in a test
    # pool. Only the router can fix that (--enable-igw), so warn loudly, once per change.
    def _check_model_isolation(self, actual):
        models = set(str(item.get("model_id") or "") for item in actual.values())
        models.discard("")
        models.discard("unknown")
        if len(models) < 2:
            self._heterogeneous_warned = None
            return
        signature = ",".join(sorted(models))
        if getattr(self, "_heterogeneous_warned", None) == signature:
            return
        info = self.router.server_info() or {}
        routers_count = info.get("routers_count")
        if isinstance(routers_count, int) and routers_count > 1:
            return                      # IGW mode: per-model routing is in effect
        self._heterogeneous_warned = signature
        LOG.warning("pool serves %d models (%s) but the router runs in single-router mode; "
                    "smg then ignores the requested model when picking a worker, so a "
                    "request can be served by the wrong model and still return 200. "
                    "Run the router with --enable-igw for per-model routing.",
                    len(models), signature)

    def _reap_pending(self, actual):
        """Confirm the AddWorker jobs we queued, and clean up the stuck ones.

        A 202 from POST /workers only means "job queued". The job then polls the worker
        until it answers, so a URL that was reachable when discovered but died moments
        later leaves a job parked in `processing` forever -- and while it is parked, the
        URL counts as taken: every later re-add is rejected with "already exists", so the
        service could never join once it recovers. Deleting the worker id releases the URL
        (the AddWorker step reports "already exists" from the live registry, so a worker
        that actually did register is never deleted here), and the next pass re-adds it.
        """
        live: Set[str] = set()
        for url, meta in list(self._pending.items()):
            if url in actual:
                LOG.info("Confirmed worker %s in router pool", url)
                self._pending.pop(url, None)
                continue
            age = time.time() - meta["queued_at"]
            if age <= self.cfg.add_confirm_timeout:
                live.add(url)
                continue
            status = self.router.job_status(meta["worker_id"]) if meta.get("worker_id") else "unknown"
            LOG.warning("AddWorker for %s not registered after %.0fs (job status: %s); releasing the URL",
                        url, age, status)
            if status in ("processing", "pending") and meta.get("worker_id"):
                ok, detail = self.router.delete(meta["worker_id"])
                if not ok:
                    LOG.error("could not release stuck AddWorker for %s: %s", url, detail)
                    live.add(url)  # keep waiting, try again next pass
                    continue
            self._pending.pop(url, None)
            self.ledger.owned.pop(url, None)
            live.add(url)
        return live

    def _model_name(self, raw: str) -> str:
        """Public model id. llama.cpp reports the served file path; a short
        basename with the weights suffix dropped is friendlier as an API id. A
        model-map entry wins over that, and may key on either the raw id or the
        short name, so both spellings work in a compose file."""
        name = raw
        if self.cfg.short_model_names:
            name = raw.rstrip("/").rsplit("/", 1)[-1].strip()
            low = name.lower()
            for suffix in (".gguf", ".safetensors", ".bin", ".pt", ".ckpt"):
                if low.endswith(suffix):
                    name = name[:-len(suffix)]
                    break
            name = name or raw
        return self.cfg.model_map.get(raw) or self.cfg.model_map.get(name) or name

    def _add(self, info):
        cfg = self.cfg
        model_id = self._model_name(info.models[0])
        if len(info.models) > 1:
            LOG.info("%s serves %d models; registering as '%s' (the router keys one model per URL)",
                     info.url, len(info.models), model_id)
        extra = {
            "labels": {"managed-by": MANAGED_LABEL, "engine": info.engine},
            "health_check_interval_secs": cfg.health_check_interval_secs,
            "health_check_timeout_secs": cfg.health_check_timeout_secs,
            "health_failure_threshold": cfg.health_failure_threshold,
            "health_success_threshold": cfg.health_success_threshold,
        }
        if cfg.worker_api_key:
            extra["api_key"] = cfg.worker_api_key
        if not info.has_health:
            extra["disable_health_check"] = True
        if cfg.dry_run:
            LOG.info("[dry-run] would ADD %s model=%s engine=%s", info.url, model_id, info.engine)
            return
        ok, detail, worker_id = self.router.add(info.url, model_id, extra)
        if not ok:
            self.stats["last_error"] = detail
            if "already exists" in detail:
                LOG.debug("%s already exists in router (%s)", info.url, detail)
                return
            LOG.error("ADD %s failed: %s", info.url, detail)
            return
        self.stats["adds"] += 1
        self._pending[info.url] = {"queued_at": time.time(), "worker_id": worker_id}
        self.ledger.owned[info.url] = {
            "model_id": model_id,
            "engine": info.engine,
            "worker_id": worker_id,
            "added_at": int(time.time()),
        }
        self.ledger.missing_since.pop(info.url, None)
        self.ledger.warned.discard(info.url)
        LOG.info("QUEUED add %s model=%s engine=%s%s (id=%s)", info.url, model_id, info.engine,
                 " health=off" if extra.get("disable_health_check") else "", worker_id)

    def _remove(self, url, entry, age):
        worker_id = str(entry.get("worker_id") or "")
        if not worker_id:
            LOG.warning("%s has no recorded worker_id; cannot delete", url)
            return
        if self.cfg.dry_run:
            LOG.info("[dry-run] would REMOVE %s (gone %.0fs)", url, age)
            return
        ok, detail = self.router.delete(worker_id)
        if not ok:
            self.stats["last_error"] = detail
            LOG.error("REMOVE %s failed: %s", url, detail)
            return
        self.stats["removes"] += 1
        self.ledger.owned.pop(url, None)
        self.ledger.missing_since.pop(url, None)
        self.ledger.warned.discard(url)
        self._pending.pop(url, None)
        LOG.info("QUEUED remove %s (unreachable %.0fs, id=%s)", url, age, worker_id)

    def _check_drift(self, url, entry, actual):
        item = actual.get(url)
        if not item:
            return
        # A router restart re-creates its workers from --worker-urls, so the ids we
        # recorded no longer exist and a later DELETE would 404. Keep the id fresh.
        live_id = str(item.get("id") or "")
        if live_id and entry.get("worker_id") != live_id:
            entry["worker_id"] = live_id

        live, want = str(item.get("model_id") or ""), str(entry.get("model_id") or "")
        if not live or not want or live == want:
            return
        if url in self.ledger.warned:
            return
        self.ledger.warned.add(url)
        LOG.warning("model drift on %s: router says '%s', service now serves '%s' (%s)", url, live, want,
                    "auto-recycling" if self.cfg.fix_model_drift else "use --fix-model-drift to recycle it")
        if self.cfg.fix_model_drift:
            self.ledger.owned.pop(url, None)
            self._remove(url, entry, 0.0)

    def _is_last_for_model(self, model_id, url, actual):
        if not model_id:
            return True
        for other_url, item in actual.items():
            if other_url == url or str(item.get("model_id") or "") != model_id:
                continue
            if item.get("is_healthy", True):
                return False
        return True

    def run(self, stop):
        LOG.info("router=%s interval=%.0fs state_dir=%s", self.router.base, self.cfg.interval, self.cfg.state_dir)
        while not stop.is_set():
            try:
                self.reconcile()
            except Exception as exc:
                self.stats["last_error"] = repr(exc)
                LOG.exception("reconcile failed")
            if self.cfg.once:
                return
            stop.wait(self.cfg.interval)


def start_metrics(reconciler, port):
    from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

    class Handler(BaseHTTPRequestHandler):
        # The metrics port doubles as the control plane: GET/POST /model-map
        # changes the original-id -> public-id renames without a restart.
        def _reply(self, code, body, ctype="text/plain; version=0.0.4"):
            raw = body.encode()
            self.send_response(code)
            self.send_header("content-type", ctype)
            self.send_header("content-length", str(len(raw)))
            self.end_headers()
            self.wfile.write(raw)

        def _json(self, code, obj):
            self._reply(code, json.dumps(obj, indent=2, sort_keys=True) + "\n",
                        "application/json")

        def do_GET(self):
            path = self.path.split("?")[0].rstrip("/")
            if path == "/model-map":
                self._json(200, reconciler.model_map())
                return
            if path not in ("/metrics", ""):
                self._reply(404, "not found\n")
                return
            rec = reconciler
            lines = [
                "llm_watcher_reconciles_total %d" % rec.stats["reconciles"],
                "llm_watcher_adds_total %d" % rec.stats["adds"],
                "llm_watcher_removes_total %d" % rec.stats["removes"],
                "llm_watcher_discovered_workers %d" % rec.stats["discovered"],
                "llm_watcher_owned_workers %d" % len(rec.ledger.owned),
                "llm_watcher_protected_workers %d" % len(rec.ledger.protected),
                "llm_watcher_model_map_entries %d" % len(rec.cfg.model_map),
                "llm_watcher_router_reachable %d" % (0 if rec.stats["last_error"] else 1),
            ]
            self._reply(200, "\n".join(lines) + "\n")

        def do_POST(self):
            if self.path.split("?")[0].rstrip("/") != "/model-map":
                self._reply(404, "not found\n")
                return
            try:
                n = int(self.headers.get("content-length") or 0)
                raw = self.rfile.read(n).decode("utf-8", "replace") if n else ""
            except Exception:
                raw = ""
            raw = raw.strip()
            if not raw:
                self._json(400, {"error": 'empty body; send {"original":"new"} or '
                                          "original:new (an empty new id deletes the entry)"})
                return
            if raw.startswith("{"):
                try:
                    obj = json.loads(raw)
                except Exception as exc:
                    self._json(400, {"error": "invalid JSON: %s" % exc})
                    return
                if not isinstance(obj, dict):
                    self._json(400, {"error": "JSON body must be an object"})
                    return
                if isinstance(obj.get("map"), dict):
                    obj = obj["map"]
                mapping = {str(k): ("" if v is None else str(v)) for k, v in obj.items()}
            else:
                mapping, bad = {}, []
                for part in re.split(r"[,;\n]+", raw):
                    part = part.strip()
                    if not part:
                        continue
                    orig, sep, new = part.partition(":")
                    if not sep or not orig.strip():
                        bad.append(part)
                        continue
                    mapping[orig.strip()] = new.strip()
                if bad:
                    self._json(400, {"error": "want original:new per entry", "ignored": bad})
                    return
            merged = reconciler.set_model_map(mapping)
            LOG.info("model-map updated via API -> %s", merged)
            self._json(200, {"model_map": merged,
                             "note": "owned workers are re-registered on the next pass"})

        def log_message(self, *args):
            return

    server = ThreadingHTTPServer(("0.0.0.0", port), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    LOG.info("metrics on :%d/metrics, model map API on :%d/model-map", port, port)
    return server


def parse_ports(spec):
    out: Set[int] = set()
    for part in filter(None, (p.strip() for p in (spec or "").split(","))):
        if "-" in part:
            lo, _, hi = part.partition("-")
            out.update(range(int(lo), int(hi) + 1))
        else:
            out.add(int(part))
    return out


def build_arg_parser():
    p = argparse.ArgumentParser(
        prog="llm-watcher",
        description="Auto-register local vLLM / sglang / llama.cpp instances into an llm-router pool.",
    )
    # Every default can also arrive as an environment variable, which is how a container
    # gets configured: LLM_WATCHER_ROUTER / LLM_WATCHER_TARGETS / ... (the LLM_WATCHER_
    # prefix is optional, and a command line flag always wins).
    p.add_argument("--router", default=_env("ROUTER", "ROUTER_URL", default="http://127.0.0.1:8800"),
                   help="llm-router base URL [$LLM_WATCHER_ROUTER]")
    p.add_argument("--router-api-key", default=_env("ROUTER_API_KEY"),
                   help="Bearer key for the router control plane [$LLM_WATCHER_ROUTER_API_KEY]")
    p.add_argument("--worker-api-key", default=_env("WORKER_API_KEY"),
                   help="key the router should use towards workers [$LLM_WATCHER_WORKER_API_KEY]")
    p.add_argument("--state-dir", default=_env("STATE_DIR", default="/data/llm-watcher"),
                   help="[$LLM_WATCHER_STATE_DIR]")
    p.add_argument("--interval", type=float, default=_env_float("INTERVAL", 15.0),
                   help="reconcile period, seconds [$LLM_WATCHER_INTERVAL]")
    p.add_argument("--probe-timeout", type=float, default=_env_float("PROBE_TIMEOUT", 3.0))
    p.add_argument("--workers", type=int, default=_env_int("WORKERS", 16), help="probe concurrency")
    p.add_argument("--target", action="append", dest="extra_targets", default=[],
                   help="worker URL discovery cannot see, e.g. an instance on another host "
                        "(repeatable; see also $LLM_WATCHER_TARGETS)")
    p.add_argument("--exclude", action="append", default=[],
                   help="regex of worker URLs never to touch (repeatable)")
    p.add_argument("--allow-port", default=_env("ALLOW_PORT", default=""),
                   help="only probe these ports, e.g. 8000-8020,11434 [$LLM_WATCHER_ALLOW_PORT]")
    p.add_argument("--deny-port", default=_env("DENY_PORT", default=""),
                   help="never probe these ports (added to the builtin list) [$LLM_WATCHER_DENY_PORT]")
    # BooleanOptionalAction gives both --x and --no-x, so an env var can switch a source
    # off and the command line can still switch it back on.
    p.add_argument("--proc-scan", dest="proc_scan", action=argparse.BooleanOptionalAction,
                   default=_env_bool("PROC_SCAN", True),
                   help="discover listening sockets from /proc/net/tcp [$LLM_WATCHER_PROC_SCAN]")
    p.add_argument("--docker", dest="docker", action=argparse.BooleanOptionalAction,
                   default=_env_bool("DOCKER", True),
                   help="discover running containers via the docker socket [$LLM_WATCHER_DOCKER]")
    p.add_argument("--container-ips", dest="container_ips", action=argparse.BooleanOptionalAction,
                   default=_env_bool("CONTAINER_IPS", True),
                   help="also probe unpublished container IPs [$LLM_WATCHER_CONTAINER_IPS]")
    p.add_argument("--docker-socket", default=_env("DOCKER_SOCKET", default="/var/run/docker.sock"))
    p.add_argument("--require-health", action="store_true",
                   default=_env_bool("REQUIRE_HEALTH", False),
                   help="only accept services answering 2xx on /health [$LLM_WATCHER_REQUIRE_HEALTH]")
    p.add_argument("--max-models", type=int, default=_env_int("MAX_MODELS", 8),
                   help="ignore servers advertising more than this many models, i.e. aggregators and "
                        "other proxies that would add a hop or loop back (0 = no limit) "
                        "[$LLM_WATCHER_MAX_MODELS]")
    p.add_argument("--allow-remove", dest="allow_remove", action=argparse.BooleanOptionalAction,
                   default=_env_bool("ALLOW_REMOVE", True),
                   help="delete owned workers that disappear for good, --no-allow-remove to keep "
                        "them and only log [$LLM_WATCHER_ALLOW_REMOVE]")
    p.add_argument("--remove-grace", type=float, default=_env_float("REMOVE_GRACE", 300.0),
                   help="seconds a worker must stay undiscovered before deletion [$LLM_WATCHER_REMOVE_GRACE]")
    p.add_argument("--keep-last", dest="keep_last", action=argparse.BooleanOptionalAction,
                   default=_env_bool("KEEP_LAST", True),
                   help="never remove the last worker of a model, --no-keep-last to override "
                        "[$LLM_WATCHER_KEEP_LAST]")
    p.add_argument("--fix-model-drift", action="store_true",
                   help="recycle a worker whose served model id changed on the same URL")
    p.add_argument("--short-model-names", dest="short_model_names",
                   action=argparse.BooleanOptionalAction,
                   default=_env_bool("SHORT_MODEL_NAMES", False),
                   help='register "/models/foo.gguf" as "foo" instead of the full served '
                        "path [$LLM_WATCHER_SHORT_MODEL_NAMES]")
    p.add_argument("--model-map", action="append", dest="model_maps", default=[],
                   metavar="ORIGINAL:NEW",
                   help='register a worker under a different public model id, e.g. '
                        "'/models/foo.gguf:foo' (repeatable). The environment accepts "
                        "the same pairs comma separated in LMR_MODEL_MAP / LMR_MODLE_MAP "
                        "[$LLM_WATCHER_MODEL_MAP]")
    p.add_argument("--add-confirm-timeout", type=float, default=_env_float("ADD_CONFIRM_TIMEOUT", 180.0))
    p.add_argument("--health-check-interval-secs", type=int, default=_env_int("HEALTH_CHECK_INTERVAL_SECS", 15))
    p.add_argument("--health-check-timeout-secs", type=int, default=_env_int("HEALTH_CHECK_TIMEOUT_SECS", 5))
    p.add_argument("--health-failure-threshold", type=int, default=_env_int("HEALTH_FAILURE_THRESHOLD", 3))
    p.add_argument("--health-success-threshold", type=int, default=_env_int("HEALTH_SUCCESS_THRESHOLD", 2))
    p.add_argument("--metrics-port", type=int, default=_env_int("METRICS_PORT", 0) or None)
    p.add_argument("--dry-run", action="store_true", help="log decisions, change nothing")
    p.add_argument("--once", action="store_true", help="single reconcile pass then exit")
    p.add_argument("-v", "--verbose", action="store_true")
    return p


def main(argv=None):
    args = build_arg_parser().parse_args(argv)
    logging.basicConfig(
        level=logging.DEBUG if args.verbose else logging.INFO,
        format="%(asctime)s %(levelname)-5s %(message)s",
        stream=sys.stdout,
    )
    cfg = Config(
        router=args.router,
        router_api_key=args.router_api_key,
        worker_api_key=args.worker_api_key,
        state_dir=args.state_dir,
        docker_socket=args.docker_socket,
        interval=args.interval,
        probe_timeout=args.probe_timeout,
        workers=args.workers,
        deny_ports=parse_ports(args.deny_port),
        allow_ports=parse_ports(args.allow_port),
        exclude=list(args.exclude),
        scan_proc=args.proc_scan,
        scan_docker=args.docker,
        scan_container_ips=args.container_ips,
        # --target flags first, then whatever the environment contributed.
        extra_targets=list(args.extra_targets) + parse_targets(
            _env("TARGETS", "WORKER_URLS", "REMOTE_WORKERS", default="")
        ),
        require_health=args.require_health,
        max_models=max(0, args.max_models),
        allow_remove=args.allow_remove,
        remove_grace=args.remove_grace,
        keep_last_per_model=args.keep_last,
        fix_model_drift=args.fix_model_drift,
        short_model_names=args.short_model_names,
        model_map=parse_model_map(
            _env("MODEL_MAP", "MODEL_ID_MAP", "MODEL_RENAME",
                 "LMR_MODEL_MAP", "LMR_MODLE_MAP", default="")
        ),
        add_confirm_timeout=args.add_confirm_timeout,
        dry_run=args.dry_run,
        health_check_interval_secs=args.health_check_interval_secs,
        health_check_timeout_secs=args.health_check_timeout_secs,
        health_failure_threshold=args.health_failure_threshold,
        health_success_threshold=args.health_success_threshold,
        metrics_port=args.metrics_port,
        once=args.once,
    )
    if not cfg.scan_proc and not cfg.scan_docker and not cfg.extra_targets and not cfg.allow_ports:
        LOG.error("nothing to scan: combine --no-proc-scan/--no-docker with --target or --allow-port")
        return 2
    reconciler = Reconciler(cfg)
    # Renames: env < --model-map flags < whatever POST /model-map saved earlier, so an
    # API change survives a restart. Delete a saved entry with {"original": ""}.
    for spec in args.model_maps:
        cfg.model_map.update(parse_model_map(spec))
    cfg.model_map.update(reconciler.ledger.model_map)
    reconciler.ledger.model_map = dict(cfg.model_map)
    if cfg.model_map:
        LOG.info("model map: %s", ", ".join("%s -> %s" % kv for kv in sorted(cfg.model_map.items())))
    if not reconciler.router.health():
        LOG.error("router %s/health did not answer; refusing to start", reconciler.router.base)
        return 3
    if args.metrics_port:
        start_metrics(reconciler, args.metrics_port)

    stop = threading.Event()

    def _handler(signum, _frame):
        LOG.info("signal %s -> shutting down", signum)
        stop.set()

    signal.signal(signal.SIGTERM, _handler)
    signal.signal(signal.SIGINT, _handler)
    reconciler.run(stop)
    return 0


if __name__ == "__main__":
    sys.exit(main())
