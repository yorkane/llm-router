# llm-watcher image: the daemon is one stdlib-only Python file, so there is nothing to
# compile and no pip layer -- the image is the base plus that file (~1MB over the base).
#
# build (base comes from ACR, not docker.io):
#   docker build -t wasu-wtvdev-registry-test-registry.cn-hangzhou.cr.aliyuncs.com/pub/llm-watcher:latest .
#
# it runs with --network host and usually -v /var/run/docker.sock, see deploy/README.md.
ARG BASE=wasu-wtvdev-registry-test-registry.cn-hangzhou.cr.aliyuncs.com/pub/python:3.12-alpine
FROM ${BASE}

COPY llm_watcher.py /opt/llm-watcher/llm_watcher.py

# Everything below is a default; override any of them with -e LLM_WATCHER_... at run time.
ENV PYTHONUNBUFFERED=1 \
    PYTHONOPTIMIZE=1 \
    LLM_WATCHER_ROUTER=http://127.0.0.1:8800 \
    LLM_WATCHER_STATE_DIR=/var/lib/llm-watcher \
    LLM_WATCHER_INTERVAL=15 \
    LLM_WATCHER_REMOVE_GRACE=300 \
    LLM_WATCHER_METRICS_PORT=9912

EXPOSE 9912
# The ledger is the daemon's memory of what it owns: lose it and it forgets which
# workers it may delete (and its protection snapshot), so keep it on a volume.
VOLUME ["/var/lib/llm-watcher"]

# Freshness rather than liveness: the process answering is not interesting, a reconcile
# loop that stopped updating the ledger is. start-period covers a slow first pass plus
# --add-confirm-timeout. Does not apply with --dry-run, which writes no ledger.
HEALTHCHECK --interval=30s --timeout=5s --start-period=90s --retries=3 \
  CMD python3 -c "import json,os,sys,time; p=os.path.join(os.environ.get('LLM_WATCHER_STATE_DIR','/var/lib/llm-watcher'),'ledger.json'); d=json.load(open(p)); sys.exit(0 if time.time()-d.get('updated_at',0) < 6*float(os.environ.get('LLM_WATCHER_INTERVAL','15')) else 1)" || exit 1

# Args after the image name reach the daemon, so flags still work: run IMAGE --dry-run --once
ENTRYPOINT ["python3", "/opt/llm-watcher/llm_watcher.py"]
