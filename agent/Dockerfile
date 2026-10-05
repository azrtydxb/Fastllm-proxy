# syntax=docker/dockerfile:1.7
#
# The node agent ships as its own image: python:3.13-slim plus the script,
# nothing else. It is stdlib-only on purpose (see the script's docstring), so
# the image exists to give the script a stable entrypoint and a digest a
# deployment can pin — not to install anything.
FROM python:3.13-slim

COPY agent/fastllm-node-agent.py /opt/fastllm/fastllm-node-agent.py

ENTRYPOINT ["python3", "/opt/fastllm/fastllm-node-agent.py"]
