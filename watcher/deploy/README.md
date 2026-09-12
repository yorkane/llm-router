# Deploy assets

- [Production compose](../../deploy/docker-compose.yml) -- the pair on this box: `llm-router`
  (:8800, with `--enable-igw`) and `llm-watcher` (host networking, `docker.sock` read-only,
  remote instances via `LLM_WATCHER_TARGETS`). `docker compose -f deploy/docker-compose.yml up -d`
  starts both; `restart: unless-stopped` brings them back after a reboot. Preferred when the
  router is a container, because router and watcher then live and die as one project.
- [llm-watcher.service](llm-watcher.service) -- the alternative for a bare host (or a
  non-container router): no image, no docker socket mount, logs in the journal, and resource
  caps so a bug here cannot take the machine down. Read it before installing: it pins the
  router URL and the state dir.
- The docker run in the parent [README](../README.md) if you would rather not add a unit to
  the host. Mount the docker socket read-only and keep `--network host`, otherwise
  `/proc/net/tcp` shows the container's own namespace and discovery finds nothing.
