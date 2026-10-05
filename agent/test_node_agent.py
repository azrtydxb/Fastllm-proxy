"""Tests for the node agent. Standard library only, like the agent itself.

Run with: python3 -m unittest discover -s agent
"""

import http.server
import importlib.util
import json
import os
import threading
import types
import unittest

_HERE = os.path.dirname(os.path.abspath(__file__))
_spec = importlib.util.spec_from_file_location(
    "node_agent", os.path.join(_HERE, "fastllm-node-agent.py")
)
agent = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(agent)


def args(**kw):
    base = {
        "provider_name": None,
        "node": "kw",
        "kubernetes": True,
        "advertise": None,
        "probe_timeout": 2,
    }
    base.update(kw)
    return types.SimpleNamespace(**base)


class Naming(unittest.TestCase):
    def setUp(self):
        agent.LABEL_OF.clear()
        agent.MODEL_OF.clear()

    def test_parts_are_folded_for_screens_and_logs(self):
        self.assertEqual(
            agent.name_part("nvidia/Qwen3.6-35B-A3B-NVFP4"),
            "nvidia-qwen3.6-35b-a3b-nvfp4",
        )
        self.assertEqual(agent.name_part("  gx10_9c17 "), "gx10-9c17")
        self.assertEqual(agent.name_part("--a//b--"), "a-b")

    # Breaks if any of the four parts is dropped: the two bge-m3 engines on
    # one port and the two NVFP4 engines on one model collided on kw.
    def test_kubernetes_name_is_cluster_node_model_port(self):
        url = "http://kuvryn-6a46.kuvryn-ai-workloads.svc:8890/v1"
        agent.LABEL_OF[url] = "gx10-48f4"
        agent.MODEL_OF[url] = "bge-m3"
        self.assertEqual(agent.provider_name(args(), url), "kw-gx10-48f4-bge-m3-8890")

    def test_same_port_and_model_on_two_nodes_differ(self):
        a, b = "http://a.svc:8000/v1", "http://b.svc:8000/v1"
        for url, node in ((a, "gx10-9c17"), (b, "gx10-48f4")):
            agent.LABEL_OF[url] = node
            agent.MODEL_OF[url] = "nvidia/Qwen3.6-35B-A3B-NVFP4"
        self.assertNotEqual(
            agent.provider_name(args(), a), agent.provider_name(args(), b)
        )
        self.assertEqual(
            agent.provider_name(args(), a),
            "kw-gx10-9c17-nvidia-qwen3.6-35b-a3b-nvfp4-8000",
        )

    def test_two_models_on_one_node_and_port_family_differ(self):
        a, b = "http://192.168.10.245:8001/v1", "http://192.168.10.245:8890/v1"
        for url, model in ((a, "qwen3.5-9b"), (b, "bge-m3")):
            agent.LABEL_OF[url] = "gx10-9c17"
            agent.MODEL_OF[url] = model
        self.assertEqual(agent.provider_name(args(), a), "kw-gx10-9c17-qwen3.5-9b-8001")
        self.assertEqual(agent.provider_name(args(), b), "kw-gx10-9c17-bge-m3-8890")

    def test_unknown_parts_are_left_out_not_guessed(self):
        url = "http://x.svc:8000/v1"
        self.assertEqual(agent.provider_name(args(), url), "kw-8000")
        agent.MODEL_OF[url] = "bge-m3"
        self.assertEqual(agent.provider_name(args(), url), "kw-bge-m3-8000")

    def test_bare_host_uses_its_own_name_as_the_node(self):
        url = "http://192.168.10.246:8000/v1"
        agent.LABEL_OF[url] = "ignored-off-kubernetes"
        agent.MODEL_OF[url] = "qwen3.5-9b"
        self.assertEqual(
            agent.provider_name(args(node="dgx-spark", kubernetes=False), url),
            "dgx-spark-qwen3.5-9b-8000",
        )


class ServiceNode(unittest.TestCase):
    def setUp(self):
        self.svc = {
            "metadata": {"name": "kuvryn-6a46", "namespace": "work"},
            "spec": {"selector": {"ai.kuvryn.worker": "kuvryn-6a46"}},
        }

    @staticmethod
    def pod(ns, labels, node):
        return {
            "metadata": {"namespace": ns, "labels": labels},
            "spec": {"nodeName": node},
        }

    def test_selected_pods_name_the_node(self):
        pods = [
            self.pod(
                "work", {"ai.kuvryn.worker": "kuvryn-6a46", "x": "y"}, "gx10-48f4"
            ),
            self.pod("work", {"ai.kuvryn.worker": "other"}, "gx10-9c17"),
            self.pod("elsewhere", {"ai.kuvryn.worker": "kuvryn-6a46"}, "gx10-9c17"),
        ]
        self.assertEqual(agent.service_node(self.svc, pods), "gx10-48f4")

    def test_pods_on_several_nodes_name_none(self):
        pods = [
            self.pod("work", {"ai.kuvryn.worker": "kuvryn-6a46"}, "gx10-48f4"),
            self.pod("work", {"ai.kuvryn.worker": "kuvryn-6a46"}, "gx10-9c17"),
        ]
        self.assertIsNone(agent.service_node(self.svc, pods))

    def test_no_selector_names_none(self):
        self.assertIsNone(
            agent.service_node({"metadata": {"namespace": "work"}, "spec": {}}, [])
        )


class Discovery(unittest.TestCase):
    def setUp(self):
        agent.LABEL_OF.clear()
        agent.MODEL_OF.clear()
        self._kube_get, self._node_addresses = agent.kube_get, agent.node_addresses

    def tearDown(self):
        agent.kube_get, agent.node_addresses = self._kube_get, self._node_addresses

    # Breaks if a Service advertised through fastllm.io/advertise registers
    # without its node, which is how every one of them became kw-<port>.
    def test_advertised_service_carries_its_pods_node(self):
        url = "http://kuvryn-6a46.work.svc:8890/v1"
        listing = {
            "/api/v1/pods": {
                "items": [
                    {
                        "metadata": {"namespace": "work", "labels": {"w": "6a46"}},
                        "spec": {"nodeName": "gx10-48f4"},
                    }
                ]
            },
            "/api/v1/services": {
                "items": [
                    {
                        "metadata": {
                            "name": "kuvryn-6a46",
                            "namespace": "work",
                            "annotations": {"fastllm.io/advertise": url},
                        },
                        "spec": {
                            "type": "ClusterIP",
                            "selector": {"w": "6a46"},
                            "ports": [{"port": 8890}],
                        },
                    }
                ]
            },
        }
        agent.kube_get = lambda path, timeout: listing[path]
        agent.node_addresses = lambda a, t: {}
        self.assertEqual(agent.kube_candidates(args()), [url])
        self.assertEqual(agent.LABEL_OF[url], "gx10-48f4")


class StaleModel(unittest.TestCase):
    # Breaks if a model the engine stopped listing keeps naming its provider.
    def test_an_empty_answer_forgets_the_model(self):
        answers = [{"data": [{"id": "bge-m3"}]}, {"data": []}]

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_GET(self):
                body = json.dumps(answers.pop(0)).encode()
                self.send_response(200)
                self.send_header("content-type", "application/json")
                self.end_headers()
                self.wfile.write(body)

            def log_message(self, *a):
                pass

        server = http.server.HTTPServer(("127.0.0.1", 0), Handler)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        try:
            base = f"http://127.0.0.1:{server.server_port}/v1"
            agent.MODEL_OF.clear()
            self.assertTrue(agent.serves_models(base, 2))
            self.assertEqual(agent.MODEL_OF[base], "bge-m3")
            self.assertTrue(agent.serves_models(base, 2))
            self.assertNotIn(base, agent.MODEL_OF)
        finally:
            server.shutdown()
            server.server_close()


class AdvertiseScheme(unittest.TestCase):
    def setUp(self):
        self._kube_get, self._node_addresses = agent.kube_get, agent.node_addresses

    def tearDown(self):
        agent.kube_get, agent.node_addresses = self._kube_get, self._node_addresses

    # Breaks if an annotation can make the agent open something other than an
    # http(s) URL: urllib reads file:// paths, and anyone who can create a
    # Service could otherwise point the probe at the agent's own files.
    def test_only_http_urls_are_taken_from_the_annotation(self):
        def listing(path, timeout):
            if path == "/api/v1/pods":
                return {"items": []}
            return {
                "items": [
                    {
                        "metadata": {
                            "name": f"s{i}",
                            "namespace": "n",
                            "annotations": {"fastllm.io/advertise": u},
                        },
                        "spec": {"type": "ClusterIP", "ports": [{"port": 1}]},
                    }
                    for i, u in enumerate(
                        [
                            "file:///etc/passwd",
                            "ftp://x/v1",
                            "https://ok.svc:443/v1",
                            "plain.svc:8000",
                        ]
                    )
                ]
            }

        agent.kube_get = listing
        agent.node_addresses = lambda a, t: {}
        self.assertEqual(
            agent.kube_candidates(args()),
            ["https://ok.svc:443/v1", "http://plain.svc:8000/v1"],
        )


class ModelsProbe(unittest.TestCase):
    # Breaks if the probe stops recording what the endpoint serves, which is
    # where the model part of every name comes from.
    def test_probe_records_the_first_model(self):
        class Handler(http.server.BaseHTTPRequestHandler):
            def do_GET(self):
                body = json.dumps(
                    {"data": [{"id": "nvidia/Qwen3.6-35B-A3B-NVFP4"}, {"id": "draft"}]}
                ).encode()
                self.send_response(200)
                self.send_header("content-type", "application/json")
                self.end_headers()
                self.wfile.write(body)

            def log_message(self, *a):
                pass

        server = http.server.HTTPServer(("127.0.0.1", 0), Handler)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        try:
            base = f"http://127.0.0.1:{server.server_port}/v1"
            agent.MODEL_OF.clear()
            self.assertTrue(agent.serves_models(base, 2))
            self.assertEqual(agent.MODEL_OF[base], "nvidia/Qwen3.6-35B-A3B-NVFP4")
        finally:
            server.shutdown()
            server.server_close()


if __name__ == "__main__":
    unittest.main()
