#!/usr/bin/env python3
"""Tests for llm-watcher.

Run:  python3 -m unittest discover -s watcher -p 'test_*.py' -v

No external services are required: fake OpenAI servers and a fake router both run
in-process on 127.0.0.1 ephemeral ports.
"""

import json
import os
import sys
import tempfile
import threading
import unittest.mock
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import llm_watcher as W  # noqa: E402


# --------------------------------------------------------------------------------------
# Fixtures: a fake inference server and a fake router
# --------------------------------------------------------------------------------------
class FakeServer(ThreadingHTTPServer):
    daemon_threads = True
    allow_reuse_address = True


class Endpoint:
    """Minimal OpenAI-ish server: models list, optional /health, optional engine hints."""

    def __init__(self, models=("m1",), health=True, extra=None, html_instead=False,
                 raw=None):
        self.models = list(models)
        self.health = health
        self.extra = extra or {}
        self.html_instead = html_instead
        self.raw = raw or {}          # path -> (content_type, body) verbatim answers
        self.calls = []
        srv = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *args):
                return

            def _json(self, obj, code=200):
                raw = json.dumps(obj).encode()
                self.send_response(code)
                self.send_header("content-type", "application/json")
                self.send_header("content-length", str(len(raw)))
                self.end_headers()
                self.wfile.write(raw)

            def _html(self, code=200):
                raw = b"<html><body>API</body></html>"
                self.send_response(code)
                self.send_header("content-type", "text/html")
                self.send_header("content-length", str(len(raw)))
                self.end_headers()
                self.wfile.write(raw)

            def do_GET(self):
                path = self.path.split("?")[0]
                srv.calls.append(path)
                if path in srv.raw:
                    ctype, body = srv.raw[path]
                    raw = body.encode()
                    self.send_response(200)
                    self.send_header("content-type", ctype)
                    self.send_header("content-length", str(len(raw)))
                    self.end_headers()
                    self.wfile.write(raw)
                    return
                if path in srv.extra:
                    obj = srv.extra[path]
                    if obj is None:
                        self._html()
                    else:
                        self._json(obj)
                elif path == "/v1/models":
                    if srv.html_instead:
                        self._html()
                    else:
                        self._json({"object": "list",
                                    "data": [{"id": m, "object": "model"} for m in srv.models]})
                elif path == "/health":
                    if srv.health:
                        self._json({"status": "ok"})
                    else:
                        self._html(404)
                else:
                    self._json({"error": "not found"}, 404)

        FakeServer.__init__  # silence linters
        self._http = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        threading.Thread(target=self._http.serve_forever, daemon=True).start()

    @property
    def url(self):
        host, port = self._http.server_address[0], self._http.server_address[1]
        return "http://%s:%d" % (host, port)

    def stop(self):
        self._http.shutdown()
        self._http.server_close()


class FakeRouter:
    """Stands in for RouterClient so reconcile() is testable without smg."""

    base = "http://127.0.0.1:1"
    port = 1

    def __init__(self, pool=None, routers_count=1):
        self.pool = dict(pool or {})          # url -> worker dict
        self.added, self.deleted = [], []
        self.add_result = (True, "", None)
        self.routers_count = routers_count
        self._n = 0

    def health(self):
        return True

    def server_info(self):
        return {"router_manager": True, "routers_count": self.routers_count,
                "workers_count": len(self.pool)}

    def list_workers(self):
        return {url: dict(item) for url, item in self.pool.items()}

    def add(self, url, model_id, extra):
        self.added.append({"url": url, "model_id": model_id, "extra": extra})
        ok, detail, wid = self.add_result
        if ok:
            self._n += 1
            wid = wid or "w%d" % self._n
            self.pool[url] = {"id": wid, "url": url, "model_id": model_id,
                              "is_healthy": True, "worker_type": "regular"}
        return ok, detail, wid

    def delete(self, worker_id):
        self.deleted.append(worker_id)
        for url, item in list(self.pool.items()):
            if item.get("id") == worker_id:
                del self.pool[url]
        return True, ""

    def job_status(self, worker_id):
        return "succeeded"


def make_cfg(state_dir, targets, **overrides):
    cfg = W.Config(router="http://127.0.0.1:1", state_dir=state_dir, scan_proc=False,
                   scan_docker=False, extra_targets=list(targets), interval=0.01)
    for key, value in overrides.items():
        setattr(cfg, key, value)
    return cfg


class WatcherTestCase(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.mkdtemp(prefix="llm-watcher-test-")
        self._servers = []

    def tearDown(self):
        for srv in self._servers:
            srv.stop()

    def server(self, *args, **kwargs):
        srv = Endpoint(*args, **kwargs)
        self._servers.append(srv)
        return srv

    def reconciler(self, targets, router, **cfg_overrides):
        cfg_overrides.setdefault("remove_grace", 0.0)
        cfg_overrides.setdefault("add_confirm_timeout", 600.0)
        rec = W.Reconciler(make_cfg(os.path.join(self._tmp, "state-%d" % id(self)), targets,
                                    **cfg_overrides))
        rec.router = router
        return rec


# --------------------------------------------------------------------------------------
# Unit tests: pure helpers
# --------------------------------------------------------------------------------------
class TestNormalize(unittest.TestCase):
    def test_canonical_form(self):
        self.assertEqual(W.normalize_url("127.0.0.1:8012"), "http://127.0.0.1:8012")
        self.assertEqual(W.normalize_url("http://127.0.0.1:8012/"), "http://127.0.0.1:8012")
        self.assertEqual(W.normalize_url("HTTP://Host:8000/v1"), "http://host:8000")
        self.assertEqual(W.normalize_url("http://host:80"), "http://host")
        self.assertEqual(W.normalize_url("https://host:443"), "https://host")
        self.assertEqual(W.normalize_url("http://[::1]:9/"), "http://[::1]:9")

    def test_ports(self):
        self.assertEqual(W.parse_ports("8012,8100-8102"), {8012, 8100, 8101, 8102})
        self.assertEqual(W.parse_ports(""), set())

    def test_ledger_roundtrip(self):
        path = os.path.join(tempfile.mkdtemp(prefix="lw-"), "ledger.json")
        led = W.Ledger(path)
        led.protected.add("http://a:1")
        led.owned["http://b:2"] = {"model_id": "m", "worker_id": "w"}
        led.save()
        again = W.Ledger(path)
        self.assertEqual(again.protected, {"http://a:1"})
        self.assertEqual(again.owned["http://b:2"]["worker_id"], "w")


class TestEnvironment(WatcherTestCase):
    """The container is configured by env alone, so the wiring needs its own coverage."""

    def test_parse_targets_accepts_commas_spaces_and_newlines(self):
        spec = "http://a:1 , http://b:2;http://c:3\n http://d:4"
        self.assertEqual(W.parse_targets(spec), ["http://a:1", "http://b:2", "http://c:3", "http://d:4"])
        self.assertEqual(W.parse_targets(""), [])
        self.assertEqual(W.parse_targets("   "), [])

    def test_env_prefix_is_optional(self):
        with unittest.mock.patch.dict(os.environ, {"LLM_WATCHER_ROUTER": "http://r:9"}):
            self.assertEqual(W._env("ROUTER", default="x"), "http://r:9")
        with unittest.mock.patch.dict(os.environ, {"ROUTER_URL": "http://r2:9"}):
            self.assertEqual(W._env("ROUTER", "ROUTER_URL", default="x"), "http://r2:9")
        with unittest.mock.patch.dict(os.environ, {}, clear=True):
            self.assertEqual(W._env("ROUTER", default="fallback"), "fallback")

    def test_blank_env_does_not_override_default(self):
        with unittest.mock.patch.dict(os.environ, {"LLM_WATCHER_ROUTER": "   "}):
            self.assertEqual(W._env("ROUTER", default="keep"), "keep")

    def test_bool_and_number_parsing(self):
        with unittest.mock.patch.dict(os.environ, {"LLM_WATCHER_KEEP_LAST": "false",
                                                   "LLM_WATCHER_INTERVAL": " 7.5 "}):
            self.assertIs(W._env_bool("KEEP_LAST", True), False)
            self.assertEqual(W._env_float("INTERVAL", 15.0), 7.5)
            self.assertEqual(W._env_int("INTERVAL", 15), 7)
        with unittest.mock.patch.dict(os.environ, {"LLM_WATCHER_INTERVAL": "banana"}):
            self.assertEqual(W._env_float("INTERVAL", 15.0), 15.0)   # bad input is ignored, not fatal

    def test_parser_reads_environment(self):
        env = {
            "LLM_WATCHER_ROUTER": "http://10.0.0.1:8800",
            "LLM_WATCHER_TARGETS": "http://10.252.25.217:8100,http://10.252.25.217:8101",
            "LLM_WATCHER_INTERVAL": "30",
            "LLM_WATCHER_DOCKER": "false",
            "LLM_WATCHER_PROC_SCAN": "0",
            "LLM_WATCHER_ALLOW_REMOVE": "no",
        }
        with unittest.mock.patch.dict(os.environ, env, clear=False):
            args = W.build_arg_parser().parse_args([])
        self.assertEqual(args.router, "http://10.0.0.1:8800")
        self.assertEqual(args.extra_targets, [])          # env targets land separately from flags
        self.assertEqual(args.interval, 30.0)
        self.assertIs(args.docker, False)
        self.assertIs(args.proc_scan, False)
        self.assertIs(args.allow_remove, False)

    def test_command_line_beats_environment(self):
        env = {"LLM_WATCHER_ROUTER": "http://from-env:1", "LLM_WATCHER_TARGETS": "http://env:1"}
        with unittest.mock.patch.dict(os.environ, env, clear=False):
            parser = W.build_arg_parser()
            args = parser.parse_args(["--router", "http://from-cli:2", "--target", "http://cli:1"])
            cfg = W.Config(router=args.router, extra_targets=list(args.extra_targets)
                           + W.parse_targets(W._env("TARGETS", default="")))
        self.assertEqual(cfg.router, "http://from-cli:2")
        self.assertEqual(cfg.extra_targets, ["http://cli:1", "http://env:1"])   # flags first

    def test_remote_targets_end_to_end_via_cli(self):
        # Two "remote" instances: they are just probed by URL, so they must join the pool
        # and leave it again when they go away -- a named target is not immortal.
        a = self.server(models=["remote-m"]); b = self.server(models=["remote-m"])
        try:
            router = FakeRouter()
            rec = self.reconciler([a.url, b.url], router, remove_grace=3600.0)
            rec.reconcile()
            self.assertEqual(sorted(i["url"] for i in router.added), sorted([a.url, b.url]))
            a.stop()
            rec.reconcile()
            self.assertIn(a.url, rec.ledger.missing_since)      # grace period, not instant
            self.assertIn(a.url, router.pool)
            rec.cfg.remove_grace = 0.0                          # window elapsed
            rec.reconcile()
            self.assertNotIn(a.url, router.pool)
            self.assertIn(b.url, router.pool)
        finally:
            b.stop()


class TestProbe(WatcherTestCase):
    def test_accepts_openai_server(self):
        srv = self.server(models=["qwen-x"], health=True)
        info = W.probe_worker(srv.url)
        self.assertIsNotNone(info)
        self.assertEqual(info.models, ["qwen-x"])
        self.assertTrue(info.has_health)
        self.assertEqual(info.engine, "openai")

    def test_rejects_html_services(self):
        # node_exporter and friends answer 200 + HTML on unknown paths.
        srv = self.server(html_instead=True)
        self.assertIsNone(W.probe_worker(srv.url))

    def test_rejects_models_without_ids(self):
        srv = self.server(models=[])
        self.assertIsNone(W.probe_worker(srv.url))

    def test_rejects_refused_connection(self):
        self.assertIsNone(W.probe_worker("http://127.0.0.1:1", timeout=0.5))

    def test_rejects_other_router(self):
        srv = self.server(extra={"/server_info": {"router_manager": True, "workers_count": 2}})
        self.assertIsNone(W.probe_worker(srv.url))

    def test_rejects_aggregator_with_many_models(self):
        srv = self.server(models=["m%d" % i for i in range(30)])
        self.assertIsNone(W.probe_worker(srv.url, max_models=8))
        self.assertIsNotNone(W.probe_worker(srv.url, max_models=0))

    def prom(self, text):
        return self.server(raw={"/metrics": ("text/plain; version=0.0.4", text)})

    def test_engine_detection_sglang(self):
        sg = self.server(extra={"/get_server_info": {"model_path": "/models/x", "tp_size": 2}})
        self.assertEqual(W.probe_worker(sg.url).engine, "sglang")
        si = self.server(extra={"/server_info": {"model_path": "/models/y"}})
        self.assertEqual(W.probe_worker(si.url).engine, "sglang")

    def test_engine_detection_vllm_from_metrics(self):
        srv = self.prom("# TYPE vllm:num_requests_running gauge\nvllm:num_requests_running 3\n")
        self.assertEqual(W.probe_worker(srv.url).engine, "vllm")

    def test_engine_detection_llama_cpp(self):
        # llama.cpp namespaces its metrics like vllm does, and both /metrics and /props
        # are opt-in there, so /props is the fallback rather than the primary signal.
        by_metrics = self.prom("# TYPE llamacpp:tokens_predicted_total counter\n")
        self.assertEqual(W.probe_worker(by_metrics.url).engine, "llama.cpp")
        by_props = self.server(extra={"/props": {"build_commit": "abc123", "webui": "/"}})
        self.assertEqual(W.probe_worker(by_props.url).engine, "llama.cpp")

    def test_engine_unknown_when_llama_cpp_flags_off(self):
        # --metrics and --props disabled: honest "openai" beats a wrong guess, and the
        # label is informational only -- routing never depends on it.
        srv = self.server()
        self.assertEqual(W.probe_worker(srv.url).engine, "openai")

    def test_engine_defaults_to_openai(self):
        srv = self.prom("# TYPE node_load1 gauge\nnode_load1 0.5\n")
        self.assertEqual(W.probe_worker(srv.url).engine, "openai")

    def test_require_health_skips_worker_without_health(self):
        srv = self.server(health=False)
        self.assertIsNotNone(W.probe_worker(srv.url, require_health=False))
        self.assertIsNone(W.probe_worker(srv.url, require_health=True))


class TestDiscovery(unittest.TestCase):
    def test_listening_sockets_shape(self):
        srv = Endpoint()
        try:
            pairs = W.listening_sockets()
            self.assertTrue(any(port == srv._http.server_address[1] for _host, port in pairs))
        finally:
            srv.stop()

    def test_local_candidates_honours_deny(self):
        cands = W.local_candidates(set(range(1, 65536)))
        self.assertEqual(cands, [])
        some = W.local_candidates(set())
        self.assertTrue(all(c.source == "proc" for c in some))
        self.assertTrue(all(c.url.startswith("http://") for c in some))

    def test_docker_candidates_prefer_published_ports(self):
        payload = [{
            "Id": "abc", "Names": ["/sglang-x"],
            "HostConfig": {"NetworkMode": "bridge"},
            "Ports": [{"IP": "0.0.0.0", "PublicPort": 8012, "PrivatePort": 8000, "Type": "tcp"}],
            "NetworkSettings": {"Networks": {"bridge": {
                "IPAddress": "172.17.0.5", "Ports": {"8000/tcp": None, "8001/tcp": None}}}},
        }, {
            "Id": "def", "Names": ["/router"],
            "HostConfig": {"NetworkMode": "host"},
            "Ports": [], "NetworkSettings": {"Networks": {}},
        }]
        orig = W.unix_http_json
        W.unix_http_json = lambda sock, path, timeout=5.0: (200, payload)
        try:
            cands, names = W.docker_candidates("/not/used")
            cands_no_ip, _ = W.docker_candidates("/not/used", include_container_ips=False)
        finally:
            W.unix_http_json = orig
        urls = [c.url for c in cands]
        self.assertIn("http://127.0.0.1:8012", urls)
        self.assertEqual(names[8012], "sglang-x")
        # The published port must not also appear as a container-internal URL.
        self.assertNotIn("http://172.17.0.5:8000", urls)
        self.assertIn("http://172.17.0.5:8001", urls)
        self.assertEqual([c.url for c in cands_no_ip], ["http://127.0.0.1:8012"])


# --------------------------------------------------------------------------------------
# Behaviour tests: reconcile()
# --------------------------------------------------------------------------------------
class TestReconcile(WatcherTestCase):
    def test_adds_discovered_worker_and_confirms_it(self):
        srv = self.server(models=["new-model"])
        router = FakeRouter()
        rec = self.reconciler([srv.url], router)
        rec.reconcile()
        self.assertEqual(len(router.added), 1)
        self.assertEqual(router.added[0]["model_id"], "new-model")
        self.assertEqual(router.added[0]["extra"]["labels"]["managed-by"], "llm-watcher")
        self.assertIn(srv.url, rec._pending)
        rec.reconcile()                       # second pass sees it in the pool
        self.assertEqual(rec._pending, {})
        self.assertEqual(rec.ledger.owned[srv.url]["model_id"], "new-model")

    def test_worker_without_health_gets_healthcheck_disabled(self):
        srv = self.server(models=["m"], health=False)
        router = FakeRouter()
        rec = self.reconciler([srv.url], router)
        rec.reconcile()
        self.assertTrue(router.added[0]["extra"]["disable_health_check"])

    def test_stable_url_not_added_twice(self):
        srv = self.server(models=["m"])
        router = FakeRouter()
        rec = self.reconciler([srv.url], router)
        for _ in range(5):
            rec.reconcile()
        self.assertEqual(len(router.added), 1)
        self.assertEqual(len(router.pool), 1)

    def test_deduplicates_two_urls_of_one_container(self):
        srv = self.server(models=["m"])
        router = FakeRouter()
        rec = self.reconciler([srv.url], router)
        # Simulate the docker source offering a second URL for the same instance.
        rec.collect = lambda: [  # type: ignore[assignment]
            W.Candidate(url=srv.url, source="docker", instance_key="ctr1"),
            W.Candidate(url=srv.url.replace("127.0.0.1", "localhost"), source="docker-net",
                        instance_key="ctr1"),
        ]
        rec.reconcile()
        self.assertEqual(len(router.added), 1)

    def test_first_contact_protects_existing_workers(self):
        alive = self.server(models=["human-model"])
        router = FakeRouter(pool={alive.url: {"id": "w-keep", "url": alive.url,
                                             "model_id": "human-model", "is_healthy": True}})
        rec = self.reconciler([alive.url], router)
        rec.reconcile()
        self.assertEqual(rec.ledger.protected, {alive.url})
        self.assertEqual(router.added, [])
        alive.stop()                          # a protected worker may never be deleted
        for _ in range(4):
            rec.reconcile()
        self.assertEqual(router.deleted, [])
        self.assertIn(alive.url, router.pool)

    def test_removes_dead_worker_when_replica_is_healthy(self):
        dying = self.server(models=["dup"])
        keeper = self.server(models=["dup"])
        router = FakeRouter()
        rec = self.reconciler([dying.url, keeper.url], router)
        rec.reconcile()
        dying.stop()
        rec.reconcile()
        self.assertEqual(len(router.deleted), 1)
        self.assertNotIn(dying.url, router.pool)
        self.assertIn(keeper.url, router.pool)

    def test_keeps_last_worker_of_a_model(self):
        only = self.server(models=["lonely"])
        router = FakeRouter()
        rec = self.reconciler([only.url], router)
        rec.reconcile()
        only.stop()
        for _ in range(3):
            rec.reconcile()
        self.assertEqual(router.deleted, [])
        self.assertIn(only.url, router.pool)

    def test_remove_grace_is_respected(self):
        srv = self.server(models=["m"])
        router = FakeRouter()
        rec = self.reconciler([srv.url], router, remove_grace=3600.0, keep_last_per_model=False)
        rec.reconcile()
        srv.stop()
        rec.reconcile()
        self.assertEqual(router.deleted, [])

    def test_no_keep_last_allows_emptying_a_model(self):
        srv = self.server(models=["solo"])
        router = FakeRouter()
        rec = self.reconciler([srv.url], router, keep_last_per_model=False)
        rec.reconcile()
        srv.stop()
        rec.reconcile()
        self.assertEqual(len(router.deleted), 1)

    def test_stuck_add_releases_the_url(self):
        class StuckRouter(FakeRouter):
            def add(self, url, model_id, extra):        # never lands in the pool
                self.added.append({"url": url, "model_id": model_id, "extra": extra})
                return True, "", "w-stuck"

            def job_status(self, worker_id):
                return "processing"

        srv = self.server(models=["m"])
        rec = self.reconciler([srv.url], StuckRouter(), add_confirm_timeout=0.0)
        rec.reconcile()                 # pass 1: queue the AddWorker job
        self.assertEqual(rec.router.deleted, [])
        self.assertIn(srv.url, rec.ledger.owned)

        rec.reconcile()                 # pass 2: the job timed out -> release the URL
        self.assertEqual(rec.router.deleted, ["w-stuck"])
        self.assertEqual(rec.ledger.owned, {})
        self.assertEqual(len(rec.router.added), 1)      # not re-added in the same pass

        rec.reconcile()                 # pass 3: retried now that the URL is free
        self.assertEqual(len(rec.router.added), 2)

    def test_router_restart_refreshes_worker_id(self):
        srv = self.server(models=["m"])
        router = FakeRouter()
        rec = self.reconciler([srv.url], router, keep_last_per_model=False)
        rec.reconcile()
        rec.reconcile()
        router.pool[srv.url]["id"] = "fresh-id"          # router restarted, ids changed
        rec.reconcile()
        self.assertEqual(rec.ledger.owned[srv.url]["worker_id"], "fresh-id")
        srv.stop()
        rec.reconcile()
        self.assertEqual(router.deleted, ["fresh-id"])

    def test_model_drift_is_warned_and_optionally_recycled(self):
        srv = self.server(models=["weight-v2"])
        router = FakeRouter()
        rec = self.reconciler([srv.url], router, keep_last_per_model=False)
        rec.reconcile()
        rec.reconcile()
        router.pool[srv.url]["model_id"] = "weight-v1"   # router still remembers the old id
        rec.reconcile()
        self.assertEqual(router.deleted, [])
        self.assertIn(srv.url, rec.ledger.warned)

        rec2 = self.reconciler([srv.url], FakeRouter(), keep_last_per_model=False,
                               fix_model_drift=True)
        rec2.reconcile()
        rec2.reconcile()
        rec2.router.pool[srv.url]["model_id"] = "weight-v1"
        rec2.reconcile()
        self.assertEqual(len(rec2.router.deleted), 1)

    def test_dry_run_changes_nothing(self):
        srv = self.server(models=["m"])
        router = FakeRouter()
        state = os.path.join(self._tmp, "dry-run-state")
        cfg = make_cfg(state, [srv.url], dry_run=True)
        rec = W.Reconciler(cfg)
        rec.router = router
        rec.reconcile()
        self.assertEqual(router.added, [])
        self.assertEqual(router.pool, {})
        # A dry run must leave no trace at all, not even a ledger / state directory.
        self.assertFalse(os.path.exists(state))
        self.assertEqual(router.deleted, [])

    def test_short_model_names_registers_basename(self):
        srv = self.server(models=["/models/Qwen3.8-27B-SimPO-Q3-LynnStyle.gguf"])
        router = FakeRouter()
        rec = self.reconciler([srv.url], router, short_model_names=True)
        rec.reconcile()
        self.assertEqual(len(router.added), 1)
        self.assertEqual(router.added[0]["model_id"], "Qwen3.8-27B-SimPO-Q3-LynnStyle")

    def test_model_names_untouched_without_the_flag(self):
        srv = self.server(models=["/models/two/paths.gguf"])
        router = FakeRouter()
        rec = self.reconciler([srv.url], router)
        rec.reconcile()
        self.assertEqual(router.added[0]["model_id"], "/models/two/paths.gguf")

    def test_short_model_names_flag_and_env(self):
        import os as _os
        old = _os.environ.get("LLM_WATCHER_SHORT_MODEL_NAMES")
        _os.environ["LLM_WATCHER_SHORT_MODEL_NAMES"] = "true"
        try:
            args = W.build_arg_parser().parse_args([])
            self.assertTrue(args.short_model_names)
        finally:
            if old is None:
                _os.environ.pop("LLM_WATCHER_SHORT_MODEL_NAMES", None)
            else:
                _os.environ["LLM_WATCHER_SHORT_MODEL_NAMES"] = old
        args = W.build_arg_parser().parse_args(["--no-short-model-names"])
        self.assertFalse(args.short_model_names)

    def test_exclude_and_target_filtering(self):
        a = self.server(models=["a"])
        b = self.server(models=["b"])
        router = FakeRouter()
        rec = self.reconciler([a.url, b.url], router, exclude=[r":%d$" % b._http.server_address[1]])
        rec.reconcile()
        self.assertEqual([item["url"] for item in router.added], [a.url])

    def test_router_unreachable_is_not_fatal(self):
        class Down(FakeRouter):
            def list_workers(self):
                raise RuntimeError("connection refused")

        srv = self.server(models=["m"])
        rec = self.reconciler([srv.url], Down())
        rec.reconcile()                                  # must not raise
        self.assertTrue(rec.stats["last_error"])
        self.assertEqual(rec.stats["reconciles"], 1)

    def test_persisted_ledger_survives_restart(self):
        srv = self.server(models=["m"])
        router = FakeRouter()
        state = os.path.join(self._tmp, "shared-state")
        first = W.Reconciler(make_cfg(state, [srv.url], remove_grace=0.0))
        first.router = router
        first.reconcile()
        first.reconcile()
        adds_before = len(router.added)
        second = W.Reconciler(make_cfg(state, [srv.url], remove_grace=0.0))
        second.router = router
        second.reconcile()
        self.assertEqual(second.ledger.owned[srv.url]["model_id"], "m")
        self.assertEqual(len(router.added), adds_before)  # no duplicate churn after restart


    def test_heterogeneous_pool_warns_in_single_router_mode(self):
        # smg silently serves the wrong model in this configuration. The daemon cannot
        # fix that, but it must not slip by unnoticed: adding a second model to a
        # single-router pool has to warn exactly once, not once per pass.
        a = self.server(models=["model-a"])
        b = self.server(models=["model-b"])
        router = FakeRouter(routers_count=1)
        rec = self.reconciler([a.url, b.url], router)
        with unittest.mock.patch.object(W.LOG, "warning") as warn:
            rec.reconcile()
            rec.reconcile()
            rec.reconcile()
        messages = [c.args[0] if c.args else "" for c in warn.call_args_list]
        self.assertEqual(len([m for m in messages if "enable-igw" in str(m)]), 1)

    def test_heterogeneous_pool_is_silent_under_igw(self):
        # IGW does route per model (verified against a live router): nothing to report.
        a = self.server(models=["model-a"])
        b = self.server(models=["model-b"])
        router = FakeRouter(routers_count=5)
        rec = self.reconciler([a.url, b.url], router)
        with unittest.mock.patch.object(W.LOG, "warning") as warn:
            rec.reconcile()
            rec.reconcile()
        messages = [c.args[0] if c.args else "" for c in warn.call_args_list]
        self.assertEqual([m for m in messages if "enable-igw" in str(m)], [])

    def test_homogeneous_pool_never_warns(self):
        a = self.server(models=["same"])
        b = self.server(models=["same"])
        router = FakeRouter(routers_count=1)
        rec = self.reconciler([a.url, b.url], router)
        with unittest.mock.patch.object(W.LOG, "warning") as warn:
            rec.reconcile()
            rec.reconcile()
        messages = [c.args[0] if c.args else "" for c in warn.call_args_list]
        self.assertEqual([m for m in messages if "enable-igw" in str(m)], [])

if __name__ == "__main__":
    unittest.main(verbosity=2)
