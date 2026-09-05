# llm-router runtime image: standalone LLM reverse proxy (cache-aware routing).
# The `smg` binary is produced by .github/workflows/build.yml (cargo --profile ci --bin smg).
FROM ubuntu:24.04
RUN apt-get update \
 && apt-get install -y --no-install-recommends libssl3 libgcc-s1 ca-certificates curl \
 && rm -rf /var/lib/apt/lists/*
ARG BIN=target/ci/smg
ENV ROUTER_PORT=8801
COPY ${BIN} /usr/local/bin/smg
RUN chmod +x /usr/local/bin/smg
EXPOSE 8801 29001
HEALTHCHECK --interval=30s --timeout=5s --start-period=15s CMD curl -sf http://127.0.0.1:${ROUTER_PORT:-8801}/health || exit 1
ENTRYPOINT ["smg", "launch"]
CMD ["--host", "0.0.0.0", "--port", "8801", "--backend", "sglang", "--policy", "cache_aware"]
